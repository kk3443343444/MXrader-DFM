//! 实体表：`battle_proxy::battle::udpxin_entity`。
//!
//! 复刻样本 `src/battle/udpxin_entity.rs`。UE 的复制是**按通道**进行的，所以雷达
//! 的"玩家表"本质上就是"通道表"：`channel_index → ActorRecord`。
//!
//! 每个 actor 记录里最关键的六件事：
//! 1. `class` —— 由 `bHasPackageMapExports` 导出或由 `assets/channel_map.json`
//!    的 `channel_map[].class` 直接命中（样本把这张表**内嵌进二进制**，见
//!    `channel_map` 常量：77 个 slot，`BP_DFMCharacter_C` / `BP_DFMPlayerState_C` …）。
//! 2. `guid` —— 每个 actor 的唯一标识，是"同一玩家换通道"时保持连续的关键。
//! 3. `position/rotation`（cm/度）—— 雷达的坐标来源。
//! 4. `team/camp` —— 分队着色。
//! 5. `health/alive` —— 血条与死亡盒。
//! 6. `player_ref` —— 关联到 `PlayerIdentity`（由 udpxin_identity 建立）。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::codec::bitstream::{rotator_to_deg, BitReader};

/// Actor 类别。判定顺序与样本一致：先看精确类名，再退化到模式匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// `BP_DFMCharacter_C` —— 真人玩家角色。
    PlayerCharacter,
    /// `BP_DFMCharacter_AI_DT_C` / `BP_DFMAICharacter_ShielderLight_C` —— 人机。
    AiCharacter,
    /// `BP_DFMPlayerState_C` —— 玩家状态（名字/队伍/血量聚合点）。
    PlayerState,
    /// `BP_DFMPlayerController_C` —— 控制器（视角朝向）。
    PlayerController,
    /// `BP_Weapon*_C` / `BP_EmptyHand_C` —— 手持物，用于武器识别与开火链。
    Weapon,
    /// `DFMContainerDataCollector` 等 —— 物资容器。
    Container,
    /// `MovementReplicationActor` —— 位移复制专用 actor。
    Movement,
    /// `DesReplicationActor` —— 可破坏物。
    Destructible,
    /// `BP_PropertyReplicationActor_C` 系列 —— 血量等属性中继。
    PropertyRelay,
    /// 其它。
    Other,
}

impl ActorKind {
    /// 雷达上是否需要画点。
    pub fn is_drawable(self) -> bool {
        matches!(
            self,
            ActorKind::PlayerCharacter | ActorKind::AiCharacter | ActorKind::Container
        )
    }
    pub fn is_character(self) -> bool {
        matches!(self, ActorKind::PlayerCharacter | ActorKind::AiCharacter)
    }

    pub fn from_class_name(class: &str) -> Self {
        let c = class;
        if c.starts_with("BP_DFMCharacter_AI") || c.starts_with("BP_DFMAICharacter") {
            ActorKind::AiCharacter
        } else if c.starts_with("BP_DFMCharacter") {
            ActorKind::PlayerCharacter
        } else if c.starts_with("BP_DFMPlayerState") {
            ActorKind::PlayerState
        } else if c.starts_with("BP_DFMPlayerController") {
            ActorKind::PlayerController
        } else if c.starts_with("BP_Weapon") || c.starts_with("BP_EmptyHand") {
            ActorKind::Weapon
        } else if c.contains("Container") || c.contains("Collector") {
            ActorKind::Container
        } else if c.contains("MovementReplication") {
            ActorKind::Movement
        } else if c.starts_with("DesReplication") {
            ActorKind::Destructible
        } else if c.starts_with("BP_PropertyReplication") {
            ActorKind::PropertyRelay
        } else {
            ActorKind::Other
        }
    }
}

/// 三维向量（UE 世界空间，厘米；Z 向上，左手系）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub fn is_finite(&self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
    pub fn distance_to(&self, o: &Vec3) -> f32 {
        let dx = self.x - o.x;
        let dy = self.y - o.y;
        let dz = self.z - o.z;
        (dx * dx + dy * dy + dz * dz).sqrt()
    }
    pub fn horizontal_distance_to(&self, o: &Vec3) -> f32 {
        let dx = self.x - o.x;
        let dy = self.y - o.y;
        (dx * dx + dy * dy).sqrt()
    }
    /// 米（雷达对外统一 SI 单位）。
    pub fn to_metres(&self) -> [f32; 3] {
        [self.x * 0.01, self.y * 0.01, self.z * 0.01]
    }
}

