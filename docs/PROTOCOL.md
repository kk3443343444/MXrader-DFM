# 协议层：把《三角洲行动》手游的复制流变成坐标

本文件是 `core/src/battle/**` 的说明书，也是**换游戏版本时唯一需要改的地方**。
所有位宽/开关都集中在三个 `*Profile` 结构里，改它们不需要动逻辑代码。

## 0. 为什么这条路走得通

三角洲行动手游是 UE5 客户端，服务端用**属性复制（property replication）**把其他玩家的
位置、朝向、队伍、血量同步给每个客户端。复制数据在 UDP 上按 **Bunch** 传输，
每个 Bunch 属于某个 **通道（channel）**，通道绑定一个 actor（`BP_DFMCharacter_C` 等）。

于是：**只要在中间人位置读到并解出这些 Bunch，就等于拿到了全图玩家的坐标**——
不需要任何内存读写、不需要越狱、不需要改客户端。这就是"免 root 雷达"的全部原理。

前提是客户端流量确实从中间人过（B 机把 SOCKS5 + UDP 转发指向 A 机），
且复制数据在链路上是**明文或可解**的（样本证明 r39 的位移复制是明文/LZ4，
加密只用在握手等少数通道上；实战中也可能遇到 AES 通道，`transport_crypto` 已覆盖）。

## 1. 五层管线

```
UDP datagram
  └─[1] transport_crypto   解密 / 解压（逐包嗅探，失败保留原始字节）
       └─[2] udpxin        包头 + Bunch 序列（含 partial 跨包重组）
            └─[3] character RepLayout 属性遍历（handle 升序 + 存在位）
                 └─[4] udpxin_move / udpxin_identity / udpxin_live
                      └─[5] combat / engine  交战判定 + 快照
```

### [1] 传输保护

`core/src/battle/transport_crypto.rs`

| 保护 | 判定依据 | 处理 |
|---|---|---|
| 明文 | `validate_datagram` 通过 | 直接用 |
| LZ4 | `lz4_flex::block::decompress` 成功且结构成立 | 解压后用 |
| AES-256-ECB-XOR | UE `FAES::DecryptData`：先 AES-ECB 解密，再异或 Key 前 16 字节 | 解密后用 |
| AES + LZ4 | 解密后再解压 | — |
| XorStream | `state = state*0x41C64E6D + 0x3039`，取高位字节 | 兜底 |

`decode_packet` 的顺序是 **Plain → LZ4 → AES → AES+LZ4 → XOR**，
validator 就是 decode gate（见 [2]）。全部失败 → `TransportKind::Unknown`，
**仍然把原始字节交给上层**（位移字段常常还在明文里）。

> 密钥来源：样本里没有硬编码传输密钥（`aes_key` 默认 `None`）。真实场景下
> 三角洲的会话密钥在握手报文里协商；若遇到加密通道，把密钥注入
> `BattleEngine::set_transport_keys()` 即可，逻辑无需改动。

### [2] Bunch 分帧

`core/src/battle/udpxin.rs`，全部位宽集中在 `ProtocolProfile`：

```rust
ProtocolProfile::dfm_r39()          // 默认档（保守）
  packet_id_bits       0            // 0 = SerializeInt(packet_id_max)
  packet_id_max        1023
  ack_bits / ack_max   0 / 1023
  has_server_frame_time true        // UE5.1+ 包头首 bit
  has_packet_info_bit  false
  channel_index_bits   0            // 0 = SerializeInt(max_channels)
  max_channels         4096
  bunch_data_bits_width 16
  supports_partial_bunch true
  compression_prefix_u32 true
  bunch_variant        Ue5ReplicationPaused
```

三种已知变体：`UeClassic`（4.20–5.0）、`Ue5ReplicationPaused`（5.1+）、
`Unresolved`（无法判定 → 只统计不解析，对应样本的 `unresolved_bunch_header_variant`）。

关键错误串与行为（与样本逐一对应）：

| 场景 | 错误 | 行为 |
|---|---|---|
| 包头读不出 | `InvalidPacketFraming` | 整包丢弃，`gate_passed=false` |
| bunch 头不符任何变体 | `UnresolvedBunchHeaderVariant` | 停止解析，保留已解出的 bunch |
| `payload_bits` > 剩余位 | `BunchPayloadExceedsPacket` | `Bunch payload exceeds packet at`，丢弃该 bunch |
| 半成品跨包没等到 | `TruncatedBunch` | 5 秒后由 `reap_partials` 清掉 |

### [3] RepLayout 属性遍历

`core/src/battle/codec/character.rs`

* 属性按 **handle 升序**（`HandleTable::ordered`），与 UE 的 `FRepLayout` 顺序一致；
* **初始状态包**（`bIsInitialState`）：无存在位，顺序读全部属性；
* **增量包**：每个属性前 1 位存在位；
* 收尾校验：`CMD_END` 之后应正好耗尽净荷；不对齐则 `rep_layout_not_exactly_closed`
  （样本串），但**仍交付已解出的字段**；
* 名字即类型（样本的 `[field_infer]`）：

| 名字特征 | 类型 | 解码 |
|---|---|---|
| `b` + 大写首字母 | Bool | 1 位 |
| `RemoteViewPitch` | Byte | 8 位 → `*360/255` 度 |
| `Role`/`RemoteRole` | Byte | 8 位 |
| `*Location`/`*Velocity`/`*Position`/`*Direction` | Vector | 每分量 1 位符号 + 20 位幅值，scale 0.01 |
| `*Rotation`/`*Rotator`/`*Aim` | Rotator | 3×16 位量化（65536 分之一圈） |
| `*Name`/`*Str`/`*Url`/`*Path` | Str | UE `FString`（int32 长度 + ANSI/UTF-16） |
| `*Speed`/`*Factor`/`*Time`/`*Damage` | Float | 位宽 + 幅值 + 符号 |
| 其余 | Unknown | 跳过（8 或 32 位），保留位偏移 |

