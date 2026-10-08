//! 物资目录：`battle_proxy::battle::loot_catalog`。
//!
//! 复刻样本 `src/battle/loot_catalog.rs`。样本把一张**压缩过的物品名录**内嵌进
//! 二进制，格式是"词典前缀 + Term 引用"：
//!
//! ```text
//! SOL_DT_Basic_AR_low_[Term#18010000014_ShortName]_14
//! SOL_DT_Basic_SMG_low_[Term#10020000013_ShortName]_1
//! {"10010000903":"SOL_DT_Basic_AR_low_M16A4_14","10010000904":"SOL_DT_Basic_AR_low_M16A4_15",…}
//! ```
//!
//! 即：`[Term#<id>_ShortName]` 是一个占位符，展开后是真正的武器名；
//! 整张表在样本里合计约数万条，覆盖 AR/SMG/Shotgun/Melee 等系列。
//! 本模块实现同一套"压缩名录"的编解码，并把它变成雷达可用的查询：
//! `item_id → 名称/类别/稀有度/估价`。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 物品大类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemCategory {
    AssaultRifle,
    SubmachineGun,
    Shotgun,
    MarksmanRifle,
    SniperRifle,
    LightMachineGun,
    Pistol,
    Melee,
    Throwable,
    Ammo,
    Helmet,
    Vest,
    Backpack,
    Medical,
    Key,
    Valuables,
    Unknown,
}

impl ItemCategory {
    pub fn zh(self) -> &'static str {
        match self {
            ItemCategory::AssaultRifle => "突击步枪",
            ItemCategory::SubmachineGun => "冲锋枪",
            ItemCategory::Shotgun => "霰弹枪",
            ItemCategory::MarksmanRifle => "精确射手步枪",
            ItemCategory::SniperRifle => "狙击枪",
            ItemCategory::LightMachineGun => "轻机枪",
            ItemCategory::Pistol => "手枪",
            ItemCategory::Melee => "近战",
            ItemCategory::Throwable => "投掷物",
            ItemCategory::Ammo => "弹药",
            ItemCategory::Helmet => "头盔",
            ItemCategory::Vest => "护甲",
            ItemCategory::Backpack => "背包",
            ItemCategory::Medical => "医疗",
            ItemCategory::Key => "钥匙",
            ItemCategory::Valuables => "贵价物",
            ItemCategory::Unknown => "未知",
        }
    }

    /// 由样本里的 `SOL_DT_Basic_*` 前缀推断。
    pub fn from_asset_name(name: &str) -> Self {
        let n = name.to_ascii_uppercase();
        if n.contains("_AR_") {
            ItemCategory::AssaultRifle
        } else if n.contains("_SMG_") {
            ItemCategory::SubmachineGun
        } else if n.contains("_SHOTGUN_") {
            ItemCategory::Shotgun
        } else if n.contains("_DMR_") || n.contains("MARKSMAN") {
            ItemCategory::MarksmanRifle
        } else if n.contains("_SR_") || n.contains("SNIPER") {
            ItemCategory::SniperRifle
        } else if n.contains("_LMG_") {
            ItemCategory::LightMachineGun
        } else if n.contains("_PISTOL_") {
            ItemCategory::Pistol
        } else if n.contains("MELEE") || n.contains("KNIFE") {
            ItemCategory::Melee
        } else if n.contains("GRENADE") || n.contains("THROW") {
            ItemCategory::Throwable
        } else if n.contains("AMMO") || n.contains("BULLET") {
            ItemCategory::Ammo
        } else if n.contains("HELMET") {
            ItemCategory::Helmet
        } else if n.contains("ARMOR") || n.contains("VEST") {
            ItemCategory::Vest
        } else if n.contains("BAG") {
            ItemCategory::Backpack
        } else if n.contains("MED") || n.contains("HEAL") {
            ItemCategory::Medical
        } else if n.contains("KEY") {
            ItemCategory::Key
        } else {
            ItemCategory::Unknown
        }
    }
}

/// 目录里的一条物品。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemEntry {
    pub id: u64,
    /// 展示名（已把 `[Term#…]` 展开）。
    pub name: String,
    /// 原始资产名（保留 `[Term#…]` 形式，便于比对版本）。
    pub asset_name: String,
    pub category: ItemCategory,
    /// 稀有度 1..6（由 `low/mid/high` 与基础价值启发式给出）。
    pub rarity: u8,
    /// 单件估价（游戏内货币），0 表示未知。
    pub unit_price: u32,
}