/// UE `FRotator` 的量化三轴（度）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Rot3 {
    pub pitch: f32,
    pub yaw: f32,
    pub roll: f32,
}

impl Rot3 {
    pub fn from_rotator16(p: u16, y: u16, r: u16) -> Self {
        Self {
            pitch: rotator_to_deg(p),
            yaw: rotator_to_deg(y),
            roll: rotator_to_deg(r),
        }
    }
    /// 从位流读 48 位 Rotator。
    pub fn read_from(r: &mut BitReader<'_>) -> Self {
        let [p, y, rl] = r.read_rotator_16();
        Self { pitch: p, yaw: y, roll: rl }
    }
}

/// 一条 actor 记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActorRecord {
    pub channel: u32,
    /// 由 PackageMap 导出或静态表命中的类名。
    pub class: String,
    pub kind: ActorKind,
    /// 类名未解析（只有 ch_index）时为 true，仍可用于位移追踪。
    pub class_unresolved: bool,
    pub guid: Option<u64>,
    /// 该通道所属的 PlayerState 通道（由 Controller/Pawn 关系推导）。
    pub owner_channel: Option<u32>,
    pub position: Vec3,
    pub rotation: Rot3,
    pub velocity: Vec3,
    /// 物化单位（UE 内部的 `CompressedFlags`：蹲/趴/跳…）。
    pub movement_flags: u8,
    pub team: i32,
    pub camp: i32,
    pub health: f32,
    pub max_health: f32,
    pub alive: bool,
    /// 是否 AI（由 `bIsPlayerAI` / `bIsPlayerAI_SOL` 位得出，比类名更准）。
    pub is_ai: bool,
    /// `MyGUIDValue`（handle 94）—— 与 PlayerState 的 Uin 对应。
    pub player_uin: Option<String>,
    pub character_name: Option<String>,
    pub hero_id: i32,
    pub weapon_class: Option<String>,
    pub last_update_ms: u64,
    /// 上次做属性解码的服务器时间戳（用于算"数据新鲜度"）。
    pub server_time_ms: Option<u64>,
    /// 是否已死亡（`DeadInfo` / `bDeadCanOPtimise`）。
    pub dead: bool,
}

impl Default for ActorRecord {
    fn default() -> Self {
        Self {
            channel: 0,
            class: String::new(),
            kind: ActorKind::Other,
            class_unresolved: true,
            guid: None,
            owner_channel: None,
            position: Vec3::default(),
            rotation: Rot3::default(),
            velocity: Vec3::default(),
            movement_flags: 0,
            team: 0,
            camp: 0,
            health: 0.0,
            max_health: 0.0,
            alive: true,
            is_ai: false,
            player_uin: None,
            character_name: None,
            hero_id: 0,
            weapon_class: None,
            last_update_ms: 0,
            server_time_ms: None,
            dead: false,
        }
    }
}

impl ActorRecord {
    pub fn new(channel: u32, class: Option<&str>, now_ms: u64) -> Self {
        let mut rec = Self { channel, last_update_ms: now_ms, ..Self::default() };
        match class {
            Some(c) => {
                rec.class = c.to_string();
                rec.kind = ActorKind::from_class_name(c);
                rec.class_unresolved = false;
            }
            None => rec.kind = ActorKind::Other,
        }
        rec
    }

    /// 位置有效性：UE 的空 actor 是 (0,0,0)，必须过滤，否则雷达上会有一堆点堆在原点。
    pub fn has_valid_position(&self) -> bool {
        self.position.is_finite()
            && !(self.position.x == 0.0 && self.position.y == 0.0 && self.position.z == 0.0)
    }

    pub fn is_stale(&self, now_ms: u64, ttl_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_update_ms) > ttl_ms
    }
}

/// 通道表。
#[derive(Debug, Default)]
pub struct EntityTable {
    actors: HashMap<u32, ActorRecord>,
    /// GUID → channel，用于"同一玩家换通道"的迁移。
    by_guid: HashMap<u64, u32>,
    /// 静态 channel_map（来自 assets/channel_map.json）。
    static_map: HashMap<u32, (String, u32, u32)>,
}

