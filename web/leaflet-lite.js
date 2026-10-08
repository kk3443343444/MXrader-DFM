/*!
 * leaflet-lite.js — a self-contained, minimal Leaflet-compatible map engine.
 *
 * Why this file exists: the MXrader radar page is served by a local Rust HTTP
 * server on a LAN without internet access, and the iOS shell probes for
 * `.leaflet-container` to decide the page is loaded. Real Leaflet cannot be
 * pulled from a CDN, so this module re-implements just the subset of the
 * Leaflet 1.x API that radar.js uses, on top of plain DOM + one canvas.
 *
 * Implemented API surface:
 *   L.map(el, opts) -> map  (.setView/.on/.off/.invalidateSize/.remove/.container/
 *                            .getBounds/.latLngToContainerPoint/.containerPointToLatLng/
 *                            .getZoom/.getCenter/.setZoom/.panBy/.addLayer/.removeLayer)
 *   L.tileLayer(urlTemplate, opts) -> layer (.addTo/.setUrl/.redraw/.remove/.setOpacity)
 *   L.marker([lat,lng], opts)      shapes: triangle (heading), ring (self),
 *   L.circleMarker([lat,lng], s)   square (deathbox), diamond (loot), circle, cross
 *   L.polyline(points, opts) / L.polygon(points, opts) / L.circle(latlng, opts)
 *   Vector layers: .addTo/.remove/.setLatLng/.setLatLngs/.setStyle/
 *                  .bindTooltip/.unbindTooltip/.bindPopup(alias)
 *   Events: mousemove, click, move, moveend, zoom, zoomend, resize
 *
 * Rendering model: every vector layer is drawn onto ONE <canvas> inside
 * `.leaflet-overlay-pane`. Any mutation marks the canvas dirty; a single
 * requestAnimationFrame flush performs the redraw (no per-layer rAF storms).
 *
 * Coordinates: Web-Mercator (EPSG:3857) exactly like Leaflet, so lat/lng pairs
 * coming from arbitrary "degrees per metre" projections still behave.
 *
 * Zero external dependencies. Plain ES2020. WKWebView iOS 16 / modern browsers.
 */
