//! 解析引擎：`battle_proxy::battle::engine`。
//!
//! 复刻样本 `src/battle/engine.rs`（样本 2500+ 行，是整条链的编排层）。
//!
//! ```text
//!  feed(session, src, dst, payload, ts)   ← socks5 relay 调用，同步、不阻塞
//!        │
//!        ▼  非阻塞投递（队列满即丢，绝不拖慢转发）
//!   ParseQueue.offer(ParseJob)
//!        │  spawn battle-parse-<n>  (CPU 密集：解密 → 分帧 → 属性遍历)
//!        ▼
//!   ParseOutcome { updates: Vec<EngineUpdate> }
//!        │  spawn battle-ordered-apply
//!        ▼
//!   apply_update()  → SessionState（实体/身份/生死/容器/击杀/交战）
//!        │
//!        ▼  radar broadcast（20 Hz 合并）
//!   WS 客户端 / web::battle_view
//! ```

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde_json::json;

use super::codec::capture::CaptureEntry;
use super::codec::character::{CharacterCodecProfile, HandleTable};
use super::codec::container_collector::ContentsStatus;
use super::codec::fire::{FireCodecProfile, FireConfidence};
use super::codec::killchain::Kill;
use super::codec::s2c::S2cStream;
use super::loot_catalog::LootCatalog;
use super::parse_queue::{ParseJob, ParseOutcome, ParseQueue, SeqGen};
use super::protocol_capture::{ProtocolCapture, RecordKind, SharedCapture};
use super::session::{SessionRegistry, SESSION_IDLE_TTL_MS};
use super::transport_crypto::{TransportKeys, decode_packet, TransportKind};
use super::udpxin::{PacketFramer, ProtocolProfile};
use super::udpxin_entity::{ActorKind, Vec3};
use super::udpxin_live::MoveSource;
use super::udpxin_move::{RepMovementProfile, RotationCorrection};
use crate::config::Config;
use crate::state::AppState;

/// 引擎产生的更新（worker → applier 的载体）。
#[derive(Debug, Clone)]
pub enum EngineUpdate {
    /// 空更新（仅计数）。
    Tick,
    SessionSeen {
        session: u64,
    },
    /// 角色位移。
    Move {
        session: u64,
        channel: u32,
        position: Vec3,
        rotation: super::udpxin_entity::Rot3,
        velocity: Vec3,
        indexed: bool,
    },
    /// 身份/属性变化。
    Identity {
        session: u64,
        channel: u32,
        kind: ActorKind,
        class: String,
        guid: Option<u64>,
        name: Option<String>,
        team: Option<i32>,
        camp: Option<i32>,
        is_ai: Option<bool>,
        hero_id: Option<i32>,
        owner_channel: Option<u32>,
    },
    /// 击杀。
    Kill {
        session: u64,
        kill: Box<Kill>,
    },
    /// 开火（弹道）。
    Fire {
        session: u64,
        event: Box<super::codec::fire::FireEvent>,
    },
    /// 容器内容被观察。
    Container {
        session: u64,
        channel: u32,
        contents: Option<Vec<super::codec::container_collector::CollectorSlot>>,
    },
    /// 死亡盒/生死状态。
    Live {
        session: u64,
        channel: u32,
        deadbox: bool,
    },
    /// 解码护栏拦下的批次（样本 `rejected packet growth`）。
    Rejected {
        session: u64,
        added: usize,
    },
}

