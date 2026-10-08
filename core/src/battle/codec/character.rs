//! `BP_DFMCharacter_C` 属性遍历：`battle_proxy::battle::codec::character`。
//!
//! 复刻样本 `src/battle/codec/character.rs`（样本在这个文件里有 6600+ 行，
//! 是整条链最厚的一层）。核心动作只有两件事：
//!
//! 1. **按 handle 号遍历 RepLayout**。UE 的 `FRepLayout` 把每个 `CPF_Net` 属性编号，
//!    初始状态包（`bHasInitialState`）无存在位、顺序写全部属性；之后的增量包
//!    每个属性前面有 **1 位存在位**。数组/结构会展开成多个 handle（这就是
//!    `channel_map.json` 里 `Duability/12`、`SpeedFactor/13` 这种重复名的来源）。
//! 2. **字段类型推断**。样本保留了 `[field_infer]` 日志标签——它并不依赖引擎元数据，
//!    而是用**属性名**推断编码方式。这就是为什么样本要把整张 `handles` 名字表
//!    内嵌进二进制：**名字就是类型信息**。
//!
//! 收尾：UE 在属性块末尾写 `CMD_END`（一个 `uint8 0xFF` + 校验）。样本在没对齐时
//! 报 `movement RepLayout not exactly closed`，本模块同样把这种包标为
//! `closure_ok = false`，但**仍然交付已解出的字段**（雷达宁可显示略旧的位置）。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::bitstream::BitReader;
use super::PropertyBlock;
use super::super::udpxin_entity::{Rot3, Vec3};

/// 解码档位。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacterCodecProfile {
    pub name: String,
    /// 增量包每个属性前是否有存在位。
    pub delta_has_presence_bits: bool,
    /// 是否存在初始状态包（无存在位）。
    pub support_initial_state: bool,
    /// 向量属性用的定点位宽（`FVector_NetQuantize100` = 20 位/分量）。
    pub vector_bits: u32,
    /// 向量定点 scale（0.01 单位）。
    pub vector_scale: f32,
    /// 收尾允许的残余位数（UE 的 CMD_END 会补齐到字节）。
    pub closure_slack_bits: usize,
    /// 单块最多读多少属性（防御损坏包）。
    pub max_properties: usize,
}

impl Default for CharacterCodecProfile {
    fn default() -> Self {
        Self {
            name: "dfm-r39".to_string(),
            delta_has_presence_bits: true,
            support_initial_state: true,
            vector_bits: 20,
            vector_scale: 0.01,
            closure_slack_bits: 16,
            max_properties: 2048,
        }
    }
}

impl CharacterCodecProfile {
    pub fn load_or_default(json: Option<&str>) -> Self {
        json.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()
    }
}

/// 由名字推断出来的字段类型（样本 `[field_infer]` 的实际语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Bool,
    Int,
    Float,
    Vector,
    Rotator,
    Byte,
    Str,
    Unknown,
}