(function (global) {
  'use strict';

  var TILE_SIZE = 256;
  var MAX_LAT = 85.0511287798066;
  var MIN_ZOOM = 0;
  var MAX_ZOOM = 22;

  /* ------------------------------------------------------------------ *
   * Small helpers
   * ------------------------------------------------------------------ */

  function nowMs() {
    return (global.performance && global.performance.now)
      ? global.performance.now()
      : Date.now();
  }

  function clamp(v, lo, hi) {
    return v < lo ? lo : (v > hi ? hi : v);
  }

  // Classic Web-Mercator forward projection; result is [0..1, 0..1] of the world.
  function lonToX(lng) {
    return (lng + 180) / 360;
  }
  function latToY(lat) {
    var clamped = clamp(lat, -MAX_LAT, MAX_LAT);
    var s = Math.sin((clamped * Math.PI) / 180);
    return 0.5 - Math.log((1 + s) / (1 - s)) / (4 * Math.PI);
  }
  function xToLon(x) {
    return x * 360 - 180;
  }
  function yToLat(y) {
    var n = Math.PI - 2 * Math.PI * y;
    return (180 / Math.PI) * Math.atan(0.5 * (Math.exp(n) - Math.exp(-n)));
  }

  function toLatLng(a, b) {
    if (a == null) return { lat: 0, lng: 0 };
    if (Array.isArray(a)) return { lat: +a[0] || 0, lng: +a[1] || 0 };
    if (typeof a === 'object') {
      if (typeof a.lat === 'number' && typeof a.lng === 'number') return { lat: a.lat, lng: a.lng };
      if (typeof a[0] === 'number' && typeof a[1] === 'number') return { lat: +a[0], lng: +a[1] };
      if (typeof a.x === 'number' && typeof a.y === 'number') return { lat: +a.y, lng: +a.x };
    }
    return { lat: 0, lng: 0 };
  }

  function toZoom(z) {
    z = Number(z);
    return isFinite(z) ? clamp(z, MIN_ZOOM, MAX_ZOOM) : 5;
  }

  function shallowMerge(target) {
    for (var i = 1; i < arguments.length; i++) {
      var src = arguments[i];
      if (!src) continue;
      for (var k in src) {
        if (Object.prototype.hasOwnProperty.call(src, k)) target[k] = src[k];
      }
    }
    return target;
  }

  function px(v) {
    return (Math.round(v * 100) / 100) + 'px';
  }

  function el(tag, cls, parent) {
    var node = document.createElement(tag);
    if (cls) node.className = cls;
    if (parent) parent.appendChild(node);
    return node;
  }

  /* ------------------------------------------------------------------ *
   * Tiny event emitter (Leaflet-ish: on / off / once / fire)
   * ------------------------------------------------------------------ */

  function Emitter() {
    this._listeners = Object.create(null);
  }
  Emitter.prototype.on = function (types, fn, ctx) {
    var list = String(types).split(/\s+/);
    for (var i = 0; i < list.length; i++) {
      var t = list[i];
      if (!t) continue;
      (this._listeners[t] || (this._listeners[t] = [])).push({ fn: fn, ctx: ctx || this });
    }
    return this;
  };
  Emitter.prototype.off = function (types, fn) {
    var list = String(types || '').split(/\s+/);
    for (var i = 0; i < list.length; i++) {
      var t = list[i];
      var arr = this._listeners[t];
      if (!arr) continue;
      if (!fn) { delete this._listeners[t]; continue; }
      this._listeners[t] = arr.filter(function (l) { return l.fn !== fn; });
    }
    return this;
  };
  Emitter.prototype.once = function (types, fn, ctx) {
    var self = this;
    function wrapper() {
      self.off(types, wrapper);
      fn.apply(ctx || self, arguments);
    }
    return this.on(types, wrapper, ctx);
  };
  Emitter.prototype.fire = function (type, data) {
    var arr = this._listeners[type];
    if (!arr || !arr.length) return this;
    var ev = data || {};
    if (!ev.type) ev.type = type;
    if (!ev.target) ev.target = this;
    if (!ev.sourceTarget) ev.sourceTarget = this;
    var copy = arr.slice();
    for (var i = 0; i < copy.length; i++) {
      try {
        copy[i].fn.call(copy[i].ctx, ev);
      } catch (e) {
        // A broken listener must never break the render loop.
        if (global.console && console.error) console.error('[leaflet-lite] listener error', e);
      }
    }
    return this;
  };

  /* ------------------------------------------------------------------ *
   * Offline tile placeholder
   * ------------------------------------------------------------------ */

  // 16x16 RGBA PNG, #15171f with a #262a36 diagonal hairline — drawn inline so
  // the fallback tile needs no network request at all.
  var OFFLINE_TILE_PNG =
    'data:image/png;base64,' +
    'iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAYAAAAf8/9hAAAAQklEQVR42mNkYPhfz0AEYBxVSF+FjAwMDP8ZGRn+Mw4YGEcV0lchIwMDw39GRsb/jAMGxlGF9FXIyMDA8J+RkfE/AwB5eBQBTvakvwAAAABJRU5ErkJggg==';

  /* ------------------------------------------------------------------ *
   * Shared vector-layer behaviour
   * ------------------------------------------------------------------ */

  var layerSeq = 1;

  function baseLayer(kind) {
    var l = new Emitter();
    l._kind = kind;
    l._id = layerSeq++;
    l._map = null;
    l._style = {};
    l._tooltip = null;
    l._tooltipOpen = false;
    l._visible = true;
    l.addTo = function (map) {
      if (map && typeof map.addLayer === 'function') map.addLayer(this);
      return this;
    };
    l.remove = function () {
      if (this._map) this._map.removeLayer(this);
      return this;
    };
    l.removeFrom = l.remove;
    l.setStyle = function (style) {
      shallowMerge(this._style, style);
      if (this._map) this._map._invalidateVectors();
      return this;
    };
    l.setOpacity = function (o) {
      this._style.opacity = o;
      this._style.fillOpacity = (typeof this._style.fillOpacity === 'number') ? o * this._style.fillOpacity : this._style.fillOpacity;
      if (this._map) this._map._invalidateVectors();
      return this;
    };
    l.setZIndexOffset = function () { return this; };
    l.bringToFront = function () { return this; };
    l.bringToBack = function () { return this; };
    l.bindTooltip = function (content) {
      this._tooltip = (content == null) ? null : String(content);
      if (this._map) this._map._invalidateVectors();
      return this;
    };
    l.unbindTooltip = function () {
      this._tooltip = null;
      this._tooltipOpen = false;
      if (this._map) this._map._invalidateVectors();
      return this;
    };
    l.bindPopup = l.bindTooltip;
    l.unbindPopup = l.unbindTooltip;
    l.isTooltipOpen = function () { return !!this._tooltipOpen; };
    l.getLatLng = function () { return this._latlng || null; };
    l.getLatLngs = function () { return this._latlngs || null; };
    l.getBounds = function () {
      var pts = this._allPoints();
      if (!pts.length) return null;
      var map = this._map;
      if (!map) return null;
      return map.getBounds();
    };
    l._allPoints = function () {
      if (this._latlng) return [this._latlng];
      return this._latlngs || [];
    };
    return l;
  }

  function makePointLayer(kind, latlng, options) {
    var l = baseLayer(kind);
    l._latlng = toLatLng(latlng);
    l._style = shallowMerge({
      stroke: true, color: '#38d39f', weight: 2, opacity: 1,
      fill: true, fillColor: '#38d39f', fillOpacity: 0.85,
      radius: 6, shape: kind === 'marker' ? 'triangle' : 'circle'
    }, options || {});
    if (options && options.icon && typeof options.icon === 'object') {
      // Leaflet icon objects: honour a shape hint + colour when provided.
      if (options.icon.shape) l._style.shape = options.icon.shape;
      if (options.icon.color) l._style.color = options.icon.color;
      if (options.icon.fillColor) l._style.fillColor = options.icon.fillColor;
      if (options.icon.fillOpacity != null) l._style.fillOpacity = options.icon.fillOpacity;
    }
    l.setLatLng = function (ll) {
      this._latlng = toLatLng(ll);
      if (this._map) this._map._invalidateVectors();
      return this;
    };
    l.setRadius = function (r) {
      this._style.radius = r;
      if (this._map) this._map._invalidateVectors();
      return this;
    };
    l.setShape = function (s) {
      this._style.shape = s;
      if (this._map) this._map._invalidateVectors();
      return this;
    };
    return l;
  }

  function makePathLayer(kind, latlngs, options) {
    var l = baseLayer(kind);
    l._latlngs = (latlngs || []).map(function (p) { return toLatLng(p); });
    l._style = shallowMerge({
      stroke: true, color: '#ffb020', weight: 2, opacity: 0.9,
      fill: kind === 'polygon', fillColor: '#ffb020', fillOpacity: 0.15,
      dashArray: null, lineJoin: 'round', lineCap: 'round'
    }, options || {});
    l.setLatLngs = function (pts) {
      this._latlngs = (pts || []).map(function (p) { return toLatLng(p); });
      if (this._map) this._map._invalidateVectors();
      return this;
    };
    return l;
  }

  function makeCircleLayer(latlng, options) {
    var l = makePathLayer('circle', [], options);
    l._latlng = toLatLng(latlng);
    l._radiusM = (options && options.radius) || 0; // metres, converted via map.unitsPerMeter
    l.setLatLng = function (ll) {
      this._latlng = toLatLng(ll);
      if (this._map) this._map._invalidateVectors();
      return this;
    };
    l.setRadius = function (r) {
      this._radiusM = r;
      if (this._map) this._map._invalidateVectors();
      return this;
    };
    l._allPoints = function () { return this._latlng ? [this._latlng] : []; };
    return l;
  }

  /* ------------------------------------------------------------------ *
   * Tile layer
   * ------------------------------------------------------------------ */

  function TileLayer(urlTemplate, options) {
    var layer = new Emitter();
    layer._kind = 'tile';
    layer._template = urlTemplate;
    layer._opts = shallowMerge({ tileSize: TILE_SIZE, opacity: 1, mapKey: null, minZoom: MIN_ZOOM, maxZoom: MAX_ZOOM }, options || {});
    layer._map = null;
    layer._images = new global.Map();   // url -> Image (insertion order == LRU order)
    layer._maxCache = 96;
    layer._active = new global.Map();   // tile key -> { img, url, x, y }
    layer._pane = null;
    layer._z = null;

    layer.addTo = function (map) {
      if (map && typeof map.addLayer === 'function') map.addLayer(this);
      return this;
    };
    layer.remove = function () {
      if (this._map) this._map.removeLayer(this);
      return this;
    };
    layer.setUrl = function (tpl) {
      this._template = tpl;
      this.redraw();
      return this;
    };
    layer.setOpacity = function (o) {
      this._opts.opacity = o;
      var nodes = this._pane ? this._pane.children : [];
      for (var i = 0; i < nodes.length; i++) nodes[i].style.opacity = String(o);
      return this;
    };
    layer.redraw = function () {
      if (this._map) this._map._invalidateTiles(true);
      return this;
    };
    layer.getTileUrl = function (coords) {
      return String(this._template)
        .replace(/\{map\}/g, this._opts.mapKey == null ? '' : this._opts.mapKey)
        .replace(/\{x\}/g, String(coords.x))
        .replace(/\{y\}/g, String(coords.y))
        .replace(/\{z\}/g, String(coords.z))
        .replace(/\{-y\}/g, String(coords.y))
        .replace(/\{s\}/g, 'a');
    };
    layer._getImage = function (url) {
      var cached = this._images.get(url);
      if (cached) {
        this._images.delete(url);      // touch for LRU
        this._images.set(url, cached);
        return cached;
      }
      var self = this;
      var img = new Image();
      img.decoding = 'async';
      img.loading = 'eager';
      img.alt = '';
      img.draggable = false;
      img.onerror = function () {
        // Missing tiles (no data for this map/zoom yet) degrade to the inline
        // offline placeholder instead of showing a broken-image glyph.
        img.onerror = null;
        img.src = OFFLINE_TILE_PNG;
      };
      this._images.set(url, img);
      img.src = url;
      this._trimCache();
      return img;
    };
    layer._trimCache = function () {
      while (this._images.size > this._maxCache) {
        var oldestKey = this._images.keys().next().value;
        var img = this._images.get(oldestKey);
        if (img && img.parentNode) img.parentNode.removeChild(img);
        this._images.delete(oldestKey);
      }
    };
    return layer;
  }

  /* ------------------------------------------------------------------ *
   * Map
   * ------------------------------------------------------------------ */

  // NOTE: deliberately NOT named `Map` — that would shadow the global Map
  // constructor for the whole page.
  function LiteMap(containerEl, options) {
    var map = new Emitter();
    map.options = shallowMerge({
      zoom: 5,
      center: { lat: 0, lng: 0 },
      minZoom: MIN_ZOOM,
      maxZoom: MAX_ZOOM,
      zoomSnap: 0,             // 0 == free/continuous zoom
      wheelDebounceTime: 0,
      zoomDelta: 1,
      attributionControl: false,
      zoomControl: false,
      preferCanvas: true,
      background: '#0c0d12'
    }, options || {});

    map._container = containerEl;
    map._zoom = toZoom(map.options.zoom);
    map._center = toLatLng(map.options.center);
    map._layers = [];
    map._tileLayers = [];
    map._vectors = [];
    map._dirtyVectors = false;
    map._dirtyTiles = true;
    map._frame = 0;
    map._destroyed = false;
    map._pins = new global.Map();       // pointerId -> {x,y}
    map._mouseContainer = { x: 0, y: 0 };
    map._hoverLayer = null;
    map._tileZ = null;
    map._viewEpoch = 0;

    // Leaflet exposes the container under both names.
    map.container = containerEl;
    map._container = containerEl;

    /* ---- DOM scaffolding ---- */

    function addClass(node, cls) {
      if ((' ' + node.className + ' ').indexOf(' ' + cls + ' ') < 0) node.className = (node.className + ' ' + cls).trim();
    }

    addClass(containerEl, 'leaflet-container');
    addClass(containerEl, 'leaflet-touch');
    if (!containerEl.style.position || containerEl.style.position === 'static') {
      containerEl.style.position = 'relative';
    }
    containerEl.style.overflow = 'hidden';
    containerEl.style.touchAction = 'none';
    containerEl.style.background = map.options.background;
    containerEl.style.webkitUserSelect = 'none';
    containerEl.style.userSelect = 'none';
    containerEl.style.webkitTapHighlightColor = 'transparent';

    // Remove anything a previous map instance left behind, then build the panes.
    var stale = containerEl.querySelectorAll('.leaflet-pane');
    for (var si = 0; si < stale.length; si++) containerEl.removeChild(stale[si]);

    map._pane = el('div', 'leaflet-pane leaflet-map-pane', containerEl);
    map._pane.style.position = 'absolute';
    map._pane.style.left = '0';
    map._pane.style.top = '0';

    map._tilePane = el('div', 'leaflet-pane leaflet-tile-pane', map._pane);
    map._tilePane.style.position = 'absolute';
    map._tilePane.style.left = '0';
    map._tilePane.style.top = '0';
    map._tilePane.style.zIndex = '200';

    map._overlayPane = el('div', 'leaflet-pane leaflet-overlay-pane', map._pane);
    map._overlayPane.style.position = 'absolute';
    map._overlayPane.style.left = '0';
    map._overlayPane.style.top = '0';
    map._overlayPane.style.zIndex = '400';

    // The map may be rotated (3D follow-heading mode); keep drawing crisp.
    map._overlayPane.style.transformOrigin = '0 0';

    var canvas = el('canvas', 'leaflet-zoom-animated', map._overlayPane);
    canvas.style.position = 'absolute';
    canvas.style.left = '0';
    canvas.style.top = '0';
    map._canvas = canvas;
    map._ctx = canvas.getContext('2d');

    map._tooltipBox = el('div', 'leaflet-tooltip leaflet-lite-tooltip', containerEl);
    map._tooltipBox.style.position = 'absolute';
    map._tooltipBox.style.display = 'none';

    map._measure = { w: 0, h: 0 };
    map._dpr = 1;
    measure();

    /* ---- geometry ---- */

    function measure() {
      var w = containerEl.clientWidth || containerEl.offsetWidth || 0;
      var h = containerEl.clientHeight || containerEl.offsetHeight || 0;
      if (w <= 0) w = 1;  // never divide by zero on a hidden container
      if (h <= 0) h = 1;
      map._measure.w = w;
      map._measure.h = h;
      var dpr = Math.min(global.devicePixelRatio || 1, 3);
      map._dpr = dpr;
      canvas.width = Math.max(1, Math.round(w * dpr));
      canvas.height = Math.max(1, Math.round(h * dpr));
      canvas.style.width = w + 'px';
      canvas.style.height = h + 'px';
      map._dirtyVectors = true;
    }

    // World size in CSS pixels at the current zoom.
    function worldSize() {
      return TILE_SIZE * Math.pow(2, map._zoom);
    }
    map._worldSize = worldSize;

    // Geographic -> absolute world pixel (CSS px at current zoom).
    function project(latlng) {
      var s = worldSize();
      return { x: lonToX(latlng.lng) * s, y: latToY(latlng.lat) * s };
    }
    map.project = project;
    map.unproject = function (point, zoom) {
      var z = (zoom == null) ? map._zoom : toZoom(zoom);
      var s = TILE_SIZE * Math.pow(2, z);
      return { lat: yToLat(point.y / s), lng: xToLon(point.x / s) };
    };

    // Absolute world pixel -> container pixel (translation by the map's pixel origin).
    function originPixel() {
      var c = project(map._center);
      return { x: c.x - map._measure.w / 2, y: c.y - map._measure.h / 2 };
    }
    map._originPixel = originPixel;

    map.latLngToContainerPoint = function (latlng, zoom) {
      var ll = toLatLng(latlng);
      var p = (zoom == null || zoom === map._zoom) ? project(ll) : map.project(ll, zoom);
      var o = (zoom == null || zoom === map._zoom) ? originPixel() : (function () {
        var c = map.project(map._center, zoom);
        return { x: c.x - map._measure.w / 2, y: c.y - map._measure.h / 2 };
      })();
      return { x: p.x - o.x, y: p.y - o.y };
    };

    map.containerPointToLatLng = function (point) {
      var o = originPixel();
      return map.unproject({ x: point.x + o.x, y: point.y + o.y });
    };

    map.latLngToLayerPoint = function (latlng) { return map.latLngToContainerPoint(latlng); };
    map.layerPointToLatLng = function (point) { return map.containerPointToLatLng(point); };

    map.getCenter = function () { return { lat: map._center.lat, lng: map._center.lng }; };
    map.getZoom = function () { return map._zoom; };

    map.getSize = function () { return { x: map._measure.w, y: map._measure.h }; };

    map.getBounds = function (padding) {
      var pad = padding || { x: 0, y: 0 };
      var nw = map.containerPointToLatLng({ x: -pad.x, y: -pad.y });
      var se = map.containerPointToLatLng({ x: map._measure.w + pad.x, y: map._measure.h + pad.y });
      var sw = { lat: se.lat, lng: nw.lng };
      var ne = { lat: nw.lat, lng: se.lng };
      var bounds = {
        _sw: sw, _ne: ne,
        getSouthWest: function () { return sw; },
        getNorthEast: function () { return ne; },
        getNorthWest: function () { return nw; },
        getSouthEast: function () { return se; },
        getCenter: function () { return { lat: (sw.lat + ne.lat) / 2, lng: (sw.lng + ne.lng) / 2 }; },
        getWest: function () { return nw.lng; },
        getEast: function () { return se.lng; },
        getSouth: function () { return sw.lat; },
        getNorth: function () { return ne.lat; },
        getLatNorth: function () { return ne.lat; },
        getLngWest: function () { return nw.lng; },
        contains: function (ll) {
          var p = toLatLng(ll);
          return p.lat >= sw.lat && p.lat <= ne.lat && p.lng >= nw.lng && p.lng <= se.lng;
        },
        intersects: function (other) {
          var o = other && other._sw ? other : null;
          if (!o) return false;
          return !(o.getEast() < nw.lng || o.getWest() > se.lng || o.getNorth() < sw.lat || o.getSouth() > ne.lat);
        },
        isValid: function () { return true; },
        extend: function () { return bounds; },
        toBBoxString: function () { return [nw.lng, sw.lat, se.lng, ne.lat].join(','); },
        pad: function () { return bounds; }
      };
      return bounds;
    };

    /* ---- view mutations ---- */

    // Keep the anchor's container pixel stable across a zoom change.
    function setZoomAround(anchorLatLng, newZoom) {
      var z = clamp(newZoom, map.options.minZoom, map.options.maxZoom);
      if (Math.abs(z - map._zoom) < 1e-6) return;
      var anchorPoint = project(anchorLatLng);          // absolute px at old zoom
      var oldCenterPoint = project(map._center);
      var oldOrigin = { x: oldCenterPoint.x - map._measure.w / 2, y: oldCenterPoint.y - map._measure.h / 2 };
      var containerPoint = { x: anchorPoint.x - oldOrigin.x, y: anchorPoint.y - oldOrigin.y };

      var scale = Math.pow(2, z - map._zoom);
      var newAnchorPoint = { x: anchorPoint.x * scale, y: anchorPoint.y * scale };
      var centerPoint = {
        x: newAnchorPoint.x - containerPoint.x + map._measure.w / 2,
        y: newAnchorPoint.y - containerPoint.y + map._measure.h / 2
      };
      map._zoom = z;
      map._center = {
        lng: xToLon(centerPoint.x / (TILE_SIZE * Math.pow(2, z))),
        lat: yToLat(centerPoint.y / (TILE_SIZE * Math.pow(2, z)))
      };
      map.fire('zoom', { zoom: map._zoom });
      map._invalidateTiles(true);
      map._invalidateVectors();
    }
    map.setZoomAround = function (latlng, zoom) {
      setZoomAround(toLatLng(latlng), zoom);
      map.fire('zoomend', { zoom: map._zoom });
      map.fire('moveend', { center: map.getCenter(), zoom: map._zoom });
      return map;
    };

    map.setZoom = function (zoom) {
      return map.setZoomAround(map._center, zoom);
    };

    map.setView = function (center, zoom, opts) {
      var z = (zoom == null) ? map._zoom : toZoom(zoom);
      map._center = toLatLng(center);
      map._zoom = clamp(z, map.options.minZoom, map.options.maxZoom);
      map._invalidateTiles(true);
      map._invalidateVectors();
      map.fire('move', { center: map.getCenter(), zoom: map._zoom });
      if (!opts || opts.reset !== false) {
        map.fire('moveend', { center: map.getCenter(), zoom: map._zoom });
        map.fire('zoomend', { zoom: map._zoom });
      }
      return map;
    };

    map.panTo = function (latlng) {
      map._center = toLatLng(latlng);
      map._invalidateTiles(false);
      map._invalidateVectors();
      map.fire('move', { center: map.getCenter(), zoom: map._zoom });
      map.fire('moveend', { center: map.getCenter(), zoom: map._zoom });
      return map;
    };
    map.panBy = function (offset) {
      var c = project(map._center);
      map._center = map.unproject({ x: c.x + (offset.x || 0), y: c.y - (offset.y || 0) });
      map._invalidateTiles(false);
      map._invalidateVectors();
      map.fire('move', { center: map.getCenter(), zoom: map._zoom });
      return map;
    };

    map.flyTo = function (center, zoom) {
      return map.setView(center, zoom == null ? map._zoom : zoom);
    };

    map.invalidateSize = function (animate) {
      var oldW = map._measure.w, oldH = map._measure.h;
      measure();
      if (oldW !== map._measure.w || oldH !== map._measure.h) {
        map._invalidateTiles(true);
        map._invalidateVectors();
      }
      map.fire('resize', { size: map.getSize() });
      return map;
    };

    /* ---- layers ---- */

    map.addLayer = function (layer) {
      if (!layer || layer._map === map) return map;
      if (layer._kind === 'tile' || (layer._template && !layer._kind)) {
        layer._map = map;
        layer._pane = el('div', 'leaflet-tile-pane', map._pane);
        layer._pane.style.position = 'absolute';
        layer._pane.style.left = '0';
        layer._pane.style.top = '0';
        layer._pane.style.zIndex = '200';
        // The first tile layer replaces the built-in empty pane.
        if (map._tilePane && map._tilePane.parentNode === map._pane) {
          map._pane.removeChild(map._tilePane);
        }
        map._tilePane = layer._pane;
        map._tileLayers.push(layer);
        map._invalidateTiles(true);
      } else {
        layer._map = map;
        map._vectors.push(layer);
        map._invalidateVectors();
      }
      map.fire('layeradd', { layer: layer });
      return map;
    };

    map.removeLayer = function (layer) {
      if (!layer) return map;
      if (layer._kind === 'tile') {
        var ti = map._tileLayers.indexOf(layer);
        if (ti >= 0) map._tileLayers.splice(ti, 1);
        if (layer._pane && layer._pane.parentNode) layer._pane.parentNode.removeChild(layer._pane);
        layer._active && layer._active.clear();
        layer._pane = null;
        layer._map = null;
        // Rebuild the base pane so later layers still have a host.
        if (!map._tileLayers.length) {
          map._tilePane = el('div', 'leaflet-pane leaflet-tile-pane', map._pane);
          map._tilePane.style.position = 'absolute';
          map._tilePane.style.zIndex = '200';
        }
      } else {
        var vi = map._vectors.indexOf(layer);
        if (vi >= 0) map._vectors.splice(vi, 1);
        layer._map = null;
        layer._tooltipOpen = false;
        map._invalidateVectors();
      }
      map.fire('layerremove', { layer: layer });
      return map;
    };

    map.hasLayer = function (layer) {
      if (!layer) return false;
      if (layer._kind === 'tile') return map._tileLayers.indexOf(layer) >= 0;
      return map._vectors.indexOf(layer) >= 0;
    };
    map.removeControl = function () { return map; };
    map.attributionControl = { addAttribution: function () { return map.attributionControl; } };

    map.remove = function () {
      if (map._destroyed) return map;
      map._destroyed = true;
      if (map._frame && global.cancelAnimationFrame) global.cancelAnimationFrame(map._frame);
      if (map._frameT && global.cancelAnimationFrame) global.cancelAnimationFrame(map._frameT);
      unbindInput();
      map._layers = [];
      map._tileLayers.forEach(function (l) { l._active && l._active.clear(); l._map = null; });
      map._tileLayers = [];
      map._vectors = [];
      containerEl.classList.remove('leaflet-container');
      containerEl.classList.remove('leaflet-touch');
      if (map._pane && map._pane.parentNode) map._pane.parentNode.removeChild(map._pane);
      if (map._tooltipBox && map._tooltipBox.parentNode) map._tooltipBox.parentNode.removeChild(map._tooltipBox);
      map.fire('unload');
      return map;
    };

    /* ---- scheduling ---- */

    map._invalidateVectors = function () {
      if (map._destroyed) return;
      map._dirtyVectors = true;
      map._schedule();
    };

    map._invalidateTiles = function (rebuild) {
      if (map._destroyed) return;
      if (rebuild) map._dirtyTiles = true;
      map._schedule();
    };

    map._schedule = function () {
      if (map._frame || map._destroyed) return;
      var raf = global.requestAnimationFrame || function (fn) { return global.setTimeout(fn, 16); };
      map._frame = raf(function () {
        map._frame = 0;
        if (map._destroyed) return;
        if (map._dirtyTiles) { map._dirtyTiles = false; _renderTiles(); }
        if (map._dirtyVectors) { map._dirtyVectors = false; _renderVectors(); }
      });
      map._frameT = map._frame;
    };

    /* ---- tile rendering ---- */

    function _renderTiles() {
      var layers = map._tileLayers;
      if (!layers.length) return;
      var o = originPixel();
      var w = map._measure.w, h = map._measure.h;

      for (var li = 0; li < layers.length; li++) {
        var layer = layers[li];
        var roundZ = Math.round(map._zoom);
        var scale = Math.pow(2, map._zoom - roundZ);   // CSS px per tile texel row
        var tileCss = TILE_SIZE * scale;
        var n = Math.pow(2, roundZ);
        // Absolute pixel space at the (integer) tile zoom.
        var ox = (lonToX(map._center.lng)) * TILE_SIZE * n - w / 2;
        var oy = (latToY(map._center.lat)) * TILE_SIZE * n - h / 2;

        var x0 = Math.floor(ox / TILE_SIZE) - 1;
        var y0 = Math.floor(oy / TILE_SIZE) - 1;
        var x1 = Math.floor((ox + w) / TILE_SIZE) + 1;
        var y1 = Math.floor((oy + h) / TILE_SIZE) + 1;

        var fresh = new global.Map();
        for (var ty = y0; ty <= y1; ty++) {
          for (var tx = x0; tx <= x1; tx++) {
            var wx = ((tx % n) + n) % n;
            if (ty < 0 || ty >= n) continue;
            var key = roundZ + '/' + wx + '/' + ty;
            var url = layer.getTileUrl({ x: wx, y: ty, z: roundZ });
            var rec = layer._active.get(key);
            if (!rec || rec.url !== url) {
              rec = { img: layer._getImage(url), url: url };
            }
            fresh.set(key, rec);
            var img = rec.img;
            if (img.parentNode !== layer._pane) layer._pane.appendChild(img);
            img.style.position = 'absolute';
            img.style.width = px(tileCss + 0.5);
            img.style.height = px(tileCss + 0.5);
            img.style.opacity = String(layer._opts.opacity);
            img.style.maxWidth = 'none';
            img.style.transform = 'translate3d(' + (tx * TILE_SIZE * scale - ox) + 'px,' + (ty * TILE_SIZE * scale - oy) + 'px,0)';
            img.style.willChange = 'transform';
          }
        }
        // Drop tiles that scrolled out of the viewport, but leave the images in
        // the LRU cache so panning back is instant.
        layer._active.forEach(function (rec, key) {
          if (!fresh.has(key)) {
            if (rec.img && rec.img.parentNode) rec.img.parentNode.removeChild(rec.img);
          }
        });
        layer._active = fresh;
        layer._z = roundZ;
      }
      map.fire('tilesloaded');
    }

    map._renderTilesNow = function () { _renderTiles(); };

    /* ---- vector rendering ---- */

    // Set by host pages that draw metre-radius circles (radar.js publishes the
    // degrees-per-metre scale here). When unset, L.circle radii fall back to px.
    map.unitsPerMeter = null;

    function styleOf(layer) {
      return layer._style || {};
    }

    function pxOf(latlng) {
      return map.latLngToContainerPoint(latlng);
    }

    function drawShape(ctx, shape, x, y, r, rot, style) {
      var fill = style.fill !== false;
      var stroke = style.stroke !== false && (style.weight || 0) > 0;
      ctx.beginPath();
      switch (shape) {
        case 'triangle':
          ctx.moveTo(x, y - r * 1.35);
          ctx.lineTo(x + r * 0.95, y + r * 0.85);
          ctx.lineTo(x - r * 0.95, y + r * 0.85);
          ctx.closePath();
          break;
        case 'diamond':
          ctx.moveTo(x, y - r * 1.25);
          ctx.lineTo(x + r, y);
          ctx.lineTo(x, y + r * 1.25);
          ctx.lineTo(x - r, y);
          ctx.closePath();
          break;
        case 'square':
          ctx.rect(x - r, y - r, r * 2, r * 2);
          break;
        case 'ring':
          ctx.arc(x, y, r, 0, Math.PI * 2);
          break;
        case 'cross':
          ctx.moveTo(x - r, y - r); ctx.lineTo(x + r, y + r);
          ctx.moveTo(x + r, y - r); ctx.lineTo(x - r, y + r);
          break;
        case 'circle':
        default:
          ctx.arc(x, y, r * 0.85, 0, Math.PI * 2);
          break;
      }
      if (fill && shape !== 'cross') {
        ctx.fillStyle = style.fillColor || style.color || '#38d39f';
        ctx.globalAlpha = (style.fillOpacity == null ? 0.85 : style.fillOpacity);
        ctx.fill();
        ctx.globalAlpha = 1;
      }
      if (stroke) {
        ctx.strokeStyle = style.color || '#ffffff';
        ctx.lineWidth = style.weight || 1.5;
        ctx.globalAlpha = (style.opacity == null ? 1 : style.opacity);
        ctx.stroke();
        ctx.globalAlpha = 1;
      }
    }

    function _renderVectors() {
      var ctx = map._ctx;
      if (!ctx) return;
      var dpr = map._dpr || 1;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, map._measure.w, map._measure.h);
      ctx.lineJoin = 'round';
      ctx.lineCap = 'round';

      var list = map._vectors;
      for (var i = 0; i < list.length; i++) {
        var layer = list[i];
        var style = styleOf(layer);
        if (style.visible === false) continue;
        var kind = layer._kind;
        if (kind === 'polyline' || kind === 'polygon') {
          var pts = layer._latlngs || [];
          if (pts.length < 2) continue;
          ctx.beginPath();
          for (var p = 0; p < pts.length; p++) {
            var cp = pxOf(pts[p]);
            if (p === 0) ctx.moveTo(cp.x, cp.y); else ctx.lineTo(cp.x, cp.y);
          }
          if (kind === 'polygon') ctx.closePath();
          if (style.fill && kind === 'polygon') {
            ctx.globalAlpha = style.fillOpacity == null ? 0.15 : style.fillOpacity;
            ctx.fillStyle = style.fillColor || style.color;
            ctx.fill();
            ctx.globalAlpha = 1;
          }
          if (style.stroke !== false) {
            ctx.globalAlpha = style.opacity == null ? 0.9 : style.opacity;
            ctx.strokeStyle = style.color || '#ffb020';
            ctx.lineWidth = style.weight || 2;
            if (style.dashArray) ctx.setLineDash(String(style.dashArray).split(/[ ,]+/).map(Number));
            ctx.stroke();
            ctx.setLineDash([]);
            ctx.globalAlpha = 1;
          }
        } else if (kind === 'circle') {
          if (!layer._latlng) continue;
          var cc = pxOf(layer._latlng);
          var metres = layer._radiusM || 0;
          // The host page supplies the metres-per-CSS-pixel figure; without one
          // a metre radius is meaningless, so the circle degrades to a point.
          var perMeter = (typeof map._metresToPx === 'function') ? map._metresToPx() : 0;
          var rr = Math.max(1, metres * perMeter);
          ctx.beginPath();
          ctx.arc(cc.x, cc.y, rr, 0, Math.PI * 2);
          if (style.fill !== false) {
            ctx.globalAlpha = style.fillOpacity == null ? 0.08 : style.fillOpacity;
            ctx.fillStyle = style.fillColor || style.color || '#ffb020';
            ctx.fill();
            ctx.globalAlpha = 1;
          }
          if (style.stroke !== false) {
            ctx.globalAlpha = style.opacity == null ? 0.5 : style.opacity;
            ctx.strokeStyle = style.color || '#ffb020';
            ctx.lineWidth = style.weight || 1;
            if (style.dashArray) ctx.setLineDash(String(style.dashArray).split(/[ ,]+/).map(Number));
            ctx.stroke();
            ctx.setLineDash([]);
            ctx.globalAlpha = 1;
          }
        } else {
          // marker / circleMarker
          if (!layer._latlng) continue;
          var pt = pxOf(layer._latlng);
          var radius = style.radius == null ? 6 : style.radius;
          var rot = style.rotation || 0;
          ctx.save();
          ctx.translate(pt.x, pt.y);
          if (rot) ctx.rotate(rot);
          ctx.translate(-pt.x, -pt.y);
          drawShape(ctx, style.shape || 'circle', pt.x, pt.y, radius, rot, style);
          ctx.restore();
          layer._screen = pt;
        }
      }
      _updateTooltip();
    }

    /* ---- tooltips (canvas hit-test) ---- */

    function _updateTooltip() {
      var box = map._tooltipBox;
      if (!box) return;
      var hover = null;
      var best = 22 * 22;
      var mx = map._mouseContainer.x, my = map._mouseContainer.y;
      for (var i = map._vectors.length - 1; i >= 0; i--) {
        var layer = map._vectors[i];
        if (!layer._tooltip || !layer._screen) continue;
        var dx = layer._screen.x - mx, dy = layer._screen.y - my;
        var d2 = dx * dx + dy * dy;
        if (d2 <= best) { best = d2; hover = layer; }
      }
      if (map._hoverLayer && map._hoverLayer !== hover) {
        map._hoverLayer._tooltipOpen = false;
      }
      map._hoverLayer = hover;
      if (!hover) { box.style.display = 'none'; return; }
      hover._tooltipOpen = true;
      box.textContent = hover._tooltip;
      box.style.display = 'block';
      box.style.left = px(hover._screen.x + 12);
      box.style.top = px(hover._screen.y - 10);
    }

    /* ---- input: drag, wheel, pinch, mouse events ---- */

    // Screen point -> container point, undoing any CSS rotation the host page
    // applied to the panes (radar.js rotates them in 跟随朝向 / 3D mode).
    function localPoint(ev) {
      var rect = containerEl.getBoundingClientRect();
      var x = ev.clientX - rect.left;
      var y = ev.clientY - rect.top;
      var rot = num(map._rotationDeg) || 0;
      if (rot) {
        var r = -rot * Math.PI / 180;
        var cx = map._measure.w / 2, cy = map._measure.h / 2;
        var dx = x - cx, dy = y - cy;
        x = cx + dx * Math.cos(r) - dy * Math.sin(r);
        y = cy + dx * Math.sin(r) + dy * Math.cos(r);
      }
      return { x: x, y: y };
    }

    function num(v) {
      v = Number(v);
      return isFinite(v) ? v : 0;
    }

    function emitMouse(type, ev) {
      var pt = localPoint(ev);
      map._mouseContainer = pt;
      var data = {
        latlng: map.containerPointToLatLng(pt),
        containerPoint: pt,
        layerPoint: pt,
        originalEvent: ev,
        clientX: ev.clientX,
        clientY: ev.clientY
      };
      map.fire(type, data);
    }

    var dragging = false;
    var draggedEnough = false;
    var lastPointer = null;

    function onPointerDown(ev) {
      if (ev.button != null && ev.button !== 0 && ev.pointerType !== 'touch') return;
      map._pins.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
      if (map._pins.size === 1) {
        dragging = true;
        draggedEnough = false;
        lastPointer = { x: ev.clientX, y: ev.clientY };
        if (containerEl.setPointerCapture) {
          try { containerEl.setPointerCapture(ev.pointerId); } catch (e) { /* ignore */ }
        }
      } else {
        dragging = false;   // two fingers: switch to pinch
      }
    }

    function onPointerMove(ev) {
      map._pins.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
      emitMouse('mousemove', ev);
      if (map._pins.size >= 2) { pinchMove(); return; }
      if (!dragging || !lastPointer) return;
      var dx = ev.clientX - lastPointer.x;
      var dy = ev.clientY - lastPointer.y;
      if (Math.abs(dx) + Math.abs(dy) > 2) draggedEnough = true;
      lastPointer = { x: ev.clientX, y: ev.clientY };
      if (!dx && !dy) return;
      // The geo under the pointer must stay put: pointer moves +dx while the
      // viewport slides -dx, so the centre walks the other way.
      var c = project(map._center);
      map._center = map.unproject({ x: c.x + dx, y: c.y + dy });
      map._invalidateTiles(false);
      map._invalidateVectors();
      map.fire('move', { center: map.getCenter(), zoom: map._zoom, originalEvent: ev });
    }

    function onPointerUp(ev) {
      map._pins.delete(ev.pointerId);
      if (map._pins.size === 0) {
        if (dragging) {
          dragging = false;
          map.fire('moveend', { center: map.getCenter(), zoom: map._zoom });
        }
        lastPointer = null;
      } else if (map._pins.size === 1) {
        var only = map._pins.values().next().value;
        lastPointer = { x: only.x, y: only.y };
        dragging = true;
      }
      if (containerEl.releasePointerCapture) {
        try { containerEl.releasePointerCapture(ev.pointerId); } catch (e) { /* ignore */ }
      }
    }

    function onPointerCancel(ev) {
      map._pins.delete(ev.pointerId);
      dragging = false;
      lastPointer = null;
    }

    function onClick(ev) {
      if (draggedEnough) { draggedEnough = false; return; }
      emitMouse('click', ev);
    }

    function onWheel(ev) {
      if (ev.ctrlKey || ev.metaKey) {
        // Trackpad/desktop pinch-zoom gesture.
        ev.preventDefault();
      } else {
        ev.preventDefault();
      }
      var pt = localPoint(ev);
      var anchor = map.containerPointToLatLng(pt);
      var delta = -ev.deltaY;
      if (ev.deltaMode === 1) delta *= 16;             // lines
      else if (ev.deltaMode === 2) delta *= map._measure.h;
      var step = clamp(delta, -160, 160) / 400;        // ~0.4 zoom per wheel notch
      map._mouseContainer = pt;
      setZoomAround(anchor, map._zoom + step);
      map._invalidateVectors();
      map.fire('zoomend', { zoom: map._zoom });
    }

    var pinchStart = null;
    function pinchMove() {
      var pts = Array.prototype.slice.call(map._pins.values());
      if (pts.length < 2) { pinchStart = null; return; }
      var dist = Math.hypot(pts[0].x - pts[1].x, pts[0].y - pts[1].y);
      var midClient = { x: (pts[0].x + pts[1].x) / 2, y: (pts[0].y + pts[1].y) / 2 };
      var rect = containerEl.getBoundingClientRect();
      var mid = { x: midClient.x - rect.left, y: midClient.y - rect.top };
      if (!pinchStart) {
        pinchStart = { dist: dist, zoom: map._zoom, anchor: map.containerPointToLatLng(mid) };
        return;
      }
      if (pinchStart.dist <= 0) return;
      var ratio = dist / pinchStart.dist;
      var target = pinchStart.zoom + Math.log(ratio) / Math.LN2;
      setZoomAround(pinchStart.anchor, target);
      map._mouseContainer = mid;
      map._invalidateVectors();
    }

    function bindInput() {
      containerEl.addEventListener('pointerdown', onPointerDown);
      containerEl.addEventListener('pointermove', onPointerMove);
      containerEl.addEventListener('pointerup', onPointerUp);
      containerEl.addEventListener('pointercancel', onPointerCancel);
      containerEl.addEventListener('wheel', onWheel, { passive: false });
      containerEl.addEventListener('click', onClick);
      containerEl.addEventListener('contextmenu', function (ev) { ev.preventDefault(); });
    }
    function unbindInput() {
      containerEl.removeEventListener('pointerdown', onPointerDown);
      containerEl.removeEventListener('pointermove', onPointerMove);
      containerEl.removeEventListener('pointerup', onPointerUp);
      containerEl.removeEventListener('pointercancel', onPointerCancel);
      containerEl.removeEventListener('wheel', onWheel);
      containerEl.removeEventListener('click', onClick);
    }

    // Reset the pinch baseline whenever the gesture ends, never mid-gesture.
    map.on('moveend', function () { if (map._pins.size < 2) pinchStart = null; });

    bindInput();

    // Host pages that rotate the panes publish the angle here so pointer
    // coordinates can be un-rotated for hit-testing / events.
    map._rotationDeg = 0;

    // First paint: synchronously build tiles + canvas so that the readiness
    // probe (`.leaflet-container` + visible children) passes immediately.
    _renderTiles();
    _renderVectors();

    return map;
  }

  /* ------------------------------------------------------------------ *
   * Public namespace
   * ------------------------------------------------------------------ */

  var L = {
    version: 'leaflet-lite/1.0.0',
    TILE_SIZE: TILE_SIZE,

    map: function (target, options) {
      var containerEl = (typeof target === 'string') ? document.getElementById(target) : target;
      if (!containerEl) throw new Error('leaflet-lite: map container not found');
      return new LiteMap(containerEl, options);
    },

    tileLayer: function (urlTemplate, options) {
      return new TileLayer(urlTemplate, options);
    },

    marker: function (latlng, options) {
      return makePointLayer('marker', latlng, options);
    },

    circleMarker: function (latlng, options) {
      return makePointLayer('circleMarker', latlng, options);
    },

    circle: function (latlng, options) {
      return makeCircleLayer(latlng, options);
    },

    polyline: function (latlngs, options) {
      return makePathLayer('polyline', latlngs, options);
    },

    polygon: function (latlngs, options) {
      return makePathLayer('polygon', latlngs, options);
    },

    latLng: function (a, b) { return toLatLng(b == null ? a : [a, b]); },
    latLngBounds: function (sw, ne) { return { getSouthWest: function () { return toLatLng(sw); }, getNorthEast: function () { return toLatLng(ne); } }; },

    // Icon is a plain descriptor here; the canvas renderer reads shape/colors.
    icon: function (options) { return shallowMerge({}, options || {}); },
    divIcon: function (options) { return shallowMerge({}, options || {}); },

    point: function (x, y) { return { x: x, y: y }; },
    bounds: function (a, b) {
      var sw = toLatLng(a), ne = toLatLng(b);
      return { getSouthWest: function () { return sw; }, getNorthEast: function () { return ne; }, isValid: function () { return true; } };
    },
    DomUtil: {
      create: function (tag, cls, parent) { return el(tag, cls, parent); },
      addClass: function (node, cls) { if ((' ' + node.className + ' ').indexOf(' ' + cls + ' ') < 0) node.className = (node.className + ' ' + cls).trim(); },
      removeClass: function (node, cls) {
        node.className = (' ' + node.className + ' ').replace(' ' + cls + ' ', ' ').trim();
      },
      setTransform: function (node, p) {
        node.style.transform = 'translate3d(' + (p.x || 0) + 'px,' + (p.y || 0) + 'px,0)';
      }
    },
    Browser: {
      touch: ('ontouchstart' in global) || (global.navigator && global.navigator.maxTouchPoints > 0),
      retina: (global.devicePixelRatio || 1) > 1
    }
  };

  L.offlineTilePng = OFFLINE_TILE_PNG;

  global.L = L;
  if (typeof module !== 'undefined' && module.exports) module.exports = L;
})(typeof window !== 'undefined' ? window : globalThis);