/// 压缩名录：`[Term#<id>_ShortName]` 形式的词典。
#[derive(Debug, Default)]
pub struct LootCatalog {
    by_id: HashMap<u64, ItemEntry>,
    /// 词典：`Term#…` → 展开文本。
    terms: HashMap<String, String>,
}

impl LootCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// 解析样本形态的 `{"<id>":"<asset_name>"}` JSON 表。
    pub fn load_id_table_json(&mut self, json: &str) -> anyhow::Result<usize> {
        let map: HashMap<String, String> = serde_json::from_str(json)?;
        let mut n = 0;
        for (id, asset) in map {
            let Ok(id) = id.parse::<u64>() else { continue };
            let name = self.expand(&asset);
            let category = ItemCategory::from_asset_name(&asset);
            let rarity = infer_rarity(&asset, category);
            self.by_id.insert(
                id,
                ItemEntry {
                    id,
                    name,
                    asset_name: asset,
                    category,
                    rarity,
                    unit_price: estimate_price(rarity, category),
                },
            );
            n += 1;
        }
        tracing::info!(items = n, "loot catalog loaded");
        Ok(n)
    }

    /// 灌入词典（`Term#…` → 文本）。
    pub fn load_terms(&mut self, terms: impl IntoIterator<Item = (String, String)>) -> usize {
        let mut n = 0;
        for (k, v) in terms {
            self.terms.insert(k, v);
            n += 1;
        }
        n
    }

    /// 展开 `[Term#18010000014_ShortName]` 这类占位符。
    pub fn expand(&self, asset: &str) -> String {
        let mut out = String::with_capacity(asset.len());
        let bytes = asset.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'[' {
                if let Some(end) = asset[i..].find(']') {
                    let key = &asset[i + 1..i + end];
                    match self.terms.get(key) {
                        Some(v) => out.push_str(v),
                        None => {
                            // 未登记的 Term：退化为 Term 尾部的短名，仍然可读。
                            out.push_str(short_name_of(key));
                        }
                    }
                    i += end + 1;
                    continue;
                }
            }
            out.push(asset[i..].chars().next().unwrap_or('?'));
            i += asset[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
        out
    }

    pub fn get(&self, id: u64) -> Option<&ItemEntry> {
        self.by_id.get(&id)
    }

    pub fn name_of(&self, id: u64) -> Option<&str> {
        self.by_id.get(&id).map(|e| e.name.as_str())
    }

    pub fn unit_price(&self, id: u64) -> Option<u32> {
        self.by_id.get(&id).map(|e| e.unit_price).filter(|p| *p > 0)
    }

    pub fn category_of(&self, id: u64) -> Option<ItemCategory> {
        self.by_id.get(&id).map(|e| e.category)
    }

    /// 按名字模糊查（雷达搜索框）。
    pub fn search(&self, needle: &str, limit: usize) -> Vec<&ItemEntry> {
        let n = needle.to_ascii_lowercase();
        let mut out: Vec<&ItemEntry> =
            self.by_id.values().filter(|e| e.name.to_ascii_lowercase().contains(&n)).collect();
        out.sort_by_key(|e| e.id);
        out.truncate(limit);
        out
    }

    /// 目录 JSON（`/api/admin/loot` 的 `loot_catalog` 字段）。
    pub fn to_json(&self, limit: usize) -> serde_json::Value {
        let items: Vec<_> = self
            .by_id
            .values()
            .take(limit)
            .map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "name": e.name,
                    "category": e.category.zh(),
                    "rarity": e.rarity,
                    "unit_price": e.unit_price,
                })
            })
            .collect();
        serde_json::json!({
            "loot_catalog": items,
            "total": self.by_id.len(),
            "terms": self.terms.len(),
            "loot_parsing_enabled": true,
            "cached_loot_cleared": false,
        })
    }
}

/// 未登记 Term 的退化处理：取 `Term#<id>_<Suffix>` 里的 Suffix/短名。
fn short_name_of(key: &str) -> &str {
    if let Some(rest) = key.strip_prefix("Term#") {
        if let Some((_, suffix)) = rest.split_once('_') {
            return suffix;
        }
        return rest;
    }
    key
}

/// 由资产名启发式判定稀有度（`low` = 1..2，`mid` = 3..4，`high` = 5..6）。
fn infer_rarity(asset: &str, category: ItemCategory) -> u8 {
    let a = asset.to_ascii_lowercase();
    let base = if a.contains("_high") {
        5
    } else if a.contains("_mid") {
        3
    } else if a.contains("_low") {
        2
    } else {
        2
    };
    let bonus = match category {
        ItemCategory::SniperRifle | ItemCategory::MarksmanRifle => 1,
        ItemCategory::Melee | ItemCategory::Pistol => 0,
        _ => 0,
    };
    (base + bonus).min(6) as u8
}

