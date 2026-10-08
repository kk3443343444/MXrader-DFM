//! 身份解析：`battle_proxy::battle::udpxin_identity`。
//!
//! 复刻样本 `src/battle/udpxin_identity.rs`。把"通道上的一个角色"变成"一个有名字、
//! 有队伍、有账号的玩家"。三角洲的复制把身份拆得很散，需要跨三个 actor 拼：
//!
//! | 来源 | 属性（handle 见 assets/channel_map.json） | 得到什么 |
//! |---|---|---|
//! | `BP_DFMCharacter_C` | `MyGUIDValue`(94) | 角色的 GUID |
//! | `BP_DFMPlayerController_C` | `PlayerState`(17)、`Pawn`(18)、`PlayerStartSpot`(34) | 控制器↔角色映射 |
//! | `BP_DFMPlayerState_C` | `TeamID`(62)、`Camp`(63)、`HeroId`(137)、`Level`(134)、 `PicUrl`(135)、`RankMatchScore`(142)、`bIsDeadBox`(129)、 `CurrentCharacterLiveStatus`(127)、`PlatformId`(60) | 队伍、英雄、段位、头像、生死 |
//! | `BP_DFMCharacter_C` | `TeamID`(184)、`Camp`(185)、`CharacterName`(170)、 `npcName`(163)、`bIsPlayerAI`(97)、`bIsPlayerAI_SOL`(98) | 名字、AI 判定 |
//!
//! 解析策略（与样本一致）：**按 handle 号索引，而不是按名字**。样本把整套
//! `handles` 表内嵌（53 个类、最多 660 个 handle），解析时只查表，不做名字比较，
//! 这样即使游戏改了属性名也不影响。

use serde::{Deserialize, Serialize};

use super::udpxin_entity::{ActorKind, ActorRecord};
use super::udpxin_entity::EntityTable;

/// 一条玩家身份（对外序列化时对应雷达 `players[]` 的元数据部分）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerIdentity {
    /// 稳定 ID：优先 `MyGUIDValue`，其次 `Uin`，最后 `Player-<channel>`。
    pub uuid: String,
    pub guid: Option<u64>,
    pub uin: Option<String>,
    /// `CharacterName`(170) —— 游戏内昵称。
    pub name: Option<String>,
    /// `npcName`(163) —— AI 的显示名。
    pub npc_name: Option<String>,
    /// `BP_DFMPlayerState_C::TeamID`(62)。
    pub team: i32,
    /// `BP_DFMPlayerState_C::Camp`(63)。
    pub camp: i32,
    /// `BP_DFMPlayerState_C::HeroId`(137)。
    pub hero_id: i32,
    pub level: i32,
    /// `PicUrl`(135) —— 头像，用于列表展示。
    pub avatar_url: Option<String>,
    /// `RankMatchScore`(142)。
    pub rank_score: i32,
    /// `PlatformId`(60)，0=iOS 1=Android 2=PC。
    pub platform_id: i32,
    /// 由 `bIsPlayerAI`(97)/`bIsPlayerAI_SOL`(98) 或类名推定。
    pub is_ai: bool,
    /// `bIsDeadBox`(129) —— 已变成死亡盒。
    pub is_dead_box: bool,
    /// `CurrentCharacterLiveStatus`(127) 原始值。
    pub live_status: u8,
    pub character_channel: Option<u32>,
    pub player_state_channel: Option<u32>,
    pub controller_channel: Option<u32>,
    pub last_seen_ms: u64,
}

impl PlayerIdentity {
    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.npc_name.clone())
            .or_else(|| self.uin.clone())
            .unwrap_or_else(|| "未知".to_string())
    }

    /// 队伍色（雷达前端也用同一套判定，这里给后端一处权威实现）。
    pub fn team_color(&self, self_team: i32) -> &'static str {
        if self.is_ai {
            "#8b93a7"
        } else if self.team == self_team && self.team != 0 {
            "#38d39f"
        } else {
            "#ff5a5a"
        }
    }
}

