# 逆向报告：`iPhone-MXrader-r39-3D转向修正.ipa`

> 本文是**复刻依据**。所有结论都来自对样本二进制的静态分析，工具在 `tools/`（本仓库自带）。
> 样本文件：`① iPhone-MXrader-r39-3D转向修正.ipa`（12,572,777 字节）。

## 1. 包结构

```
Payload/BattleReceiverOpen.app/
    BattleReceiverOpen            15,502,640 B   主二进制（唯一可执行文件）
    Info.plist                        1,786 B   二进制 plist
    PkgInfo                               8 B   "APPL????"
    MXIcon-20/29/40/60/76/83.5*.png             品牌图标（MX 系列）
    Icon-20/29/40/60/76/83.5*.png               备用图标系列
    MXMark.png                       44,362 B
    BattleMark.png                2,778,051 B
    battle-network-bg.png         1,967,864 B
```

**没有 `Frameworks/`、没有 `PlugIns/`、没有 `embedded.mobileprovision`、没有 `Data/`。**
即：不是 Unity/UE 游戏本体，是一个纯原生 app；而且 Rust 核心是**静态链接进主二进制**的
（否则必然出现 dylib 或 Frameworks 目录）。后者是本次复刻最关键的结构判断。

## 2. Info.plist 关键键

| 键 | 值 | 含义 |
|---|---|---|
| `CFBundleIdentifier` | `com.mxrader.monstervision` | — |
| `CFBundleExecutable` | `BattleReceiverOpen` | 与二进制同名 |
| `CFBundleDisplayName` | `MXrader 三角洲` | GBK 解码显示；实际为 UTF-8 中文 |
| `CFBundleShortVersionString` / `CFBundleVersion` | `2.3.7` / `32` | — |
| `BattleBrandVariant` | `mx` | 自定义品牌开关（同代码出多个马甲） |
| `MinimumOSVersion` | `16.0` | — |
| `DTXcode` / `DTSDKName` | `2630` / `iphoneos26.2` | 用 Xcode 26.3 构建 |
| `NSBonjourServices` | `_battleproxy._tcp` `_battleproxy._udp` | **局域网服务发现** |
| `NSLocalNetworkUsageDescription` | "…SOCKS5 TCP/UDP 隧道" | 明确写了中继协议 |
| `NSAppTransportSecurity` | `NSAllowsArbitraryLoads=true`, `NSAllowsLocalNetworking=true` | 要访问 `http://<lan-ip>` |
| `UIRequiredDeviceCapabilities` | `arm64` | — |
| `UIDeviceFamily` | `[1,2]` | iPhone + iPad |

## 3. 主二进制（Mach-O）

```
magic       MH_MAGIC_64
cputype     arm64 (0x0100000C)
filetype    MH_EXECUTE
ncmds       47
flags       0x00A18085 [NOUNDEFS, DYLDLINK, TWOLEVEL, WEAK_DEFINES, BINDS_TO_WEAK, PIE, HAS_TLV_DESCRIPTORS]
段          __PAGEZERO __TEXT(25 节) __DATA_CONST __DATA __LINKEDIT
rpath       /usr/lib/swift
加密        **无 LC_ENCRYPTION_INFO** → 已解密（可静态分析）
签名        LC_CODE_SIGNATURE，内嵌 entitlements: `get-task-allow = false`
符号        22,925 条
```

链接的系统库（说明技术栈）：

```
SwiftUI  Combine  WebKit  CoreImage  Security  CoreGraphics  CoreFoundation
UIKit  Foundation  libobjc  libc++  libSystem  + 一串 Swift overlay
```

**没有 `CFNetwork`/`Network.framework` 直连**，也没有任何第三方 SDK 框架 —— 网络能力全部来自 Rust 侧静态编译进去的 `tokio/hyper/axum/rustls`。

## 4. 代码组成：Swift 壳 + 静态 Rust 核心

### 4.1 Swift 侧（11 个源文件，符号表可读）

