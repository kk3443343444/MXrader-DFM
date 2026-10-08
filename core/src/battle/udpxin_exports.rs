//! PackageMap 导出解析：`battle_proxy::battle::udpxin_exports`。
//!
//! 复刻样本 `src/battle/udpxin_exports.rs`。当某个 bunch 头里
//! `bHasPackageMapExports = 1` 时，净荷最前面是一段"我在引用哪些对象"的导出表：
//! 它把 `ch_index` 上的 actor 绑定到具体类路径（`BP_DFMCharacter_C` 等）。
//!
//! UE5 的 `UPackageMapClient::ReceiveNetFieldExportsCompat`：
//!
//! ```text
//! uint32/SerializeIntPacked NumExports
//! repeat NumExports:
//!     uint32/SerializeIntPacked NetGUID
//!     bits 2  ExportFlags           (0 = 无, 1 = 对象, 2 = 路径)
//!     bits 1  bHasPath
//!     [bHasPath] FString PathName
//!     bits 1  bNoLoad
//!     [...]
//! ```
//!
//! 现实中的难点：片段位宽与小版本强相关，解析失败**不能影响转发**。所以本模块
//! 的契约是「尽力解析 + 失败不致命」，并且样本给了我们一条更稳的退路：
//! **内嵌 `channel_map.json`**（77 项静态映射）+ 运行期"GUID 目录"增量学习
//! （样本 `guid_catalog` / `automatic_guid_ingestion` / `manual_full_catalog` 三个
//! 管理接口正是维护这张表）。本模块两种来源都支持。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::codec::bitstream::BitReader;

/// 一次导出表解析结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportBatch {
    /// guid → 类路径
    pub export_class: HashMap<u64, String>,
    /// guid → 外层 guid（对象内嵌在哪个 actor）
    pub outer: HashMap<u64, u64>,
    /// 本次成功解析的条数
    pub parsed: usize,
    /// 尝试解析的条数（与实际不符说明版本漂移）
    pub declared: usize,
    pub flags_seen: Vec<u8>,
    /// 解析中断原因（诊断用）
    pub truncated_reason: Option<String>,
}

/// 导出表的解析档位。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportParseProfile {
    /// 条数用 `SerializeIntPacked` 还是 32 位整数。
    pub count_packed: bool,
    /// 导出标志位宽。
    pub flags_bits: u32,
    /// 是否有 `bHasPath` 位。
    pub has_path_bit: bool,
    /// 路径字符串是 `FString` 还是 `FName`（后者更紧凑）。
    pub path_as_fstring: bool,
    /// 是否有 `bNoLoad` 位。
    pub has_no_load_bit: bool,
    /// 是否有外层 GUID。
    pub has_outer_guid: bool,
}

impl Default for ExportParseProfile {
    fn default() -> Self {
        Self {
            count_packed: true,
            flags_bits: 2,
            has_path_bit: true,
            path_as_fstring: true,
            has_no_load_bit: true,
            has_outer_guid: true,
        }
    }
}

impl ExportParseProfile {
    pub fn load_or_default(json: Option<&str>) -> Self {
        json.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()
    }
}

/// 读一个 `FNetworkGUID`（UE 用 `SerializeIntPacked` 传，静态/动态标志在最高位）。
fn read_net_guid(r: &mut BitReader<'_>) -> u64 {
    r.read_packed_int64()
}

/// 按位读一个 `FString`（int32 长度 + ANSI/UTF-16 净荷）。
///
/// 导出表的字段**不是字节对齐**的：`NetGUID` + `flags` + 外层 GUID + `bHasPath`
/// 共 27 位之后就是字符串长度，字符串净荷又从第 59 位开始。`BitReader::read_bytes`
/// 会先对齐到字节边界，把 5 个非填充位当成填充丢掉，于是本条之后的每一次读取都
/// 错位 5 位（`bHasPath`/`bNoLoad` 都会读到别人的数据，末尾还会误报 bit overflow，
/// 整批 `parsed` 归零）。UE 的 `FBitReader::Serialize` 走 `SerializeBits`，不做
/// 对齐，所以这里也按位读。
fn read_path_fstring(r: &mut BitReader<'_>) -> Option<String> {
    let len = r.read_bits(32) as i32;
    if len == 0 {
        return Some(String::new());
    }
    if len > 0 {
        let n = len as usize;
        let mut bytes = Vec::with_capacity(n);
        for _ in 0..n {
            bytes.push(r.read_bits(8) as u8);
        }
        if r.overflowed() {
            return None;
        }
        // 长度含结尾 NUL，去掉它。
        Some(String::from_utf8_lossy(&bytes[..bytes.len().saturating_sub(1)]).to_string())
    } else {
        let n = (-len) as usize;
        let mut s = String::with_capacity(n);
        for _ in 0..n {
            let u = r.read_bits(16) as u16;
            if u == 0 {
                break;
            }
            s.push(char::from_u32(u as u32).unwrap_or('\u{fffd}'));
        }
        if r.overflowed() {
            return None;
        }
        Some(s)
    }
}