impl EntityTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 载入内嵌的 `channel_map.json`（样本里的 `channel_map` 常量，77 项）。
    pub fn load_static_channel_map(&mut self, json: &str) -> anyhow::Result<usize> {
        #[derive(Deserialize)]
        struct Root {
            #[serde(default)]
            channel_map: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            #[serde(default)]
            slot: u32,
            ch_index: u32,
            class: String,
            #[serde(default)]
            parent_num: u32,
            #[serde(default)]
            cmd_num: u32,
        }
        let root: Root = serde_json::from_str(json)?;
        let mut n = 0;
        for e in root.channel_map {
            let key = if e.slot != 0 { e.slot } else { e.ch_index };
            self.static_map.insert(key, (e.class, e.parent_num, e.cmd_num));
            n += 1;
        }
        // 同时按 ch_index 建一份，UE 的通道索引就在这个区间里。
        tracing::info!(entries = n, "channel_map loaded");
        Ok(n)
    }

    /// 静态表里该通道对应的类名（可能为空）。
    pub fn class_for_channel(&self, channel: u32) -> Option<&str> {
        self.static_map.get(&channel).map(|v| v.0.as_str())
    }

    /// 声明/打开一个通道（bunch 的 bOpen 路径）。
    pub fn open_channel(
        &mut self,
        channel: u32,
        class: Option<&str>,
        guid: Option<u64>,
        now_ms: u64,
    ) -> &mut ActorRecord {
        let resolved = class
            .map(|c| c.to_string())
            .or_else(|| self.static_map.get(&channel).map(|v| v.0.clone()));

        // 先取出"同一 GUID 的旧记录"快照，再动 actors —— 否则会同时可变/不可变借用 self.actors。
        let migrated: Option<ActorRecord> = match guid {
            Some(g) => match self.by_guid.get(&g).copied() {
                Some(prev_ch) if prev_ch != channel => self.actors.get(&prev_ch).cloned(),
                _ => None,
            },
            None => None,
        };

        if let Some(g) = guid {
            self.by_guid.insert(g, channel);
        }

        let now = now_ms;
        let rec = self
            .actors
            .entry(channel)
            .or_insert_with(|| ActorRecord::new(channel, resolved.as_deref(), now));
        if rec.class.is_empty() {
            if let Some(c) = resolved {
                rec.kind = ActorKind::from_class_name(&c);
                rec.class = c;
                rec.class_unresolved = false;
            }
        }
        if let Some(prev) = migrated {
            // 同一 GUID 换通道：把位置与玩家信息迁移过来，避免雷达上"跳点"。
            // 只有新记录还没有有效位置时才搬运，避免用旧位置覆盖新数据。
            if !rec.has_valid_position() && prev.has_valid_position() {
                rec.position = prev.position;
            }
            rec.rotation = prev.rotation;
            rec.player_uin = prev.player_uin.clone();
            rec.character_name = prev.character_name.clone();
        }
        if let Some(g) = guid {
            rec.guid = Some(g);
        }
        rec
    }

    pub fn close_channel(&mut self, channel: u32, keep_ghost: bool) {
        if !keep_ghost {
            if let Some(rec) = self.actors.remove(&channel) {
                if let Some(g) = rec.guid {
                    self.by_guid.remove(&g);
                }
            }
        } else if let Some(rec) = self.actors.get_mut(&channel) {
            // 样本在 `move_after_recoverable_death` 场景保留幽灵记录：
            // 通道关了但玩家可能复活，位置不能立刻丢。
            rec.last_update_ms = rec.last_update_ms.saturating_sub(1);
        }
    }

    pub fn get(&self, channel: u32) -> Option<&ActorRecord> {
        self.actors.get(&channel)
    }
    pub fn contains(&self, channel: u32) -> bool {
        self.actors.contains_key(&channel)
    }
    pub fn get_mut(&mut self, channel: u32) -> Option<&mut ActorRecord> {
        self.actors.get_mut(&channel)
    }
    pub fn by_guid(&self, guid: u64) -> Option<&ActorRecord> {
        self.by_guid.get(&guid).and_then(|c| self.actors.get(c))
    }
    pub fn len(&self) -> usize {
        self.actors.len()
    }
    pub fn is_empty(&self) -> bool {
        self.actors.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &ActorRecord> {
        self.actors.values()
    }

    /// 可绘制实体（真人 + AI + 物资）。
    pub fn drawable(&self) -> impl Iterator<Item = &ActorRecord> {
        self.actors.values().filter(|a| a.kind.is_drawable() && a.has_valid_position())
    }

    /// 清理过期通道（默认 15 秒未见更新）。
    pub fn reap_stale(&mut self, now_ms: u64, ttl_ms: u64) -> usize {
        let stale: Vec<u32> = self
            .actors
            .iter()
            .filter(|(_, a)| a.is_stale(now_ms, ttl_ms) && !a.dead)
            .map(|(c, _)| *c)
            .collect();
        for c in &stale {
            if let Some(rec) = self.actors.remove(c) {
                if let Some(g) = rec.guid {
                    self.by_guid.remove(&g);
                }
            }
        }
        stale.len()
    }

    /// 重复 GUID 检测：样本用 `deferred_spawns` / `guid_catalog` 处理"同一玩家
    /// 先以未知类出现、稍后才导出类名"的时序问题。
    pub fn pending_class_resolution(&self) -> usize {
        self.actors.values().filter(|a| a.class_unresolved).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_to_kind_matrix() {
        assert_eq!(ActorKind::from_class_name("BP_DFMCharacter_C"), ActorKind::PlayerCharacter);
        assert_eq!(
            ActorKind::from_class_name("BP_DFMCharacter_AI_DT_C"),
            ActorKind::AiCharacter
        );
        assert_eq!(
            ActorKind::from_class_name("BP_DFMAICharacter_ShielderLight_C"),
            ActorKind::AiCharacter
        );
        assert_eq!(ActorKind::from_class_name("BP_DFMPlayerState_C"), ActorKind::PlayerState);
        assert_eq!(ActorKind::from_class_name("BP_WeaponMeleeNoModular_C"), ActorKind::Weapon);
        assert_eq!(ActorKind::from_class_name("DFMContainerDataCollector"), ActorKind::Container);
        assert_eq!(ActorKind::from_class_name("MovementReplicationActor"), ActorKind::Movement);
        assert_eq!(ActorKind::from_class_name("Unknown"), ActorKind::Other);
    }

    #[test]
    fn static_channel_map_drives_class_resolution() {
        let json = r#"{"channel_map":[
            {"slot":3,"ch_index":3,"class":"BP_DFMCharacter_C","parent_num":106,"cmd_num":200},
            {"slot":4,"ch_index":4,"class":"BP_DFMPlayerState_C","parent_num":111,"cmd_num":672}]}"#;
        let mut t = EntityTable::new();
        assert_eq!(t.load_static_channel_map(json).unwrap(), 2);
        let rec = t.open_channel(3, None, Some(0xDEAD_BEEF), 100);
        assert_eq!(rec.kind, ActorKind::PlayerCharacter);
        assert!(!rec.class_unresolved);
    }

    #[test]
    fn guid_migration_keeps_position_across_channel_switch() {
        let mut t = EntityTable::new();
        {
            let r = t.open_channel(3, Some("BP_DFMCharacter_C"), Some(7), 1000);
            r.position = Vec3 { x: 1000.0, y: 2000.0, z: 30.0 };
        }
        let r2 = t.open_channel(40, None, Some(7), 2000);
        assert_eq!(r2.position.x, 1000.0, "position should migrate with the GUID");
        assert_eq!(t.len(), 2);
        assert!(t.by_guid(7).is_some());
    }

    #[test]
    fn zero_position_is_not_drawable() {
        let mut t = EntityTable::new();
        let r = t.open_channel(3, Some("BP_DFMCharacter_C"), Some(1), 0);
        r.position = Vec3::default();
        assert_eq!(t.drawable().count(), 0);
        t.get_mut(3).unwrap().position = Vec3 { x: 1.0, y: 2.0, z: 3.0 };
        assert_eq!(t.drawable().count(), 1);
    }

    #[test]
    fn stale_actor_reaping() {
        let mut t = EntityTable::new();
        t.open_channel(3, Some("BP_DFMCharacter_C"), None, 1_000);
        assert_eq!(t.reap_stale(20_000, 15_000), 1);
        assert!(t.is_empty());
    }

    #[test]
    fn metres_conversion_is_centimetre_based() {
        let v = Vec3 { x: 1234.0, y: -5600.0, z: 300.0 };
        assert_eq!(v.to_metres(), [12.34, -56.0, 3.0]);
    }
}