```
BattleReceiverApp.swift      BattleSplashView.swift     BattleTheme.swift
BattleWebView.swift          BonjourAdvertiser.swift    BrandMarkView.swift
DiagnosticsSheet.swift       PairingSheet.swift         ReceiverFailureView.swift
ReceiverRootView.swift       ReceiverStartupView.swift
```

Swift 类型名被**故意混淆**为 `X0B2`/`X0H8`/`X0W1`/`X1D8` 等（`_TtC18BattleReceiverOpen4X0B2`），
但成员名保留了语义，足以还原职责：

| 混淆名 | 真实职责 | 关键成员 |
|---|---|---|
| `X0A1` | `App` | `_model`, `body` |
| `X0B2` | 主 ViewModel | `phase` `endpoint` `runtime` `advertiser` `socksPort` `webURL` `webSessionToken` `start` `preflight` `beginHealthMonitoring` `enteredBackground` `updateSceneActivity` `webInterfaceDidFail` `retry` |
| `X0H8` | 服务句柄 | `dataDirectory` `mode` `socksPort` `runtimeHandle` `status` `start` `preflight` |
| `X0W1` | Bonjour 广播器 | `NSNetService` `services` `port` `start` `stop` |
| `X1D8` | `WKWebView` 包装 | `makeUIView` `updateUIView` `Coordinator` `sizeThatFits` |
| `Coordinator` | 导航/JS 桥 | `decidePolicyFor` `runJavaScriptAlertPanelWithMessage` `didFailProvisionalNavigation` `webViewWebContentProcessDidTerminate` |
| `X0E5` | 地址模型 | `socksURL` `radarURL` `radarDisplayAddress` `displayAddress` |
| `X0V0` | 局域网地址枚举 | `isPrivateIPv4` `privateIPv4Addresses` |
| `X0N1` | 联网权限探测 | `probeURL`（`https://www.baidu.com/`）`requestInternetAccessConfirmation` |
| `X0G7/X0G8/X0I9` | 状态 JSON 解码 | `CodingKeys` + `Decoder` |
| `X0F6` | 错误类型 | `LocalizedError`: `failureReason` `recoverySuggestion` `helpAnchor` |
| `X1G1` | 二维码 | `CIContext` + `UIImage` |
| `X1Q9` | 主题色 | `amber` `surface` `hairline` `secondaryText` `warning` |

### 4.2 Rust 侧（crate 名从 `__const` 里直接读出：`battle_proxy`）

从 `.cargo/registry` 路径与 `src/*.rs` 字符串可还原**完整模块树**：

```
src/lib.rs            src/state.rs           src/config.rs
src/ios_bridge.rs     src/announcement.rs    src/card_activation.rs
src/desktop_auth.rs   src/debug_monitor.rs
src/socks5/{handshake,relay}.rs
src/web/{embed,battle_view,ws}.rs
src/battle/{engine,session,combat,parse_queue,loot_catalog,
            protocol_capture,selftest,transport_crypto,
            udpxin,udpxin_entity,udpxin_identity,udpxin_move,
            udpxin_live,udpxin_exports}.rs
src/battle/codec/{bitstream,capture,character,container_collector,fire,killchain,s2c}.rs
```

依赖栈（从符号与字符串判定）：`tokio 1.52` `axum 0.8.9` `hyper 1.10` `tower 0.5`
`rustls 0.23 + aws-lc-rs 1.17` `reqwest 0.13` `tungstenite 0.29` `lz4_flex 0.11`
`aes 0.8 / cipher 0.4` `dashmap` `parking_lot` `arc-swap` `chrono` `serde_json`。

> Rust 源文件路径前缀是 `/Users/Admin/.cargo/registry/src/rsproxy.cn-…`，说明构建机是 macOS，
> 用户 `Admin`，并且用了国内镜像（rsproxy.cn）。

## 5. 内嵌资产（复刻时直接复用/参照）

### 5.1 `channel_map.json`（83,709 字节，已提取为 `core/assets/channel_map.json`）

这是**三角洲行动手游的 UE5 通道/属性名录**，也是整个雷达的知识底座：

* `channel_map`：77 条 `{slot, ch_index, class, parent_num, cmd_num}`
* `handles`：53 个类 → `{handle 号: 属性名}`，最大 660 条（`BP_DFMPlayerState_C`）