/// 名字 → 类型。规则顺序很重要（先特殊名，再后缀，最后前缀）。
pub fn infer_field_type(name: &str) -> FieldType {
    // 1. 明确知道编码方式的特殊属性
    match name {
        // UE 的 `RemoteViewPitch` 是 uint8 压缩俯仰
        "RemoteViewPitch" => return FieldType::Byte,
        "bHidden" | "bReplicateMovement" | "bTearOff" | "bCanBeDamaged" => return FieldType::Bool,
        "ReplicatedMovement" | "AttachmentReplication" | "ReplicatedBasedMovement" => {
            return FieldType::Unknown
        }
        "Role" | "RemoteRole" => return FieldType::Byte,
        "Owner" | "Instigator" | "PlayerState" | "Pawn" | "Controller" => return FieldType::Int,
        _ => {}
    }

    // 2. 布尔：UE 的 `b` + 大写首字母
    let mut chars = name.chars();
    if let Some('b') = chars.next() {
        if let Some(c2) = chars.next() {
            if c2.is_ascii_uppercase() {
                return FieldType::Bool;
            }
        }
    }

    // 3. 字符串
    for kw in ["Name", "Str", "Url", "Path", "Pool", "Debug"] {
        if name.contains(kw) && !name.contains("Number") {
            return FieldType::Str;
        }
    }

    // 4. 向量
    for kw in [
        "Location",
        "Velocity",
        "Translation",
        "Position",
        "PreWeaponPosition",
        "Offset",
        "Direction",
        "Extent",
        "Scale3D",
    ] {
        if name.contains(kw) {
            return FieldType::Vector;
        }
    }

    // 5. 旋转
    for kw in [
        "Rotation",
        "Rotator",
        "Aim",
        "LookedAt",
        "Turning",
    ] {
        if name.contains(kw) {
            return FieldType::Rotator;
        }
    }

    // 6. 浮点
    for kw in [
        "Speed",
        "Factor",
        "Scale",
        "Gauge",
        "Quantity",
        "Time",
        "Damage",
        "Progress",
        "Duration",
        "Ratio",
        "Radius",
    ] {
        if name.contains(kw) {
            return FieldType::Float;
        }
    }

    // 7. 整数
    for kw in [
        "ID", "Id", "Num", "Count", "Index", "Level", "Size", "Slot", "Handle", "Team",
        "Camp", "Rank", "Score", "GUID", "State", "Mode", "Type", "Flag", "Pitch",
    ] {
        if name.contains(kw) {
            return FieldType::Int;
        }
    }

    FieldType::Unknown
}

/// 名字表：类名 → (handle 号 → 属性名)。直接吃 `assets/channel_map.json` 的 `handles`。
#[derive(Debug, Default)]
pub struct HandleTable {
    by_class: HashMap<String, HashMap<u32, String>>,
}

impl HandleTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 解析 `channel_map.json`。返回载入的属性总数。
    pub fn load_channel_map_json(&mut self, json: &str) -> anyhow::Result<usize> {
        let root: serde_json::Value = serde_json::from_str(json)?;
        let Some(handles) = root.get("handles").and_then(|h| h.as_object()) else {
            return Ok(0);
        };
        let mut total = 0;
        for (class, props) in handles {
            let Some(map) = props.as_object() else { continue };
            let entry = self.by_class.entry(class.clone()).or_default();
            for (handle, name) in map {
                if let (Ok(h), Some(n)) = (handle.parse::<u32>(), name.as_str()) {
                    entry.insert(h, n.to_string());
                    total += 1;
                }
            }
        }
        tracing::info!(classes = self.by_class.len(), properties = total, "handle table loaded");
        Ok(total)
    }

    pub fn class_count(&self) -> usize {
        self.by_class.len()
    }

    pub fn props(&self, class: &str) -> Option<&HashMap<u32, String>> {
        self.by_class.get(class)
    }

    /// 按 handle 升序遍历（UE 的 RepLayout 顺序 = handle 升序）。
    pub fn ordered(&self, class: &str) -> Vec<(u32, &str)> {
        let Some(map) = self.by_class.get(class) else { return Vec::new() };
        let mut v: Vec<(u32, &str)> = map.iter().map(|(h, n)| (*h, n.as_str())).collect();
        v.sort_by_key(|(h, _)| *h);
        v
    }

    /// 名字 → handle（反向查询，诊断/工具用）。
    pub fn handle_of(&self, class: &str, name: &str) -> Option<u32> {
        self.by_class.get(class)?.iter().find(|(_, n)| n.as_str() == name).map(|(h, _)| *h)
    }
}

