/*!
 * radar.js — MXrader 雷达前端应用
 *
 * 运行时依赖：web/leaflet-lite.js（本地内联，零外网依赖）、web/maps.json（每图标定）。
 * 数据来源：GET /ws（WebSocket），消息 schema 见 docs/INTERFACES.md §5。
 *
 * 单位与坐标系假设（与 INTERFACES.md §5 对齐）：
 *   - 玩家条目里的 x / y / z / vx / vy / vz 一律 SI 单位「米」，世界为 Z 轴向上（UE 风格）。
 *   - 平面雷达只使用 x（东向）与 z（北向）两个分量；y 是高度，仅在 3D 修正里当 pitch 的近似输入。
 *   - yaw / pitch / roll 单位为「度」。yaw 在服务器侧已做转向修正：0 = 正北，顺时针为正。
 *     yaw_sign / yaw_offset_deg（maps.json）用于再校正一次，是「3D 转向修正」的两个旋钮。
 *   - world → 地图经纬度：lng = origin_x + x * scale，lat = origin_y + z * scale，
 *     其中 origin_* 为「度」，scale 为「度/米」（等距圆柱近似，见 web/tiles/README.md §4）。
 *   - distance 直接采用服务端给的米值；缺失时用 self 与目标的平面欧氏距离兜底。
 *
 * 代码注释使用英文；所有面向用户的文案使用简体中文。
 */