/// 解析一段导出净荷（位流）。
///
/// `bit_limit` 限制最大读取量，避免恶意/损坏包把 CPU 吃掉（样本会记
/// `Bunch payload exceeds packet` 并放弃）。
pub fn parse_export_batch(
    payload: &[u8],
    profile: &ExportParseProfile,
    bit_limit: usize,
) -> ExportBatch {
    let mut out = ExportBatch::default();
    let mut r = BitReader::new(payload);
    let limit = bit_limit.min(payload.len() * 8);

    let count = if profile.count_packed {
        r.read_packed_int()
    } else {
        r.read_bits(32)
    };
    out.declared = count as usize;

    // 合理性闸门：真实对局里一次导出不会超过 4096 条。
    if count == 0 || count > 4096 {
        out.truncated_reason = Some(format!("implausible export count {count}"));
        return out;
    }

    for _ in 0..count {
        if r.bit_pos() >= limit {
            out.truncated_reason = Some("bit limit reached".to_string());
            break;
        }
        let guid = read_net_guid(&mut r);
        let flags = r.read_bits(profile.flags_bits) as u8;
        out.flags_seen.push(flags);

        let mut outer = None;
        if profile.has_outer_guid && flags == 1 {
            outer = Some(read_net_guid(&mut r));
        }
        let has_path = if profile.has_path_bit { r.read_bit() } else { true };
        if has_path {
            let path = if profile.path_as_fstring {
                read_path_fstring(&mut r)
            } else {
                None
            };
            if let Some(p) = path {
                if !p.is_empty() {
                    out.export_class.insert(guid, normalize_class_path(&p));
                }
            }
        }
        if profile.has_no_load_bit {
            let _ = r.read_bit();
        }
        if let Some(o) = outer {
            out.outer.insert(guid, o);
        }
        if r.overflowed() {
            out.truncated_reason = Some("bit overflow".to_string());
            break;
        }
        out.parsed += 1;
    }

    if r.overflowed() && out.truncated_reason.is_none() {
        out.truncated_reason = Some("bit overflow".to_string());
    }
    out
}

/// `/Game/Blueprints/Character/BP_DFMCharacter.BP_DFMCharacter_C` → `BP_DFMCharacter_C`
pub fn normalize_class_path(path: &str) -> String {
    let tail = path.rsplit('/').next().unwrap_or(path);
    // 去掉 `XXX.XXX_C` 的点号前缀
    let last = tail.rsplit('.').next().unwrap_or(tail);
    last.trim().to_string()
}

/// 运行期 GUID 目录：把静态 `channel_map` 与运行期导出合并，供解析器查询。
///
/// 对应样本的三个管理接口：
/// * `automatic_guid_ingestion` —— 是否自动吸收导出（默认开）
/// * `manual_full_catalog` —— 一次性灌入完整目录
/// * `guid_catalog` —— 查询当前目录
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GuidCatalog {
    /// guid → 类名
    by_guid: HashMap<u64, String>,
    /// ch_index → 类名（来自导出或静态表）
    by_channel: HashMap<u32, String>,
    /// 自动吸收开关
    pub automatic_guid_ingestion: bool,
    /// 被观察到的导出次数（诊断）
    pub observed_exports: u64,
    /// 冲突次数（同一 guid 出现两个类名）
    pub conflicts: u64,
}

impl GuidCatalog {
    pub fn new(automatic: bool) -> Self {
        Self { automatic_guid_ingestion: automatic, ..Default::default() }
    }

    pub fn len(&self) -> usize {
        self.by_guid.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_guid.is_empty()
    }

    pub fn class_of_guid(&self, guid: u64) -> Option<&str> {
        self.by_guid.get(&guid).map(String::as_str)
    }
    pub fn class_of_channel(&self, ch: u32) -> Option<&str> {
        self.by_channel.get(&ch).map(String::as_str)
    }

    /// 吸收一批导出；返回新增条目数。
    pub fn absorb(&mut self, batch: &ExportBatch, channel_hint: Option<u32>) -> usize {
        self.observed_exports += 1;
        if !self.automatic_guid_ingestion {
            return 0;
        }
        let mut added = 0;
        for (guid, class) in &batch.export_class {
            match self.by_guid.get(guid) {
                Some(existing) if existing != class => {
                    self.conflicts += 1;
                    // 后到者优先：类名改名/热更时新值更可信，但要留痕。
                    tracing::debug!(
                        target: "battle_proxy",
                        guid = format!("{guid:016X}"),
                        old = existing.as_str(),
                        new = class.as_str(),
                        "guid class conflict"
                    );
                    self.by_guid.insert(*guid, class.clone());
                }
                Some(_) => {}
                None => {
                    self.by_guid.insert(*guid, class.clone());
                    added += 1;
                }
            }
            if let Some(ch) = channel_hint {
                self.by_channel.entry(ch).or_insert_with(|| class.clone());
            }
        }
        added
    }

    /// `manual_full_catalog`：用一份完整目录覆盖（例如抓包离线分析出来的）。
    pub fn load_full_catalog(&mut self, entries: &[(u64, String)], reset: bool) -> usize {
        if reset {
            self.by_guid.clear();
        }
        let mut n = 0;
        for (g, c) in entries {
            if self.by_guid.insert(*g, c.clone()).is_none() {
                n += 1;
            }
        }
        n
    }

