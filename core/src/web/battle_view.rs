//! Radar view serializer: turns the battle engine's raw snapshot into the wire shapes pinned by
//! INTERFACES.md 5 (`hello` / `state` / `diag` / `bye`).
//!
//! # Matched shared API (verified against the sibling modules in this crate)
//!
//! ```text
//! crate::config::Config
//!   brand: String, read_only_radar: bool, data_directory: PathBuf,
//!   session_model: SessionModel, parser_async: bool, loot_parsing_enabled: bool,
//!   collection_policy: CollectionPolicy,
//!   diagnostics: DiagnosticsConfig { protocol_capture, max_capture_mb, max_capture_seconds }
//!
//! crate::state::AppState
//!   async fn counters(&self) -> SessionCounters
//!   fn read_only(&self) -> bool
//!
//! crate::state::SessionCounters   // Debug + Clone + Default + Serialize
//!   active_sessions: usize, total_sessions: u64, udp_packets_up: u64, udp_packets_down: u64,
//!   udp_invalid_packets: u64, udp_outbound_sockets: u64, tcp_relay_failures: u64,
//!   udp_relay_bytes: u64, loot_payloads_skipped: u64, parse_queue_depth: u64,
//!   parsed_packets: u64, matched_entities: u64
//!
//! crate::battle::BattleEngine
//!   async fn radar_state(&self) -> serde_json::Value
//! ```
//!
//! # Unit contract
//!
//! The engine emits **centimetres** for `x/y/z` and `vx/vy/vz`. This module divides by 100 so the
//! radar only ever sees **metres** (INTERFACES.md 5: "全部为 SI 单位：米 / 度").
//! Angles stay in degrees and are corrected for the 3D turn offset here, so the web layer receives
//! already-corrected headings (`yaw 0 = north, clockwise positive`) and additionally gets the raw
//! values plus `yaw_offset_deg` / `yaw_sign` in `hello.world` for the 3D perspective matrix.
//! `distance` is recomputed in metres against the radar origin.
//!
//! # Where the engine snapshot is expected to provide data
//!
//! The engine snapshot is a JSON object. Optional keys this module understands:
//! `ts`, `tick`, `map`, `origin_x`, `origin_y`, `scale`, `yaw_offset_deg`, `yaw_sign`,
//! `server_decode_gate`, `total_pages`, `positioned_in_page`, `positioned_total`,
//! `self` (player object), `players`, `kills`, `loot`, `traces`.
//! Everything else is passed through untouched, and missing keys fall back to the documented
//! zero values (a missing player position becomes 0 m rather than dropping the player).

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::state::{AppState, SessionCounters};

/// Engine centimetres -> radar metres.
pub const CM_PER_METRE: f64 = 100.0;

/// Tile URL template advertised to the frontend (INTERFACES.md 5).
pub const TILE_TEMPLATE: &str = "/tiles/{map}/{z}/{x}/{y}.png";

/// Default map name when the engine has not seen a match yet.
pub const DEFAULT_MAP: &str = "ZeroDam";

/// Default world scale advertised in `hello.world` when the engine does not override it.
pub const DEFAULT_WORLD_SCALE: f64 = 0.01;

/// Default brand when the config leaves it empty.
pub const DEFAULT_BRAND: &str = "mx";

/// Position fields converted from centimetres to metres.
pub const POSITION_FIELDS: [&str; 3] = ["x", "y", "z"];
/// Velocity fields converted from centimetres per second to metres per second.
pub const VELOCITY_FIELDS: [&str; 3] = ["vx", "vy", "vz"];
/// Orientation fields, kept in degrees (already corrected by [`apply_rotation_correction`]).
pub const ORIENTATION_FIELDS: [&str; 3] = ["yaw", "pitch", "roll"];

/// Fields whose absence would drop a radar entry entirely; they get a numeric 0 default.
const NUMERIC_DEFAULTS: [&str; 17] = [
    "x", "y", "z", "vx", "vy", "vz", "yaw", "pitch", "roll", "team", "camp", "hp", "max_hp",
    "hero_id", "level", "rank_score", "last_seen_ms",
];

/// Fields that stay booleans when missing.
const BOOLEAN_DEFAULTS: [&str; 3] = ["alive", "visible", "is_self"];

/// Fields that stay strings when missing.
const STRING_DEFAULTS: [&str; 4] = ["uuid", "name", "kind", "weapon"];

// ---------------------------------------------------------------------------------------------
// hello
// ---------------------------------------------------------------------------------------------