(function () {
  'use strict';

  /* ========================================================================
   * 0. 常量 / 小工具
   * ====================================================================== */

  var STALE_MS = 10000;        // 10s without an update -> entry is dropped
  var INTERP_MS = 150;         // smooth decay window between two samples
  var FRAME_MS = 33;           // ~30 fps render cadence
  var KILL_LIMIT = 40;
  var TRACE_LIMIT = 48;
  var BACKOFF_MIN = 250;       // reconnect backoff floor (ms)
  var BACKOFF_MAX = 8000;      // reconnect backoff ceiling (ms)

  var LS_PREFIX = 'mxrader.radar.';

  var DAMAGE_CN = {
    EKilledByWeapon: '武器击杀',
    EkilledBySelf: '自伤',
    EkilledByPoisonGas: '毒气',
    EKilledFallDown: '坠落',
    EKilledFromImpendingDeath: '濒死',
    EKilledFromBuff: '增益',
    EKilledByGm: '管理员',
    EKilledFromEnvExplosion: '环境爆炸',
    EKilledByVehicleWeapon: '载具武器',
    EKilledByAssassinateDamage: '处决',
    EKilledByBattleFieldSupportSkill: '战场支援',
    EKilledBySectorArtilerrateSkill: '区域炮击',
    EKilledByGuidedMissleSkill: '制导导弹'
  };

  // Damage enum keys as seen from the game, plus the common variants found in
  // capture traces (capitalisation differs between UE versions).
  var DAMAGE_ALIASES = {
    EKilledByWeaponDamage: 'EKilledByWeapon',
    EKilledByVehicle: 'EKilledByVehicleWeapon',
    EKilledFromEnvironmentExplosion: 'EKilledFromEnvExplosion',
    EKilledBySectorArtillerySkill: 'EKilledBySectorArtilerrateSkill',
    EKilledByGuidedMissileSkill: 'EKilledByGuidedMissleSkill',
    EKilledByBattlefieldSupportSkill: 'EKilledByBattleFieldSupportSkill'
  };

  function $(id) { return document.getElementById(id); }

  function clamp(v, lo, hi) { return v < lo ? lo : (v > hi ? hi : v); }

  function num(v, fallback) {
    var n = Number(v);
    return isFinite(n) ? n : (fallback == null ? 0 : fallback);
  }

  function readLS(key, fallback) {
    try {
      var raw = window.localStorage.getItem(LS_PREFIX + key);
      return raw == null ? fallback : JSON.parse(raw);
    } catch (e) { return fallback; }
  }

  function writeLS(key, value) {
    try { window.localStorage.setItem(LS_PREFIX + key, JSON.stringify(value)); } catch (e) { /* quota/private mode */ }
  }

  function fmt(n, digits) {
    if (!isFinite(n)) return '—';
    return n.toFixed(digits == null ? 1 : digits);
  }

  function fmtDistance(m) {
    if (!isFinite(m)) return '—';
    if (m >= 1000) return fmt(m / 1000, 2) + ' km';
    return fmt(m, m < 100 ? 1 : 0) + ' m';
  }

  function fmtClock(ms) {
    var d = new Date(ms || Date.now());
    function p(n) { return (n < 10 ? '0' : '') + n; }
    return p(d.getHours()) + ':' + p(d.getMinutes()) + ':' + p(d.getSeconds());
  }

  function damageCN(code) {
    if (!code) return '未知';
    var key = DAMAGE_ALIASES[code] || code;
    return DAMAGE_CN[key] || String(code);
  }

  // 坐标规范化：任意 [?,?] / {x,y} / {lat,lng} 都收敛成 {lat,lng}
  function ll(a, b) {
    if (a == null) return { lat: 0, lng: 0 };
    if (Array.isArray(a)) return { lat: num(a[0]), lng: num(a[1]) };
    if (typeof a === 'object') {
      if (typeof a.lat === 'number') return { lat: num(a.lat), lng: num(a.lng) };
      if (typeof a.x === 'number') return { lat: num(a.y), lng: num(a.x) };
      if (typeof a[0] === 'number') return { lat: num(a[0]), lng: num(a[1]) };
    }
    return { lat: 0, lng: 0 };
  }

  /* ========================================================================
   * 1. 全局状态
   * ====================================================================== */

  var state = {
    ready: false,
    readOnly: false,
    brand: 'mx',
    mapKey: 'ZeroDam',
    tileTemplate: '/tiles/{map}/{z}/{x}/{y}.png',
    // hello.world: degrees per metre + degree origins, see header comment.
    // Defaults are IDENTITY for yaw (the server already emits
    // "0 = north, clockwise positive" per INTERFACES.md §5).
    world: { origin_x: 0, origin_y: 0, scale: 0.00001, yaw_offset_deg: 0 },
    yawSign: 1,
    calibration: null,
    serverWorldProvided: false,
    maps: {},
    selfUuid: null,
    kills: [],
    killKeys: {},
    traces: [],
    tick: 0,
    serverTs: 0,
    lastMsgTs: 0,
    wsUrl: '',
    wsState: '未连接',
    reconnects: 0,
    nextRetryMs: 0,
    centered: false,
    loopStarted: false
  };

  var ui = {
    mode: readLS('mode', 'north'),           // north | follow | 3d
    selectedUuid: null,
    autoCenter: readLS('autoCenter', true),
    search: readLS('search', ''),
    panels: readLS('panels', { legend: false, players: false, diag: false, netlog: false })
  };

  var LAYERS = [
    { key: 'players', label: '玩家', dot: 'var(--friend)', def: true },
    { key: 'ai', label: '人机', dot: 'var(--ai)', def: true },
    { key: 'deathbox', label: '死亡盒', dot: 'var(--deathbox)', def: true },
    { key: 'loot', label: '物资', dot: 'var(--loot)', def: true },
    { key: 'traces', label: '弹道', dot: 'var(--trace)', def: true },
    { key: 'teammates', label: '队友', dot: 'var(--friend)', def: true },
    { key: 'enemies', label: '敌人', dot: 'var(--enemy)', def: true },
    { key: 'rings', label: '距离圈', dot: 'var(--amber)', def: true },
    { key: 'arrows', label: '朝向箭头', dot: 'var(--text)', def: true }
  ];

  var layerOn = {};
  (function initLayers() {
    var saved = readLS('layers', null);
    LAYERS.forEach(function (l) {
      layerOn[l.key] = saved && typeof saved[l.key] === 'boolean' ? saved[l.key] : l.def;
    });
  })();

  var diag = {
    counters: null,
    msgCounts: { hello: 0, state: 0, diag: 0, bye: 0, other: 0 },
    frames: 0,
    fps: 0,
    tilesLoaded: 0,
    lastError: '—'
  };

  var events = [];           // recent network/UI events for the 网络日志 panel
  var lastRawJSON = '—';

  function logEvent(text) {
    events.unshift('[' + fmtClock(Date.now()) + '] ' + text);
    if (events.length > 60) events.length = 60;
    schedulePanelRefresh();
  }

  /* ========================================================================
   * 2. 投影 / 转向修正
   * ====================================================================== */

  // worldToLatLng: metres (x = east, z = north) -> [lat, lng] in degrees.
  // Both axes share `scale` (degrees per metre) — the map patch is modelled as
  // a small equirectangular rectangle, see web/tiles/README.md §4.
  function worldToLatLng(x, z) {
    var w = state.world;
    return [w.origin_y + num(z) * w.scale, w.origin_x + num(x) * w.scale];
  }

  function latLngToWorld(lat, lng) {
    var w = state.world;
    var s = w.scale || 1e-5;
    return { x: (num(lng) - w.origin_x) / s, z: (num(lat) - w.origin_y) / s };
  }

  // Degrees per metre on each axis, used for metre-accurate circles/rings.
  function metresToDegLat() { return state.world.scale; }
  function metresToDegLng() { return state.world.scale; }

  // Metre radius -> map latlng radius, done by offsetting the centre.
  function circleLatLngs(centre, radiusM, segments) {
    var seg = segments || 48;
    var out = [];
    var dLat = radiusM * metresToDegLat();
    var dLng = radiusM * metresToDegLng();
    for (var i = 0; i <= seg; i++) {
      var a = (i / seg) * Math.PI * 2;
      out.push([centre.lat + Math.cos(a) * dLat, centre.lng + Math.sin(a) * dLng]);
    }
    return out;
  }

  function normalizeDeg(d) {
    d = num(d) % 360;
    if (d > 180) d -= 360;
    if (d < -180) d += 360;
    return d;
  }

  /**
   * applyRotationCorrection — 3D 转向修正。
   *
   * heading = normalize(yaw_sign * rawYaw + yaw_offset_deg)  (0 = north, cw +)
   *   1) yaw_offset_deg / yaw_sign  : UE's yaw origin offset + handedness.
   *   2) "跟随朝向" mode             : on-screen heading = world heading - mapRotation,
   *                                    because the map itself is rotated by -selfHeading.
   *   3) pitch foreshortening       : the ground trace of the view vector
   *                                    (cos p, sin p, 0) shrinks by cos(pitch); the arrow
   *                                    keeps its direction but loses length as the player
   *                                    looks further up/down. roll tilts it on screen.
   *
   * @param {object} player  entry with .yaw/.pitch/.roll (degrees, server-corrected)
   * @param {object} self    the local player entry (may be null)
   * @param {object} camera  { mode: 'north'|'follow'|'3d', mapRotation: deg }
   * @returns {{heading:number, foreshortening:number, roll:number}}
   */
  function applyRotationCorrection(player, self, camera) {
    var w = state.world;
    var mode = (camera && camera.mode) || 'north';
    var sign = (num(state.yawSign, 1) >= 0 ? 1 : -1);

    // (a) UE yaw origin offset + handedness.
    var rawYaw = num(player && player.yaw, 0) * sign;
    var offset = num(w.yaw_offset_deg, 0);
    var heading = rawYaw + offset;

    // (b) local player's own heading rotation while the map follows the view.
    if (mode === 'follow' || mode === '3d') {
      var follow = self || player;
      if (follow) {
        var selfHeading = num(follow.yaw, 0) * sign + offset;
        var mapRot = (camera && isFinite(camera.mapRotation)) ? camera.mapRotation : -selfHeading;
        heading -= mapRot;    // counter-rotate: screenHeading = world - mapRotation
      }
    }

    // (c) pitch-dependent foreshortening of the forward vector's ground trace.
    var foreshortening = 1;
    var rollOut = 0;
    if (mode === '3d') {
      var pitch = clamp(num(player && player.pitch, 0), -89, 89);
      var roll = clamp(num(player && player.roll, 0), -89, 89);
      // cos(pitch) == |horizontal component| of a unit forward vector.
      foreshortening = clamp(Math.abs(Math.cos(pitch * Math.PI / 180)), 0.15, 1);
      // At full pitch the horizontal part degenerates: 0° means nose-up, 180° nose-down.
      if (num(player && player.pitch, 0) < 0) heading += 180 * (1 - foreshortening);
      rollOut = roll * foreshortening;
    }

    return { heading: normalizeDeg(heading), foreshortening: foreshortening, roll: rollOut };
  }

  /* ========================================================================
   * 3. 玩家表 / 插值
   * ====================================================================== */

  var table = Object.create(null);   // uuid -> entry
  var entryList = [];                // hot copy of table values, rebuilt on change

  function kindCN(kind) {
    if (kind === 'ai') return '人机';
    if (kind === 'deathbox') return '死亡盒';
    if (kind === 'loot') return '物资';
    return '玩家';
  }

  function isEnemyOfSelf(e) {
    var self = table[selfUuid()];
    if (!self) return false;
    if (e.team != null && self.team != null) return num(e.team) !== num(self.team);
    if (e.camp != null && self.camp != null) return num(e.camp) !== num(self.camp);
    return false;
  }

  function selfUuid() { return state.selfUuid || null; }

  function upsertPlayer(p) {
    if (!p || !p.uuid) return null;
    var prev = table[p.uuid];
    var now = Date.now();
    var tx = num(p.x), tz = num(p.z);
    if (!prev) {
      prev = table[p.uuid] = {
        uuid: p.uuid,
        kind: p.kind || 'player',
        // interpolation state
        px: tx, pz: tz, cx: tx, cz: tz, interp: 1,
        firstSeen: now,
        kills: 0, killed: 0
      };
      for (var k in p) { if (Object.prototype.hasOwnProperty.call(p, k)) prev[k] = p[k]; }
      entryList.push(prev);
      logEvent('新增目标 ' + (p.name || p.uuid) + ' (' + kindCN(prev.kind) + ')');
    } else {
      // Start a new interpolation segment from the current visual position.
      prev.px = prev.cx;
      prev.pz = prev.cz;
      prev.interp = 0;
      for (var k2 in p) { if (Object.prototype.hasOwnProperty.call(p, k2)) prev[k2] = p[k2]; }
    }
    prev.tx = tx;
    prev.tz = tz;
    prev.kind = p.kind || prev.kind || 'player';
    prev.lastSeen = now;
    return prev;
  }

  function prunePlayers() {
    var now = Date.now();
    var dropped = false;
    for (var uuid in table) {
      var e = table[uuid];
      if (now - num(e.lastSeen, 0) > STALE_MS) {
        delete table[uuid];
        dropped = true;
        logEvent('丢失目标 ' + (e.name || uuid));
      }
    }
    if (dropped) rebuildEntryList();
  }

  function rebuildEntryList() {
    entryList = [];
    for (var uuid in table) entryList.push(table[uuid]);
    // 距离升序，保证列表顺序稳定
    entryList.sort(function (a, b) { return num(a._dist, 1e9) - num(b._dist, 1e9); });
  }

  /* ========================================================================
   * 4. 地图 / 图层
   * ====================================================================== */

  var map = null;
  var tileLayer = null;
  var vectors = {};       // per-uuid marker layers, keyed "m:"+uuid
  var ringLayers = [];
  var tracePool = [];
  var arrowPool = {};

  var RING_RADII = [50, 100, 200, 400, 800];

  function mapCentreLatLng() {
    var c = state.calibration;
    if (c && c.max_x > c.min_x && c.max_y > c.min_y) {
      var cx = (c.min_x + c.max_x) / 2;
      var cz = (c.min_y + c.max_y) / 2;
      return ll(worldToLatLng(cx, cz));
    }
    return ll(worldToLatLng(0, 0));
  }

  function autoZoomLevel() {
    var c = state.calibration;
    var spanM = (c && c.max_x > c.min_x) ? Math.max(c.max_x - c.min_x, c.max_y - c.min_y) : 1200;
    var size = map ? map.getSize() : { x: 390, y: 700 };
    var pxPerM = Math.min(size.x, size.y) / Math.max(spanM, 1) * 0.9;
    // world px per metre at zoom 0 == metresToDegLng() * 256
    var pxPerMAtZoom0 = metresToDegLng() * 256;
    return clamp(Math.log2(Math.max(pxPerM, 1e-9) / pxPerMAtZoom0), 1, 12);
  }

  function initMap() {
    var host = $('map');
    if (!host || !window.L) {
      fail('地图引擎 leaflet-lite.js 未加载');
      return false;
    }
    map = window.L.map(host, {
      zoom: autoZoomLevel(),
      center: mapCentreLatLng(),
      minZoom: 1,
      maxZoom: 14,
      background: '#0c0d12',
      preferCanvas: true
    });

    // Metre -> container pixel ratio; leaflet-lite uses it for L.circle radii.
    map._metresToPx = function () {
      return map._worldSize() * metresToDegLng();
    };
    map.unitsPerMeter = map._metresToPx;

    tileLayer = window.L.tileLayer(state.tileTemplate, { mapKey: state.mapKey, opacity: 1 });
    tileLayer.addTo(map);
    // 命中计数：以 Image 解码完成数衡量底图覆盖情况
    map.on('tilesloaded', function () { diag.tilesLoaded++; });

    map.on('mousemove', onMapMouseMove);
    map.on('click', onMapClick);
    map.on('zoomend', updateScaleBar);
    map.on('move', updateScaleBar);
    map.on('resize', updateScaleBar);

    // 距离圈（以自身为中心）
    for (var i = 0; i < RING_RADII.length; i++) {
      var ring = window.L.polyline([], {
        color: 'var(--amber)',
        weight: 1,
        opacity: 0.28,
        dashArray: '4 6',
        fill: false,
        visible: layerOn.rings
      });
      ring._radiusM = RING_RADII[i];
      ring.addTo(map);
      ringLayers.push(ring);
    }

    buildMarkers();
    return true;
  }

  function buildMarkers() {
    // Marker layers are created lazily per player in syncMarkers().
    updateScaleBar();
  }

  var markerKeys = {};

  function shapeFor(e) {
    if (e.uuid === selfUuid()) return 'ring';
    if (e.kind === 'deathbox') return 'square';
    if (e.kind === 'loot') return 'diamond';
    return 'triangle';
  }

  function colorFor(e) {
    if (!e.alive) return 'var(--muted)';
    if (e.uuid === selfUuid()) return 'var(--self)';
    if (e.kind === 'ai') return 'var(--ai)';
    if (e.kind === 'deathbox') return 'var(--deathbox)';
    if (e.kind === 'loot') return 'var(--loot)';
    return isEnemyOfSelf(e) ? 'var(--enemy)' : 'var(--friend)';
  }

  function visibleByLayer(e) {
    if (e.kind === 'deathbox') return layerOn.deathbox;
    if (e.kind === 'loot') return layerOn.loot;
    if (e.kind === 'ai') return layerOn.ai && (layerOn.enemies || !isEnemyOfSelf(e));
    if (layerOn.players !== true) return false;
    return isEnemyOfSelf(e) ? layerOn.enemies : layerOn.teammates;
  }

  function tooltipFor(e) {
    var bits = [];
    bits.push(e.name || e.uuid);
    bits.push(kindCN(e.kind));
    if (isFinite(num(e._dist, NaN))) bits.push(fmtDistance(e._dist));
    if (isFinite(num(e.hp, NaN))) bits.push('HP ' + fmt(num(e.hp), 0) + '/' + fmt(num(e.max_hp, 100), 0));
    if (e.weapon) bits.push(String(e.weapon));
    if (e.last_seen_ms) bits.push('视距 ' + fmt((Date.now() - num(e.last_seen_ms)) / 1000, 1) + 's 前');
    if (e.source) bits.push(e.source);
    return bits.join(' · ');
  }

  function syncMarkers() {
    var used = {};
    for (var i = 0; i < entryList.length; i++) {
      var e = entryList[i];
      var key = 'm:' + e.uuid;
      var visible = visibleByLayer(e);
      var layer = vectors[key];
      if (!visible) {
        if (layer) { layer.remove(); delete vectors[key]; }
        continue;
      }
      if (!layer) {
        layer = window.L.marker([0, 0], {
          shape: shapeFor(e),
          radius: shapeFor(e) === 'triangle' ? 7 : 6,
          color: colorFor(e),
          fillColor: colorFor(e),
          fillOpacity: e.alive ? 0.85 : 0.35,
          weight: 1.5,
          stroke: true
        });
        layer._uuid = e.uuid;
        layer.addTo(map);
        vectors[key] = layer;
      }
      var latlng = worldToLatLng(e.cx, e.cz);
      layer.setLatLng(latlng);

      var cam = { mode: ui.mode, mapRotation: currentMapRotation() };
      var corr = applyRotationCorrection(e, table[selfUuid()], cam);
      var shape = shapeFor(e);
      var color = colorFor(e);
      var fade = clamp(1 - (Date.now() - num(e.lastSeen, Date.now())) / STALE_MS, 0.25, 1);
      if (!e.alive) {
        shape = 'cross';
        fade *= 0.6;
      } else if (e.visible === false) {
        fade *= 0.45;
      }
      layer.setStyle({
        shape: shape,
        color: color,
        fillColor: color,
        fillOpacity: e.alive ? 0.85 * fade : 0.25 * fade,
        opacity: fade,
        weight: shape === 'ring' ? 2.5 : 1.5,
        radius: (shape === 'triangle' ? 7 : 6) * (shape === 'triangle' ? corr.foreshortening : 1),
        rotation: shape === 'triangle' ? corr.heading * Math.PI / 180 : 0,
        visible: true
      });
      layer.bindTooltip(tooltipFor(e));
      used[key] = true;
    }
    for (var k in vectors) {
      if (!used[k]) { vectors[k].remove(); delete vectors[k]; }
    }
    syncArrows();
  }

  function syncArrows() {
    var used = {};
    for (var i = 0; i < entryList.length; i++) {
      var e = entryList[i];
      if (!layerOn.arrows) continue;
      if (!e.alive || !visibleByLayer(e)) continue;
      if (e.kind === 'loot' || e.kind === 'deathbox') continue;
      var cam = { mode: ui.mode, mapRotation: currentMapRotation() };
      var corr = applyRotationCorrection(e, table[selfUuid()], cam);
      var lenM = clamp(num(e._dist, 30), 12, 60);        // arrow length in metres
      var rad = corr.heading * Math.PI / 180;
      // World metres: heading 0 = +z (north), clockwise -> +x (east)
      var dx = Math.sin(rad) * lenM * corr.foreshortening;
      var dz = Math.cos(rad) * lenM * corr.foreshortening;
      var pts = [
        worldToLatLng(e.cx, e.cz),
        worldToLatLng(e.cx + dx, e.cz + dz)
      ];
      var layer = arrowPool[e.uuid];
      if (!layer) {
        layer = window.L.polyline(pts, {
          color: colorFor(e), weight: 2, opacity: 0.75, fill: false, dashArray: '3 3'
        });
        layer.addTo(map);
        arrowPool[e.uuid] = layer;
      } else {
        layer.setLatLngs(pts);
        layer.setStyle({ color: colorFor(e), opacity: 0.75, visible: true });
      }
      used[e.uuid] = true;
    }
    for (var k in arrowPool) {
      if (!used[k]) { arrowPool[k].remove(); delete arrowPool[k]; }
    }
  }

  function currentMapRotation() {
    if (ui.mode === 'north') return 0;
    var focus = table[selfUuid()];
    if (!focus) return 0;
    var corr = applyRotationCorrection(focus, focus, { mode: ui.mode, mapRotation: 0 });
    return -corr.heading;
  }

  function applyMapRotation() {
    if (!map) return;
    var rot = currentMapRotation();
    var xform = rot ? 'rotate(' + rot.toFixed(3) + 'deg)' : '';
    map._tilePane.style.transformOrigin = '50% 50%';
    map._overlayPane.style.transformOrigin = '50% 50%';
    map._tilePane.style.transform = xform;
    map._overlayPane.style.transform = xform;
    // Publish the angle so leaflet-lite can un-rotate pointer coordinates.
    map._rotationDeg = rot;
  }

  function syncRings() {
    var self = table[selfUuid()];
    var centre = self ? ll(worldToLatLng(self.cx, self.cz)) : ll(mapCentreLatLng());
    for (var i = 0; i < ringLayers.length; i++) {
      if (!layerOn.rings) {
        ringLayers[i]._style.visible = false;
        ringLayers[i].setLatLngs([]);
        continue;
      }
      ringLayers[i]._style.visible = true;
      ringLayers[i].setLatLngs(circleLatLngs(centre, ringLayers[i]._radiusM, 64));
    }
    if (map) map._invalidateVectors();
  }

  /* ========================================================================
   * 5. 弹道
   * ====================================================================== */

  // traces[] schema is not pinned by INTERFACES.md §5, so each field is read
  // with a small alias list (documented in the room README) and defaulted:
  //   uuid/owner, weapon, speed/InitSpeed, yaw/FireRotation.yaw, pitch, origin/start.
  var TRACE_FIELDS = {
    uuid: ['uuid', 'owner', 'owner_uuid', 'shooter', 'instigator'],
    weapon: ['weapon', 'weapon_name'],
    speed: ['init_speed', 'InitSpeed', 'speed', 'muzzle_speed'],
    yaw: ['yaw', 'fire_yaw', 'FireYaw'],
    pitch: ['pitch', 'fire_pitch', 'FirePitch'],
    startX: ['start_x', 'startX', 'origin_x', 'fire_x', 'x'],
    startY: ['start_y', 'startY', 'origin_y', 'fire_y', 'y'],
    startZ: ['start_z', 'startZ', 'origin_z', 'fire_z', 'z'],
    endX: ['end_x', 'endX', 'hit_x'],
    endZ: ['end_z', 'endZ', 'hit_z'],
    ts: ['ts', 'time', 'timestamp', 'ts_ms']
  };

  function pick(obj, aliases) {
    if (!obj) return undefined;
    for (var i = 0; i < aliases.length; i++) {
      var a = aliases[i];
      if (obj[a] != null) return obj[a];
    }
    return undefined;
  }

  function traceGeom(t, owner) {
    var sx = num(pick(t, TRACE_FIELDS.startX), owner ? owner.cx : 0);
    var sz = num(pick(t, TRACE_FIELDS.startZ), owner ? owner.cz : 0);
    var ex = pick(t, TRACE_FIELDS.endX);
    var ez = pick(t, TRACE_FIELDS.endZ);
    var yaw = num(pick(t, TRACE_FIELDS.yaw), owner ? owner.yaw : 0);
    var pitch = num(pick(t, TRACE_FIELDS.pitch), owner ? owner.pitch : -5);
    var speed = num(pick(t, TRACE_FIELDS.speed), 0);

    if (ex == null || ez == null) {
      // Reconstruct the ballistic direction from v = forward(FireRotation)*InitSpeed
      // + OwnerVelocity, then take a bounded ground trace as the drawn segment.
      var vyaw = yaw * (num(state.yawSign, 1) || 1) + num(state.world.yaw_offset_deg, 0);
      var rad = vyaw * Math.PI / 180;
      var pr = pitch * Math.PI / 180;
      var vGround = Math.cos(pr) * (speed > 0 ? speed : 120);
      var vx = Math.sin(rad) * vGround + num(owner ? owner.vx : 0);
      var vz = Math.cos(rad) * vGround + num(owner ? owner.vz : 0);
      var len = Math.hypot(vx, vz) || 1;
      var drawLen = clamp(speed > 0 ? vGround * 0.25 : 60, 12, 240);
      ex = sx + (vx / len) * drawLen;
      ez = sz + (vz / len) * drawLen;
    }
    return { sx: sx, sz: sz, ex: num(ex), ez: num(ez), yaw: yaw, pitch: pitch, speed: speed };
  }

  function ingestTraces(traces) {
    if (!Array.isArray(traces)) return;
    for (var i = 0; i < traces.length; i++) {
      var t = traces[i];
      if (!t || typeof t !== 'object') continue;
      var uuid = pick(t, TRACE_FIELDS.uuid);
      var owner = uuid ? table[uuid] : null;
      var g = traceGeom(t, owner);
      state.traces.push({
        g: g,
        weapon: pick(t, TRACE_FIELDS.weapon) || (owner && owner.weapon) || '',
        born: Date.now()
      });
    }
    while (state.traces.length > TRACE_LIMIT) state.traces.shift();
  }

  function syncTraces() {
    if (!layerOn.traces) {
      tracePool.forEach(function (l) { l.remove(); });
      tracePool = [];
      state.traces.length = 0;
      return;
    }
    var now = Date.now();
    var keep = [];
    for (var i = 0; i < state.traces.length; i++) {
      if (now - state.traces[i].born <= 4000) keep.push(state.traces[i]);
    }
    state.traces = keep;
    var n = state.traces.length;

    while (tracePool.length < n * 2) {
      var extra = window.L.polyline([], { color: 'var(--trace)', weight: 1.6, opacity: 0.9, fill: false });
      extra.addTo(map);
      tracePool.push(extra);
    }

    for (var j = 0; j < n; j++) {
      var tr = state.traces[j];
      var age = clamp((now - tr.born) / 4000, 0, 1);
      var alpha = (1 - age) * 0.9;
      var g = tr.g;
      // Main segment: reconstructed fire origin -> impact / draw limit.
      var main = tracePool[j * 2];
      main.setLatLngs([worldToLatLng(g.sx, g.sz), worldToLatLng(g.ex, g.ez)]);
      main.setStyle({ opacity: alpha, weight: 1 + (1 - age) * 1.6, visible: true });

      // Origin arrow: short thick stub pointing back along the fire direction,
      // marking the reconstructed muzzle position from v = forward*InitSpeed.
      var rad = (g.yaw * (num(state.yawSign, 1) || 1) + num(state.world.yaw_offset_deg, 0)) * Math.PI / 180;
      var back = 6;
      var tip = tracePool[j * 2 + 1];
      tip.setLatLngs([
        worldToLatLng(g.sx, g.sz),
        worldToLatLng(g.sx - Math.sin(rad) * back, g.sz - Math.cos(rad) * back)
      ]);
      tip.setStyle({ opacity: alpha, weight: 3, visible: true });
    }

    for (var k = n * 2; k < tracePool.length; k++) {
      tracePool[k]._style.visible = false;
      tracePool[k].setLatLngs([]);
    }
  }

  /* ========================================================================
   * 6. 渲染循环
   * ====================================================================== */

  var lastFrame = 0;
  var fpsWindowStart = 0;
  var fpsWindowFrames = 0;

  function interpolate() {
    var now = Date.now();
    var dt = lastFrame ? (now - lastFrame) : FRAME_MS;
    lastFrame = now;
    for (var i = 0; i < entryList.length; i++) {
      var e = entryList[i];
      var t = clamp(e.interp + dt / INTERP_MS, 0, 1);
      // Exponential-ish ease so motion decays smoothly into the newest sample.
      var eased = 1 - Math.pow(1 - t, 3);
      e.cx = e.px + (e.tx - e.px) * eased;
      e.cz = e.pz + (e.tz - e.pz) * eased;
      e.interp = t;
      // Derived values used by the list / tooltip.
      var self = table[selfUuid()];
      if (self) {
        e._dist = Math.hypot(e.cx - self.cx, e.cz - self.cz);
      } else if (e.distance != null) {
        e._dist = num(e.distance);
      } else {
        e._dist = NaN;
      }
    }
  }

  function frame() {
    if (!state.ready) return;
    requestAnimationFrame(frame);
    var now = Date.now();
    if (now - lastFrame < FRAME_MS - 2) return;

    interpolate();
    prunePlayers();
    // 跟随朝向 / 3D：地图跟着自身走，否则拖拽后的视野会被立刻拉回
    if ((ui.mode === 'follow' || ui.mode === '3d') && ui.autoCenter) {
      var self = table[selfUuid()];
      if (self && map) map.panTo(worldToLatLng(self.cx, self.cz));
    }
    syncMarkers();
    syncRings();
    syncTraces();
    applyMapRotation();
    updateHud();

    fpsWindowFrames++;
    if (!fpsWindowStart) fpsWindowStart = now;
    if (now - fpsWindowStart >= 1000) {
      diag.fps = fpsWindowFrames * 1000 / (now - fpsWindowStart);
      fpsWindowFrames = 0;
      fpsWindowStart = now;
    }
  }

  /* ========================================================================
   * 7. HUD
   * ====================================================================== */

  var hudNodes = {};
  var panelDirty = false;

  function schedulePanelRefresh() {
    if (panelDirty) return;
    panelDirty = true;
    setTimeout(function () { panelDirty = false; refreshPanels(); }, 250);
  }

  function buildSwitches() {
    var host = $('layer-switches');
    if (!host) return;
    host.textContent = '';
    LAYERS.forEach(function (l) {
      var label = document.createElement('label');
      label.className = 'switch';
      var input = document.createElement('input');
      input.type = 'checkbox';
      input.checked = !!layerOn[l.key];
      input.dataset.layer = l.key;
      var tick = document.createElement('span');
      tick.className = 'tick';
      var dot = document.createElement('span');
      dot.className = 'dot';
      dot.style.background = l.dot;
      var text = document.createElement('span');
      text.textContent = l.label;
      label.appendChild(input);
      label.appendChild(tick);
      label.appendChild(dot);
      label.appendChild(text);
      host.appendChild(label);
      input.addEventListener('change', function () {
        layerOn[l.key] = input.checked;
        writeLS('layers', layerOn);
        if (l.key === 'traces' && !input.checked) {
          state.traces.length = 0;
        }
      });
    });
  }

  function buildModeSeg() {
    var seg = $('mode-seg');
    if (!seg) return;
    var items = seg.querySelectorAll('.seg-item');
    for (var i = 0; i < items.length; i++) {
      items[i].setAttribute('aria-checked', items[i].dataset.mode === ui.mode ? 'true' : 'false');
      items[i].addEventListener('click', function (ev) {
        setMode(ev.currentTarget.dataset.mode);
      });
    }
  }

  function setMode(mode) {
    ui.mode = mode;
    writeLS('mode', mode);
    var seg = $('mode-seg');
    if (seg) {
      var items = seg.querySelectorAll('.seg-item');
      for (var i = 0; i < items.length; i++) {
        items[i].setAttribute('aria-checked', items[i].dataset.mode === mode ? 'true' : 'false');
      }
    }
    logEvent('切换视角：' + (mode === 'north' ? '2D 北向上' : mode === 'follow' ? '跟随朝向' : '3D'));
    if (mode !== 'north' && ui.autoCenter) centreOnSelf(false);
    applyMapRotation();
  }

  function panel(name, open) {
    var node = $(name);
    if (!node) return;
    if (open == null) {
      open = node.hidden === true;
    }
    node.hidden = !open;
    ui.panels[name] = open;
    writeLS('panels', ui.panels);
  }

  function setCollapsed(which, collapsed) {
    var node = $(which);
    if (!node) return;
    node.classList.toggle('collapsed', !!collapsed);
    var btn = $(which + '-collapse');
    if (btn) btn.textContent = collapsed ? '+' : '−';
    var key = which === 'legend' ? 'legend' : 'players';
    ui[key + 'Collapsed'] = !!collapsed;
    writeLS(key + 'Collapsed', !!collapsed);
  }

  function wireHud() {
    hudNodes.conn = $('conn-badge');
    hudNodes.readonly = $('readonly-badge');
    hudNodes.map = $('stat-map');
    hudNodes.tick = $('stat-tick');
    hudNodes.players = $('stat-players');
    hudNodes.fps = $('stat-fps');
    hudNodes.zoom = $('stat-zoom');
    hudNodes.coords = $('stat-coords');
    hudNodes.scaleLabel = $('scale-label');
    hudNodes.scaleBar = $('scale-bar');
    hudNodes.list = $('player-list');
    hudNodes.empty = $('player-empty');
    hudNodes.kills = $('kill-feed');
    hudNodes.search = $('player-search');
    hudNodes.legendHint = $('legend-hint');
    hudNodes.diagTable = $('diag-table').querySelector('tbody');
    hudNodes.netlogTable = $('netlog-table');
    hudNodes.netlogJson = $('netlog-json');
    hudNodes.netlogEvents = $('netlog-events');
    hudNodes.toast = $('toast');

    $('btn-diag').addEventListener('click', function () { panel('diag-panel'); refreshPanels(); });
    $('btn-netlog').addEventListener('click', function () { panel('netlog-panel'); refreshPanels(); });
    $('diag-close').addEventListener('click', function () { panel('diag-panel', false); });
    $('netlog-close').addEventListener('click', function () { panel('netlog-panel', false); });
    $('btn-legend').addEventListener('click', function () {
      var el = $('legend');
      setCollapsed('legend', !el.classList.contains('collapsed'));
    });
    $('btn-players').addEventListener('click', function () {
      var el = $('players');
      setCollapsed('players', !el.classList.contains('collapsed'));
    });
    $('legend-collapse').addEventListener('click', function () {
      setCollapsed('legend', !$('legend').classList.contains('collapsed'));
    });
    $('players-collapse').addEventListener('click', function () {
      setCollapsed('players', !$('players').classList.contains('collapsed'));
    });
    $('btn-center-self').addEventListener('click', function () { centreOnSelf(true); });

    hudNodes.search.addEventListener('input', function () {
      ui.search = hudNodes.search.value.trim().toLowerCase();
      writeLS('search', ui.search);
      renderPlayerList();
    });

    var autoCenter = $('auto-center');
    if (autoCenter) {
      autoCenter.checked = !!ui.autoCenter;
      autoCenter.addEventListener('change', function () {
        ui.autoCenter = !!autoCenter.checked;
        writeLS('autoCenter', ui.autoCenter);
        if (ui.autoCenter) centreOnSelf(false);
        logEvent('跟随居中：' + (ui.autoCenter ? '开' : '关'));
      });
    }

    if (hudNodes.toast) hudNodes.toast.hidden = true;

    // 面板初始状态
    ['diag-panel', 'netlog-panel'].forEach(function (id) {
      if (ui.panels[id]) panel(id, true);
    });
    // 移动端默认收起两侧面板
    var narrow = window.matchMedia && window.matchMedia('(max-width: 699px)').matches;
    setCollapsed('legend', readLS('legendCollapsed', narrow));
    setCollapsed('players', readLS('playersCollapsed', narrow));
  }

  function updateHud() {
    if (hudNodes.conn) {
      // conn is updated by the socket layer; only counts change here
    }
    if (hudNodes.tick) hudNodes.tick.innerHTML = 'tick <b>' + (state.tick || '—') + '</b>';
    if (hudNodes.players) {
      var n = 0;
      for (var i = 0; i < entryList.length; i++) if (entryList[i].kind === 'player' || entryList[i].kind === 'ai') n++;
      hudNodes.players.innerHTML = '目标 <b>' + n + '</b>';
    }
    if (hudNodes.fps) hudNodes.fps.innerHTML = 'fps <b>' + (diag.fps ? diag.fps.toFixed(0) : '—') + '</b>';
    updateScaleBar();
    schedulePanelRefresh();
  }

  function updateScaleBar() {
    if (!map || !hudNodes.scaleLabel) return;
    var perM = map._metresToPx ? map._metresToPx() : 0;
    if (!perM) return;
    var targetPx = 88;
    var rawM = targetPx / perM;
    // 取整到 1/2/5 × 10^n
    var pow = Math.pow(10, Math.floor(Math.log10(Math.max(rawM, 1e-6))));
    var mant = rawM / pow;
    var nice = mant >= 5 ? 5 : (mant >= 2 ? 2 : 1);
    var metres = nice * pow;
    var widthPx = clamp(metres * perM, 24, 160);
    hudNodes.scaleBar.style.width = widthPx.toFixed(0) + 'px';
    hudNodes.scaleLabel.textContent = fmtDistance(metres);
    if (hudNodes.zoom) hudNodes.zoom.innerHTML = '缩放 <b>' + map.getZoom().toFixed(2) + '</b>';
  }

  function onMapMouseMove(ev) {
    if (!hudNodes.coords || !ev || !ev.latlng) return;
    var w = latLngToWorld(ev.latlng.lat, ev.latlng.lng);
    hudNodes.coords.textContent = 'X ' + fmt(w.x, 1) + '  Z ' + fmt(w.z, 1);
  }

  function onMapClick(ev) {
    if (!ev || !ev.latlng) return;
    var w = latLngToWorld(ev.latlng.lat, ev.latlng.lng);
    logEvent('地图点击 X ' + fmt(w.x, 1) + ' Z ' + fmt(w.z, 1));
  }

  function centreOnSelf(animate) {
    var self = table[selfUuid()];
    if (!self || !map) {
      toast('未收到自身位置');
      return;
    }
    map.panTo(worldToLatLng(self.cx, self.cz));
    if (animate) map.invalidateSize();
  }

  function toast(text) {
    if (!hudNodes.toast) return;
    hudNodes.toast.textContent = text;
    hudNodes.toast.hidden = false;
    clearTimeout(toast._t);
    toast._t = setTimeout(function () { hudNodes.toast.hidden = true; }, 2400);
  }

  /* ---- 目标列表 ---- */

  function renderPlayerList() {
    var list = hudNodes.list;
    if (!list) return;
    var rows = entryList.filter(function (e) {
      if (e.kind === 'loot' || e.kind === 'deathbox') return false;
      if (!ui.search) return true;
      var hay = ((e.name || '') + ' ' + (e.weapon || '') + ' ' + (e.uuid || '')).toLowerCase();
      return hay.indexOf(ui.search) >= 0;
    });
    rows.sort(function (a, b) { return num(a._dist, 1e9) - num(b._dist, 1e9); });

    list.textContent = '';
    if (hudNodes.empty) hudNodes.empty.hidden = rows.length > 0;
    for (var i = 0; i < rows.length; i++) {
      list.appendChild(buildPlayerRow(rows[i]));
    }
  }

  function buildPlayerRow(e) {
    var li = document.createElement('li');
    li.className = 'player-row' + (e.alive ? '' : ' dead') + (ui.selectedUuid === e.uuid ? ' selected' : '');
    li.dataset.uuid = e.uuid;

    var camp = document.createElement('span');
    camp.className = 'camp-bar';
    camp.style.background = colorFor(e);

    var name = document.createElement('span');
    name.className = 'p-name';
    name.textContent = e.name || e.uuid;
    var kind = document.createElement('span');
    kind.className = 'kind';
    kind.textContent = kindCN(e.kind) + (e.uuid === selfUuid() ? ' · 自身' : '');
    name.appendChild(kind);

    var dist = document.createElement('span');
    dist.className = 'p-dist';
    dist.textContent = fmtDistance(num(e._dist, NaN));

    var hpWrap = document.createElement('span');
    hpWrap.className = 'p-hp';
    var track = document.createElement('span');
    track.className = 'hp-track';
    var fill = document.createElement('i');
    fill.className = 'hp-fill';
    var hp = num(e.hp, NaN);
    var maxHp = num(e.max_hp, 100) || 100;
    var ratio = isFinite(hp) ? clamp(hp / maxHp, 0, 1) : 0;
    fill.style.width = (ratio * 100).toFixed(1) + '%';
    if (ratio <= 0.3) fill.classList.add('low');
    else if (ratio <= 0.6) fill.classList.add('mid');
    track.appendChild(fill);
    var weapon = document.createElement('span');
    weapon.className = 'p-weapon';
    weapon.textContent = e.weapon || (isFinite(hp) ? fmt(hp, 0) + ' HP' : '—');
    hpWrap.appendChild(track);
    hpWrap.appendChild(weapon);

    li.appendChild(camp);
    li.appendChild(name);
    li.appendChild(dist);
    li.appendChild(hpWrap);
    li.addEventListener('click', function () {
      ui.selectedUuid = e.uuid;
      if (map) map.panTo(worldToLatLng(e.cx, e.cz));
      logEvent('居中到 ' + (e.name || e.uuid));
      toast('已居中：' + (e.name || e.uuid) + '（' + fmtDistance(num(e._dist, NaN)) + '）');
      renderPlayerList();
    });
    return li;
  }

  /* ---- 击杀播报 ---- */

  function ingestKills(kills, ts) {
    if (!Array.isArray(kills)) return;
    for (var i = 0; i < kills.length; i++) {
      var k = kills[i];
      if (!k || typeof k !== 'object') continue;
      var key = [
        pick(k, ['killer_uuid', 'killerUuid', 'killer', 'source_uuid']) || '',
        pick(k, ['victim_uuid', 'victimUuid', 'victim', 'target_uuid']) || '',
        pick(k, ['damage_type', 'damageType', 'type', 'cause']) || '',
        pick(k, ['ts', 'time', 'timestamp']) || ''
      ].join('|');
      if (state.killKeys[key]) continue;
      state.killKeys[key] = 1;
      state.kills.unshift({
        killer: pick(k, ['killer_name', 'killerName', 'killer', 'instigator']) || '未知',
        victim: pick(k, ['victim_name', 'victimName', 'victim', 'target']) || '未知',
        damage: pick(k, ['damage_type', 'damageType', 'type', 'cause']) || '',
        weapon: pick(k, ['weapon', 'weapon_name']) || '',
        ts: num(pick(k, ['ts', 'time', 'timestamp']), ts || Date.now()),
        key: key
      });
      if (state.kills.length > KILL_LIMIT) state.kills.length = KILL_LIMIT;
    }
  }

  function renderKillFeed() {
    var host = hudNodes.kills;
    if (!host) return;
    if (!state.kills.length) {
      host.innerHTML = '<p class="hint">暂无击杀</p>';
      return;
    }
    var frag = document.createDocumentFragment();
    state.kills.slice(0, 20).forEach(function (k) {
      var line = document.createElement('div');
      line.className = 'kill-line';
      var w = k.weapon ? '<span class="w">[' + escapeHtml(k.weapon) + ']</span> ' : '';
      line.innerHTML = '<span class="t">' + fmtClock(k.ts) + '</span> ' +
        '<span class="k">' + escapeHtml(k.killer) + '</span> ' + w +
        '<span class="t">' + escapeHtml(damageCN(k.damage)) + '</span> → ' +
        '<span class="v">' + escapeHtml(k.victim) + '</span>';
      frag.appendChild(line);
    });
    host.textContent = '';
    host.appendChild(frag);
  }

  function escapeHtml(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c];
    });
  }

  /* ---- 诊断 / 网络日志 ---- */

  var lastListRefresh = 0;

  function refreshPanels() {
    if (hudNodes.list && Date.now() - lastListRefresh > 1000) {
      lastListRefresh = Date.now();
      renderPlayerList();
    }
    renderKillFeed();
    if (!$('diag-panel').hidden) renderDiag();
    if (!$('netlog-panel').hidden) renderNetlog();
  }

  function renderDiag() {
    var tbody = hudNodes.diagTable;
    if (!tbody) return;
    var c = diag.counters || {};
    var rows = [
      ['地图', state.mapKey + (state.calibration && state.calibration.name_cn ? '（' + state.calibration.name_cn + '）' : '')],
      ['品牌', state.brand],
      ['tick', String(state.tick || '—')],
      ['视角模式', ui.mode === 'north' ? '2D 北向上' : ui.mode === 'follow' ? '跟随朝向' : '3D'],
      ['地图旋转', fmt(currentMapRotation(), 1) + '°'],
      ['yaw_offset_deg', fmt(state.world.yaw_offset_deg, 2) + '°'],
      ['yaw_sign', String(state.yawSign)],
      ['origin_x / origin_y', fmt(state.world.origin_x, 6) + ' / ' + fmt(state.world.origin_y, 6)],
      ['scale（度/米）', String(state.world.scale)],
      ['只读模式', state.readOnly ? '是' : '否'],
      ['udp_packets_up', String(num(c.udp_packets_up, 0))],
      ['udp_packets_down', String(num(c.udp_packets_down, 0))],
      ['udp_invalid_packets', String(num(c.udp_invalid_packets, 0))],
      ['active_sessions', String(num(c.active_sessions, 0))],
      ['total_sessions', String(num(c.total_sessions, 0))],
      ['loot_payloads_skipped', String(num(c.loot_payloads_skipped, 0))],
      ['目标条目', String(entryList.length)],
      ['底图重绘', String(diag.tilesLoaded)],
      ['渲染帧率', diag.fps ? diag.fps.toFixed(1) + ' fps' : '—'],
      ['最后错误', diag.lastError]
    ];
    var frag = document.createDocumentFragment();
    rows.forEach(function (r) {
      var tr = document.createElement('tr');
      var td1 = document.createElement('td');
      td1.textContent = r[0];
      var td2 = document.createElement('td');
      td2.textContent = r[1];
      tr.appendChild(td1);
      tr.appendChild(td2);
      frag.appendChild(tr);
    });
    tbody.textContent = '';
    tbody.appendChild(frag);
  }

  function renderNetlog() {
    var tbody = hudNodes.netlogTable;
    if (!tbody) return;
    var rows = [
      ['WebSocket', state.wsUrl || '—'],
      ['连接状态', state.wsState || '—'],
      ['重连次数', String(num(state.reconnects, 0))],
      ['下次重连', state.nextRetryMs ? fmt(state.nextRetryMs, 0) + ' ms' : '—'],
      ['hello', String(diag.msgCounts.hello)],
      ['state', String(diag.msgCounts.state)],
      ['diag', String(diag.msgCounts.diag)],
      ['bye', String(diag.msgCounts.bye)],
      ['其他消息', String(diag.msgCounts.other)],
      ['最后消息', state.lastMsgTs ? fmtClock(state.lastMsgTs) : '—']
    ];
    var frag = document.createDocumentFragment();
    rows.forEach(function (r) {
      var tr = document.createElement('tr');
      var td1 = document.createElement('td');
      td1.textContent = r[0];
      var td2 = document.createElement('td');
      td2.textContent = r[1];
      tr.appendChild(td1);
      tr.appendChild(td2);
      frag.appendChild(tr);
    });
    tbody.textContent = '';
    tbody.appendChild(frag);
    if (hudNodes.netlogJson) hudNodes.netlogJson.textContent = lastRawJSON;
    if (hudNodes.netlogEvents) hudNodes.netlogEvents.textContent = events.slice(0, 12).join('\n') || '—';
  }

  /* ========================================================================
   * 8. WebSocket
   * ====================================================================== */

  var ws = null;
  var backoff = BACKOFF_MIN;
  var retryTimer = null;

  function wsURL() {
    var proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    return proto + '//' + window.location.host + '/ws';
  }

  // 客户端 → 服务端控制消息：连接建立（含每次重连成功）后发送订阅请求。
  function sendSubscribe() {
    if (!ws || ws.readyState !== 1) return false;
    try {
      ws.send(JSON.stringify({ type: 'subscribe' }));
      state.subscribeSent = num(state.subscribeSent, 0) + 1;
      logEvent('已发送 subscribe（第 ' + state.subscribeSent + ' 次）');
      return true;
    } catch (e) {
      diag.lastError = 'subscribe 发送失败：' + e.message;
      logEvent('subscribe 发送失败');
      return false;
    }
  }

  function setConn(text, stateAttr) {
    if (!hudNodes.conn) return;
    hudNodes.conn.textContent = text;
    hudNodes.conn.dataset.state = stateAttr;
    hudNodes.conn.className = 'badge ' + (stateAttr === 'open' ? 'badge-ok' : stateAttr === 'closed' ? 'badge-err' : 'badge-warn');
  }

  function connect() {
    if (ws) { try { ws.close(); } catch (e) { /* ignore */ } }
    clearTimeout(retryTimer);
    state.wsUrl = wsURL();
    state.wsState = '连接中';
    setConn('连接中', 'connecting');
    try {
      ws = new WebSocket(state.wsUrl);
    } catch (e) {
      diag.lastError = 'WebSocket 构造失败：' + e.message;
      scheduleReconnect();
      return;
    }

    ws.onopen = function () {
      backoff = BACKOFF_MIN;
      state.wsState = '已连接';
      state.nextRetryMs = 0;
      setConn('已连接', 'open');
      logEvent('WebSocket 已连接');
      if (hudNodes.legendHint) hudNodes.legendHint.textContent = '已连接，等待数据…';
      // 订阅状态流。服务器默认已订阅，这里显式发送一次，重连后同样重发，
      // 保证语义不依赖服务端的默认值。
      sendSubscribe();
    };

    ws.onmessage = function (ev) {
      handleMessage(ev.data);
    };

    ws.onerror = function () {
      diag.lastError = 'WebSocket 错误';
      logEvent('WebSocket 错误');
    };

    ws.onclose = function (ev) {
      state.wsState = '已断开';
      setConn('重连中', 'connecting');
      logEvent('WebSocket 断开（code ' + ev.code + '），准备重连');
      scheduleReconnect();
    };
  }

  function scheduleReconnect() {
    clearTimeout(retryTimer);
    // Schedule with the current delay, then step the backoff up *immediately* so
    // repeated close events inside the same window keep escalating instead of
    // rescheduling at the floor over and over.
    var delay = clamp(backoff, BACKOFF_MIN, BACKOFF_MAX);
    backoff = clamp(delay * 2, BACKOFF_MIN, BACKOFF_MAX);
    state.nextRetryMs = delay;
    state.reconnects = num(state.reconnects, 0) + 1;
    setConn('重连中 ' + (delay / 1000).toFixed(2) + 's', 'connecting');
    retryTimer = setTimeout(connect, delay);
  }

  function handleMessage(raw) {
    state.lastMsgTs = Date.now();
    lastRawJSON = (typeof raw === 'string' ? raw : String(raw)).slice(0, 4000);
    var msg;
    try {
      msg = JSON.parse(raw);
    } catch (e) {
      diag.msgCounts.other++;
      diag.lastError = 'JSON 解析失败';
      logEvent('收到无法解析的消息');
      return;
    }
    var type = msg && msg.type;
    if (type === 'hello') applyHello(msg);
    else if (type === 'state') applyState(msg);
    else if (type === 'diag') applyDiag(msg);
    else if (type === 'bye') {
      diag.msgCounts.bye++;
      logEvent('服务端 bye：雷达会话结束');
      toast('雷达会话已结束（bye）');
      try { ws.close(); } catch (e) { /* ignore */ }
    } else {
      diag.msgCounts.other++;
      logEvent('未知消息类型 ' + String(type));
    }
  }

  function applyHello(msg) {
    diag.msgCounts.hello++;
    state.brand = msg.brand || state.brand;
    state.mapKey = msg.map || state.mapKey;
    if (msg.tile_template) state.tileTemplate = msg.tile_template;
    if (msg.world && typeof msg.world === 'object') {
      state.serverWorldProvided = true;
      state.world = {
        origin_x: num(msg.world.origin_x, state.world.origin_x),
        origin_y: num(msg.world.origin_y, state.world.origin_y),
        scale: num(msg.world.scale, state.world.scale) || state.world.scale,
        yaw_offset_deg: num(msg.world.yaw_offset_deg, state.world.yaw_offset_deg)
      };
    }
    // read_only_radar may arrive on hello or be echoed on state messages.
    if (msg.read_only_radar != null) applyReadOnly(!!msg.read_only_radar);
    else if (msg.read_only != null) applyReadOnly(!!msg.read_only);

    applyCalibration(state.mapKey);

    if (tileLayer) tileLayer.setUrl(state.tileTemplate);
    if (map) {
      var centre = ui.autoCenter ? mapCentreLatLng() : map.getCenter();
      map.setView(centre, autoZoomLevel(), { reset: false });
      // One frame later the container may have letterboxed; re-fit then.
      setTimeout(function () {
        if (!map) return;
        map.setView(ui.autoCenter ? mapCentreLatLng() : map.getCenter(), autoZoomLevel(), { reset: false });
        if (ui.autoCenter) centreOnSelf(false);
      }, 120);
    }
    if (hudNodes.map) hudNodes.map.innerHTML = '地图 <b>' + state.mapKey + '</b>';
    logEvent('hello：地图 ' + state.mapKey + '，瓦片 ' + state.tileTemplate);
    if (hudNodes.legendHint) {
      hudNodes.legendHint.textContent = '地图 ' + state.mapKey +
        (state.calibration && state.calibration.name_cn ? '（' + state.calibration.name_cn + '）' : '') + ' · 标定已应用';
    }
  }

  function applyCalibration(mapKey) {
    var table0 = state.maps || {};
    var c = null;
    var keys = Object.keys(table0);
    for (var i = 0; i < keys.length; i++) {
      if (keys[i].toLowerCase() === String(mapKey).toLowerCase()) { c = table0[keys[i]]; break; }
    }
    if (!c) c = table0.default || null;
    if (!c) { state.calibration = null; return; }
    state.calibration = c;
    state.yawSign = num(c.yaw_sign, 1) >= 0 ? 1 : -1;
    // maps.json is the baseline; a hello that carries its own world block wins.
    if (!state.serverWorldProvided) {
      state.world.origin_x = num(c.origin_x, state.world.origin_x);
      state.world.origin_y = num(c.origin_y, state.world.origin_y);
      state.world.scale = num(c.scale, state.world.scale) || state.world.scale;
      state.world.yaw_offset_deg = num(c.yaw_offset_deg, state.world.yaw_offset_deg);
    }
  }

  function applyReadOnly(flag) {
    var changed = state.readOnly !== flag;
    state.readOnly = flag;
    if (hudNodes.readonly) hudNodes.readonly.hidden = !flag;
    document.documentElement.dataset.readonly = flag ? '1' : '0';
    document.querySelectorAll('[data-write-op]').forEach(function (n) { n.disabled = flag; n.hidden = flag; });
    if (changed) logEvent(flag ? '进入只读模式（隐藏写操作）' : '退出只读模式');
  }

  function applyState(msg) {
    diag.msgCounts.state++;
    state.tick = num(msg.tick, state.tick);
    state.serverTs = num(msg.ts, state.serverTs);
    if (msg.read_only_radar != null) applyReadOnly(!!msg.read_only_radar);

    if (msg.self && typeof msg.self === 'object') {
      var selfEntry = upsertPlayer(msg.self);
      if (selfEntry) state.selfUuid = selfEntry.uuid;
    }
    if (Array.isArray(msg.players)) {
      for (var i = 0; i < msg.players.length; i++) upsertPlayer(msg.players[i]);
    }
    if (Array.isArray(msg.loot)) {
      for (var j = 0; j < msg.loot.length; j++) {
        var item = msg.loot[j];
        if (item && typeof item === 'object' && !item.kind) item.kind = 'loot';
        upsertPlayer(item);
      }
    }
    ingestKills(msg.kills, state.serverTs);
    ingestTraces(msg.traces);

    rebuildEntryList();
    if (!state.selfUuid) {
      // Fallback: pick the closest 'player' entry so the list has a reference frame.
      var best = null;
      for (var uuid in table) {
        if (table[uuid].kind !== 'player') continue;
        if (!best || num(table[uuid].distance, 1e9) < num(best.distance, 1e9)) best = table[uuid];
      }
      if (best) state.selfUuid = best.uuid;
    }
  }

  function applyDiag(msg) {
    diag.msgCounts.diag++;
    if (msg.counters && typeof msg.counters === 'object') {
      diag.counters = msg.counters;
      if (msg.counters.udp_invalid_packets) diag.lastError = '—';
    }
    if (msg.read_only_radar != null) applyReadOnly(!!msg.read_only_radar);
  }

  /* ========================================================================
   * 9. 启动
   * ====================================================================== */

  function fail(reason) {
    diag.lastError = reason;
    if (hudNodes.legendHint) hudNodes.legendHint.textContent = '初始化失败：' + reason;
    var host = $('map');
    if (host) {
      host.innerHTML = '<div class="hint" style="padding:16px;color:var(--warning)">雷达初始化失败：' + escapeHtml(reason) + '</div>';
    }
    logEvent('初始化失败：' + reason);
  }

  function loadMaps() {
    return new Promise(function (resolve) {
      var xhr = new XMLHttpRequest();
      xhr.open('GET', '/maps.json', true);
      xhr.onreadystatechange = function () {
        if (xhr.readyState !== 4) return;
        if (xhr.status >= 200 && xhr.status < 300) {
          try {
            state.maps = JSON.parse(xhr.responseText);
            logEvent('maps.json 已加载（' + Object.keys(state.maps).length + ' 项）');
          } catch (e) {
            state.maps = {};
            diag.lastError = 'maps.json 解析失败';
          }
        } else {
          state.maps = {};
          diag.lastError = 'maps.json 不可用（HTTP ' + xhr.status + '）';
        }
        resolve(state.maps);
      };
      try { xhr.send(); } catch (e) { state.maps = {}; resolve(state.maps); }
    });
  }

  function onResize() {
    if (!map) return;
    map.invalidateSize();
    updateScaleBar();
  }

  function watchSize() {
    // WKWebView can change size without fireing window.resize (rotation, split view).
    var last = '';
    setInterval(function () {
      if (!map) return;
      var sig = window.innerWidth + 'x' + window.innerHeight;
      if (sig !== last) {
        last = sig;
        onResize();
      } else if (map._measure.w > 2 && Math.abs(map._measure.w - map.getSize().x) > 2) {
        onResize();
      }
    }, 700);
  }

  function init() {
    wireHud();
    buildSwitches();
    buildModeSeg();
    applyReadOnly(false);
    if (hudNodes.search) hudNodes.search.value = ui.search || '';

    if (!window.L) { fail('未加载 leaflet-lite.js'); return; }

    loadMaps().then(function (maps) {
      var key = state.mapKey;
      var table0 = maps || {};
      var entry = null;
      var ks = Object.keys(table0);
      for (var i = 0; i < ks.length; i++) {
        if (ks[i].toLowerCase() === String(key).toLowerCase()) { entry = table0[ks[i]]; break; }
      }
      entry = entry || table0.default || null;
      state.calibration = entry;
      state.serverWorldProvided = false;   // maps.json is the baseline until hello arrives
      if (entry) {
        state.world = {
          origin_x: num(entry.origin_x, 0),
          origin_y: num(entry.origin_y, 0),
          scale: num(entry.scale, 1e-5) || 1e-5,
          yaw_offset_deg: num(entry.yaw_offset_deg, 0)
        };
        state.yawSign = num(entry.yaw_sign, 1) >= 0 ? 1 : -1;
      }

      if (!initMap()) return;

      window.addEventListener('resize', onResize);
      window.addEventListener('orientationchange', onResize);
      document.addEventListener('visibilitychange', function () {
        // Coming back from background: sockets may be silently dead on iOS.
        if (document.visibilityState === 'visible') {
          onResize();
          if (!ws || ws.readyState === WebSocket.CLOSED || ws.readyState === WebSocket.CLOSING) {
            backoff = BACKOFF_MIN;
            connect();
          }
        }
      });

      watchSize();
      connect();

      // 就绪标记：地图已建、WS 已构造、#app 有子节点。
      // 先建立就绪标记，再启动渲染循环 —— 循环的首帧会检查 ready，
      // 反过来的话首帧可能白跑一次就不再排下一帧。
      var app = $('app');
      if (app && app.children.length > 0 && document.querySelector('.leaflet-container')) {
        markReady();
      } else {
        // 兜底：下一拍再探测一次，最多重试一段时间，之后照常标记就绪，
        // 免得 iOS 壳的加载探针永远等不到。
        var tries = 0;
        (function poll() {
          var host = $('app');
          if (host && host.children.length > 0 && document.querySelector('.leaflet-container')) {
            markReady();
            return;
          }
          if (++tries < 20) { setTimeout(poll, 50); return; }
          markReady();
        })();
      }
      startLoop();
      setMode(ui.mode);
    });
  }

  function markReady() {
    if (state.ready) return;
    state.ready = true;
    document.documentElement.dataset.battleReady = '1';
    logEvent('雷达就绪（battle-ready=1）');
  }

  function startLoop() {
    if (!state.loopStarted) {
      state.loopStarted = true;
      requestAnimationFrame(frame);
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }

  // Expose a tiny surface for the shell / manual debugging.
  window.MXraderRadar = {
    state: state,
    ui: ui,
    diag: diag,
    layerOn: layerOn,
    worldToLatLng: worldToLatLng,
    latLngToWorld: latLngToWorld,
    applyRotationCorrection: applyRotationCorrection,
    setMode: setMode,
    connect: connect,
    getMap: function () { return map; },
    // 测试/排查用：读取玩家表内部状态（不用于正常渲染路径）
    _debug: {
      explain: function () {
        return entryList.map(function (e) {
          return {
            uuid: e.uuid,
            kind: e.kind,
            alive: e.alive,
            ageMs: Date.now() - num(e.lastSeen, 0)
          };
        });
      },
      count: function () { return entryList.length; },
      // 立刻跑一次过期回收，便于在不依赖渲染节奏的情况下断言
      pruneNow: function () { prunePlayers(); return entryList.length; }
    }
  };
})();
