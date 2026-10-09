//! 解析核心：`battle_proxy::battle`。
//!
//! 模块切分与样本一一对应（`src/battle/*.rs`）：
//!
//! ```text
//! engine.rs            编排：feed → 队列 → 解析 → 应用 → 广播
//! session.rs           一设备一会话的独立状态
//! parse_queue.rs       有界异步流水（battle-parse-<n>/ordered-apply/watchdog）
//! transport_crypto.rs  AES-256-ECB-XOR / LZ4 / XOR / **XTEA+8 键 bank** 逐包嗅探
//! xtea.rs              XTEA（64 轮）解密 + `bank[block_index & 7]` 轮换（参考实现 UDP 剖面）
//! udpxin.rs            UDP 数据报 → UE Bunch 序列
//! udpxin_entity.rs     通道表（actor 记录）
//! udpxin_identity.rs   身份表（GUID/名字/队伍/英雄）
//! udpxin_move.rs       FRepMovement 解码 + **3D 转向修正**
//! udpxin_live.rs       生死四态（倒地/复活规则）
//! udpxin_exports.rs    PackageMap 导出 → 类名；运行期 GUID 目录
//! combat.rs            武器注册表 / 视角扫描 / 包增长护栏
//! loot_catalog.rs      压缩物资名录
//! protocol_capture.rs  采集策略与限时限量
//! selftest.rs          内嵌金标准向量自检
//! codec/*              位流、S2C 分帧、属性遍历、开火、击杀链、容器、原始采集
//! ```

pub mod codec;
pub mod combat;
pub mod engine;
pub mod loot_catalog;
pub mod parse_queue;
pub mod protocol_capture;
pub mod selftest;
pub mod session;
pub mod transport_crypto;
pub mod udpxin;
pub mod udpxin_entity;
pub mod udpxin_exports;
pub mod udpxin_identity;
pub mod udpxin_live;
pub mod udpxin_move;
pub mod xtea;

pub use engine::{BattleEngine, EngineUpdate};
pub use session::{SessionRegistry, SessionState};
pub use udpxin::ProtocolProfile;
pub use xtea::{XteaKeyBank, XTEA_BANK_LEN, XTEA_BLOCK_LEN, XTEA_KEY_LEN};

/// 一局游戏里所有已知的地图键（雷达 `maps.json` 与前端一致）。
pub const MAP_KEYS: &[(&str, &str)] = &[
    ("ZeroDam", "零号大坝"),
    ("Layali", "长弓溪谷"),
    ("Bakesh", "巴克什"),
    ("SpaceCity", "航天基地"),
    ("Brakesh", "巴克什·夜战"),
];

/// 由房间/世界属性串猜地图（样本用世界 actor 与 POI 属性判定）。
pub fn guess_map(world_name: &str) -> Option<&'static str> {
    let n = world_name.to_ascii_lowercase();
    for (key, zh) in MAP_KEYS {
        if n.contains(&key.to_ascii_lowercase()) || world_name.contains(zh) {
            return Some(key);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_guessing_by_asset_and_chinese_name() {
        assert_eq!(guess_map("World_ZeroDam_01"), Some("ZeroDam"));
        assert_eq!(guess_map("零号大坝"), Some("ZeroDam"));
        assert_eq!(guess_map("Layali_Grove_HD"), Some("Layali"));
        assert_eq!(guess_map("Unknown"), None);
    }

    #[test]
    fn every_map_key_has_a_chinese_label() {
        for (k, zh) in MAP_KEYS {
            assert!(!k.is_empty() && !zh.is_empty());
        }
    }
}