/// `BP_DFMPlayerState_C` 的 handle 号（取自内嵌 handles 表，勿改）。
pub mod ps_handle {
    pub const PLATFORM_ID: u32 = 60;
    pub const ACCOUNT_TYPE: u32 = 61;
    pub const TEAM_ID: u32 = 62;
    pub const CAMP: u32 = 63;
    pub const FORCES_TYPE: u32 = 64;
    pub const ARM_FORCE_ID: u32 = 65;
    pub const CHARACTER_MODE_ID: u32 = 70;
    pub const B_IS_WANTED: u32 = 71;
    pub const CURRENT_LIVE_STATUS: u32 = 127;
    pub const B_IN_GLITCH_VOLUME: u32 = 128;
    pub const B_IS_DEAD_BOX: u32 = 129;
    pub const ROLE_TYPE: u32 = 133;
    pub const LEVEL: u32 = 134;
    pub const PIC_URL: u32 = 135;
    pub const HERO_ID: u32 = 137;
    pub const RANK_MATCH_SCORE: u32 = 142;
    pub const IS_RANKED_MATCH: u32 = 143;
    pub const DFM_ANONYMOUS_INDEX: u32 = 243;
}

/// `BP_DFMCharacter_C` 的 handle 号。
pub mod char_handle {
    pub const REMOTE_ROLE: u32 = 6;
    pub const REPLICATED_MOVEMENT: u32 = 7;
    pub const REMOTE_VIEW_PITCH: u32 = 17;
    pub const PLAYER_STATE: u32 = 18;
    pub const CONTROLLER: u32 = 19;
    pub const REPLICATED_MOVEMENT_MODE: u32 = 30;
    pub const B_IS_CROUCHED: u32 = 31;
    pub const B_IS_PRONED: u32 = 47;
    pub const MY_GUID_VALUE: u32 = 94;
    pub const B_IS_PLAYER_AI: u32 = 97;
    pub const B_IS_PLAYER_AI_SOL: u32 = 98;
    pub const CHARACTER_ROTATION: u32 = 121;
    pub const TARGET_ROTATION: u32 = 120;
    pub const LOOKING_ROTATION: u32 = 122;
    pub const LATEST_MOVE_PACKAGE: u32 = 126;
    pub const CHARACTER_NAME: u32 = 170;
    pub const NPC_NAME: u32 = 163;
    pub const TEAM_ID: u32 = 184;
    pub const CAMP: u32 = 185;
    pub const DEATH_WAIT_RESCUE_TIME: u32 = 153;
    pub const B_OWNER_IS_GOD: u32 = 155;
}

/// `BP_DFMPlayerController_C` 的 handle 号。
pub mod pc_handle {
    pub const OWNER: u32 = 14;
    pub const PLAYER_STATE: u32 = 17;
    pub const PAWN: u32 = 18;
    pub const TARGET_VIEW_ROTATION: u32 = 19;
    pub const SPAWN_LOCATION: u32 = 20;
    pub const B_SERVER_ENABLE_PROCESS_PLAYER_INPUT: u32 = 31;
}

/// 身份表：uuid → 身份，并维护三类通道的互相指向。
#[derive(Debug, Default)]
pub struct IdentityTable {
    players: std::collections::HashMap<String, PlayerIdentity>,
    /// 控制器通道 → 玩家 uuid
    controller_to_uuid: std::collections::HashMap<u32, String>,
    /// 角色通道 → 玩家 uuid
    character_to_uuid: std::collections::HashMap<u32, String>,
    /// PlayerState 通道 → 玩家 uuid
    ps_to_uuid: std::collections::HashMap<u32, String>,
    /// 本地（本机所连设备上的）玩家 uuid —— 死亡盒/自己被击杀的判定基准。
    local_uuid: Option<String>,
}

