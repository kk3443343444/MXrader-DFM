# MXrader-DFM

《三角洲行动》手游的 **iOS 局域网雷达接收器**——按样本
`iPhone-MXrader-r39-3D转向修正.ipa` 的结构 1:1 复刻的一份可编译工程。

* 逆向依据（样本每个结论的证据）：[docs/REVERSE_ENGINEERING.md](docs/REVERSE_ENGINEERING.md)
* **从零上手的步骤清单（含每步的通过标准）**：[docs/GETTING_STARTED.md](docs/GETTING_STARTED.md)
* 分层与数据流：[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
* 协议解码与版本校准：[docs/PROTOCOL.md](docs/PROTOCOL.md)
* 跨层契约（C ABI / JSON / HTTP / WS）：[docs/INTERFACES.md](docs/INTERFACES.md)

## 现在就能跑的两件事（不用 Mac、不用设备）

```bash
python scripts/preview_selftest.py                  # 19 项断言，验证前端 + WS 契约
python scripts/preview_web.py --open                # 假数据雷达预览：http://127.0.0.1:8770/battle.html
cd core && cargo test                               # 协议核心的 100+ 单元测试（纯 Rust，无需 C 工具链）
```

## 它怎么工作

```
B 机（跑游戏，开小火箭/Hiddify，必须启用 UDP 转发）
        │  SOCKS5 TCP + UDP  →  A 机 2025–2045
        ▼
A 机（本 app）  先转发、后解析 → UE Bunch 解码 → 坐标/朝向/血量/武器/弹道
        └─ 本机 http://127.0.0.1:<port>/battle.html  实时雷达（WKWebView 内嵌）
```

A 机不需要越狱、不读游戏内存、不注入进程：它只是一个**中间人代理 + 协议解码器**。
所有解密都在链路层完成（明文/LZ4/AES-ECB-XOR 逐包嗅探）。

## 目录

```
core/     Rust crate `battle_proxy`（静态库；样本同为静态链接，产物无 Frameworks/）
ios/      SwiftUI 壳 BattleReceiverOpen（11 个与样本同名的源文件 + C ABI 桥）
web/      雷达前端（自研 Leaflet 兼容引擎，零外网依赖）
scripts/  build_rust.sh / package_ipa.sh / verify_structure.sh / fetch_tiles.sh
tools/    逆向分析脚本（复现逆向报告用）
docs/     契约与文档
```

## 构建

> 需要 macOS + Xcode 15+（真机部署建议 Xcode 26 SDK，与样本一致）+ Rust 1.79+。

```bash
# 0) 一次性依赖
brew install xcodegen
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios

# 1) 编译 Rust 静态库（产出 core/target/ios/libbattle_proxy.a）
./scripts/build_rust.sh

# 2) 生成 Xcode 工程
cd ios && xcodegen generate && open BattleReceiverOpen.xcodeproj
#    在 Xcode 里选你的 Team，然后 Run（真机）

# 3) 或者直接出 IPA
./scripts/package_ipa.sh
#    产物：dist/BattleReceiverOpen-mxrader-r39.ipa，并自动做结构校验

# 4) 与参考样本对比结构（可选）
./scripts/verify_structure.sh dist/BattleReceiverOpen-mxrader-r39.ipa \
    "/path/to/① iPhone-MXrader-r39-3D转向修正.ipa"
```

只在 PC 上开发核心逻辑时（不需要 Xcode）：

```bash
cd core && cargo test          # 全部单元测试，含协议自检
cargo run --bin selftest -- --json   # 若加了 bin（当前用 C ABI 的 battle_proxy_selftest）
```

## 签名与侧载

样本 IPA **没有** `embedded.mobileprovision`，说明它由侧载工具注入凭据。本工程同样：

* 免费账号：AltStore / Sideloadly（7 天有效）
* 自签：`./scripts/package_ipa.sh` 后交给你的侧载工具
* 越狱设备：TrollStore 直接装（无需重签）

## 首次使用

1. A 机启动 app → 等待"端口已绑定，正在等待雷达页面" → 出图。
2. 打开 A 机的「连接游戏设备」：
   * 扫二维码把 Hiddify / sing-box 配置导到 B 机（`/api/socks5/hiddify.json`）；
   * 或手工把 B 机的 SOCKS5 指向 `socks5://<A机IP>:<端口>`，**并开启 UDP 转发**。
3. B 机进入对局 → 雷达上出现玩家点、朝向箭头、弹道与击杀条。
4. 左下「网络日志」可看计数器与性能（`milliseconds_since_last_server_transmission`），
   它也是判断"B 机代理没生效 vs 不在对局"的依据。

## HTTP 接口（本机）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/battle.html?brand=mx` | 雷达页 |
| GET | `/license` / `/license/status` | 卡密页 / 授权状态 |
| POST | `/license/activate` | `{"card":"..."}` |
| GET | `/api/status` | 完整状态 JSON |
| GET | `/api/socks5/hiddify.json` | Hiddify/sing-box 分享配置（含直连规则） |
| GET | `/ws` | WebSocket 状态流 |
| POST | `/api/admin/*` | **仅本机 + `X-Battle-Admin: <admin_token>`** |

管理动作：`diag` `loot` `session/reset` `announcement` `capture/start` `capture/stop`
`capture/download` `shutdown`。`read_only_radar=true` 时（默认）远端页面连看都不能改。

## 使用边界（工程事实，不是免责声明）

* SOCKS5 与雷达页面**都是明文**（样本原文亦如此）——只在可信局域网用，别把端口映射到公网。
* 抓包落盘默认关闭；开启后**端点强制匿名化**（`h:<hash>`）并且限时限量自动停。
* 雷达只能显示**服务端已经复制给你的信息**：未开箱容器内容物不在其中（协议层不下发）。
* 本工程不读场景几何，因此没有墙体遮挡判定。

## 验证状态（诚实清单）

| 项 | 状态 |
|---|---|
| 样本结构逆向（Mach-O/Info.plist/模块树/内嵌资产） | ✅ 完成，见 docs/REVERSE_ENGINEERING.md |
| `core/assets/channel_map.json`（77 通道 / 53 类名字表） | ✅ 从样本提取（83,709 B） |
| `core/assets/loot_ids.json`（物品 ID→资产名） | ✅ 从样本提取（122 条样例 + 词典展开逻辑） |
| `core/assets/battle_card.html`（激活页） | ✅ 从样本提取（3,135 B） |
| `ios/BattleReceiverOpen/Resources/MXIcon*`、`MXMark`、`BattleMark` | ✅ 从样本复制 |
| Rust 代码（模块树/逻辑/单元测试） | ✅ 已写完；⚠️ **本机无 Rust 工具链，未执行 `cargo test`** |
| Swift 代码（11 文件 + C ABI 桥） | ✅ 已写完；⚠️ **本机无 Xcode，未真机编译** |
| 端到端（真机 + 真实对局） | ❌ 未验证：需要 macOS 构建 + 两台设备 + 校准 `ProtocolProfile`/`maps.json` |

**校准顺序**（拿到真实流量后）：`ProtocolProfile` → `RepMovementProfile` →
`handles` 名字表 → `maps.json` 的 `origin/scale/yaw_offset_deg`。详见
[docs/PROTOCOL.md](docs/PROTOCOL.md) §2。