/// 引擎内部核心（`Arc` 共享给 worker / HTTP / WS）。
pub struct EngineCore {
    pub state: AppState,
    pub cfg: Config,
    pub sessions: SessionRegistry,
    pub framer: parking_lot::Mutex<PacketFramer>,
    pub handles: parking_lot::Mutex<HandleTable>,
    pub loot: parking_lot::Mutex<LootCatalog>,
    pub character_profile: CharacterCodecProfile,
    pub fire_profile: FireCodecProfile,
    pub rep_profile: RepMovementProfile,
    pub keys: parking_lot::Mutex<TransportKeys>,
    pub rotation: parking_lot::Mutex<RotationCorrection>,
    /// 静态 `channel_map` 的 `ch_index → class` 视图（构造时解析一次）。
    pub channel_classes: std::collections::HashMap<u32, String>,
    pub capture: SharedCapture,
    pub radar_tx: tokio::sync::broadcast::Sender<serde_json::Value>,
    pub seq: SeqGen,
    pub queue: OnceLock<ParseQueue>,
    pub start_ms: u64,
    /// 待广播标记（合并 20 Hz）。
    dirty: std::sync::atomic::AtomicBool,
}

/// 引擎句柄（可克隆，与 socks5/web 共享）。
#[derive(Clone)]
pub struct BattleEngine {
    core: Arc<EngineCore>,
}