impl IdentityTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn local_uuid(&self) -> Option<&str> {
        self.local_uuid.as_deref()
    }

    pub fn set_local_uuid(&mut self, uuid: Option<String>) {
        self.local_uuid = uuid;
    }

    pub fn len(&self) -> usize {
        self.players.len()
    }
    pub fn is_empty(&self) -> bool {
        self.players.is_empty()
    }
    pub fn get(&self, uuid: &str) -> Option<&PlayerIdentity> {
        self.players.get(uuid)
    }
    pub fn iter(&self) -> impl Iterator<Item = &PlayerIdentity> {
        self.players.values()
    }

    /// 稳定 ID 生成：`guid` > `uin` > `Player-<channel>`。
    fn uuid_for(guid: Option<u64>, uin: Option<&str>, channel: u32) -> String {
        if let Some(g) = guid {
            return format!("G{g:016X}");
        }
        if let Some(u) = uin {
            if !u.is_empty() {
                return format!("U{u}");
            }
        }
        format!("Player-{channel}")
    }

    /// 绑定 `PlayerController::Pawn`(18) / `Character::Controller`(19) 关系。
    pub fn bind_controller_pawn(&mut self, controller_channel: u32, pawn_channel: u32, now_ms: u64) {
        if let Some(uuid) = self.controller_to_uuid.get(&controller_channel).cloned() {
            self.character_to_uuid.insert(pawn_channel, uuid.clone());
            if let Some(p) = self.players.get_mut(&uuid) {
                p.character_channel = Some(pawn_channel);
                p.controller_channel = Some(controller_channel);
                p.last_seen_ms = now_ms;
            }
        } else {
            // 控制器先出现、Pawn 后出现是常态，先埋一个占位。
            let uuid = Self::uuid_for(None, None, controller_channel);
            self.controller_to_uuid.insert(controller_channel, uuid.clone());
            self.character_to_uuid.insert(pawn_channel, uuid.clone());
            self.players.entry(uuid.clone()).or_insert_with(|| PlayerIdentity {
                uuid,
                controller_channel: Some(controller_channel),
                character_channel: Some(pawn_channel),
                last_seen_ms: now_ms,
                ..Default::default()
            });
        }
    }

    /// `Character::PlayerState`(18) 或 `Controller::PlayerState`(17) 绑定。
    pub fn bind_player_state(&mut self, ps_channel: u32, by_character_channel: Option<u32>, now_ms: u64) {
        let uuid = by_character_channel
            .and_then(|c| self.character_to_uuid.get(&c).cloned())
            .unwrap_or_else(|| Self::uuid_for(None, None, ps_channel));
        self.ps_to_uuid.insert(ps_channel, uuid.clone());
        let entry = self.players.entry(uuid.clone()).or_insert_with(|| PlayerIdentity {
            uuid: uuid.clone(),
            last_seen_ms: now_ms,
            ..Default::default()
        });
        entry.player_state_channel = Some(ps_channel);
        entry.last_seen_ms = now_ms;
    }

    /// 应用 `BP_DFMPlayerState_C` 上读到的属性（handle 号 → 值）。
    pub fn apply_player_state(
        &mut self,
        ps_channel: u32,
        props: &PlayerStateProps,
        now_ms: u64,
    ) {
        self.bind_player_state(ps_channel, props.character_channel, now_ms);
        let Some(uuid) = self.ps_to_uuid.get(&ps_channel).cloned() else { return };
        let entry = self.players.entry(uuid.clone()).or_insert(PlayerIdentity {
            uuid: uuid.clone(),
            ..Default::default()
        });
        if let Some(t) = props.team_id {
            entry.team = t;
        }
        if let Some(c) = props.camp {
            entry.camp = c;
        }
        if let Some(h) = props.hero_id {
            entry.hero_id = h;
        }
        if let Some(l) = props.level {
            entry.level = l;
        }
        if props.pic_url.is_some() {
            entry.avatar_url = props.pic_url.clone();
        }
        if let Some(r) = props.rank_score {
            entry.rank_score = r;
        }
        if let Some(p) = props.platform_id {
            entry.platform_id = p;
        }
        if let Some(s) = props.live_status {
            entry.live_status = s;
        }
        if let Some(d) = props.is_dead_box {
            entry.is_dead_box = d;
        }
        if let Some(u) = props.uin.clone() {
            entry.uin = Some(u);
        }
        entry.last_seen_ms = now_ms;
    }

    /// 应用 `BP_DFMCharacter_C` 上读到的属性。
    pub fn apply_character(&mut self, ch_channel: u32, props: &CharacterProps, now_ms: u64) {
        let uuid = match self.character_to_uuid.get(&ch_channel).cloned() {
            Some(u) => u,
            None => {
                let u = Self::uuid_for(props.guid, None, ch_channel);
                self.character_to_uuid.insert(ch_channel, u.clone());
                u
            }
        };
        let entry = self.players.entry(uuid.clone()).or_insert(PlayerIdentity {
            uuid: uuid.clone(),
            ..Default::default()
        });
        if let Some(g) = props.guid {
            entry.guid = Some(g);
            // GUID 出现后把 uuid 升级成 G<hex>，并回填三张映射表。
            let new_uuid = Self::uuid_for(Some(g), None, ch_channel);
            if new_uuid != uuid {
                if let Some(mut moved) = self.players.remove(&uuid) {
                    moved.uuid = new_uuid.clone();
                    moved.guid = Some(g);
                    self.players.insert(new_uuid.clone(), moved);
                }
                for m in [
                    &mut self.character_to_uuid,
                    &mut self.ps_to_uuid,
                    &mut self.controller_to_uuid,
                ] {
                    for v in m.values_mut() {
                        if *v == uuid {
                            *v = new_uuid.clone();
                        }
                    }
                }
                if self.local_uuid.as_deref() == Some(uuid.as_str()) {
                    self.local_uuid = Some(new_uuid.clone());
                }
            }
        }
        let entry = self
            .players
            .get_mut(self.character_to_uuid.get(&ch_channel).cloned().unwrap_or(uuid).as_str())
            .expect("entry was just inserted");
        if let Some(n) = props.character_name.clone() {
            entry.name = Some(n);
        }
        if let Some(n) = props.npc_name.clone() {
            entry.npc_name = Some(n);
        }
        if let Some(t) = props.team_id {
            entry.team = t;
        }
        if let Some(c) = props.camp {
            entry.camp = c;
        }
        if let Some(ai) = props.is_player_ai {
            entry.is_ai = ai;
        }
        entry.character_channel = Some(ch_channel);
        entry.last_seen_ms = now_ms;
    }

    /// 一个 `ActorRecord` 对应的身份（雷达序列化时用）。
    pub fn resolve(&self, rec: &ActorRecord) -> Option<&PlayerIdentity> {
        let uuid = self
            .character_to_uuid
            .get(&rec.channel)
            .or_else(|| self.ps_to_uuid.get(&rec.channel))
            .or_else(|| self.controller_to_uuid.get(&rec.channel))?;
        self.players.get(uuid)
    }

    /// 从实体表整体重建（每帧调用，保证僵尸通道不残留身份）。
    pub fn refresh_from_entities(&mut self, table: &EntityTable, now_ms: u64) -> usize {
        let mut bound = 0;
        for rec in table.iter() {
            match rec.kind {
                ActorKind::PlayerState => {
                    self.bind_player_state(
                        rec.channel,
                        None,
                        rec.last_update_ms.max(now_ms.saturating_sub(1_000)),
                    );
                    bound += 1;
                }
                ActorKind::PlayerController => {
                    if let Some(pawn) = rec.owner_channel {
                        self.bind_controller_pawn(rec.channel, pawn, rec.last_update_ms);
                        bound += 1;
                    }
                }
                ActorKind::PlayerCharacter | ActorKind::AiCharacter => {
                    if let Some(g) = rec.guid {
                        let uuid = Self::uuid_for(Some(g), None, rec.channel);
                        self.character_to_uuid.insert(rec.channel, uuid.clone());
                        let e = self.players.entry(uuid.clone()).or_insert(PlayerIdentity {
                            uuid,
                            ..Default::default()
                        });
                        e.character_channel = Some(rec.channel);
                        if rec.is_ai {
                            e.is_ai = true;
                        }
                        if let Some(n) = rec.character_name.clone() {
                            e.name = Some(n);
                        }
                        if rec.team != 0 {
                            e.team = rec.team;
                        }
                        if rec.camp != 0 {
                            e.camp = rec.camp;
                        }
                        e.hero_id = rec.hero_id;
                        e.last_seen_ms = rec.last_update_ms;
                        bound += 1;
                    }
                }
                _ => {}
            }
        }
        bound
    }

    /// 清理长期未见身份（默认 60 秒），返回清理数量。
    pub fn reap_stale(&mut self, now_ms: u64, ttl_ms: u64) -> usize {
        let before = self.players.len();
        let dead: Vec<String> = self
            .players
            .iter()
            .filter(|(_, p)| now_ms.saturating_sub(p.last_seen_ms) > ttl_ms)
            .map(|(k, _)| k.clone())
            .collect();
        for uuid in &dead {
            self.players.remove(uuid);
            self.character_to_uuid.retain(|_, v| v != uuid);
            self.ps_to_uuid.retain(|_, v| v != uuid);
            self.controller_to_uuid.retain(|_, v| v != uuid);
        }
        before - self.players.len()
    }
}