/// 一个直接可用的属性值（雷达用的"热字段"）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CharacterHotFields {
    pub replicated_movement: Option<super::super::udpxin_move::RepMovement>,
    pub remote_view_pitch: Option<f32>,
    pub character_rotation: Option<Rot3>,
    pub target_rotation: Option<Rot3>,
    pub looking_rotation: Option<Rot3>,
    pub team_id: Option<i32>,
    pub camp: Option<i32>,
    pub character_name: Option<String>,
    pub npc_name: Option<String>,
    pub is_player_ai: Option<bool>,
    pub guid: Option<u64>,
    pub player_state_channel: Option<u32>,
    pub controller_channel: Option<u32>,
    pub is_prone: Option<bool>,
    pub is_crouched: Option<bool>,
    /// 命中的 handle 数（诊断：属性命中率）。
    pub hits: usize,
}

/// 读一个属性块。
///
/// * `class` —— 用于查名字表；查不到也能走 `FieldType::Unknown` 兜底跳过。
/// * `is_initial` —— 初始状态包（无存在位）。
pub fn read_property_block(
    r: &mut BitReader<'_>,
    table: &HandleTable,
    class: &str,
    is_initial: bool,
    profile: &CharacterCodecProfile,
) -> (PropertyBlock, CharacterHotFields) {
    let mut block = PropertyBlock { closure_ok: true, ..Default::default() };
    let mut hot = CharacterHotFields::default();
    let start = r.bit_pos();

    let ordered = table.ordered(class);
    if ordered.is_empty() {
        // 名字表未命中：无法解属性，但位移可能就在最前面，交给上层按 profile 兜底。
        block.notes.push("handle_table_miss");
        block.bits_used = 0;
        return (block, hot);
    }

    let presence_bits = profile.delta_has_presence_bits && !(is_initial && profile.support_initial_state);

    for (idx, (handle, name)) in ordered.iter().enumerate() {
        if idx >= profile.max_properties || r.bits_left() < 2 {
            block.closure_ok = false;
            break;
        }
        let present = if presence_bits { r.read_bit() } else { true };
        if !present {
            continue;
        }

        let ftype = infer_field_type(name);
        let value = match ftype {
            FieldType::Bool => super::FieldValue::Bool(r.read_bit()),
            FieldType::Byte => super::FieldValue::Int(r.read_bits(8) as i64),
            FieldType::Int => {
                // UE 的整数属性多用 `SerializeInt`/`IntPacked`；这里按 handle 号奇偶
                // 走两条最常见路径，靠收尾校验决定是否可信。
                let v = if *handle % 2 == 0 { r.read_packed_int() as i64 } else { r.read_serialized_int(0x7FFF_FFFF) as i64 };
                super::FieldValue::Int(v)
            }
            FieldType::Float => super::FieldValue::Float(r.read_compressed_float(24)),
            FieldType::Vector => {
                let v = read_net_quantize_vector(r, profile.vector_bits, profile.vector_scale);
                super::FieldValue::Vector(v)
            }
            FieldType::Rotator => super::FieldValue::Rotator(Rot3::read_from(r)),
            FieldType::Str => match r.read_fstring() {
                Some(s) => super::FieldValue::String(s),
                None => super::FieldValue::Raw { bit_start: r.bit_pos(), bit_len: 0 },
            },
            FieldType::Unknown => {
                // 不知道类型就只能跳过。样本同样是"未知即跳过"，并把位偏移记下来
                // 供离线分析（`raw_preserved_not_required_for_ballistic_reconstruction`）。
                let skip = unknown_skip_bits(name);
                let bit_start = r.bit_pos();
                for _ in 0..skip {
                    r.read_bit();
                }
                super::FieldValue::Raw { bit_start, bit_len: skip }
            }
        };

        apply_hot(&mut hot, *handle, name, &value, profile);
        block.fields.push(super::DecodedField {
            handle: *handle,
            name: (*name).to_string(),
            value,
        });
        hot.hits += 1;

        if r.overflowed() {
            block.closure_ok = false;
            block.notes.push("bit_overflow");
            break;
        }
    }

    block.bits_used = r.bit_pos().saturating_sub(start);

    // 收尾校验：UE 的 CMD_END 之后应当正好耗尽净荷（允许少量补齐位）。
    let remaining = (block.bits_used % 8) as usize;
    if remaining > 0 && remaining < 8 - profile.closure_slack_bits.min(7) {
        // 未对齐但仍在容差内：补齐即可。
        r.align_to_byte();
    }
    if r.bits_left() > profile.closure_slack_bits.max(remaining) {
        block.closure_ok = false;
        block.notes.push("rep_layout_not_exactly_closed");
    }
    (block, hot)
}