### [4] 位移 / 身份 / 生死

**`FRepMovement`（`udpxin_move.rs`）** —— 坐标的真正来源：

```
PackedFlags(1B): bit0 bSimulatedPhysicSleep, bit1 bRepPhysics,
                 bit2 bServerHasBase, bit3 bRelativeRotation
bRepPhysics ?  Location(NetQuantize100) + Rotation(3×16) + LinearVelocity(NetQuantize100)
            : [base? Location] [bRelativeRotation? Rotation] LinearVelocity
```

`RepMovementProfile` 可调：`location_bits=20`、`location_scale=0.01`（1/100 cm 定点）、
`velocity_bits=16`、`rotator_16bit=true`。

**身份（`udpxin_identity.rs`）** —— 用 handle 号而非名字查表，跨版本更稳：

```
BP_DFMCharacter_C::MyGUIDValue(94) → GUID → uuid = "G<16 hex>"
BP_DFMCharacter_C::Controller(19) ↔ BP_DFMPlayerController_C::Pawn(18)
BP_DFMCharacter_C::PlayerState(18) ↔ BP_DFMPlayerState_C → TeamID(62)/Camp(63)/HeroId(137)…
BP_DFMCharacter_C::CharacterName(170) / npcName(163) / bIsPlayerAI(97)
```

**生死（`udpxin_live.rs`）** —— 四态机 + 样本命名的那两条关键规则：

```
Alive ──倒地(bIsBeingRescueReplicate/DeathWaitRescueTime_Custom)──▶ Downed
  ▲                                                                   │
  │  indexed 位移 / RPC 活动（indexed_movement_proves_revival_after_dead）
  └───────────────────────────────────────────────────────────────────┘
Downed ──DeadInfo/bDeadCanOPtimise──▶ Dead ──bIsDeadBox(129)──▶ DeadBox
```

**陈旧位移不改状态机** —— 否则死亡盒会"自己走起来"（这就是把 `MoveSource`
分成 `Stale`/`Indexed`/`RpcActivity` 的原因）。

### [5] 交战与弹道

`core/src/battle/combat.rs` + `codec/fire.rs`：

```
开火 RPC ServerProcessWeaponEventDataForFirer
  → fire_rotation / origin(SpawnLocation) / owner_velocity / InitSpeedQ100
  → v = forward(FireRotation) * InitSpeed + OwnerVelocity     （ballistic_formula）
  → 弹道采样 p(t) = origin + v·t − ½·g·t²     g = 980 cm/s²
  → 雷达 traces[]：起点→终点（米），带 shooter/weapon/置信度
视角扫描（[aim_parse] view scan #）：在 fire_direction 的锥体里按角度误差排序找候选目标
包增长护栏：GrowthGuard（单批 >64 通道或 >3 倍膨胀 → 整批丢弃）
武器注册表：BP_Weapon* 通道 → CharacterOwner(41)；缺失时用 GUID 兜底（combat_fallback）
```

击杀链的伤害类型枚举完整保留（含游戏自身的拼写错误 `Artilerrate`/`Missle`），
前端会翻成中文：武器击杀/自伤/毒气/坠落/濒死/增益/管理员/环境爆炸/载具武器/处决/
战场支援/区域炮击/制导导弹。

## 2. 换版本时怎么校准（按优先级）

1. **抓一份对局流量**：B 机开代理，进入对局 30 秒，`/api/admin/capture/start` 落盘
   （端点已匿名化，只含净荷）。
2. **跑 `battle_proxy_selftest()`**：它会把 profile 的自洽性、金标准向量、转向修正
   一次性报给你；`ok=false` 说明位宽档需要改。
3. **看 `decode gate`**：如果 `packets_gated / packets_seen` 接近 0，说明分帧档不对
   → 调整 `packet_id_bits` / `ack_bits` / `channel_index_bits` / `bunch_variant`。
4. **看属性命中**：`property_blocks` 有值但 `moves=0` → `FRepMovement` 档不对
   （`location_bits`/`location_scale`/`has_base_bit`）。
5. **看实体数**：`entities` 正常但名字/队伍全空 → `handles` 名字表过期，
   需要重新导出 `channel_map.json`（用 `tools/catalogs.py` 在新版客户端上重抠）。
6. **标定地图**：进图后站在两个已知地标（如出生点、地图角落），
   记录雷达坐标与游戏坐标，解 `origin/scale`；朝向不对则改 `yaw_offset_deg/yaw_sign`。

## 3. 已知边界（不要对用户吹的）

* **容器内容物**：开箱前不下发（`randomized_container_contents_not_transmitted`），
  未开箱只能给位置。
* **无视野遮挡计算**：本工程不读场景几何，因此不做"墙后不显示"的判定，
  也不该宣称有。雷达只反映"服务端复制给我了什么"。
* **距离/朝向的精度**：受量化精度限制（位置 1 cm、旋转 0.0055°），
  远距离显示应做四舍五入。
* **观战/回放**：观战者视角的通道结构不同，需要单独校准。
* **明文传输**：局域网中 SOCKS5 与雷达页均无 TLS（样本原文也这么写），
  只在可信局域网用。