/// `BP_DFMPlayerState_C` 一次属性块里解析出来的字段。
#[derive(Debug, Clone, Default)]
pub struct PlayerStateProps {
    pub character_channel: Option<u32>,
    pub team_id: Option<i32>,
    pub camp: Option<i32>,
    pub hero_id: Option<i32>,
    pub level: Option<i32>,
    pub pic_url: Option<String>,
    pub rank_score: Option<i32>,
    pub platform_id: Option<i32>,
    pub live_status: Option<u8>,
    pub is_dead_box: Option<bool>,
    pub uin: Option<String>,
}

/// `BP_DFMCharacter_C` 一次属性块里解析出来的字段。
#[derive(Debug, Clone, Default)]
pub struct CharacterProps {
    pub guid: Option<u64>,
    pub character_name: Option<String>,
    pub npc_name: Option<String>,
    pub team_id: Option<i32>,
    pub camp: Option<i32>,
    pub is_player_ai: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_prefers_guid_then_uin_then_channel() {
        assert_eq!(IdentityTable::uuid_for(Some(0x1f), Some("u1"), 3), "G000000000000001F");
        assert_eq!(IdentityTable::uuid_for(None, Some("u1"), 3), "Uu1");
        assert_eq!(IdentityTable::uuid_for(None, None, 3), "Player-3");
    }