/// World transform advertised in the `hello` message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorldTransform {
    /// World origin X in metres.
    pub origin_x: f64,
    /// World origin Y in metres.
    pub origin_y: f64,
    /// World -> map scale factor.
    pub scale: f64,
    /// Heading correction the frontend applies on top of the corrected `yaw`
    /// (`screenHeading = yaw + yaw_offset_deg`; compensated here so headings are consistent).
    pub yaw_offset_deg: f64,
    /// Heading handedness (`+1` when `screenHeading` grows clockwise, `-1` otherwise).
    pub yaw_sign: f64,
}

impl Default for WorldTransform {
    fn default() -> Self {
        Self {
            origin_x: 0.0,
            origin_y: 0.0,
            scale: DEFAULT_WORLD_SCALE,
            yaw_offset_deg: 0.0,
            yaw_sign: 1.0,
        }
    }
}

impl WorldTransform {
    /// Reads the transform out of the engine snapshot, falling back to the documented defaults.
    pub fn from_snapshot(snapshot: &Value) -> Self {
        let defaults = WorldTransform::default();
        Self {
            origin_x: num_or(snapshot, "origin_x", defaults.origin_x),
            origin_y: num_or(snapshot, "origin_y", defaults.origin_y),
            scale: num_or(snapshot, "scale", defaults.scale),
            yaw_offset_deg: num_or(snapshot, "yaw_offset_deg", defaults.yaw_offset_deg),
            yaw_sign: num_or(snapshot, "yaw_sign", defaults.yaw_sign),
        }
    }
}

/// Builds the `hello` message (`{"type":"hello", ...}`).
pub fn radar_hello(state: &AppState, cfg: &Config) -> Value {
    hello_json(state.read_only(), cfg, WorldTransform::default())
}

/// Pure `hello` builder: no `AppState` needed, so the wire shape is unit-testable.
pub fn hello_json(read_only: bool, cfg: &Config, world: WorldTransform) -> Value {
    json!({
        "type": "hello",
        "brand": brand_of(cfg),
        "map": DEFAULT_MAP,
        "tile_template": TILE_TEMPLATE,
        "world": {
            "origin_x": world.origin_x,
            "origin_y": world.origin_y,
            "scale": world.scale,
            "yaw_offset_deg": world.yaw_offset_deg,
            "yaw_sign": world.yaw_sign,
        },
        // 只读模式: the frontend hides every write control when this is true.
        "read_only_radar": read_only || cfg.read_only_radar,
        "loot_parsing_enabled": cfg.loot_parsing_enabled,
        "session_model": json_enum(&cfg.session_model),
        "collection_policy": json_enum(&cfg.collection_policy),
        "parser_async": cfg.parser_async,
    })
}

/// Brand advertised on the wire; falls back to `mx` when the config leaves it empty.
pub fn brand_of(cfg: &Config) -> String {
    let brand = cfg.brand.trim();
    if brand.is_empty() {
        DEFAULT_BRAND.to_string()
    } else {
        brand.to_string()
    }
}

/// Serde-derived enums (session model / collection policy) as their snake_case wire names.
fn json_enum<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Overlays the engine's own `map` / world transform onto a `hello` produced by [`radar_hello`].
///
/// Kept separate so the crash-free `hello` above can be sent before the engine has any snapshot.
pub fn radar_hello_with_snapshot(state: &AppState, cfg: &Config, snapshot: &Value) -> Value {
    hello_with_snapshot(state.read_only(), cfg, snapshot)
}

/// Pure variant of [`radar_hello_with_snapshot`] (no `AppState`), used by the unit tests.
pub fn hello_with_snapshot(read_only: bool, cfg: &Config, snapshot: &Value) -> Value {
    let mut hello = match hello_json(read_only, cfg, WorldTransform::from_snapshot(snapshot)) {
        Value::Object(map) => map,
        _ => Map::new(),
    };

    if let Some(map_name) = snapshot
        .get("map")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        hello.insert("map".to_string(), Value::String(map_name));
    }

    if let Some(gate) = snapshot.get("server_decode_gate") {
        hello.insert("server_decode_gate".to_string(), gate.clone());
    }
    if let Some(pages) = snapshot.get("total_pages") {
        hello.insert("total_pages".to_string(), pages.clone());
    }
    if let Some(address) = snapshot.get("display_address").and_then(|value| value.as_str()) {
        hello.insert(
            "radar_address".to_string(),
            Value::String(address.to_string()),
        );
    }

    Value::Object(hello)
}

