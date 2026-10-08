# 架构：MXrader-DFM 复刻工程

```
MXrader-DFM/
├── core/            Rust crate `battle_proxy`（静态库，链接进 iOS 主二进制）
│   ├── src/         与样本同名的模块树
│   ├── assets/      channel_map.json（77 通道 / 53 类名字表）、battle_card.html
│   └── include/     battle_proxy.h（C ABI，Swift 唯一入口）
├── ios/             SwiftUI 壳 BattleReceiverOpen（Bundle ID com.mxrader.monstervision）
│   └── BattleReceiverOpen/Resources/   MXIcon*/MXMark/BattleMark（与样本一致）
├── web/             雷达前端（Leaflet 兼容自研引擎，零外网依赖）
├── scripts/         build_rust.sh / package_ipa.sh / verify_structure.sh / fetch_tiles.sh
├── tools/           逆向分析脚本（本工程自带，可复现 docs/REVERSE_ENGINEERING.md）
└── docs/            INTERFACES.md（契约）/ REVERSE_ENGINEERING.md / PROTOCOL.md / 本文
```

## 1. 分层与职责

| 层 | 单元 | 职责 | 关键约束 |
|---|---|---|---|
| L0 | `ios/` SwiftUI | 启动接收器、探针加载本地雷达页、配对（QR/复制地址）、诊断 | 不碰网络，只调 7 个 C 符号 |
| L1 | `core/src/ios_bridge.rs` | C ABI + 全局单例 + 端口选择 + 5 秒启动预算 | 返回串全部堆分配，`free_string` 释放 |
| L2 | `core/src/socks5/*` | TCP CONNECT + UDP ASSOCIATE，双监听同端口 | **先转发后解析**；永不阻塞网络任务 |
| L3 | `core/src/battle/parse_queue.rs` | 有界队列 + N worker + ordered-apply + watchdog | 队满即丢最旧，绝不反压转发 |
| L4 | `core/src/battle/{transport_crypto,udpxin,codec}` | 解密解压 → UE 分帧 → RepLayout 属性 | 纯 CPU、无锁、不 panic、失败保留原始字节 |
| L5 | `core/src/battle/{session,combat,udpxin_*}` | 一设备一会话的实体/身份/生死/弹道状态 | 会话间完全隔离 |
| L6 | `core/src/web/*` | axum：雷达页 / WS 状态流 / 分享配置 / 管理接口 | 管理接口 = 仅本机 + 令牌 + 只读闸门 |

## 2. 一次数据报的完整旅程

```
① socks5/relay 读到 B 机的 UDP 包
   ├─ 先原样转发到真实服务器（延迟不受解析影响）
   └─ 同时 engine.feed(session, src, dst, payload, now_ms)   ← 同步、非阻塞
② parse_queue：try_send；满则丢弃并计数（丢最旧语义）
③ worker（spawn battle-parse-<n>）
   a. transport_crypto::decode_packet 逐包嗅探
      Plain → LZ4 → AES-256-ECB-XOR → AES+LZ4 → XorStream
      validator = framer.validate_datagram（decode gate）
   b. udpxin::PacketFramer::frame  → 包头 + Bunch 序列（partial 跨包重组）
   c. codec::character::read_property_block
      按 handle 升序遍历；初始包无存在位，增量包每属性 1 位存在位
      名字即类型（`[field_infer]`）：bXxx→Bool、*Location→Vector、*Rotation→Rotator…
   d. 产出 Vec<EngineUpdate>
④ ordered-apply（spawn battle-ordered-apply）按 seq 单调应用；
   watchdog（spawn battle-apply-watchdog）10 秒无进展即告警
⑤ SessionState 更新
   entities（通道表）/ identities（GUID↔玩家）/ liveness（四态生死）
   containers（容器与已开箱内容）/ kills（击杀链）/ combat（武器、交战、护栏）
⑥ 雷达广播：dirty 标记 + 20 Hz 合并 → web::battle_view 换算成 SI 单位 → 广播给 WS
```

## 3. 转向修正（"3D 转向修正"的本体）

UE 的 `FRotator` 与雷达地图的"北"不是一回事，三层修正合起来才算对：