/// 估价：只用公开的"稀有度 × 类别"量级，不编造具体价格。
fn estimate_price(rarity: u8, category: ItemCategory) -> u32 {
    let cat_mult = match category {
        ItemCategory::SniperRifle | ItemCategory::MarksmanRifle => 1_800,
        ItemCategory::AssaultRifle | ItemCategory::LightMachineGun => 1_500,
        ItemCategory::SubmachineGun | ItemCategory::Shotgun => 1_000,
        ItemCategory::Pistol | ItemCategory::Melee => 500,
        ItemCategory::Throwable | ItemCategory::Ammo => 120,
        ItemCategory::Helmet | ItemCategory::Vest => 1_200,
        ItemCategory::Backpack | ItemCategory::Medical => 400,
        ItemCategory::Key => 2_500,
        ItemCategory::Valuables => 3_000,
        ItemCategory::Unknown => 300,
    };
    cat_mult * rarity as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_prefix_maps_to_category() {
        assert_eq!(
            ItemCategory::from_asset_name("SOL_DT_Basic_AR_low_M16A4_14"),
            ItemCategory::AssaultRifle
        );
        assert_eq!(
            ItemCategory::from_asset_name("SOL_DT_Basic_SMG_low_UZI_1"),
            ItemCategory::SubmachineGun
        );
        assert_eq!(
            ItemCategory::from_asset_name("SOL_DT_Basic_Shotgun_low_M870_26"),
            ItemCategory::Shotgun
        );
        assert_eq!(ItemCategory::from_asset_name("BP_WeaponMeleeKnife"), ItemCategory::Melee);
    }

    #[test]
    fn term_expansion_uses_dictionary_then_falls_back() {
        let mut c = LootCatalog::new();
        c.load_terms([(
            "Term#18010000014_ShortName".to_string(),
            "M4A1".to_string(),
        )]);
        assert_eq!(
            c.expand("SOL_DT_Basic_AR_low_[Term#18010000014_ShortName]_14"),
            "SOL_DT_Basic_AR_low_M4A1_14"
        );
        // 未登记 Term 退化为后缀
        assert_eq!(
            c.expand("SOL_DT_Basic_AR_low_[Term#999_ShortName]_3"),
            "SOL_DT_Basic_AR_low_ShortName_3"
        );
    }

    #[test]
    fn load_id_table_and_query() {
        let json = r#"{
            "10010000903":"SOL_DT_Basic_AR_low_M16A4_14",
            "10020000901":"SOL_DT_Basic_SMG_low_UZI_1",
            "10030000881":"SOL_DT_Basic_Shotgun_low_M870_26"}"#;
        let mut c = LootCatalog::new();
        assert_eq!(c.load_id_table_json(json).unwrap(), 3);
        assert_eq!(c.name_of(10010000903), Some("SOL_DT_Basic_AR_low_M16A4_14"));
        assert_eq!(c.category_of(10020000901), Some(ItemCategory::SubmachineGun));
        assert!(c.unit_price(10010000903).unwrap() > 0);
        assert_eq!(c.search("uzi", 10).len(), 1);
    }

    #[test]
    fn rarity_follows_low_mid_high() {
        assert_eq!(infer_rarity("SOL_DT_Basic_AR_low_X_1", ItemCategory::AssaultRifle), 2);
        assert_eq!(infer_rarity("SOL_DT_Basic_AR_mid_X_1", ItemCategory::AssaultRifle), 3);
        assert_eq!(infer_rarity("SOL_DT_Basic_AR_high_X_1", ItemCategory::AssaultRifle), 5);
    }

    #[test]
    fn malformed_id_is_skipped_not_fatal() {
        let json = r#"{"not_an_id":"X","5":"SOL_DT_Basic_AR_low_Y_1"}"#;
        let mut c = LootCatalog::new();
        assert_eq!(c.load_id_table_json(json).unwrap(), 1);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn json_shape_matches_admin_api() {
        let mut c = LootCatalog::new();
        c.load_id_table_json(r#"{"1":"SOL_DT_Basic_AR_low_X_1"}"#).unwrap();
        let j = c.to_json(10);
        assert!(j.get("loot_catalog").is_some());
        assert_eq!(j["total"], 1);
        assert!(j.get("cached_loot_cleared").is_some());
    }
}