其中的类名直接证明游戏引擎与关键属性：

```
BP_DFMCharacter_C              188 props  ReplicatedMovement(7) RemoteViewPitch(17)
                                          MyGUIDValue(94) CharacterRotation(121) TeamID(184)
BP_DFMPlayerState_C            660 props  TeamID(62) Camp(63) HeroId(137) bIsDeadBox(129)
                                          RankMatchScore(142) CurrentCharacterLiveStatus(127)
BP_DFMPlayerController_C        48 props  PlayerState(17) Pawn(18)
BP_WeaponMeleeNoModular_C       92 props  CharacterOwner(41) ServerWeaponIdentity(40)
DFMContainerDataCollector / DFMSceneActorReplicator / MovementReplicationActor …
```

### 5.2 物资名录（压缩词典）

样本里以"词典前缀 + Term 引用"形式内嵌数万条：

```
SOL_DT_Basic_AR_low_[Term#18010000014_ShortName]_14
SOL_DT_Basic_SMG_low_[Term#10020000013_ShortName]_1
{"10010000903":"SOL_DT_Basic_AR_low_M16A4_14","10010000904":"…M16A4_15",…}
```

即 `[Term#<id>_ShortName]` 是占位符，展开后为武器名；ID 体系形如 `100_1_0000903`（大类-子类-序号）。

### 5.3 协议金标准向量

```
request  KCgzACsIBBABGI6625ihk/H+ygEgcyoUMTg0MTkxODc2NjQwNDMzMjM4MjAwBjgAGkiVfUOQ
response KoiKKjbytVEADA==
response ggqNuX9aEbEADA==
response MyqKHh8QLkIADA==
```

已内嵌进 `core/src/battle/selftest.rs` 作为回归向量（只含 SOCKS5 头与极小应用层净荷，无账号信息）。

### 5.4 卡密激活页（HTML/CSS/JS）

二进制里有一张**完整明文**的激活页（已提取为 `core/assets/battle_card.html`，3,135 字节）：

* 标题"卡密激活"，输入框 + `fetch('/license/activate')`
* 打开时轮询 `fetch('/license/status')`，`j.authorized` 为真则 `location.href='/'`
* 文案：`请输入卡密` / `卡密验证通过，进入雷达` / `未激活` / `已激活，正在进入雷达…` / `网络异常，请重试`
* `<html data-battle-ready="1">` 与一个 1×1 的 `.leaflet-container` 探针层

**`.leaflet-container` 是雷达前端的就绪标志** —— 说明真正的雷达页面用 Leaflet 渲染，
且 iOS 壳会执行 JS 探针：

```js
(() => document.documentElement.dataset.battleReady === '1'
  && Boolean(document.querySelector('#app')?.childElementCount)
  && Boolean(document.querySelector('.leaflet-container')))()
```

雷达页面本身（`/battle.html`）**不在二进制里**：Rust 侧有
`runtime resource capability is unavailable` / `runtime resource authentication failed` /
`runtime entry document is not UTF-8` 三条错误，说明前端包是**带鉴权的"运行时资源"**，
可远端下发。复刻时我们把前端放在 `web/` 并内嵌，省掉这一层依赖。

## 6. 运行模型（从字符串还原）

```
B 机（跑《三角洲行动》手游）                 A 机（本 app）
┌──────────────────┐   SOCKS5 + UDP   ┌────────────────────────────────┐
│ 游戏 UE5 客户端   │ ───────────────▶ │ socks5::run                    │
│ 小火箭/Hiddify   │  TCP 2025–2045   │  ├ TCP CONNECT                 │
│ 必须开启 UDP 转发 │                  │  └ UDP ASSOCIATE               │
└──────────────────┘                  │       │ 先转发，后解析（tap）  │
                                       │       ▼                        │
                                       │ battle::engine                │
                                       │  transport_crypto(AES/LZ4)    │
                                       │  udpxin → codec → combat      │
                                       │       │ radar snapshot         │
                                       │       ▼                        │
                                       │ web::serve  /battle.html +/ws │
                                       └────────────────────────────────┘
```