// ---------------------------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------------------------

/// Converts one engine snapshot into the `state` message.
///
/// * positions/velocities are divided by [`CM_PER_METRE`];
/// * headings get [`apply_rotation_correction`] applied;
/// * `distance` is recomputed in metres from the radar origin;
/// * `total_pages` / `positioned_in_page` / `server_decode_gate` are preserved from the engine and
///   reported as `pagination` / `decode` bookkeeping blocks the frontend can rely on.
pub fn radar_state(engine_snapshot: Value, state: &AppState) -> Value {
    radar_state_with(engine_snapshot, state.read_only())
}

/// Pure state serializer: same as [`radar_state`] but without touching `AppState`, so the unit
/// conversion can be exercised in tests and by the parent's self-test.
pub fn radar_state_with(engine_snapshot: Value, read_only: bool) -> Value {
    let Value::Object(mut snapshot) = engine_snapshot else {
        return json!({
            "type": "state",
            "ts": now_ms(),
            "tick": 0,
            "self": Value::Null,
            "players": [],
            "kills": [],
            "loot": [],
            "traces": [],
            "read_only": read_only,
        });
    };

    // Read the transform while the snapshot is still whole (cheap, immutable borrows only).
    let (
        origin_x,
        origin_y,
        yaw_offset_deg,
        yaw_sign,
        total_pages,
        positioned_in_page,
        positioned_total,
        server_decode_gate,
    ) = {
        let view = Value::Object(snapshot.clone());
        (
            num_or(&view, "origin_x", 0.0),
            num_or(&view, "origin_y", 0.0),
            num_or(&view, "yaw_offset_deg", 0.0),
            num_or(&view, "yaw_sign", 1.0),
            view.get("total_pages").cloned(),
            view.get("positioned_in_page").cloned(),
            view.get("positioned_total").cloned(),
            view.get("server_decode_gate")
                .cloned()
                .unwrap_or(Value::Bool(true)),
        )
    };

    let convert = |value: Value| {
        convert_entity(value, origin_x, origin_y, yaw_offset_deg, yaw_sign)
    };

    let players = snapshot
        .remove("players")
        .map(|value| convert_list(value, &convert))
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let kills = snapshot
        .remove("kills")
        .map(|value| convert_list(value, &convert))
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let loot = snapshot
        .remove("loot")
        .map(|value| convert_list(value, &convert))
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let traces = snapshot
        .remove("traces")
        .map(|value| convert_list(value, &convert))
        .unwrap_or_else(|| Value::Array(Vec::new()));

    let self_entry = snapshot
        .remove("self")
        .map(|value| match value {
            Value::Null => Value::Null,
            other => convert(other),
        })
        .unwrap_or(Value::Null);

    let ts = snapshot
        .remove("ts")
        .and_then(|value| value.as_u64())
        .unwrap_or_else(now_ms);
    let tick = snapshot
        .remove("tick")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);

    // Anything the engine added beyond the documented keys is forwarded verbatim.
    let mut message = snapshot;
    message.insert("type".to_string(), Value::String("state".to_string()));
    message.insert("ts".to_string(), json!(ts));
    message.insert("tick".to_string(), json!(tick));
    message.insert("self".to_string(), self_entry);
    message.insert("players".to_string(), players);
    message.insert("kills".to_string(), kills);
    message.insert("loot".to_string(), loot);
    message.insert("traces".to_string(), traces);
    message.insert("read_only".to_string(), Value::Bool(read_only));

    message.insert(
        "pagination".to_string(),
        json!({
            "total_pages": total_pages.unwrap_or(Value::from(1)),
            "positioned_in_page": positioned_in_page.unwrap_or(Value::from(0)),
            "positioned_total": positioned_total.unwrap_or(Value::from(0)),
        }),
    );
    message.insert(
        "decode".to_string(),
        json!({
            "server_decode_gate": server_decode_gate,
            "units": "metres",
            "yaw_unit": "degrees",
        }),
    );

    Value::Object(message)
}