    /// `guid_catalog` 查询接口的 JSON 形态。
    pub fn to_json(&self) -> serde_json::Value {
        let mut items: Vec<_> = self
            .by_guid
            .iter()
            .map(|(g, c)| serde_json::json!({"guid": format!("{g:016X}"), "class": c}))
            .collect();
        items.sort_by(|a, b| a["guid"].as_str().cmp(&b["guid"].as_str()));
        serde_json::json!({
            "guid_catalog": items,
            "deferred_spawns": 0,
            "manual_full_catalog": self.by_guid.len(),
            "automatic_guid_ingestion": self.automatic_guid_ingestion,
            "observed_exports": self.observed_exports,
            "conflicts": self.conflicts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::codec::bitstream::BitWriter;

    fn write_fstring(w: &mut BitWriter, s: &str) {
        let bytes = s.as_bytes();
        w.write_bits((bytes.len() + 1) as u32, 32);
        w.write_bytes(bytes);
        w.write_bits(0, 8); // NUL
    }

    fn build_batch(entries: &[(u64, &str)]) -> Vec<u8> {
        let p = ExportParseProfile::default();
        let mut w = BitWriter::new();
        // count as packed int
        let mut c = entries.len() as u32;
        loop {
            let mut b = (c & 0x7f) as u32;
            c >>= 7;
            if c != 0 {
                b |= 0x80;
            }
            w.write_bits(b, 8);
            if c == 0 {
                break;
            }
        }
        for (guid, class) in entries {
            let mut g = *guid;
            loop {
                let mut b = (g & 0x7f) as u32;
                g >>= 7;
                if g != 0 {
                    b |= 0x80;
                }
                w.write_bits(b, 8);
                if g == 0 {
                    break;
                }
            }
            w.write_bits(1, p.flags_bits); // flags = 1 (object)
            w.write_bits(0, 8); // outer guid packed = 0
            w.write_bit(true); // bHasPath
            write_fstring(&mut w, class);
            w.write_bit(false); // bNoLoad
        }
        w.into_bytes()
    }

    #[test]
    fn parse_roundtrip_two_exports() {
        let bytes = build_batch(&[(7, "/Game/BP_DFMCharacter.BP_DFMCharacter_C")]);
        let b = parse_export_batch(&bytes, &ExportParseProfile::default(), bytes.len() * 8);
        assert_eq!(b.declared, 1);
        assert_eq!(b.parsed, 1);
        assert_eq!(b.export_class.get(&7).map(String::as_str), Some("BP_DFMCharacter_C"));
    }

    #[test]
    fn implausible_count_is_rejected_without_scanning() {
        let mut w = BitWriter::new();
        w.write_bits(0xFF, 8);
        w.write_bits(0xFF, 8);
        let bytes = w.into_bytes();
        let b = parse_export_batch(&bytes, &ExportParseProfile::default(), 64);
        assert!(b.truncated_reason.is_some());
        assert_eq!(b.parsed, 0);
    }

    #[test]
    fn class_path_normalisation() {
        assert_eq!(
            normalize_class_path("/Game/Blueprints/BP_DFMCharacter.BP_DFMCharacter_C"),
            "BP_DFMCharacter_C"
        );
        assert_eq!(normalize_class_path("BP_EmptyHand_C"), "BP_EmptyHand_C");
    }

    #[test]
    fn catalog_absorbs_and_detects_conflicts() {
        let mut cat = GuidCatalog::new(true);
        let b1 = ExportBatch {
            export_class: HashMap::from([(1u64, "BP_DFMCharacter_C".to_string())]),
            ..Default::default()
        };
        assert_eq!(cat.absorb(&b1, Some(3)), 1);
        assert_eq!(cat.class_of_guid(1), Some("BP_DFMCharacter_C"));
        assert_eq!(cat.class_of_channel(3), Some("BP_DFMCharacter_C"));

        let b2 = ExportBatch {
            export_class: HashMap::from([(1u64, "BP_DFMCharacter_AI_DT_C".to_string())]),
            ..Default::default()
        };
        cat.absorb(&b2, None);
        assert_eq!(cat.conflicts, 1);
        assert_eq!(cat.class_of_guid(1), Some("BP_DFMCharacter_AI_DT_C"));
    }

    #[test]
    fn automatic_ingestion_can_be_disabled() {
        let mut cat = GuidCatalog::new(false);
        let b = ExportBatch {
            export_class: HashMap::from([(9u64, "X".to_string())]),
            ..Default::default()
        };
        assert_eq!(cat.absorb(&b, None), 0);
        assert!(cat.is_empty());
    }

    #[test]
    fn full_catalog_json_shape_matches_admin_api() {
        let mut cat = GuidCatalog::new(true);
        cat.load_full_catalog(&[(2, "BP_EmptyHand_C".to_string())], true);
        let j = cat.to_json();
        assert!(j.get("guid_catalog").is_some());
        assert!(j.get("deferred_spawns").is_some());
        assert!(j.get("automatic_guid_ingestion").is_some());
    }
}