支撑这个模型的原字符串：

* `3SOCKS5 plaintext endpoint listening on TCP 0.0.0.0:` / `] UDP ASSOCIATE on SOCKS port `
* `_battleproxy._tcp.` / `_battleproxy._udp.`（Bonjour 让 B 机自动发现 A 机）
* `小火箭必须启用 UDP 转发` / `全局 / TUN（服务器直连）` / `必须开启（多流）` / `每源端口独立 NAT · 双向并发`
* `先转发，后解析` / `先放行后解析`
* `&/api/socks5/hiddify.json`、`profile-title`、`profile-update-interval`、
  `Hiddify iOS must use VPN/TUN service mode`、`BATTLE SOCKS5 GLOBAL UDP`
* `SOCKS5 端口（2025–2045 至少一个可用）`
* `普通 SOCKS5 与局域网雷达网页均不提供传输加密，只应在可信局域网中使用。远程雷达页面为只读…`

### 端口与会话策略（配置键）

`primary_port` `udp_relay_mode=per_association_ephemeral` `udp_nat_mapping=per_client_endpoint_isolated`
`udp_concurrent_associations` `udp_same_port` `session_model=one_port_one_player`
`parser_async` `read_only_radar` `loot_parsing_enabled` `collection_policy=enabled_only`
`server_decode_gate` `transport` `authentication` `encryption`。

### HTTP 路由（axum）

```
/                       302 → /battle.html?brand=mx
/battle.html            雷达页面（运行时资源）
/license                卡密激活页（内嵌）
/license/activate       卡密校验
/license/status         授权状态
/api/status             状态 JSON
/api/socks5/hiddify.json  Hiddify/sing-box 分享配置
/ws                     WebSocket 状态流（subscribe/unsubscribe）
/api/admin/*            仅本机 + BATTLE_ADMIN_TOKEN
    diag / loot / session/reset / announcement
    capture/start|stop|download / shutdown
```

管理接口的拒绝串（逐字取自样本）：
`remote diagnostics control requires BATTLE_ADMIN_TOKEN`、
`remote loot parser control requires BATTLE_ADMIN_TOKEN`、
`remote protocol capture control requires BATTLE_ADMIN_TOKEN`、
`remote protocol capture download requires BATTLE_ADMIN_TOKEN`、
`no capture data available`、`application/x-ndjson; charset=utf-8`、
`已关闭`/`已开启`/`cached_loot_cleared`/`battle engine unavailable`。

## 7. 解析链（协议层的全部证据）

### 7.1 传输保护

```
UE FAES::EncryptData（异或 + AES-256-ECB）  ← aes-0.8.4 被链进
LZ4 块压缩                                  ← lz4_flex-0.11.6 被链进
```

### 7.2 UE 网络日志串（能直接读出解析器的分支）

```
Bunch payload exceeds packet at            partial repacketization unavailable
MaxPacket=                                 invalid_packet_framing
unresolved_bunch_header_variant            movement RepLayout not exactly closed
not a RepLayout Actor ContentBlock         closure marker truncated
[slot_map] ready: slots=                   [slot_map] updated: matched
[combat] discovered weapon_object=         [combat] discovered weapon_channel=
[combat] CharacterMesh0: actor=            [combat_fallback] weapon via GUID: channel=
[combat] rejected packet growth: bytes     weapon_ch=   mesh=
[aim_parse] view scan #                    (BP_Weapon upstream fire)
udp_packets_up / udp_packets_down / udp_invalid_packets / tcp_relay_failures
```

### 7.3 位移与弹道（本文件名里的"3D 转向修正"）