/// Builds the `diag` message (`{"type":"diag","counters":{...}}`).
///
/// The counters are copied out field by field from the real `SessionCounters`, so a field rename
/// in `state.rs` is a compile error here rather than a silently missing wire value.
pub fn radar_diag(counters: SessionCounters) -> Value {
    json!({
        "type": "diag",
        "counters": {
            "active_sessions": counters.active_sessions,
            "total_sessions": counters.total_sessions,
            "udp_packets_up": counters.udp_packets_up,
            "udp_packets_down": counters.udp_packets_down,
            "udp_invalid_packets": counters.udp_invalid_packets,
            "udp_outbound_sockets": counters.udp_outbound_sockets,
            "tcp_relay_failures": counters.tcp_relay_failures,
            "udp_relay_bytes": counters.udp_relay_bytes,
            "loot_payloads_skipped": counters.loot_payloads_skipped,
            "parse_queue_depth": counters.parse_queue_depth,
            "parsed_packets": counters.parsed_packets,
            "matched_entities": counters.matched_entities,
        }
    })
}

/// Builds the `bye` message.
pub fn radar_bye(reason: &str) -> Value {
    json!({ "type": "bye", "reason": reason })
}

/// Converts a JSON array of entities; non-array input becomes an empty array.
fn convert_list<F>(value: Value, convert: &F) -> Value
where
    F: Fn(Value) -> Value,
{
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(convert).collect()),
        Value::Null => Value::Array(Vec::new()),
        other => Value::Array(vec![convert(other)]),
    }
}

/// Converts one entity (player / kill / loot / trace) into radar units.
pub fn convert_entity(
    value: Value,
    origin_x: f64,
    origin_y: f64,
    yaw_offset_deg: f64,
    yaw_sign: f64,
) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };

    for field in POSITION_FIELDS {
        if let Some(raw) = object.get(field).and_then(|v| as_f64(v)) {
            object.insert(field.to_string(), number(raw / CM_PER_METRE));
        }
    }
    for field in VELOCITY_FIELDS {
        if let Some(raw) = object.get(field).and_then(|v| as_f64(v)) {
            object.insert(field.to_string(), number(raw / CM_PER_METRE));
        }
    }

    // Keep the raw heading around, then publish the corrected one in `yaw`.
    let raw_yaw = object.get("yaw").and_then(|v| as_f64(v));
    if let Some(raw_yaw) = raw_yaw {
        object
            .entry("yaw_raw".to_string())
            .or_insert_with(|| number(raw_yaw));
        object.insert(
            "yaw".to_string(),
            number(apply_rotation_correction(raw_yaw, yaw_offset_deg, yaw_sign)),
        );
    }

    let x = object.get("x").and_then(|v| as_f64(v)).unwrap_or(0.0);
    let y = object.get("y").and_then(|v| as_f64(v)).unwrap_or(0.0);
    let distance = compute_distance(x, y, origin_x, origin_y);
    let rounded = (distance * 10.0).round() / 10.0;
    object.insert("distance".to_string(), number(rounded));

    for field in NUMERIC_DEFAULTS {
        object.entry(field.to_string()).or_insert_with(|| Value::from(0));
    }
    for field in BOOLEAN_DEFAULTS {
        object
            .entry(field.to_string())
            .or_insert_with(|| Value::Bool(false));
    }
    for field in STRING_DEFAULTS {
        object
            .entry(field.to_string())
            .or_insert_with(|| Value::String(String::new()));
    }

    Value::Object(object)
}

/// Computes the planar distance between an entity and the radar origin, in **metres**.
pub fn compute_distance(x_m: f64, y_m: f64, origin_x_m: f64, origin_y_m: f64) -> f64 {
    let dx = x_m - origin_x_m;
    let dy = y_m - origin_y_m;
    (dx * dx + dy * dy).sqrt()
}

/// Applies the 3D turn correction: `yaw' = normalize(yaw * sign - offset)` in `[0, 360)`.
///
/// `yaw_sign` is `+1` when headings grow clockwise (already the radar convention) and `-1` when the
/// engine feeds counter-clockwise yaw. `yaw_offset_deg` compensates the constant mounting offset.
pub fn apply_rotation_correction(yaw_deg: f64, yaw_offset_deg: f64, yaw_sign: f64) -> f64 {
    if !yaw_deg.is_finite() {
        return 0.0;
    }
    let sign = if yaw_sign < 0.0 { -1.0 } else { 1.0 };
    let offset = if yaw_offset_deg.is_finite() {
        yaw_offset_deg
    } else {
        0.0
    };
    let corrected = yaw_deg * sign - offset;
    normalize_degrees(corrected)
}

/// Normalizes an angle into `[0, 360)`.
pub fn normalize_degrees(deg: f64) -> f64 {
    if !deg.is_finite() {
        return 0.0;
    }
    let normalized = deg % 360.0;
    if normalized < 0.0 {
        normalized + 360.0
    } else {
        normalized
    }
}