```rust
// core/src/battle/udpxin_move.rs
heading = ue_yaw * yaw_sign + yaw_offset_deg        // 轴向与镜像
if follow_heading { heading -= self_heading }        // 地图随自机旋转
length  = 1 - pitch_gain * (1 - cos(pitch))          // 俯仰透视压缩
```

* **单一事实源**：`web/maps.json` 是标定值的唯一出处，core 启动时读它
  （`RotationCorrection::from_maps_json`），前端也从它读。三层默认值全部是
  **恒等映射**（`yaw_offset_deg = 0`、`yaw_sign = +1`）——这样即使某一层漏读配置，
  也只是"没修正"，而不会变成"修正两次"（朝向偏 2 倍，是这套东西最容易踩的坑）。
* 服务端若下发**未修正的 UE 原始 yaw**，把 `maps.json` 改成
  `yaw_offset_deg = -90`、`yaw_sign = -1`（等价于代码里的
  `RotationCorrection::ue_raw()`）：UE yaw 0（+X）→ 270°（西）。
* 输出直接是"游戏里的朝向"，前端不做二次换算；前端只负责渲染与"跟随朝向"时的
  地图旋转。
* 自检对**两档**都做了断言（恒等：`0°→0°`、`90°→90°`；UE 原始：`0°→270°`、`90°→180°`），
  参数漂移会被 `battle_proxy_selftest()` 抓到。诊断页会实时显示当前生效的
  `yaw_offset_deg`/`yaw_sign`，方便现场标定。

## 4. 状态与契约

* Swift ↔ Rust：`docs/INTERFACES.md` §1/§2/§3（C ABI、配置 JSON、状态 JSON）。
* Rust ↔ 前端：§4 路由 + §5 WS 消息（`hello`/`state`/`diag`/`bye`）+ 玩家条目字段。
* 前端就绪探针：§6 三条（`data-battleReady`、`#app` 有子节点、存在 `.leaflet-container`）。

任何一层换实现，只要这三份契约不变，其它层不用动。

## 5. 失败模式与对应设计

| 失败 | 表现 | 设计对策 |
|---|---|---|
| 端口被占用 | 启动失败 | 2025–2045 逐个试；失败文案与样本一致（含"已自动尝试 2025–2045"） |
| B 机没开 UDP 转发 | 雷达空白 | `debug_monitor` 判 `milliseconds_since_last_server_transmission`，给中文提示 |
| 游戏版本漂移 | 解析全废 | `decode gate` 保守拒绝 + `selftest` 的 profile 回归 + 大量诊断计数器 |
| 错位解析炸出假实体 | 雷达被刷爆 | `combat::GrowthGuard`（`rejected packet growth`）整批丢弃 |
| 解析卡住 | 延迟上升 | 有界队列丢最旧 + watchdog 告警；转发路径永不等解析 |
| 恶意/损坏包 | CPU 打满 | 处处有 `bit_limit` / `max_properties` / `max_kills` / `max_projectiles` 上限 |
| 隐私 | 抓包落盘泄露 | 端点强制匿名化（`h:<hash>`），限时限量自动停 |

## 6. 未实现 / 需要你补的部分（诚实清单）

| 项 | 状态 | 说明 |
|---|---|---|
| `channel_map.json` | ✅ 已从样本提取 | 但游戏更新后会变，需要重新抓 `ch_index/handles` |
| `ProtocoProfile` 位宽 | ⚠️ 保守默认 | `packet_id_bits=0`（走 SerializeInt(1023)）等参数需按你的实测抓包校准 |
| `RepMovement` 布局 | ⚠️ 参数化 | 已覆盖 3 种变体；具体哪一版对，要用真实对局确定 |
| 地图底图瓦片 | ❌ 需自备 | `scripts/fetch_tiles.sh` 只给框架，不替你选源 |
| `maps.json` 标定 | ❌ 需实测 | 每张图 2 个已知地标即可标出 `origin/scale/yaw_offset_deg` |
| 卡密服务端 | ❌ 占位 | `card.activation_url` 默认 `https://license.invalid/...`；离线宽限逻辑已就绪 |
| 远端运行时资源 | ⚠️ 已留接口 | `desktop_auth::resolve_runtime_resource`；iOS 走内嵌 `web/` |
| 真机编译 | ❌ 未验证 | 本机（Windows）无 Rust/Swift/Xcode 工具链；见 README "验证状态" |