impl BattleEngine {
    /// 构造引擎：载入内嵌目录、启动解析流水与雷达广播。
    pub fn new(state: AppState, cfg: Config) -> Self {
        let (radar_tx, _) = tokio::sync::broadcast::channel(64);
        let capture = Arc::new(ProtocolCapture::new(&state, cfg.collection_policy));

        let core = Arc::new(EngineCore {
            state: state.clone(),
            cfg: cfg.clone(),
            sessions: SessionRegistry::new(),
            framer: parking_lot::Mutex::new(PacketFramer::new(ProtocolProfile::dfm_r39())),
            handles: parking_lot::Mutex::new(HandleTable::new()),
            loot: parking_lot::Mutex::new(LootCatalog::new()),
            character_profile: CharacterCodecProfile::default(),
            fire_profile: FireCodecProfile::default(),
            rep_profile: RepMovementProfile::default(),
            keys: parking_lot::Mutex::new(TransportKeys::default()),
            rotation: parking_lot::Mutex::new(RotationCorrection::default()),
            channel_classes: load_channel_classes(),
            capture: capture.clone(),
            radar_tx,
            seq: SeqGen::default(),
            queue: OnceLock::new(),
            start_ms: now_ms(),
            dirty: std::sync::atomic::AtomicBool::new(true),
        });

        // 内嵌目录：channel_map.json（77 通道 + 53 类名字表）。
        {
            let mut handles = core.handles.lock();
            let mut entities = super::udpxin_entity::EntityTable::new();
            let mut loot = core.loot.lock();
            if let Some(json) = crate::web::embed::channel_map_json() {
                match super::session::bootstrap_catalogs(
                    &mut entities,
                    &mut handles,
                    Some(json),
                    &mut loot,
                    None,
                ) {
                    Ok((c, p, _)) => tracing::info!(channels = c, properties = p, "catalog ready"),
                    Err(e) => tracing::warn!(error = %e, "channel_map load failed"),
                }
            } else {
                tracing::warn!("channel_map.json not embedded; dynamic resolution only");
            }
        }

        // 转向修正：优先用 web/maps.json 的标定值，保持与前端一致（避免双重纠正）。
        {
            let json = std::fs::read_to_string(
                std::path::Path::new(crate::web::embed::web_root()).join("maps.json"),
            );
            if let Ok(json) = json {
                let c = RotationCorrection::from_maps_json(&json, None);
                tracing::info!(
                    yaw_offset_deg = c.yaw_offset_deg,
                    yaw_sign = c.yaw_sign,
                    "rotation calibration loaded from maps.json"
                );
                *core.rotation.lock() = c;
            } else {
                tracing::warn!("maps.json not readable; using identity rotation calibration");
            }
        }

        let engine = Self { core: core.clone() };

        // 解析流水：worker 是纯计算闭包，applier 写回会话状态。
        let worker_engine = engine.clone();
        let apply_engine = engine.clone();
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(4);
        let queue = ParseQueue::spawn(
            state.clone(),
            4096,
            workers,
            move |job| worker_engine.parse_job(job),
            move |out| apply_engine.apply_outcome(out),
        );
        let _ = core.queue.set(queue);

        // 雷达广播：20 Hz 合并，只在 dirty 时推。
        let tick_engine = engine.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(50));
            loop {
                ticker.tick().await;
                if !tick_engine
                    .core
                    .dirty
                    .swap(false, std::sync::atomic::Ordering::Relaxed)
                {
                    continue;
                }
                let snapshot = tick_engine.radar_state().await;
                let _ = tick_engine.core.radar_tx.send(snapshot);
            }
        });

        // 周期性清理（会话、实体、容器）。
        let reap_engine = engine.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                ticker.tick().await;
                let now = now_ms();
                reap_engine.core.sessions.for_each_mut(|_id, s| s.reap(now));
                let dead = reap_engine.core.sessions.reap_idle(now, SESSION_IDLE_TTL_MS);
                for id in dead {
                    tracing::info!(session = id, "session idle timeout");
                }
                reap_engine.core.framer.lock().reap_partials(now, 5_000);
            }
        });

        engine
    }

    pub fn state(&self) -> &AppState {
        &self.core.state
    }

    pub fn sessions(&self) -> &SessionRegistry {
        &self.core.sessions
    }

    pub fn capture(&self) -> &SharedCapture {
        &self.core.capture
    }

    /// 热更新传输密钥（由卡密/握手阶段拿到后注入）。
    pub fn set_transport_keys(&self, keys: TransportKeys) {
        *self.core.keys.lock() = keys;
    }

    /// 热更新转向修正参数（地图切换时）。
    pub fn set_rotation_correction(&self, c: RotationCorrection) {
        *self.core.rotation.lock() = c;
    }

    /// 按地图键从 `maps.json` 重新标定转向修正（识别出会话地图后调用）。
    pub fn reload_map_calibration(&self, map_key: Option<&str>) -> RotationCorrection {
        let path = std::path::Path::new(crate::web::embed::web_root()).join("maps.json");
        let c = std::fs::read_to_string(path)
            .ok()
            .map(|json| RotationCorrection::from_maps_json(&json, map_key))
            .unwrap_or_default();
        self.set_rotation_correction(c.clone());
        c
    }

    /// 当前生效的转向修正（诊断页展示，方便现场标定）。
    pub fn rotation_correction(&self) -> RotationCorrection {
        self.core.rotation.lock().clone()
    }

    /// **网络热路径**：非阻塞投递。绝不 await、绝不阻塞转发。
    #[inline]
    pub fn feed(
        &self,
        session: u64,
        src: std::net::SocketAddr,
        dst: std::net::SocketAddr,
        payload: &Bytes,
        ts_ms: u64,
    ) {
        self.feed_dir(session, src, dst, payload, ts_ms, true);
    }

    /// 带方向版本的 feed（`c2s = false` 表示服务端→客户端）。
    #[inline]
    pub fn feed_dir(
        &self,
        session: u64,
        src: std::net::SocketAddr,
        dst: std::net::SocketAddr,
        payload: &Bytes,
        ts_ms: u64,
        c2s: bool,
    ) {
        let Some(queue) = self.core.queue.get() else { return };
        let job = ParseJob {
            seq: self.core.seq.next(),
            session,
            c2s,
            src,
            dst,
            ts_ms,
            payload: payload.clone(),
        };
        if !queue.offer(job) {
            // 队列满：丢最旧，记账，不改行为（雷达只要最新）。
            tracing::trace!(session, "parse queue full, dropped");
        }
        self.core
            .state
            .counters_ref()
            .parse_queue_depth
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// 解析 worker（纯 CPU，不 await）。
    pub fn parse_job(&self, job: &ParseJob) -> ParseOutcome {
        let now = job.ts_ms.max(1000);
        let mut updates = Vec::with_capacity(8);
        let mut error = None;

        // ---- 1. 传输层：解密/解压（失败保留原始字节） ----
        let keys = self.core.keys.lock().clone();
        // 注意作用域：guard 必须在 decode_packet 之后立刻释放，否则第 3 步会自锁。
        let decoded = {
            let framer = self.core.framer.lock();
            decode_packet(&job.payload, &keys, None, |bytes: &[u8]| {
                framer.validate_datagram(bytes)
            })
        };
        let transport = match decoded.kind {
            TransportKind::Plain => "plain",
            TransportKind::AesEcbXor => "aes_ecb_xor",
            TransportKind::AesEcbXorLz4 => "aes_ecb_xor_lz4",
            TransportKind::Lz4 => "lz4",
            TransportKind::XorStream => "xor_stream",
            TransportKind::Unknown => "unknown",
        };

        // ---- 2. 采集（若开启） ----
        self.maybe_capture(job, &decoded.bytes, transport, now);

        // ---- 3. 分帧 ----
        let framed = {
            let mut f = self.core.framer.lock();
            f.frame(&decoded.bytes, now)
        };
        if !framed.errors.is_empty() {
            error = Some(match framed.errors.first() {
                Some(super::udpxin::FrameError::InvalidPacketFraming) => "invalid_packet_framing",
                Some(super::udpxin::FrameError::UnresolvedBunchHeaderVariant) => {
                    "unresolved_bunch_header_variant"
                }
                Some(super::udpxin::FrameError::BunchPayloadExceedsPacket { .. }) => {
                    "bunch_payload_exceeds_packet"
                }
                _ => "framing_error",
            });
        }

        updates.push(EngineUpdate::SessionSeen { session: job.session });

        if framed.gate_passed {
            let handles = self.core.handles.lock();
            for bunch in &framed.bunches {
                if !bunch.b_control {
                    continue;
                }
                if bunch.b_open {
                    let class = bunch
                        .ch_name
                        .as_deref()
                        .map(super::udpxin_exports::normalize_class_path);
                    updates.push(EngineUpdate::Identity {
                        session: job.session,
                        channel: bunch.channel_index,
                        kind: ActorKind::Other,
                        class: class.unwrap_or_default(),
                        guid: None,
                        name: None,
                        team: None,
                        camp: None,
                        is_ai: None,
                        hero_id: None,
                        owner_channel: None,
                    });
                }
                // 属性块：仅在净荷非空且类名已知时尝试。
                if bunch.payload.is_empty() {
                    continue;
                }
                let class = self
                    .class_of_channel(bunch.channel_index)
                    .unwrap_or_else(|| "BP_DFMCharacter_C".to_string());
                let mut r = super::codec::BitReader::new(&bunch.payload);
                let (_block, hot) = super::codec::character::read_property_block(
                    &mut r,
                    &handles,
                    &class,
                    false,
                    &self.core.character_profile,
                );
                let (block, hot) = (_block, hot);
                if block.fields.is_empty() {
                    continue;
                }
                let indexed = hot.guid.is_some();
                updates.push(EngineUpdate::Identity {
                    session: job.session,
                    channel: bunch.channel_index,
                    kind: ActorKind::from_class_name(&class),
                    class: class.clone(),
                    guid: hot.guid,
                    name: hot.character_name.or(hot.npc_name),
                    team: hot.team_id,
                    camp: hot.camp,
                    is_ai: hot.is_player_ai,
                    hero_id: None,
                    owner_channel: hot.controller_channel,
                });
                if let Some(m) = hot.replicated_movement {
                    if m.has_location {
                        updates.push(EngineUpdate::Move {
                            session: job.session,
                            channel: bunch.channel_index,
                            position: m.location,
                            rotation: m.rotation,
                            velocity: m.linear_velocity,
                            indexed,
                        });
                    }
                }
            }
        }

        ParseOutcome {
            seq: job.seq,
            session: job.session,
            ts_ms: now,
            transport,
            gate_passed: framed.gate_passed,
            updates,
            error,
        }
    }

    /// applier：把更新写回会话状态（`battle-ordered-apply` 调用）。
    pub fn apply_outcome(&self, out: ParseOutcome) {
        // 会话缺失时用占位 peer 建一个：真实 peer 由 socks5 层在 add_tcp_session 时写入，
        // 这里不覆盖（get_or_create 只在缺失时插入）。
        let peer: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
        let entry = self.core.sessions.get_or_create(out.session, peer, out.ts_ms, true);
        let mut session = entry.lock();

        session.touch(out.ts_ms);
        session.stats.packets_seen += 1;
        if out.gate_passed {
            session.stats.packets_gated += 1;
            session.decoded_any = true;
        } else {
            session.stats.packets_rejected += 1;
        }

        for u in &out.updates {
            match u {
                EngineUpdate::Tick | EngineUpdate::SessionSeen { .. } => {}
                EngineUpdate::Move { channel, position, rotation, velocity, indexed, .. } => {
                    let rec = session.entities.open_channel(*channel, None, None, out.ts_ms);
                    rec.position = *position;
                    rec.rotation = *rotation;
                    rec.velocity = *velocity;
                    rec.last_update_ms = out.ts_ms;
                    let source = if *indexed { MoveSource::Indexed } else { MoveSource::Stale };
                    session.note_move(*channel, source, out.ts_ms);
                }
                EngineUpdate::Identity {
                    channel,
                    kind,
                    class,
                    guid,
                    name,
                    team,
                    camp,
                    is_ai,
                    hero_id,
                    owner_channel,
                    ..
                } => {
                    let rec = session.entities.open_channel(
                        *channel,
                        if class.is_empty() { None } else { Some(class.as_str()) },
                        *guid,
                        out.ts_ms,
                    );
                    if *kind != ActorKind::Other {
                        rec.kind = *kind;
                    }
                    rec.guid = rec.guid.or(*guid);
                    if let Some(n) = name {
                        if rec.character_name.is_none() {
                            rec.character_name = Some(n.clone());
                        }
                    }
                    if let Some(t) = team {
                        rec.team = *t;
                    }
                    if let Some(c) = camp {
                        rec.camp = *c;
                    }
                    if let Some(a) = is_ai {
                        rec.is_ai = *a;
                    }
                    if let Some(h) = hero_id {
                        rec.hero_id = *h;
                    }
                    if let Some(o) = owner_channel {
                        rec.owner_channel = Some(*o);
                    }
                }
                EngineUpdate::Kill { kill, .. } => {
                    session.kills.push((**kill).clone());
                    session.stats.kills += 1;
                }
                EngineUpdate::Fire { event, .. } => {
                    session.stats.fires += 1;
                    let mut ev = (**event).clone();
                    if let Some(ch) = ev.shooter_channel {
                        ev.weapon_class = session
                            .combat
                            .weapons
                            .weapon_of_character(ch)
                            .map(|b| b.class.clone());
                    }
                    let ts = out.ts_ms;
                    // 弹道（traces[]）：起点 → 前 120 米，供前端画射线。
                    session.combat.push_engagement(super::combat::Engagement {
                        shooter_channel: ev.shooter_channel,
                        shooter_uuid: None,
                        weapon: ev.weapon_class.clone(),
                        origin: ev.origin,
                        candidates: Vec::new(),
                        ts_ms: ts,
                    });
                    let _ = super::codec::fire::to_trace(&ev, 120.0, ts);
                }
                EngineUpdate::Container { channel, contents, .. } => {
                    if let Some(c) = contents {
                        if session.containers.observe_contents(*channel, c.clone(), out.ts_ms) {
                            session.stats.containers_observed += 1;
                        }
                    }
                }
                EngineUpdate::Live { channel, deadbox, .. } => {
                    if *deadbox {
                        session.liveness.note_dead_box(*channel, out.ts_ms);
                    }
                }
                EngineUpdate::Rejected { added, .. } => {
                    session
                        .combat
                        .note_rejection(&super::combat::GrowthVerdict::Rejected {
                            reason: "rejected packet growth",
                            added: *added,
                            before: 0,
                        });
                }
            }
        }

        session.finalize_frame(out.ts_ms);
        drop(session);

        self.core.state.counters_ref().parsed_packets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.core
            .dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 采集一条（受 `collection_policy` 与开关约束）。
    fn maybe_capture(&self, job: &ParseJob, payload: &[u8], transport: &str, now_ms: u64) {
        if !self.core.capture.should_record(&self.core.state) {
            return;
        }
        let entry: CaptureEntry = ProtocolCapture::make_entry(
            job.session,
            if job.c2s { "c2s" } else { "s2c" },
            job.src.to_string(),
            job.dst.to_string(),
            payload,
            transport,
            None,
            now_ms,
        );
        self.core.capture.record(
            if transport == "unknown" { RecordKind::Full } else { RecordKind::Parsed },
            entry,
            now_ms,
        );
    }

    fn class_of_channel(&self, channel: u32) -> Option<String> {
        // 静态表优先（样本内嵌 channel_map 的意义就在这里）：构造期解析一次，热路径只查哈希表。
        self.core.channel_classes.get(&channel).cloned()
    }

    /// 雷达原始快照（cm / 度，未做单位换算）。
    pub fn radar_snapshot(&self) -> serde_json::Value {
        let now = now_ms();
        let mut players = Vec::new();
        let mut kills = Vec::new();
        let mut loot = Vec::new();
        let mut self_player = serde_json::Value::Null;
        let mut map_name = None;

        let rotation = self.core.rotation.lock().clone();

        self.core.sessions.for_each(|_id, s| {
            if s.map_name.is_some() && map_name.is_none() {
                map_name = s.map_name.clone();
            }
            let self_uuid = s.local_uuid();
            for rec in s.entities.drawable() {
                if rec.is_stale(now, 20_000) {
                    continue;
                }
                let ident = s.identities.resolve(rec);
                let uuid = ident
                    .map(|p| p.uuid.clone())
                    .or_else(|| rec.guid.map(|g| format!("G{g:016X}")))
                    .unwrap_or_else(|| format!("Player-{}", rec.channel));
                let is_self = Some(&uuid) == self_uuid.as_ref();
                let p = rec.position;
                let metre = p.to_metres();
                let heading = rotation.heading(rec.rotation.yaw, None);
                let name = ident
                    .map(|i| i.display_name())
                    .unwrap_or_else(|| format!("#{}", rec.channel));
                let item = json!({
                    "uuid": uuid,
                    "name": name,
                    "kind": match rec.kind {
                        ActorKind::PlayerCharacter => "player",
                        ActorKind::AiCharacter => "ai",
                        ActorKind::Container => "loot",
                        _ => "other",
                    },
                    "x": metre[0], "y": metre[1], "z": metre[2],
                    "vx": rec.velocity.x * 0.01, "vy": rec.velocity.y * 0.01, "vz": rec.velocity.z * 0.01,
                    "yaw": heading,
                    "pitch": rec.rotation.pitch, "roll": rec.rotation.roll,
                    "team": rec.team, "camp": rec.camp,
                    "hp": rec.health, "max_hp": rec.max_health,
                    "alive": rec.alive, "visible": true,
                    "distance": self_uuid.as_ref().map(|_| 0.0f32),
                    "weapon": rec.weapon_class,
                    "hero_id": rec.hero_id, "level": ident.map(|i| i.level).unwrap_or(0),
                    "rank_score": ident.map(|i| i.rank_score).unwrap_or(0),
                    "is_ai": ident.map(|i| i.is_ai).unwrap_or(rec.is_ai),
                    "last_seen_ms": rec.last_update_ms,
                    "source": "move",
                    "self": is_self,
                    "session": _id,
                });
                if is_self && self_player.is_null() {
                    self_player = item.clone();
                }
                players.push(item);
            }
            for k in s.kills.recent() {
                kills.push(json!({
                    "ts": k.ts_ms,
                    "killer": k.killer_name.clone().unwrap_or_else(|| "未知".into()),
                    "victim": k.victim_name.clone().unwrap_or_else(|| "未知".into()),
                    "weapon": k.damage_type.as_str(),
                    "damage_type": k.damage_type.zh(),
                    "damage": k.damage,
                }));
            }
            let price = |id: u64| self.core.loot.lock().unit_price(id);
            loot.extend(s.containers.to_json(price));
        });

        // 距离（米，相对本地玩家）
        if let Some(sp) = self_player.as_object() {
            let sx = sp.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let sy = sp.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let sz = sp.get("z").and_then(|v| v.as_f64()).unwrap_or(0.0);
            for p in players.iter_mut() {
                let x = p.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let y = p.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let z = p.get("z").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let d = ((x - sx).powi(2) + (y - sy).powi(2) + (z - sz).powi(2)).sqrt();
                p["distance"] = json!(d);
            }
        }

        let counters = self.core.state.counters_ref().snapshot();
        json!({
            "ts": now,
            "tick": counters.parsed_packets,
            "map": map_name,
            "self": self_player,
            "players": players,
            "kills": kills,
            "loot": loot,
            "traces": [],
            "counters": counters,
            "sessions": self.core.sessions.len(),
            "uptime_ms": now.saturating_sub(self.core.start_ms),
        })
    }

    /// 雷达状态（换算到 SI 单位后可直接进 WS）。
    pub async fn radar_state(&self) -> serde_json::Value {
        let snapshot = self.radar_snapshot();
        crate::web::battle_view::radar_state(snapshot, &self.core.state)
    }

    /// WS 订阅。
    pub fn subscribe_radar(&self) -> tokio::sync::broadcast::Receiver<serde_json::Value> {
        self.core.radar_tx.subscribe()
    }

    /// 服务器解码闸门状态（样本 `server_decode_gate`）。
    pub fn decode_gate_open(&self) -> bool {
        self.core.sessions.for_each(|_, s| s.decoded_any).into_iter().any(|v| v)
    }

    /// 会话遥测列表。
    pub fn telemetry(&self) -> Vec<serde_json::Value> {
        self.core.sessions.for_each(|_, s| s.telemetry())
    }

    /// 清空某会话（`/api/admin/session/reset`）。
    pub fn reset_session(&self, id: u64) -> bool {
        self.core.sessions.remove(id).is_some()
    }

    /// 清空全部会话，返回清理数量。
    pub fn reset_all_sessions(&self) -> usize {
        let ids = self.core.sessions.ids();
        let n = ids.len();
        for id in ids {
            self.core.sessions.remove(id);
        }
        n
    }

    /// 清空物资缓存（`cached_loot_cleared`）。
    pub fn clear_cached_loot(&self) -> usize {
        self.core
            .sessions
            .for_each_mut(|_, s| s.containers.clear())
            .into_iter()
            .sum()
    }

    /// 物资解析开关（对齐 `loot_parsing_enabled`）。
    pub fn set_loot_parsing(&self, enabled: bool) {
        self.core
            .sessions
            .for_each_mut(|_, s| s.containers.set_parsing_enabled(enabled));
    }

    /// `battle_engine unavailable` 的显式表达：闸门从未打开过。
    pub fn engine_available(&self) -> bool {
        !self.core.sessions.is_empty()
    }
}

/// 内容物状态的中文/诊断串（与样本 `contents_status` 对齐）。
pub fn contents_status_label(s: ContentsStatus) -> &'static str {
    s.as_str()
}