/// Smallest signed difference `a - b` in degrees, in `(-180, 180]`.
pub fn heading_delta(a_deg: f64, b_deg: f64) -> f64 {
    let mut delta = normalize_degrees(a_deg) - normalize_degrees(b_deg);
    if delta > 180.0 {
        delta -= 360.0;
    }
    if delta <= -180.0 {
        delta += 360.0;
    }
    delta
}

/// Numeric JSON value from an `f64`.
fn number(value: f64) -> Value {
    match serde_json::Number::from_f64(value) {
        Some(num) => Value::Number(num),
        // NaN / infinite never belong on the wire.
        None => Value::from(0),
    }
}

/// Reads a numeric field, falling back to `default` when absent or not a number.
fn num_or(snapshot: &Value, key: &str, default: f64) -> f64 {
    snapshot
        .get(key)
        .and_then(|value| as_f64(value))
        .filter(|value| value.is_finite())
        .unwrap_or(default)
}

/// Accepts JSON numbers (and float-looking numeric strings) as `f64`.
fn as_f64(value: &Value) -> Option<f64> {
    match value.as_f64() {
        Some(num) => Some(num),
        None => value
            .as_str()
            .and_then(|text| text.trim().parse::<f64>().ok()),
    }
}

/// Milliseconds since the UNIX epoch.
fn now_ms() -> u64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(delta) => delta.as_millis() as u64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_converted_from_centimetres_to_metres() {
        let entity = json!({
            "uuid": "u1", "name": "p", "kind": "player",
            "x": 123456.0, "y": -78900.0, "z": 3700.0,
            "vx": 1200.0, "vy": 0.0, "vz": -300.0,
            "yaw": 90.0, "pitch": 0.0, "roll": 0.0
        });
        let out = convert_entity(entity, 0.0, 0.0, 0.0, 1.0);
        assert!((out["x"].as_f64().unwrap_or_default() - 1234.56).abs() < 1e-9);
        assert!((out["y"].as_f64().unwrap_or_default() + 789.0).abs() < 1e-9);
        assert!((out["z"].as_f64().unwrap_or_default() - 37.0).abs() < 1e-9);
        assert!((out["vz"].as_f64().unwrap_or_default() + 3.0).abs() < 1e-9);
        // Angles stay in degrees.
        assert_eq!(out["yaw"].as_f64(), Some(90.0));
        assert_eq!(out["pitch"].as_f64(), Some(0.0));
    }

    #[test]
    fn distance_is_stored_in_metres_and_rounded() {
        let entity = json!({"x": 3000.0, "y": 4000.0, "kind": "ai"});
        let out = convert_entity(entity, 0.0, 0.0, 0.0, 1.0);
        // 30 m east and 40 m north -> 50 m.
        assert_eq!(out["distance"].as_f64(), Some(50.0));

        let shifted = convert_entity(
            json!({"x": 4000.0, "y": 5000.0}),
            1000.0,
            1000.0,
            0.0,
            1.0,
        );
        let distance = shifted["distance"].as_f64().unwrap_or_default();
        assert!((distance - 50.0).abs() < 1e-6, "distance was {distance}");
    }

    #[test]
    fn rotation_correction_normalizes_and_applies_the_sign() {
        assert!((apply_rotation_correction(350.0, 20.0, 1.0) - 330.0).abs() < 1e-9);
        assert!((apply_rotation_correction(10.0, 20.0, 1.0) - 350.0).abs() < 1e-9);
        assert!((apply_rotation_correction(90.0, 0.0, -1.0) - 270.0).abs() < 1e-9);
        assert_eq!(apply_rotation_correction(0.0, 0.0, 1.0), 0.0);
        assert_eq!(apply_rotation_correction(f64::NAN, 0.0, 1.0), 0.0);
    }

    #[test]
    fn heading_delta_wraps_around_north() {
        assert!((heading_delta(10.0, 350.0) - 20.0).abs() < 1e-9);
        assert!((heading_delta(350.0, 10.0) + 20.0).abs() < 1e-9);
        assert!((heading_delta(180.0, 0.0) - 180.0).abs() < 1e-9);
    }

    #[test]
    fn state_message_keeps_the_documented_shape() {
        let snapshot = json!({
            "ts": 1699999999999u64,
            "tick": 4211,
            "map": "ZeroDam",
            "total_pages": 4,
            "positioned_in_page": 17,
            "players": [{"uuid": "a", "name": "n", "kind": "player", "x": 100.0, "y": 0.0}],
            "kills": [{"x": 200.0, "y": 0.0}],
        });
        let state_message = radar_state_with(snapshot, false);

        assert_eq!(state_message["type"], "state");
        assert_eq!(state_message["ts"].as_u64(), Some(1699999999999));
        assert_eq!(state_message["tick"].as_u64(), Some(4211));
        // Centimetres -> metres.
        assert_eq!(state_message["players"][0]["x"].as_f64(), Some(1.0));
        assert_eq!(state_message["players"][0]["distance"].as_f64(), Some(1.0));
        assert_eq!(state_message["kills"][0]["x"].as_f64(), Some(2.0));
        // Defaults are filled in so the frontend never has to guard for missing keys.
        assert_eq!(state_message["players"][0]["name"], "n");
        assert_eq!(state_message["players"][0]["alive"], false);
        assert_eq!(state_message["players"][0]["source"], "");
        // Bookkeeping blocks.
        assert_eq!(state_message["pagination"]["total_pages"].as_u64(), Some(4));
        assert_eq!(state_message["pagination"]["positioned_in_page"].as_u64(), Some(17));
        assert_eq!(state_message["decode"]["server_decode_gate"], true);
        assert_eq!(state_message["decode"]["units"], "metres");
        assert_eq!(state_message["read_only"], false);
        assert_eq!(state_message["traces"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    fn world_transform_defaults_match_the_contract() {
        let world = WorldTransform::from_snapshot(&json!({}));
        assert_eq!(world.origin_x, 0.0);
        assert_eq!(world.origin_y, 0.0);
        assert_eq!(world.scale, 0.01);
        assert_eq!(world.yaw_offset_deg, 0.0);
        assert_eq!(world.yaw_sign, 1.0);

        let custom = WorldTransform::from_snapshot(&json!({
            "origin_x": 120.0, "origin_y": -80.0, "scale": 0.02,
            "yaw_offset_deg": 45.0, "yaw_sign": -1.0
        }));
        assert_eq!(custom.origin_x, 120.0);
        assert_eq!(custom.scale, 0.02);
        assert_eq!(custom.yaw_sign, -1.0);
    }

    #[test]
    fn hello_message_carries_the_documented_fields() {
        let cfg = Config::default();
        let hello = hello_json(true, &cfg, WorldTransform::default());
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["brand"], "mx");
        assert_eq!(hello["map"], DEFAULT_MAP);
        assert_eq!(hello["tile_template"], TILE_TEMPLATE);
        assert_eq!(hello["world"]["scale"], 0.01);
        assert_eq!(hello["world"]["origin_x"], 0.0);
        assert_eq!(hello["world"]["yaw_sign"], 1.0);
        assert_eq!(hello["read_only_radar"], true);
        assert_eq!(hello["session_model"], "one_port_one_player");
        assert_eq!(hello["collection_policy"], "enabled_only");

        // The engine's own map name and transform win over the defaults.
        let with_snapshot = hello_with_snapshot(
            false,
            &cfg,
            &json!({"map": "SpaceCity", "scale": 0.02, "yaw_offset_deg": 15.0}),
        );
        assert_eq!(with_snapshot["map"], "SpaceCity");
        assert_eq!(with_snapshot["world"]["scale"], 0.02);
        assert_eq!(with_snapshot["world"]["yaw_offset_deg"], 15.0);
        assert_eq!(with_snapshot["read_only_radar"], true, "config read-only still wins");
    }

    #[test]
    fn diag_message_carries_the_documented_counters() {
        let counters = SessionCounters {
            active_sessions: 1,
            total_sessions: 2,
            udp_packets_up: 3,
            udp_packets_down: 4,
            udp_invalid_packets: 5,
            udp_outbound_sockets: 6,
            tcp_relay_failures: 7,
            udp_relay_bytes: 8,
            loot_payloads_skipped: 9,
            parse_queue_depth: 10,
            parsed_packets: 11,
            matched_entities: 12,
        };
        let diag = radar_diag(counters);
        assert_eq!(diag["type"], "diag");
        assert_eq!(diag["counters"]["active_sessions"], 1);
        assert_eq!(diag["counters"]["udp_packets_up"], 3);
        assert_eq!(diag["counters"]["loot_payloads_skipped"], 9);
        assert_eq!(diag["counters"]["matched_entities"], 12);
    }
}