    #[test]
    fn controller_pawn_binding_then_state_enrichment() {
        let mut t = IdentityTable::new();
        t.bind_controller_pawn(11, 3, 100);
        let uuid = t.controller_to_uuid.get(&11).cloned().unwrap();
        t.apply_player_state(
            4,
            &PlayerStateProps {
                character_channel: Some(3),
                team_id: Some(1),
                camp: Some(2),
                hero_id: Some(1024),
                level: Some(42),
                rank_score: Some(3200),
                ..Default::default()
            },
            200,
        );
        let p = t.get(&uuid).unwrap();
        assert_eq!(p.team, 1);
        assert_eq!(p.camp, 2);
        assert_eq!(p.hero_id, 1024);
        assert_eq!(p.level, 42);
        assert_eq!(p.rank_score, 3200);
        assert_eq!(p.character_channel, Some(3));
    }

    #[test]
    fn guid_upgrade_rewrites_all_maps() {
        let mut t = IdentityTable::new();
        t.bind_controller_pawn(11, 3, 100);
        let old = t.controller_to_uuid.get(&11).cloned().unwrap();
        t.set_local_uuid(Some(old.clone()));
        t.apply_character(
            3,
            &CharacterProps { guid: Some(0xAB), character_name: Some("老六".into()), ..Default::default() },
            300,
        );
        assert!(t.get(&old).is_none(), "old placeholder uuid must be migrated");
        // `uuid_for` = `format!("G{g:016X}")` = 'G' + 16 位十六进制（见
        // `uuid_prefers_guid_then_uin_then_channel` 与 docs/PROTOCOL.md §1）。
        // 本用例原来的字面量少了一位（'G' + 15 位），
        // 与 uuid_for 的格式和那个**通过**的用例都矛盾，故按契约改为 16 位。
        let p = t.get("G00000000000000AB").unwrap();
        assert_eq!(p.name.as_deref(), Some("老六"));
        assert_eq!(t.local_uuid(), Some("G00000000000000AB"));
        assert_eq!(t.controller_to_uuid.get(&11).map(String::as_str), Some("G00000000000000AB"));
    }

    #[test]
    fn ai_flag_from_props() {
        let mut t = IdentityTable::new();
        t.apply_character(7, &CharacterProps { is_player_ai: Some(true), ..Default::default() }, 1);
        let p = t.iter().next().unwrap();
        assert!(p.is_ai);
        assert_eq!(p.team_color(1), "#8b93a7");
    }

    #[test]
    fn team_color_rules() {
        let mut p = PlayerIdentity { team: 2, ..Default::default() };
        assert_eq!(p.team_color(2), "#38d39f");
        assert_eq!(p.team_color(1), "#ff5a5a");
        p.team = 0;
        assert_eq!(p.team_color(0), "#ff5a5a", "team 0 must not be treated as self");
        p.is_ai = true;
        assert_eq!(p.team_color(0), "#8b93a7");
    }

    #[test]
    fn reap_stale_clears_all_indexes() {
        let mut t = IdentityTable::new();
        t.apply_character(7, &CharacterProps { guid: Some(5), ..Default::default() }, 1_000);
        assert_eq!(t.len(), 1);
        assert_eq!(t.reap_stale(100_000, 60_000), 1);
        assert!(t.is_empty());
        assert!(t.character_to_uuid.is_empty());
    }
}