fn unknown_skip_bits(name: &str) -> usize {
    // 未知类型的保守跳过长度：UE 的结构化复制通常至少 8 位对齐。
    if name.contains("Array") || name.contains("Info") || name.contains("Data") {
        32
    } else {
        8
    }
}

fn read_net_quantize_vector(r: &mut BitReader<'_>, bits: u32, scale: f32) -> Vec3 {
    let mut out = [0f32; 3];
    for slot in out.iter_mut() {
        let neg = r.read_bit();
        let raw = r.read_bits(bits.min(32)) as f32;
        *slot = if neg { -raw * scale } else { raw * scale };
    }
    Vec3 { x: out[0], y: out[1], z: out[2] }
}

/// 把解出的字段填进"热字段"，供 engine 直接构图。
fn apply_hot(
    hot: &mut CharacterHotFields,
    handle: u32,
    name: &str,
    value: &super::FieldValue,
    _profile: &CharacterCodecProfile,
) {
    use super::FieldValue as F;
    match (name, value) {
        ("RemoteViewPitch", F::Int(v)) => {
            // UE: uint8 → 度，`*360/255`
            hot.remote_view_pitch = Some(*v as f32 * 360.0 / 255.0);
        }
        ("CharacterRotation", F::Rotator(rot)) => hot.character_rotation = Some(*rot),
        ("TargetRotation", F::Rotator(rot)) => hot.target_rotation = Some(*rot),
        ("LookingRotation", F::Rotator(rot)) => hot.looking_rotation = Some(*rot),
        ("TeamID", F::Int(v)) => hot.team_id = Some(*v as i32),
        ("Camp", F::Int(v)) => hot.camp = Some(*v as i32),
        ("CharacterName", F::String(s)) => hot.character_name = Some(s.clone()),
        ("npcName", F::String(s)) => hot.npc_name = Some(s.clone()),
        ("bIsPlayerAI", F::Bool(b)) => hot.is_player_ai = Some(*b),
        ("bIsPlayerAI_SOL", F::Bool(b)) => {
            hot.is_player_ai = Some(hot.is_player_ai.unwrap_or(false) || *b);
        }
        ("MyGUIDValue", F::Int(v)) => hot.guid = Some(*v as u64),
        ("MoveHandle", F::Int(v)) => hot.guid = hot.guid.or(Some(*v as u64)),
        ("PlayerState", F::Int(v)) => hot.player_state_channel = Some(*v as u32),
        ("Controller", F::Int(v)) => hot.controller_channel = Some(*v as u32),
        ("bIsProned", F::Bool(b)) => hot.is_prone = Some(*b),
        ("bIsCrouched", F::Bool(b)) => hot.is_crouched = Some(*b),
        _ => {
            let _ = handle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::codec::bitstream::BitWriter;

    const SAMPLE_JSON: &str = r#"{"handles":{
      "BP_DFMCharacter_C":{
        "0":"bHidden","6":"RemoteRole","7":"ReplicatedMovement","17":"RemoteViewPitch",
        "18":"PlayerState","19":"Controller","30":"ReplicatedMovementMode","31":"bIsCrouched",
        "47":"bIsProned","94":"MyGUIDValue","97":"bIsPlayerAI","120":"TargetRotation",
        "121":"CharacterRotation","122":"LookingRotation","163":"npcName","170":"CharacterName",
        "184":"TeamID","185":"Camp"}}}"#;

    fn table() -> HandleTable {
        let mut t = HandleTable::new();
        t.load_channel_map_json(SAMPLE_JSON).unwrap();
        t
    }

    #[test]
    fn sample_channel_map_loads_and_orders_by_handle() {
        let t = table();
        assert_eq!(t.class_count(), 1);
        assert_eq!(t.handle_of("BP_DFMCharacter_C", "MyGUIDValue"), Some(94));
        let ordered = t.ordered("BP_DFMCharacter_C");
        assert_eq!(ordered.first().unwrap().0, 0);
        assert!(ordered.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    #[test]
    fn field_type_inference_rules() {
        assert_eq!(infer_field_type("bReplicateMovement"), FieldType::Bool);
        assert_eq!(infer_field_type("RemoteViewPitch"), FieldType::Byte);
        assert_eq!(infer_field_type("CharacterName"), FieldType::Str);
        assert_eq!(infer_field_type("PreWeaponPosition"), FieldType::Vector);
        assert_eq!(infer_field_type("FireRotation"), FieldType::Rotator);
        assert_eq!(infer_field_type("SpeedFactor"), FieldType::Float);
        assert_eq!(infer_field_type("TeamID"), FieldType::Int);
        assert_eq!(infer_field_type("ReplicatedMovement"), FieldType::Unknown);
    }

    #[test]
    fn initial_state_block_reads_bools_without_presence_bits() {
        let t = table();
        let p = CharacterCodecProfile { max_properties: 32, ..Default::default() };
        let mut w = BitWriter::new();
        // bHidden=false, RemoteRole=3(byte), ReplicatedMovement=unknown(8 位), RemoteViewPitch=64
        w.write_bit(false);
        w.write_bits(0, 8); // RemoteRole 走 Byte
        w.write_bits(0, 8); // unknown skip
        w.write_bits(64, 8);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let (block, hot) = read_property_block(&mut r, &t, "BP_DFMCharacter_C", true, &p);
        assert!(block.fields.len() >= 3);
        assert!((hot.remote_view_pitch.unwrap() - 90.35).abs() < 0.2);
    }

    #[test]
    fn delta_block_uses_presence_bits_and_only_reads_present_props() {
        let t = table();
        let p = CharacterCodecProfile { max_properties: 8, ..Default::default() };
        let mut w = BitWriter::new();
        // handle 0 bHidden: present=false
        w.write_bit(false);
        // handle 6 RemoteRole: present=true -> byte
        w.write_bit(true);
        w.write_bits(2, 8);
        // 其余 present=false 直到 max_properties
        for _ in 0..6 {
            w.write_bit(false);
        }
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let (block, _hot) = read_property_block(&mut r, &t, "BP_DFMCharacter_C", false, &p);
        assert_eq!(block.fields.len(), 1);
        assert_eq!(block.fields[0].name, "RemoteRole");
        assert_eq!(block.fields[0].value, super::super::FieldValue::Int(2));
    }

    #[test]
    fn unknown_class_reports_handle_table_miss() {
        let t = table();
        let mut r = BitReader::new(&[0xFF; 8]);
        let (block, hot) = read_property_block(
            &mut r,
            &t,
            "BP_NotInTable_C",
            false,
            &CharacterCodecProfile::default(),
        );
        assert!(block.notes.contains(&"handle_table_miss"));
        assert_eq!(hot.hits, 0);
    }

    #[test]
    fn never_panics_on_random_bits() {
        let t = table();
        let p = CharacterCodecProfile::default();
        for seed in 0u32..32 {
            let bytes: Vec<u8> = (0..128)
                .map(|i| (seed.wrapping_mul(2654435761).wrapping_add(i * 13) & 0xFF) as u8)
                .collect();
            let mut r = BitReader::new(&bytes);
            let _ = read_property_block(&mut r, &t, "BP_DFMCharacter_C", false, &p);
            let mut r = BitReader::new(&bytes);
            let _ = read_property_block(&mut r, &t, "BP_DFMCharacter_C", true, &p);
        }
    }
}
