# BattleReceiverOpen — iOS 壳工程（MXrader 三角洲）

局域网雷达接收器。Rust 静态库 `libbattle_proxy.a` 通过 C ABI（`docs/INTERFACES.md` §1）暴露接收器，
SwiftUI 壳负责启动它、把本机雷达页面放进 `WKWebView`，并展示配对信息，
让第二台设备（跑游戏的那台）把小火箭 / Hiddify 的 SOCKS5 代理与 UDP 中继指向本机。

- Bundle ID：`com.mxrader.monstervision`
- 显示名：`MXrader 三角洲`
- 版本：`2.3.7` (build `32`)
- 最低系统：iOS 16.0，Swift 5.9，纯 Apple 框架（无第三方 SPM）
- 方向：竖屏 + 横屏；设备：iPhone + iPad；界面语言：简体中文

## 1. 工程文件由 XcodeGen 生成

`BattleReceiverOpen.xcodeproj` **不是手写的**，它由 `project.yml` 生成：

```bash
cd ios
xcodegen generate
open BattleReceiverOpen.xcodeproj
```

任何工程设置的改动（新增源文件、链接参数、Build Phase）都改 `project.yml` 后重新生成；
不要直接编辑 `.xcodeproj/project.pbxproj`，重新生成会覆盖。

`project.yml` 里已经写好：

| 设置 | 值 |
|---|---|
| `INFOPLIST_FILE` | `BattleReceiverOpen/Info.plist`（`GENERATE_INFOPLIST_FILE=NO`） |
| `CODE_SIGN_ENTITLEMENTS` | `BattleReceiverOpen/BattleReceiverOpen.entitlements` |
| `SWIFT_OBJC_BRIDGING_HEADER` | `BattleReceiverOpen/Bridge/BattleReceiverOpen-Bridging-Header.h` |
| `IPHONEOS_DEPLOYMENT_TARGET` / `ENABLE_BITCODE` | `16.0` / `NO` |
| `CODE_SIGN_STYLE` / `DEVELOPMENT_TEAM` | `Automatic` / `""` |
| `OTHER_LDFLAGS` | `-lc++ -lresolv -framework Security -framework Network -framework WebKit -framework CoreImage` |
| `TARGETED_DEVICE_FAMILY` | `1,2` |

## 2. Rust 静态库

`project.yml` 声明了名为 **Build Rust static library** 的 pre-build script phase：

```bash
cd "${SRCROOT}"            # = ios/
../scripts/build_rust.sh   # 必须在，且可执行
../core/target/ios/libbattle_proxy.a   # 期望产物
```

`libbattle_proxy.a` 同时作为 `framework` 依赖被链接（`embed: false`），
系统库 `libc++.tbd`、`libresolv.tbd`、`Security.framework`、`Network.framework`、
`WebKit.framework`、`CoreImage.framework` 通过 `dependencies` 声明。

C 头文件通过 `HEADER_SEARCH_PATHS` 解析：`BattleReceiverOpen/Bridge`（本仓库自带副本）、
`../core/include`、`../core`。**假设**：`scripts/build_rust.sh` 会把与
`BattleReceiverOpen/Bridge/battle_proxy.h` 一致的 `battle_proxy.h` 放到 `../core/include`
（或直接沿用仓库内的副本）；如果 Rust 工程的头文件在别处，请把该目录加进 `HEADER_SEARCH_PATHS`。

## 3. 目录结构

```
ios/
  project.yml                          # XcodeGen 规格（唯一工程事实源）
  README.md                            # 本文件
  BattleReceiverOpen/
    BattleReceiverApp.swift            # @main + scenePhase 注入
    ReceiverStartupView.swift          # 启动页（5 秒预算 + 状态文案）
    ReceiverRootView.swift             # 主界面（WKWebView + 工具栏 + 弹窗）
    ReceiverFailureView.swift          # 失败页 + 重试 + 原始错误
    BattleWebView.swift                # UIViewRepresentable(WKWebView) + 就绪探针
    BattleSplashView.swift             # 启动画面组合（logo / 名称 / 信号弧）
    BrandMarkView.swift                # 品牌标识（位图优先，矢量兜底）
    BattleTheme.swift                  # 颜色 / 卡片 / 字体 / 渐变
    PairingSheet.swift                 # B 机配对（地址、复制、二维码、清单）
    DiagnosticsSheet.swift             # 状态 JSON、计数器、管理操作
    BonjourAdvertiser.swift            # _battleproxy._tcp / _udp 广播
    ReceiverModel.swift                # ObservableObject（原 X0B2）
    Info.plist                         # BattleBrandVariant=mx 等
    BattleReceiverOpen.entitlements
    Bridge/
      battle_proxy.h                   # C ABI（与 INTERFACES.md §1 一致）
      BattleReceiverOpen-Bridging-Header.h
      BattleBridge.swift               # Swift 封装 + Codable 模型
```

## 4. 还需要补的资源（不在本次改动范围）

1. **App 图标**：`Info.plist` 引用了 `MXIcon-60/76/83.5/40/29/20`（另含
   `CFBundleIcons` / `CFBundleIcons~ipad`）。请把对应 PNG 放进
   `BattleReceiverOpen/Resources/`（或资产目录）并确保随 target 拷贝，
   否则图标名会指向不存在的资源（编译通过、安装后无图标）。
2. **品牌位图**：`BrandMarkView` 优先使用 `MXMark.png` / `BattleMark.png`，
   找不到时使用纯 SwiftUI 矢量标识，因此缺图不影响编译与运行。
3. **雷达前端**：`web/`（`index.html` + `radar.js` + `style.css`）由 Rust 内嵌并在
   `web_port` 上提供；iOS 侧只做加载与就绪探针，不打包前端。

## 5. 权限与系统能力

- `NSLocalNetworkUsageDescription`：中文说明，用于 2025–2045 端口段的 SOCKS5(TCP)+UDP
  中继，以及局域网广播/发现。
- `NSBonjourServices`：`_battleproxy._tcp`、`_battleproxy._udp`。
- `NSAppTransportSecurity`：`NSAllowsArbitraryLoads=true` + `NSAllowsLocalNetworking=true`
  （本机 `http://127.0.0.1` 与局域网 `http://<lan-ip>:<port>` 均为明文，无 TLS）。
- Entitlements：`get-task-allow=false`、`application-identifier`、`keychain-access-groups`、
  `com.apple.developer.networking.multicast`（iOS 14+ 的 Bonjour/mDNS 必需，需向 Apple 申请）、
  `com.apple.developer.networking.wifi-info`。

## 6. 手动验证清单（真机）

1. 首次启动弹出「本地网络」权限 → 允许。
2. 启动页在 ≤5 秒内出现雷达地图；超时才会进入失败页。
3. 主界面左上 ⋯ →「配对信息」：看到 `socks5://<lan-ip>:<port>`、
   `http://<lan-ip>:<port>/battle.html?brand=mx`、以及 Hiddify 配置二维码。
4. 主界面左上 ⋯ →「诊断信息」：计数器随 B 机流量增长，可复制完整状态 JSON。
5. 杀掉重复启动：连续点「重新启动接收服务」不应出现端口冲突（端口段 2025–2045 自动换）。
6. 旋转设备：竖屏/横屏均铺满；iPad 分屏下 `UIRequiresFullScreen=false` 生效。