```
RemoteViewPitch                                  俯仰（uint8 压缩）
ReplicatedMovement / RepMovement                 位置 + 朝向 + 速度
CharacterRotation / TargetRotation / LookingRotation
LatestMovePackage / MoveHandle                   索引化位移（判复活）
authoritative_indexed_movement_after_recoverable_death
indexed_movement_proves_revival_after_dead
move_after_recoverable_death / current_local_char_move_proves_revival_after_dead
battle_fire_cli 1.3.0 / sub_101877D34
ServerProcessWeaponEventDataForFirer             ← 开火 RPC
fire_rotation owner_velocity_m_s initial_projectile_velocity_m_s direction_unit
ballistic_formula v = forward(FireRotation)*InitSpeed + OwnerVelocity; origin=SpawnLocation
FWeaponFireInfo.InitialSpeedQ100                 ← 初速定点 1/100
bullet_array_start_bit full_struct_end_candidate_bit variable_payload_region
ballistic_trajectory_complete / partial_raw_preserved / no_projectile_and_fire_move_exact
raw_preserved_not_required_for_ballistic_reconstruction
```

### 7.4 击杀链（伤害类型枚举完整内嵌，含游戏自身的拼写错误）

```
EKilledByWeapon  EkilledBySelf  EkilledByPoisonGas  EKilledFallDown
EKilledFromImpendingDeath  EKilledFromBuff  EKilledByGm  EKilledFromEnvExplosion
EKilledByVehicleWeapon  EKilledByAssassinateDamage  EKilledByBattleFieldSupportSkill
EKilledBySectorArtilerrateSkill   ← 游戏内拼写就是 Artilerrate
EKilledByGuidedMissleSkill        ← 游戏内拼写就是 Missle
invalid zero-length kill array
```

### 7.5 物资的诚实边界

```
DFMContainerDataCollector  collector_slots  contents_status
randomized_container_contents_not_transmitted   ← 开箱前内容物不下发
runtime_loot_observed  loot_catalog  loot_parsing_enabled  loot_payloads_skipped
```

→ 雷达只能给**容器位置**与**已开箱容器的真实内容**；"未开箱看内容"在协议层不成立。

## 8. 复刻工程的对应关系

| 样本 | 本工程 |
|---|---|
| `Payload/BattleReceiverOpen.app` | `ios/BattleReceiverOpen/` + XcodeGen `project.yml` |
| Swift 11 文件（混淆名） | `ios/BattleReceiverOpen/*.swift`（同名，语义名恢复） |
| Rust crate `battle_proxy` | `core/`（同模块树、同文件名） |
| 内嵌 `channel_map.json` | `core/assets/channel_map.json`（原样提取） |
| 内嵌激活页 | `core/assets/battle_card.html`（原样提取） |
| 运行时资源下发（远端雷达页） | `web/`（本地内嵌，去掉了远端鉴权依赖） |
| `MXIcon*` / `MXMark` / `BattleMark` | `ios/BattleReceiverOpen/Resources/`（原样复制） |
| 静态链接 Rust（无 Frameworks） | `scripts/build_rust.sh` 产 `libbattle_proxy.a` 静态链接 |
| `LC_UUID 697E6A61…` | 重新构建自然生成新 UUID |

## 9. 复刻工具链（`tools/`，本次分析实际使用）

```bash
python tools/macho_scan.py  <binary>        # 头 + 加载命令 + 加密信息 + entitlements
python tools/macho_dump.py  <binary> out.json   # 全量符号 + 各节字符串
python tools/swift_types.py out.json        # 还原 Swift 类型/成员（启发式 demangle）
python tools/modules.py     strings.txt     # 还原 crate 模块树 + 日志标签
python tools/raw_strings.py <binary> outdir # 长块提取（内嵌 JSON/HTML）
python tools/catalogs.py    <binary> outdir # 抠出 channel_map.json
python tools/find_web.py    <binary> outdir # 定位内嵌 HTML / 探测压缩流
```

复现整份报告：

```bash
python tools/macho_scan.py  "Payload/BattleReceiverOpen.app/BattleReceiverOpen"
python tools/macho_dump.py  "Payload/BattleReceiverOpen.app/BattleReceiverOpen" build/dump.json
python tools/raw_strings.py "Payload/BattleReceiverOpen.app/BattleReceiverOpen" build/
python tools/modules.py     build/allstrings.txt
python tools/catalogs.py    "Payload/BattleReceiverOpen.app/BattleReceiverOpen" build/
```