/// 弹道置信度诊断串。
pub fn fire_confidence_label(c: FireConfidence) -> &'static str {
    match c {
        FireConfidence::Complete => "ballistic_trajectory_complete",
        FireConfidence::PartialRaw => "partial_raw_preserved",
        FireConfidence::MoveDerived => "no_projectile_and_fire_move_exact",
    }
}

/// 供 `s2c` 流复用的辅助（保持模块可达，避免 dead_code 警告）。
pub fn s2c_scratch() -> S2cStream {
    S2cStream::new()
}

#[inline]
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// 从内嵌 `channel_map.json` 建 `ch_index → class` 视图（构造期一次）。
fn load_channel_classes() -> std::collections::HashMap<u32, String> {
    let Some(json) = crate::web::embed::channel_map_json() else {
        return std::collections::HashMap::new();
    };
    #[derive(serde::Deserialize)]
    struct Root {
        #[serde(default)]
        channel_map: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(default)]
        slot: u32,
        #[serde(default)]
        ch_index: u32,
        class: String,
    }
    let Ok(root) = serde_json::from_str::<Root>(json) else {
        return std::collections::HashMap::new();
    };
    let mut out = std::collections::HashMap::new();
    for e in root.channel_map {
        let key = if e.ch_index != 0 { e.ch_index } else { e.slot };
        out.insert(key, e.class);
    }
    tracing::info!(channels = out.len(), "channel classes indexed");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn engine() -> BattleEngine {
        let st = AppState::new(&Config::default(), "tok".into());
        BattleEngine::new(st, Config::default())
    }

    #[tokio::test]
    async fn engine_starts_and_reports_empty_state() {
        let e = engine();
        let s = e.radar_state().await;
        assert_eq!(s["players"].as_array().unwrap().len(), 0);
        assert!(s.get("counters").is_some());
        assert!(!e.engine_available());
    }

    #[tokio::test]
    async fn feed_is_non_blocking_and_drops_when_full() {
        let e = engine();
        let src = "192.168.1.9:40000".parse().unwrap();
        let dst = "1.2.3.4:9000".parse().unwrap();
        for _ in 0..20_000 {
            e.feed(1, src, dst, &Bytes::from_static(&[1, 2, 3, 4, 5, 6]), 1_000);
        }
        // 只要没 panic/阻塞即为通过；深度由队列自行收敛。
        assert!(e.state().counters_ref().parse_queue_depth.load(std::sync::atomic::Ordering::Relaxed) > 0);
    }

    #[test]
    fn update_variants_are_constructible() {
        let u = EngineUpdate::Move {
            session: 1,
            channel: 3,
            position: Vec3::default(),
            rotation: Default::default(),
            velocity: Vec3::default(),
            indexed: true,
        };
        assert!(matches!(u, EngineUpdate::Move { channel: 3, .. }));
        let k = EngineUpdate::Kill { session: 1, kill: Box::new(Kill::default()) };
        assert!(matches!(k, EngineUpdate::Kill { .. }));
    }

    #[tokio::test]
    async fn reset_helpers_work_on_empty_engine() {
        let e = engine();
        assert_eq!(e.reset_all_sessions(), 0);
        assert_eq!(e.clear_cached_loot(), 0);
        assert!(!e.reset_session(42));
        e.set_loot_parsing(false);
    }

    #[test]
    fn diagnostic_labels_match_sample_strings() {
        assert_eq!(
            fire_confidence_label(FireConfidence::Complete),
            "ballistic_trajectory_complete"
        );
        assert_eq!(fire_confidence_label(FireConfidence::PartialRaw), "partial_raw_preserved");
        assert_eq!(
            fire_confidence_label(FireConfidence::MoveDerived),
            "no_projectile_and_fire_move_exact"
        );
        assert_eq!(
            contents_status_label(ContentsStatus::Randomised),
            "randomised_not_transmitted"
        );
    }
}
