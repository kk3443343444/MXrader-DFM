# 参考实现逆向报告：`MyRader6Pro.ipa`（MyRader Pro 5.1.58）

> 样本：`MyRader6Pro.ipa`（1,418,000 B，WeChat 收件目录 2026-10）
> 产物目录：`reference/myraderpro/`（纯文本字符串/符号清单）
> 分析工具：本仓库 `tools/`（`macho_scan.py` / `macho_dump.py`）+ `dist/` 下临时脚本（已列出，可删）
> 所有结论后面都带**文件偏移或字符串原文**。分不清的地方写 **未确认**。

---

## 0. 一句话结论

这是**同一条产品线（三角洲行动雷达接收端）的另一套实现，且是"云化"版本**：

| | r39 (`BattleReceiverOpen`) | 本样本 (`DeltaRadarIOS`) |
|---|---|---|
| Rust crate | `battle_proxy` | **`delta_radar_edge`**（`delta-edge`）|
| 解析位置 | **本地**（UE bunch / RepLayout / FAES 全在本机解）| **云端**（本机只做镜像上报，见 §4.2）|
| 内嵌知识库 | `channel_map.json`(83 KB) + `loot_ids.json` + 激活页 HTML | **一个都没有**（§5）|
| 本地 Web 服务 | 有（axum：`/battle.html` `/ws` `/api/*`）| **没有**（无 axum/hyper/reqwest）|
| 授权 | 本地卡密页 | **云账号 + 卡密 + `receiver-sessions`** |
| 构建环境 | `/Users/Admin/.cargo/...rsproxy.cn-...` | `/Users/mac/.cargo/...index.crates.io-...` |
| 游戏协议档位 | bunch/RepLayout profile | **TGCP + UDP_C**（完全不同，零重叠，见 §6）|

**两者没有可复用的代码或协议常量**：r39 的 `Bunch payload exceeds packet at`、`unresolved_bunch_header_variant`、
`movement RepLayout not exactly closed`、`MaxPacket=`、`[slot_map]`、`[combat]`、`RemoteViewPitch`、
`SOL_DT`、`BP_DFM` 在本样本中命中数**全部为 0**。

---

## 1. 结构（Mach-O 事实）

### 1.1 包内容（注意：第一遍扫描说的"只有 3 个文件"不准）

```
Payload/MyRaderPro.app/
    DeltaRadarIOS                 2,624,688 B   主二进制（唯一可执行文件）
    Info.plist                        1,533 B
    Assets.car                      102,712 B   仅 AppIcon-1024.png（见 §5.3）
    AppIcon60x60@2x.png              14,571 B
    AppIcon76x76@2x~ipad.png         20,909 B
    PkgInfo                               8 B   "APPL????"
```

没有 `Frameworks/`、`PlugIns/`、`embedded.mobileprovision`、`Data/`、**没有 `.lproj/`**。

### 1.2 Mach-O

```
magic       MH_MAGIC_64
cputype     arm64 (0x0100000c)   cpusubtype 0x00000000
filetype    MH_EXECUTE
ncmds       54      sizeofcmds 6656
flags       0x00a18085 [NOUNDEFS,DYLDLINK,TWOLEVEL,WEAK_DEFINES,BINDS_TO_WEAK,PIE,HAS_TLV_DESCRIPTORS]
段          __PAGEZERO __TEXT(27 节, 2,293,760 B) __DATA_CONST(98,304) __DATA(32,768) __LINKEDIT(199,856)
LC_UUID     4509E432-D17C-3F55-855B-2C4B8CC6B5F2
LC_MAIN     entryoff 0x272f80
加密        无 LC_ENCRYPTION_INFO → 未加密，可静态分析
签名        LC_CODE_SIGNATURE
符号表      933 条（r39 有 22,925 条 → **已被 strip**）
rpath       /usr/lib/swift, @executable_path/Frameworks（后者无对应目录）
```

链接的系统库（=技术栈）：

```
SwiftUI  Combine  WebKit  UIKit  Foundation  CoreFoundation  CoreGraphics
CoreImage  Security  SystemConfiguration  AVFAudio  libc++  libobjc  libz  libSystem
+ Swift overlays: libswift{Core,Foundation,Dispatch,os,_Concurrency,AVFoundation,
  CoreAudio,CoreImage,CoreMedia,CoreMIDI,Metal,OSLog,ObjectiveC,QuartzCore,Spatial,
  UniformTypeIdentifiers,XPC,Darwin,simd,UIKit}
```

**关键结构判断**

* `AVFAudio` 是**直接链接**（非 weak），配合 `Info.plist UIBackgroundModes=[audio]` → 后台保活手段（§4.3）。
* **没有** `Network.framework`、**没有** `CFNetwork` 直连、**没有**任何第三方 SDK → 全部网络能力来自静态链接进主二进制的 Rust。
* 静态链接证据：包内无 `Frameworks/`、无额外 dylib，但二进制里有完整 tokio/rustls/tungstenite 家族与 `delta_radar_edge` 的 mangled 符号。
* **Swift/Rust 共存的直接证据**（同一文件内）：
  * Rust 侧：`/Users/mac/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tokio-1.53.1/src/net/tcp/listener.rs`（0x1c55xx 一带，`__TEXT,__cstring`）
  * Swift 侧：`DeltaRadarIOS/ContentView.swift` 等 `#file` 字面量（0x211770 起）
  * 桥接面：`struct EdgeConfig with 8 elements`（0x1ab54a，serde derive 报错串）+ 8 个字段名（0x1ab4db 起，见 §4.2）

### 1.3 Info.plist 关键键（原文）

| 键 | 值 | 备注 |
|---|---|---|
| `CFBundleIdentifier` | `com.deltaradar.pro` | |
| `CFBundleExecutable` | `DeltaRadarIOS` | |
| `CFBundleDisplayName` | `MyRader Pro` | |
| `CFBundleShortVersionString` / `CFBundleVersion` | `5.1.58` / `5158` | |
| `MinimumOSVersion` | `16.0` | |
| `DTXcode` / `DTSDKName` | `2660` / `iphoneos26.5` | Xcode 26.6 |
| `BuildMachineOSBuild` | `25F80` | |
| `NSBonjourServices` | `["_deltaradar._tcp"]` | **只有 TCP 一项**（r39 是 `_battleproxy._tcp` + `_udp`）|
| `NSLocalNetworkUsageDescription` | `用于在同一 Wi-Fi 内接收主手机的 SOCKS5 TCP/UDP 流量并直接转发。` | 官方自己写明"SOCKS5 TCP/UDP" |
| `UIBackgroundModes` | `["audio"]` | 后台保活 |
| `UIRequiresFullScreen` | `true` | |
| `UIDeviceFamily` | `[1,2]` | |
| `NSAppTransportSecurity` | **不存在** | 与 r39（任意加载）不同，只走 HTTPS |
| `DRAPIBaseURL` | `https://103.215.81.113` | 备用/默认服务端 |
| `DRProductName` / `DRProMode` | `MyRader Pro` / `true` | 多马甲开关（r39 是 `BattleBrandVariant`）|
| `UILaunchScreen` | `{}` | |

> **两个服务端地址并存**：`Info.plist` 的 `DRAPIBaseURL=https://103.215.81.113`，而二进制里硬编码 `https://122.10.118.52`（0x212940）。
> Swift 侧还有错误串 `自有服务器 HTTPS 地址无效`（0x212e30）、`服务器地址已变化，请重新登录`（0x212de0）、
> `ReceiverAccountService.invalidServer / serverChanged` → 说明**服务端地址是运行期可变**（服务端下发或用户配置）。
> 这解释了为什么用户在不同时间看到不同"服务器"。

---

## 2. UI 结构还原（重点）

### 2.1 先说清楚：哪些文案在二进制里、哪些不在

* 本二进制 `__TEXT,__cstring` 共 **670** 条；其中**中文可见文案 94 条**，全部** UTF-8 明文**，
  **没有任何混淆/加密**（判断依据：直接 `grep` 明文命中；且无 `lproj`、无 `Localizable.strings`、
  无 `NSLocalizedString`、无 `_swift5_fieldmd` 条目 → 文案是编译期字面量，不是本地化表）。
* 用户提到的这批字串，**只有一部分在本 build 里**。逐条核对（✓=本二进制存在，✗=不存在）：

| 用户看到的文案 | 本 build | 证据 |
|---|---|---|
| 接收状态 | ✗ | 全量 CJK 扫描无此串 |
| 内置雷达 | ✗ | 无（有类型 `EmbeddedRadarBrowser` 0x1a81c0）|
| 连接节点 | ✗ | 无（有类型 `ReceiverLoginView` 0x1a9xxx）|
| 主端口 | ✗ | 有 `socksPort` 字段（0x20e3a5）、`局域网 SOCKS 端口启动中`（0x212570）|
| SOCKS5 | ✗（App 内）| 仅 `Info.plist NSLocalNetworkUsageDescription` 有 "SOCKS5" |
| 雷达网址 | ✗ | 有服务端字段 `viewerURL`（0x21010b）|
| 已完成会话 | ✗ | 有 `udpAssociations` / `activeUdpAssociations` 计数 |
| **局域网接收节点** | ✓ | **0x2117c0** |
| 地址 / 端口 / 节点代次 | 端口 ✓（0x212570 附近）；地址、节点代次 ✗ | 有字段 `connectionGeneration`（0x2100ef / 0x20be..）|
| 数据传输 | ✗ | 有对应数据（见 §2.3），但无此标题 |
| TCP 接入次数 | ✓ | 0x211f50（键 `tcpConnections`）|
| SOCKS 认证 · 成功 / 失败 | ✓ | 0x211f70（`socksAuthSuccesses` / `socksAuthFailures`）|
| UDP 关联 · 当前 / 累计 | ✓ | 0x211fd0（`activeUdpAssociations`）|
| UDP 上行 / 下行 | ✓（作为 TGCP/`udpPacketsUp`/`udpPacketsDown` 键）| 0x1b91f2 / 0x1b91f9 |
| 连接诊断 | ✓ | 0x2125c0（在 `UDP 转发异常，请查看连接诊断` 中）|
| 服务器镜像 | ✓ | 0x2119a0 `等待服务器镜像通道`、0x2119c0 `服务器镜像通道已连接` |
| 详细状态 | ✗ | 无 |
| 复制诊断 | ✗ | 无 |
| 用户名 | ✗ | 有 `localProxyUsername`（0x2101xx）、`username` |
| **复制节点链接** | ✓ | **0x211840** |
| **复制导入链接** | ✓ | **0x2117e0** |
| 雷达网页 | ✗ | 有 `RadarWebView` / `EmbeddedRadarBrowser` 类型 |
| App 内打开 | ✗ | 无 |
| 共享雷达 | ✓ | 0x212130 `DeltaRadar 共享雷达`、0x212250 `共享雷达当前不可用` |
| **刷新局域网地址** | ✓ | **0x211860** |

> **结论**：用户真机看到的那套行标签（接收状态/内置雷达/连接节点、主端口、雷达网址、已完成会话、节点代次、
> 数据传输、详细状态、复制诊断、用户名、雷达网页、App 内打开）**不在 5.1.58 这个 build 里**。
> 两个可能：① 用户手上是**更晚的 build**（同产品线，UI 重排+重命名）；② 用户是凭记忆转述。
> 本 build 里能确证的对应物是 §2.2–§2.6 那批（同样能表达全部信息，只是措辞不同）。
> **这一点我们不要照抄标签，直接按我们自己的文案体系做即可。**

### 2.2 Swift 侧代码组成（7 个源文件，全部从 `#file` 字面量确认）

```
DeltaRadarIOS/DeltaRadarApp.swift            0x212270
DeltaRadarIOS/ContentView.swift              0x2117a0   （另见 0x211770 `View.task @ ...ContentView.swift:`）
DeltaRadarIOS/LANServicePublisher.swift      0x2122a0
DeltaRadarIOS/RadarService.swift             0x212470
DeltaRadarIOS/RadarWebView.swift             0x2126d0
DeltaRadarIOS/ReceiverAccountService.swift   0x212be0
DeltaRadarIOS/ReceiverLoginView.swift        0x212f50
```

**类型清单**（来自 ObjC 类名表 + Swift 类型元数据 blob 0x1a7000–0x1ab600）：

| 类型 | 地址 | 职责（由字段/方法反推）|
|---|---|---|
| `DeltaRadarApp` | 0x1a7da6 | `@main App`（`WindowGroup`）|
| `ContentView` | 0x1a7150 | 根视图：`TabView(selection:)` + `tabItem` |
| `RadarService` | 0x1a7e96 | 接收端服务（`ObservableObject`），桥 Rust |
| `LANServicePublisher` | 0x1a7e60 | `NSNetService` Bonjour 发布 `_deltaradar._tcp.` |
| `BackgroundNodeKeepAlive` | 0x1a7050 | 后台保活（AVAudioEngine）|
| `RadarWebView` | 0x1a8700 | 雷达网页 `WKWebView` 包装 |
| `EmbeddedRadarBrowser` | 0x1a81c0 | `UIViewRepresentable`（`UIViewType`/`Coordinator`）|
| `RadarBrowserController` | 0x1a8150 | WK 导航代理 + `init()` |
| `RadarShareView` | 0x1a7b48 | 共享雷达 sheet（`_showingRadarShare` 驱动）|
| `QRImage` | 0x1a70ce | 二维码图视图 |
| `ReceiverLoginView` | 0x1aa270 | 登录页（`_username`/`_password`/`_focusedField`）|
| `ReceiverAccountService` | 0x1a88a0 | 账号/会话/共享（最大一个 VM）|
| `ReceiverAccountKeychain` | 0x1a9270 | Keychain（service `com.deltaradar.receiver.account.v1`）|
| `ReceiverCredentialStore` | 0x1a8860 | 协议（`$s13DeltaRadarIOS23ReceiverCredentialStoreP` @0x20be1a）|
| `ReceiverAPIRedirectPolicy` | 0x1a8880 | URLSession 重定向策略 |
| `ReceiverAccountError` | 0x1a9230 | 错误枚举 |
| 数据模型 | — | `ReceiverUser` `ReceiverDevice` `ReceiverSession` `ReceiverRadarShare` `ReceiverSavedAccount` `ReceiverAnnouncement` `ReceiverAuthResponse` `ReceiverMeResponse` `ReceiverShareResponse` `ReceiverShareRevokeResponse` `ReceiverViewerTicketResponse` `ReceiverAnnouncementResponse` `SessionResponse` `DeviceResponse` `EdgeConfig` |
| 混淆情况 | — | **只有 7 个 ObjC 可见类 + 1 个协议**保留名字；纯 SwiftUI struct 的 mangled 名被 strip（全二进制 `$s13DeltaRadarIOS*` 只有 8 处）|

**SwiftUI API 面**（符号表直接读出，决定了 UI 长什么样）：

```
TabView(selection:)  •  tabItem  •  NavigationStack(root:)  •  navigationTitle
navigationBarTitleDisplayMode(.inline)  •  toolbar/ToolbarItem(.confirmationAction)
Form  •  Section  •  LabeledContent  •  DisclosureGroup  •  Label(_, systemName:)
Button(role:.destructive)  •  BorderedProminentButtonStyle  •  PlainButtonStyle
ProgressView(value:total:) + LinearProgressViewStyle  •  ShareLink(item:)
TextField  •  SecureField  •  FocusState  •  SubmitLabel(.go/.next)  •  onSubmit
alert(item:)  •  sheet(isPresented:onDismiss:content:)  •  Material.regular/.ultraThin
RoundedRectangle(style:.continuous)  •  Color(uiColor:)  •  Image(systemName:)
```

**SF Symbols（全部 7 个，无遗漏）**：
`antenna.radiowaves.left.and.right`、`rectangle.portrait.and.arrow.right`、`checkmark.circle.fill`、
`exclamationmark.circle`、`wifi.exclamationmark`、`square.and.arrow.up`、`person.crop.circle.badge.checkmark`

### 2.3 「接收状态」页（本 build 的还原）

**状态行枚举**（`RadarService.status` / `interruptionMessage` / `backgroundNodeActive` 驱动，20 条，原文+偏移）：

| 偏移 | 文案 | 语义 |
|---|---|---|
| 0x212550 | 接收器未运行 | 未启动 |
| 0x2122d0 | 局域网接收端未发布 | Bonjour 未 publish |
| 0x212350 | 正在申请局域网访问并发布 SOCKS : | 等本地网络授权 |
| 0x212310 | 局域网 SOCKS 已发布 · : | 已发布 `_deltaradar._tcp.` + 端口 |
| 0x212570 | 局域网 SOCKS 端口启动中 | 端口绑定中 |
| 0x2122f0 | 局域网发布失败 · code | NSNetService 失败码 |
| 0x212380 | 局域网接收端已停止 | 已 stop |
| 0x2124a0 | 未找到局域网 IPv4，请确认副手机已连接 Wi-Fi | 无私网 IPv4 |
| 0x212430 | 接收会话配置无效 | EdgeConfig 不合法 |
| 0x212450 | 接收器启动失败（ | 启动失败 + 详情 |
| 0x2124e0 | 接收会话已过期，请重新连接 | 会话 TTL 到期 |
| 0x2126a0 | 等待主手机的小火箭连接 | **明确点名"小火箭"（Shadowrocket）**|
| 0x212680 | 等待 SOCKS 认证 | CONNECT 到了，等 RFC1929 认证 |
| 0x212660 | SOCKS 认证失败 | 用户名/密码错 |
| 0x212640 | 已认证，等待 UDP 关联 | 等 UDP ASSOCIATE |
| 0x212620 | UDP 关联失败 | ASSOCIATE 失败 |
| 0x2125f0 | UDP 已关联，等待游戏流量 | 链路就绪 |
| 0x212590 | 主手机流量正在经副手机直接转发 | 转发中 |
| 0x2125c0 | UDP 转发异常，请查看连接诊断 | 异常提示 → 指向诊断区 |
| 0x2123a0 | 副手机进入后台，iOS 可能暂停接收；保持 App 前台常亮 | 后台提醒 |

**「局域网接收节点」卡（0x2117c0）**——四个动作按钮 + 一行地址：
`复制导入链接`(0x2117e0) / `复制节点链接`(0x211840) / `刷新局域网地址`(0x211860) / `创建或管理只读共享链接`(0x2118a0)，
图标 `square.and.arrow.up`(0x211800)；节点名模板 **`DeltaRadar-Receiver-g`**(0x2118f0) + `connectionGeneration`。

**诊断/统计行**（标签 ↔ 键名一一对应，标签在 `__cstring`、键名在 Rust 常量池；这是本 build 的"数据传输+连接诊断"）：

| 标签（偏移） | 绑定键 |
|---|---|
| `镜像通道重连中，本地转发继续` (0x211970) / `等待服务器镜像通道` (0x2119a0) / `服务器镜像通道已连接` (0x2119c0) | `mirrorConnected` / `mirrorReconnects` |
| `TCP 建立 · 成功 / 失败` (0x2119e0) | `socksTcpConnectSuccesses` / `socksTcpConnectFailures` |
| `TGCP TCP / 握手` (0x211a40) | `tgcpTcpConnections` |
| `TGCP 握手完成` (0x211a80) | `tgcpHandshakesCompleted` |
| `控制帧 上行 / 下行` (0x211ac0) | `tgcpHeaderOnlyUp` / `tgcpHeaderOnlyDown` |
| `TGCP 帧 上行 / 下行` (0x211b20) | `tgcpFramesUp` / `tgcpFramesDown` |
| `TGCP 转译峰值` (0x211b40) | `tgcpTransformMaxUs` |
| `TGCP 初始化峰值` (0x211b80) | `tgcpSessionInitMaxUs` |
| `TCP 服务端先发 / 首包等待超时` (0x211bc0) | `tcpProbeUpstreamFirst` / `tcpProbeTimeouts` |
| `密钥提取 / 已上报` (0x211c30) | `tgcpKeysExtracted` / `drk1KeysSent` |
| `候选密钥 / 已验证绑定` (0x211c70) | `tgcpKeyCandidates` / `tgcpVerifiedKeyBindings` |
| `密钥归属 UDP` (0x211cd0) | `lastKeyAssociation` / `lastKeyEndpoint` |
| `密钥验证峰值` (0x211d10) | `tgcpKeyProbeMaxUs` / `lastKeyFingerprint` |
| `UDP 请求 / 建立失败` (0x211d70) | `udpAssociateRequests` / `udpAssociateFailures` |
| `UDP 收到数据报 / 转发错误` (0x211dd0) | `udpDatagramsReceived` / `udpRelayErrors` |
| `UDP 来源拒绝 / 解析失败 / 发送失败` (0x211e20) | `udpSourceRejected` / `udpParseErrors` / `udpSendErrors` |
| `镜像发送 / 等待` (0x211e70) | `mirrorSent` / `mirrorQueueDepth` |
| `镜像重连 / 错误` (0x211eb0) | `mirrorReconnects` / `mirrorErrors` |
| `最近 UDP 异常` (0x211ef0) / `最近镜像异常` (0x211f10) / `最近代理异常` (0x211f30) | `lastUdpError` / `lastMirrorError` / `lastProxyError` |
| `TCP 接入次数` (0x211f50) | `tcpConnections` |
| `SOCKS 认证 · 成功 / 失败` (0x211f70) | `socksAuthSuccesses` / `socksAuthFailures` |
| `UDP 关联 · 当前 / 累计` (0x211fd0) | `activeUdpAssociations` / `udpAssociations` |
| （无标签，图标）`exclamationmark.circle` | `lastError` |
| （无标签）| `socksListenerReady` / `running` / `mirrorAccepted` / `mirrorDropped` / `tgcpHandshakes` / `tgcpBytesUp` / `tgcpBytesDown` / `tgcpKeyCandidates` / `drk1KeysQueued` / `drk1Conflicts` / `lastKeyAgeMs` / `socksAuthAttempts` |
| （镜像前缀，Rust 日志）`mirror: ` `mirror heartbeat: ` `mirror key frame: ` `mirror frame: ` | 0x213da1/0x213dac/0x213dc1/0x213dd6 |

> 完整 46 个键名原文（无分隔符连排）见 `reference/myraderpro/metrics_blob.txt`；
> 该键表原文起点 0x1b90fd，覆盖 0x1b90fd–0x1b9470。

**共享雷达卡**（`RadarShareView`，sheet）：

```
0x212050 只读共享链接
0x212070 正在等待服务器确认
0x212090 确认完成前不会显示或复制原共享链接
0x212110 重置共享链接
0x212130 DeltaRadar 共享雷达
0x212190 复制共享链接
0x2121b0 发送共享链接       ← ShareLink(item: URL)
0x2121d0 正在生成共享链接
0x2121f0 共享链接尚未生成
0x212230 生成共享链接
0x212250 共享雷达当前不可用
```
通知/标识符：`radar.share.open` / `radar.share.reset` / `radar.share.revoke` / `radar.share.system` /
`radar.share.copy` / `radar.share.create`（在 `__cstring` 中与上述文案相邻）。
图标 `checkmark.circle.fill`。

### 2.4 「内置雷达」页（`RadarWebView` + `EmbeddedRadarBrowser` + `RadarBrowserController`）

雷达**不是本地渲染**，是服务端网页塞进 WKWebView：
`RadarWebView.homeURL` / `viewerExchangeURL` / `webView` / `viewerTicket` / `exchangeInFlight` /
`viewerSessionInvalidNotified` / `onViewerSessionInvalid` / `reloadPending` / `_controller` / `_pageTitle` /
`_isLoading` / `_progress` / `_canGoBack` / `_canGoForward` / `_errorMessage` / `_lastHTTPStatus`。

错误/提示文案（原文+偏移）：

```
0x212700 雷达会话地址已变化，请重新建立接收会话
0x212760 重试雷达页面
0x2127a0 雷达页加载失败：
0x2127c0 雷达登录票据无效或已过期，请重新建立接收会话
0x212810 雷达页返回 HTTP 
0x212830 ，请点击重试。
0x212850 雷达观看会话已失效（HTTP 
0x212880 ），请返回连接页重新建立会话。
0x2128b0 雷达节点暂时不可用（HTTP 503）。账号已登录，请点击重试。
```
图标 `wifi.exclamationmark`。`MIME: application/json`（0x2128f0 附近）。

### 2.5 「连接节点」页（`ReceiverLoginView` = 账号/节点）

```
字段：_username(0x210330) _password(0x21033a) _focusedField _account
控件：TextField + SecureField + FocusState + SubmitLabel(.go/.next) + onSubmit
图标：person.crop.circle.badge.checkmark (0x20d3xx 区 / 0x212fb0 附近)
```

登录/会话状态文案：

```
0x2129e0 登记接收设备            0x212a00 建立接收会话
0x212a40 接收会话已就绪          0x212c90 等待接收会话
0x212cb0 验证雷达会话            0x212cd0 雷达会话已就绪
0x212c40 接收会话未就绪          0x212e60 接收会话尚未就绪，请先重新连接
0x212b80 接收会话已到期，请重新连接
0x212a60 登录已失效，请重新登录
0x212cf0 未开通或已过期          0x212e90 当前账号未开通或已到期
0x212ad0 当前卡密不支持共享雷达   0x212ef0 当前卡密未开通共享雷达
0x212b00 当前卡密已过期，请续费后重新连接
0x212bb0 共享权限响应无效，请重新连接   0x212ec0 共享权限尚未同步，请重新连接
0x2129a0 请求失败（HTTP 
0x2129c0 api/v1/auth/refresh
0x212d70 账号凭据保存失败（Keychain 
0x212da0 续期账号与当前账号不一致，请重新登录
0x212de0 服务器地址已变化，请重新登录
0x212e10 服务器响应格式无效
0x212e30 自有服务器 HTTPS 地址无效
服务端错误码（原文，直接是后端返回的字符串常量）：
0x212a90 shareable_card_required
0x212ab0 active_entitlement_required
```

### 2.6 没被用户提到、但本 build 里确实有（增量）

* 全部 TGCP/镜像/密钥类统计行（§2.3 表，18 行）——用户没描述。
* 共享雷达整套（生成/重置/撤销/发送/复制 + `确认完成前不会显示或复制原共享链接`）。
* `等待主手机的小火箭连接`（明确写了 A 机用 Shadowrocket）。
* `接收会话已过期/已到期`、`接收会话配置无效`、`局域网接收端未发布/已停止/发布失败 · code`。
* 账号侧：`登记接收设备` / `建立接收会话` / `验证雷达会话` / `续期账号与当前账号不一致` /
  `自有服务器 HTTPS 地址无效` / `服务器地址已变化`。
* `副手机进入后台，iOS 可能暂停接收；保持 App 前台常亮`。

---

## 3. 机制一：怎么接管 B 机流量

**结论：和 r39 一样是「B 机做 SOCKS5 服务端」，A 机（小火箭）主动连入；但本版本多加了用户名/密码认证。**
**没有** TUN / NEPacketTunnel / tproxy / iptables / utun / NetworkExtension —— 全库命中数 0。

证据：

| 证据 | 偏移/原文 |
|---|---|
| `Info.plist` 自述 | `用于在同一 Wi-Fi 内接收主手机的 SOCKS5 TCP/UDP 流量并直接转发。` |
| 状态机点名 A 机客户端 | `等待主手机的小火箭连接` (0x2126a0) |
| SOCKS5 版本/认证协商 | `SOCKS version` (0x1b8b23)、`SOCKS auth required`、`auth version`、`SOCKS authentication`、`request version`、`invalid local credentials` (0x1b8b53) |
| RFC1929 用户名/密码 | `EdgeConfig.socksUsername / socksPassword` (0x1ab4e8 起)、服务端下发 `localProxyUsername` / `localProxyPassword` (0x2101xx) |
| CONNECT / ASSOCIATE 分支 | `UDP ASSOCIATE` (0x1b8ba5)、`udp domain`、`udp address`、`udp header`、`dns`、`address type` |
| UDP 来源校验 | `source is neither authenticated peer nor contacted remote` (0x1b8c4b)、`client port differs from bound endpoint`、`client_send`、`upstream_send`、`TCP connect failed` |
| 统计闭环 | `socksAuthAttempts` / `socksAuthSuccesses` / `socksAuthFailures` / `socksTcpConnectSuccesses` / `socksTcpConnectFailures` / `udpAssociateRequests` / `udpAssociateFailures` / `activeUdpAssociations` / `udpPacketsUp` / `udpPacketsDown` |
| Bonjour 自动发现 | `_deltaradar._tcp.` (0x212330)，`NSNetServiceDelegate` / `initWithDomain:type:name:port:` |
| 端口来源 | `RadarService.socksPort`（0x20e3a5 / 0x211a20），由 Rust 运行时给出；**具体端口值/区间在本二进制中未确认**（`主端口` 字串不存在） |
| 节点命名 | `DeltaRadar-Receiver-g` (0x2118f0) + `connectionGeneration` → 节点名/备注随"代次"变化，用于 A 机重连时区分新旧 |

Rust 侧实现模块（符号名，`delta_radar_edge::`）：

```
delta_radar_edge::relay_udp          子类型 KeyGuard（Drop 时释放密钥绑定）
  函数： encode_udp_packet · observe_udp · observe_downlink · begin_association
delta_radar_edge::key_broker
  类型： Flow（值：HashMap<SocketAddr, Flow>）· Candidate · TgcpKeyBroker
  函数： install_tgcp_keys · extract_keys · key_is_unsent · encode_key_frame ·
        publish_bound_key_for_endpoint
delta_radar_edge::tgcp              类型： TgcpSession
delta_radar_edge::unreal_probe      类型： Bunch
delta_radar_edge::season_xtea       类型： SeasonXteaKeyBank
delta_radar_edge::{EdgeConfig, EdgeState, KeyMaterial, MirrorQueue, MirrorBuffer,
                   MirrorUploader::pop_next_frame, QueuedMirrorFrame, dr_edge_start*}
```

> 注意 `relay_udp::KeyGuard` 与 `key_broker::Flow` 键为 `SocketAddr`：**每个 UDP 流（源端点）单独绑一套密钥**，
> 与 r39 的 `udp_nat_mapping=per_client_endpoint_isolated` 思路一致，但实现完全不同。

---

## 4. 机制二：`ingestUrl` / `ingestToken`「服务器镜像」通道

### 4.1 是什么

**是一条 app → 云端的一次性 WSS 上行通道**，用来把"解密所需的东西"和"原始帧"送到云服务器，
由云端解出雷达。Swift 侧字段名就是 `ingestURL` / `ingestToken`（0x2100f5 / 0x2100ff，reflstr 0x..113/84），
Rust 侧叫 `ingest_url`（0x1b9097）/ `lan_ip`（0x1b9077）。

### 4.2 桥接口：`EdgeConfig`（8 字段）

Swift 传给 Rust 的配置，字段名按出现顺序（0x1ab4db 起，紧跟 `struct EdgeConfig with 8 elements` @0x1ab54a）：

```
lanIp  socksPort  socksUsername  socksPassword  ingestUrl  ingestToken
receiverSessionId  connectionGeneration
```

（Rust 侧另有一份 `lan_ip` / `ingest_url` snake_case 常量，属 serde rename/alias。）

### 4.3 协议与鉴权

| 项 | 证据 |
|---|---|
| 必须 `wss://` | `ingest requires wss` (0x1b90a2) → 明文 ws 会被拒 |
| 身份校验 | `invalid ingest identity` (0x1b90b5) |
| 端点校验 | `endpoint length out of range` / `endpoint port out of range` / `invalid port or generation` |
| HTTP 头 | `Authorization` (0x1b8d00) + `Bearer ` (0x213d83) + `X-Receiver-Session` (0x1b8d0f) |
| 客户端栈 | `tokio-tungstenite 0.27.0` + `tungstenite 0.27.0` + `rustls 0.23.44` + `tokio-rustls 0.26.5` |

### 4.4 发什么、什么时候发

| 帧类型 | 证据 |
|---|---|
| 心跳 | `mirror heartbeat: ` (0x213dac)，状态 `mirrorConnected`、计数 `mirrorReconnects` |
| **密钥帧** | `mirror key frame: ` (0x213dc1)、`encode_key_frame`、`key_is_unsent`、`drk1KeysQueued` / `drk1KeysSent` / `drk1Conflicts` |
| **数据帧（镜像）** | `mirror frame: ` (0x213dd6)、`encode_frame`、`mirrorSent` / `mirrorQueueDepth` / `mirrorAccepted` / `mirrorDropped` / `mirrorErrors` |
| 出队模型 | `MirrorQueue::record`、`MirrorUploader::pop_next_frame`、`QueuedMirrorFrame`、`MirrorBuffer`（带队列深度计数 → 云断线时本地缓冲，不阻塞转发）|
| 时机 | 接收会话建立后常开；未连上时 UI 显示 `等待服务器镜像通道`(0x2119a0) / `镜像通道重连中，本地转发继续`(0x211970) —— **镜像断掉不影响本地 SOCKS 转发** |
| 密钥归属 | `密钥归属 UDP`(0x211cd0)→`lastKeyAssociation`/`lastKeyEndpoint`；`publish_bound_key_for_endpoint` 把密钥绑定到具体 UDP 端点后上报 |

**未确认**：镜像帧的具体二进制封装（长度前缀/序列化格式）——本二进制里没有任何 JSON schema 或编解码表；
`encode_frame` / `encode_key_frame` 只有函数名，无字段名。速率/批量策略也未确认。

---

## 5. 机制三：`BackgroundNodeKeepAlive` 后台保活

**结论：`UIBackgroundModes=[audio]` + `AVAudioSession(category: .playback, mode: .default, setActive)` +
`AVAudioEngine` + `AVAudioPlayerNode` 循环播放**静音**缓冲；另外用 `setIdleTimerDisabled` 让屏幕常亮。
没有任何 `beginBackgroundTask` / `BGTaskScheduler`。**

| 证据类别 | 内容 |
|---|---|
| 类名 | `_TtC13DeltaRadarIOS23BackgroundNodeKeepAlive` (0x20d120 / 0x1a7050) |
| 反射字段 | `__swift5_reflstr`: `engine` `player` `format` `silence` `active`（前 5 条，文档最前）|
| ObjC 类引用 | `_OBJC_CLASS_$_AVAudioEngine`、`_OBJC_CLASS_$_AVAudioPlayerNode`、`_OBJC_CLASS_$_AVAudioFormat`、`_OBJC_CLASS_$_AVAudioPCMBuffer`、`_OBJC_CLASS_$_AVAudioSession`；类型元数据 `So13AVAudioEngineC` / `So17AVAudioPlayerNodeC` / `So13AVAudioFormatC` / `So16AVAudioPCMBufferCSg` (0x1fff8c 起) |
| 常量 | `_AVAudioSessionCategoryPlayback`、`_AVAudioSessionModeDefault` |
| 选择器 | `setCategory:mode:options:error:`、`setActive:withOptions:error:`、`initStandardFormatWithSampleRate:channels:`、`initWithPCMFormat:frameCapacity:`、`attachNode:`、`connect:to:format:`、`scheduleBuffer:atTime:options:completionCallbackType:completionHandler:`、`setFrameLength:`、`targetFrame`、`frameCapacity`、`mainMixerNode`、`play`、`startAndReturnError:`、`stop`、`invalidate` |
| 常亮 | `setIdleTimerDisabled:` + `sharedApplication` |
| plist | `UIBackgroundModes: ["audio"]` |
| 本地变量 | `RadarService._backgroundNodeActive`(0x20d850)、`_interruptionMessage`、`backgroundNode`、`configuredSession`、`lanPublisher`、`timer` |
| 提示文案 | `副手机进入后台，iOS 可能暂停接收；保持 App 前台常亮` (0x2123a0) |
| 反证 | `beginBackgroundTask` / `BGTask` / `BGTaskScheduler` 命中 0 → 不依赖后台任务配额 |

`scheduledTimerWithTimeInterval:repeats:block:` + `RadarService.timer` → 另有轮询定时器（用途未确认，可能是状态刷新）。

---

## 6. 机制四：SOCKS5 / 导入二维码 —— 给的是 SOCKS5 还是 HTTP？

### 6.1 二维码怎么生成（已确证）

```
CIFilter(name: "QRCodeGenerator")    ← 字面量 0x20d360（只有 15 字节，无 "CI" 前缀；
                                         CIFilter 会自动补 CI，这是 CoreImage 的既有行为）
  setMessage: / setCorrectionLevel:
  outputImage
  CIContext.createCGImage:fromRect:
  UIImage.initWithCGImage: → SwiftUI 视图 QRImage (0x1a70ce)
```
选择器原文：`QRCodeGenerator` `setMessage:` `setCorrectionLevel:` `outputImage`
`createCGImage:fromRect:` `initWithCGImage:` `setFormatOptions:` `CIContext` `CIFilter`。

### 6.2 导入链接（已确证的部分）

* **硬编码前缀只有** `shadowrocket://add/`（0x211820，16 字节，紧跟其后的 0x211840 是 `复制节点链接`）。
* 全二进制**不存在**任何 `socks5://`、`ss://`、`vmess://`、`trojan://`、`clash://`、`http://`、`wss://` 字面量。
  `https://` 全库仅 3 处：两处是 rustls 文档链接（0x1c57e6、0x1f96db），一处是 API base（0x212940）。
* 裸 `http` 出现在 `__cstring` 两处（0x20dc04、0x210094），但上下文是
  `ReceiverAccountService` 的成员名（与 `credentials` / `defaults` 并列）→ **是属性名，不是 URL scheme**。
* App 侧用于拼 URL 的 API 面只有：`URLComponents`（`scheme`/`user`/`password`/`host`/`port`/`fragment`/`string` 全有 setter）、
  `URL.appendingPathComponent`、`CharacterSet.urlPathAllowed`、`URL(string:)`、
  `URL.absoluteString`、`URLRequest.setValue(_:forHTTPHeaderField:)`。
* 服务端在响应里给了候选载荷：`ReceiverSession.url`（reflstr 0x..93）、`ReceiverRadarShare.shareUrl`（0x2102xx）、`ReceiverSession.viewerURL`（0x21010b）。编码键表：`receiverSessionID revision title content updatedAt apiBaseURL refreshToken refreshExpiresAt accessToken expiresIn user shareUnavailable keychain invalidServer invalidResponse serverChanged accountChanged enabled receiverSession device receiverSessionId ok shareable shareUrl deviceId`（0x210180–0x210300）。

### 6.3 结论

* **本二进制无法绝对断定**扫码得到的 scheme —— **未确认**。
* 但可以**排除"App 自己写死了 socks5://"**：不存在该字面量，且 `URLComponents.scheme` 有 setter，
  说明 scheme 是**运行期值**。
* 因此最可能的链路是：
  `服务端下发 url 字符串 → App 用 URLComponents 组装/校验 → 追加到 shadowrocket://add/ → 生成 QRImage / 复制到剪贴板`。
* **结合用户"扫码出来是 HTTP"的反馈 → 服务端下发的载荷是 `http(s)://…`**。
  Shadowrocket 对 `shadowrocket://add/http(s)://…` 的处理是**当成订阅/导入 URL**，而不是 SOCKS5 节点，
  于是扫码后不会直接出现 SOCKS5 节点 —— 与反馈一致。
* **我们要做得更好的一点**：本地自己拼 `socks5://<localProxyUsername>:<localProxyPassword>@<lanIp>:<socksPort>#DeltaRadar-Receiver-g<N>`
  （用户名/密码用百分号编码），再套 `shadowrocket://add/`，这样扫码/导入必然直接得到 SOCKS5 节点。
  本仓库目前是发 Hiddify/sing-box `hiddify.json`，也可以额外补一个"Shadowrocket 一键导入"。

---

## 7. 内嵌资产：**什么都没挖到**（这一节是结论，也是最重要的负面结果）

### 7.1 扫描证据

| 目标 | 本样本命中 |
|---|---|
| `channel_map` / `handles` / `slot` | **0 / 0 / 0** |
| `SOL_DT` / `Term#` / `loot_ids` / `name_cn` / `origin_x` / `yaw_offset` | **0** |
| `BP_DFM` / `ReplicatedMovement` / `RepMovement` / `RemoteViewPitch` / `TeamID` / `HeroId` / `DFMContainer` | **0**（`BP_` 仅 2 处，是 gimli/tokio 的 mangled 名碎片：0x25fabf、0x2622a9）|
| `maps.json` / `leaflet` / `<!DOCTYPE` / `<html` | **0** |
| 任意 ≥60 字节、含 >8 个 `"` 的 `{...}` JSON 块 | **0** |
| gzip / zlib / zip 内嵌资源 | 无（只有 2 字节魔数的偶然命中，无任何可解压数据块）|
| 长 base64 块（≥120 字符）| 34 处，**全部**是 rustls 错误枚举名连排与 TGCP DH prime，非资产 |
| 唯一长十六进制常量 | `97981e0a…fede3`，**128 hex = 64 字节 = 512 bit**，即 **TGCP DH prime**（0x1b94db），见 §8.2 |
| `Assets.car` | 仅 `AppIcon-1024.png`（+ CoreUI 固定头），无自定义图片/无 JSON |
| `.lproj` / `Localizable.strings` / `NSLocalizedString` | **0** |

### 7.2 与本仓库资产逐字段对比

| 本仓库 | MyRader Pro 对应物 | 结论 |
|---|---|---|
| `core/assets/channel_map.json`（`schema=delta-fields/1`, 77 条 channel_map + 53 类 handles）| **不存在** | 无法逐字段比对。本样本不携带通道表 |
| `core/assets/loot_ids.json`（SOL_DT_* 词条）| **不存在** | 同上；本样本无任何物资 ID 表 |
| `core/assets/battle_card.html`（本地卡密激活页）| **不存在**（改用云账号 API）| 能力被服务端取代 |
| `web/maps.json`（6 张地图标定：`name_cn/origin_x/origin_y/scale/yaw_offset_deg/…`）| **不存在** | 本样本不做世界坐标→经纬度投影（渲染在云端）|
| `web/*`（leaflet-lite / radar.js）| **不存在** | 本样本无本地前端 |

**适用结论**：

1. **对协议标定没有直接帮助** —— 没有可抄的表。
2. 它反而证明了一条产品路线：**把游戏知识库（通道表/物资表/地图标定/解密）全部放服务端**，
   客户端只做"管道+密钥搬运"。这是商业版本控制知识资产的方式（也意味着**没有网络就完全不可用**）。
3. 唯一可对齐、值得借用的接口样板是 `EdgeConfig` 8 字段（`lanIp socksPort socksUsername socksPassword
   ingestUrl ingestToken receiverSessionId connectionGeneration`）——如果我们以后加"上传/云解析"，这就是现成契约形状。

---

## 8. 协议线索（反推它的档位）

### 8.1 它用了**两套**与 r39 完全不同的协议

**完全没有** r39 的 UE codec 痕迹（`__TEXT,__const` 全文比对，命中 0）：
`Bunch payload exceeds packet at`、`unresolved_bunch_header_variant`、`partial repacketization unavailable`、
`movement RepLayout not exactly closed`、`not a RepLayout Actor ContentBlock`、`closure marker truncated`、
`invalid_packet_framing`、`MaxPacket=`、`[slot_map]`、`[combat]`、`[aim_parse]`、`battle_fire_cli`、`FAES::EncryptData`。

### 8.2 TGCP（TCP 方向，`delta_radar_edge::tgcp`）——**这是它的核心**

形态：**在客户端侧对游戏 TCP 做 MITM 式拦截 + 与自有服务器做 DH 握手**，全程不经 TUN。

| 线索（原文） | 偏移 | 推断 |
|---|---|---|
| `TGCP DH public value must be  bytes` | 0x213fd6 | 公钥长度是格式参数（值未确认；结合 512-bit prime 推测 64 B）|
| 硬编码 128-hex(64 B) 常量 | 0x1b94db | **TGCP DH prime**（另有 `valid TGCP prime` @0x1aa8e0、`generated TGCP private exponent`）|
| `TGCP DH shared secret is degenerate` | 0x1b944c | 退化共享密钥检查 |
| `TGCP DH peer public value is outside the accepted range` / `…private exponent…` | 0x1b9474/0x1b94a4 | 范围校验 |
| `TGCP upstream closed before the server hello` | 0x1b8be0 一带 | 先连上游，收服务器 hello |
| `TGCP client data arrived before the server hello` / `TGCP server data arrived before the client hello` | 0x1b96xx | 严格握手顺序 |
| `duplicate TGCP handshake` / `unexpected TGCP mode-3 handshake` | — | 有多档握手模式（至少 mode-1/2/3）|
| `TGCP mode-3 ciphertext is not block aligned` / `TGCP mode-3 padding or trailer mismatch` / `TGCP mode-3 padding is out of range` / `TGCP mode-3 plaintext is too short` | 0x1b9b8c 一带 | **mode-3 是分组加密 + 自定义填充/尾部** |
| `TGCP replacement public value changes the header length` / `TGCP handshake header is too short` / `invalid TGCP DH public value length` | — | 握手头**变长**（随公钥）|
| `TGCP frame length mismatch` / `invalid TGCP frame length(s)` / `invalid TGCP frame header` / `TGCP stream buffer exceeds the parser limit` | — | 帧：header + length + payload |
| `non-TGCP bytes encountered after interception` | 0x1b97be | **"interception"** 明确写了拦截语义 |
| `TGCP  opcode=0x  header=  payload=` | 0x1aa8f0 一带 | 调试日志（`TGCP  opcode=0x` / `header=` / `payload=`）|
| `DHE` `ECDHE` | 0x1b9fxx | 握手类型枚举名 |
| `bit read exceeds buffer` / `bit extraction exceeds buffer` | — | 位流读取器 |

> 与 r39 的差异是本报告最有价值的一条：**r39 是把密文搬回本机用 FAES+AES-256-ECB 自己解；
> MyRader Pro 是在客户端就把 TCP 拦下来和自有服务器做 DH，把密钥和帧交给云端解。**
> 后者不需要在客户端保存 UE 加密知识，且天然支持"云端更新协议"。

### 8.3 UDP_C（UDP 方向，`delta_radar_edge::unreal_probe` / `relay_udp`）

bunch 级分帧，错误串（原文，`__TEXT,__const` 0x1b986f 起连续排布）：

```
UDP_C packet exceeds bunch limit
UDP_C packet contains no complete bunch
UDP_C notify history exceeds packet
UDP_C notify framing bit is set
UDP_C packet is shorter than its notify header
UE packed uint exceeds 10 octets          ← UE 变长整数（7-bit 打包）上限 10 字节
truncated UE packed uint
UDP_C open/close flag appears outside a control bunch
UDP_C partial boundary flag appears on a complete bunch
UDP_C reserved bunch flag is set
UDP_C bunch payload storage does not match its bit count
UDP_C bunch extends beyond the framed packet
UDP_C bunch header size is invalid
UDP_C bunch channel index is outside the live range
UDP_C packet has no complete bunch boundary
UDP_C channel FName offset overflow
UDP_C channel FName length is invalid
UDP_C bunch payload length exceeds packet
```

**档位判读（相对 r39）**：

| 维度 | r39 | MyRader Pro |
|---|---|---|
| 容器 | UE packet → bunch，`MaxPacket`/bit 位宽由校验器推断 | 同族但**自带 notify header**（`UDP_C notify …`），有 framing bit |
| bunch 头 | `unresolved_bunch_header_variant`（多候选）| `UDP_C bunch header size is invalid` → 头长有约束（**更像固定小头**）|
| 通道位宽 | `channel index` 有 `slot_map` 校验（`[slot_map] updated: matched`）| `UDP_C bunch channel index is outside the live range` → **有"活通道范围"概念，但没有本地 channel_map** |
| 包号/整数 | — | `UE packed uint`（变长 7-bit，≤10 字节）|
| 旗标位 | — | open/close flag、partial boundary flag、reserved bunch flag、notify framing bit |
| 加密 | FAES::EncryptData + AES-256-ECB + LZ4 | `season_xtea::SeasonXteaKeyBank` + `key_broker`（密钥从 TGCP 提取后**绑定到 UDP 端点**）|

**结论**：**不是同一 profile**。MyRader Pro 的 UDP_C 是"带 notify 头 + 显式旗标位 + UE 变长整数 + 活通道范围"的
另一种 bunch 变体；它的密钥来自 TGCP 拦截（TCP 方向）→ 所以**它必须先拿到 TCP 方向的密钥才能解 UDP**，
这也解释了为什么统计里有 `tcpProbeUpstreamFirst`（TCP 服务端先发探测）和 `tgcpKeysExtracted`。

**未确认**：mode-1/2/3 各自的具体算法、UDP_C 各旗标的位序、DH 参数组 ID。这些需要动态抓包或云侧配合才能定。

### 8.4 依赖栈（可据以判断作者/年代）

```
tokio 1.53.1 · tokio-tungstenite 0.27.0 · tungstenite 0.27.0 · httparse 1.10.1 · http 1.5.0
rustls 0.23.44 · tokio-rustls 0.26.5 · rustls-webpki 0.103.15 · ring 0.17.14 · untrusted 0.9.0
serde_json 1.0.151 · base64 0.22.1 · rand 0.9.5 · rand_chacha 0.9.0 · aes 0.8.4
num-bigint 0.4.8 · bytes 1.12.1 · mio · parking_lot_core 0.9.12 · anyhow 1.0.104 · data-encoding 2.11.1
```
* **`ring` 而不是 `aws-lc-rs`**（r39 用 aws-lc-rs）→ 不同依赖树/不同年代。
* **没有** `axum` / `hyper` / `tower` / `reqwest` / `tungstenite-server` → 它**不提供本地 HTTP 服务**（r39 有）。
* 无 `lz4_flex` → 它**不做 LZ4 解压**（r39 有）→ 解压也在云端。
* rustc 版本哈希 `8bab26f4f68e0e26f0bb7960be334d5b520ea452`；构建机用户 **`mac`**，用 **index.crates.io**（非国内镜像）。

---

## 9. 差距清单（它有我们没有的能力，按"对用户实际使用的影响"排序）

| # | 能力 | 对用户的影响 | 实现思路 | 工作量 |
|---|---|---|---|---|
| 1 | **云端镜像 + 云端解析 + 任意端观看** | 最大。现在雷达页必须由 B 机本地 Rust(axum) 提供、在 B 机上看；云化后 B 机可以只当管道，主手机/PC/别人发的只读链接都能看，还能跨网 | Rust 侧加一条 WSS 上行（`tokio-tungstenite` + `rustls`），实现 `MirrorQueue`/`MirrorUploader`/`encode_frame`/`encode_key_frame` + 重连与队列上限；服务端按 `Authorization: Bearer` + `X-Receiver-Session` 鉴权；`EdgeConfig` 直接照抄那 8 个字段 | **大** |
| 2 | **只读共享链接（共享雷达）+ 服务端确认 + 撤销/重置** | 大。分发与变现；也是"多端同看"的前提 | 服务端 `share` 资源（`shareable_card_required` / `active_entitlement_required` 这类错误码直接沿用语义），客户端 `ShareLink(item:)` + `QRImage` + 「确认完成前不显示旧链接」的两阶段状态机 | 中 |
| 3 | **云账号 + 卡密 + 会话（登录/刷新/设备登记/公告/权限）** | 大。可运营、可按卡密开权限、可远程公告、可强制下线 | 5 个端点照抄形状；`ReceiverAccountKeychain` 用 Keychain（service 常量），`com.deltaradar.announcement.seen.<id>` 记已读 | 中 |
| 4 | **TGCP：客户端侧拦截 TCP + 与自有服务器 DH，云端解密** | 中-大。无 TUN/免 root 也能拿到会话密钥，且协议可云端热更新 | 需要在 SOCKS5 CONNECT 成功后插一层 MITM 转发：客户端 ↔ 自有服务器 DH (512-bit prime, mode-3 分组加密) / 服务器 ↔ 真实游戏服；密钥绑定 UDP 端点上报。**风险最高、最依赖服务端配合** | **大** |
| 5 | **后台保活（audio 模式静音播放 + 屏幕常亮）** | 中。B 机切后台/锁屏仍能收，用户体验差别很明显 | `UIBackgroundModes=[audio]` + `AVAudioSession(.playback,.default,setActive)` + `AVAudioEngine/AVAudioPlayerNode` 循环 `AVAudioPCMBuffer` 静音 + `setIdleTimerDisabled`；注意 App Store 审核口径 | 小-中 |
| 6 | **App 内嵌雷达 WebView + viewer ticket 兑换 + 会话失效回调** | 中。B 机自己不切浏览器就能看；ticket 失效能自愈 | `RadarWebView`/`EmbeddedRadarBrowser`/`RadarBrowserController`：`viewerURL + viewerTicket` → `viewerExchangeURL` 兑换 → `homeURL`；`viewerSessionInvalidNotified`/`onViewerSessionInvalid` 回调、（503 可重试、票据过期需重建会话）三档错误分别处理 | 中 |
| 7 | **SOCKS5 用户名/密码认证（RFC1929）** | 中。局域网内防蹭流量/防冒用 | Rust SOCKS5 握手加 `0x02` 方法分支 + 校验 `socksUsername/socksPassword`；统计 `socksAuthAttempts/Successes/Failures` | **小** |
| 8 | **一键导入（`shadowrocket://add/` + 二维码）+ 局域网地址刷新 + Bonjour 自动发现** | 中。配对摩擦直接决定首装成功率 | `QRImage`（`CIFilter("QRCodeGenerator")`）+ 前缀 `shadowrocket://add/` + **载荷自己拼 `socks5://user:pass@ip:port#name`**（避开我们发现的"扫码变 HTTP"坑）；`NSNetService` 发 `_<name>._tcp.`（r39 已有 Bonjour，可复用） | 小 |
| 9 | **40+ 项诊断计数 + 最近异常三分类** | 小-中。售后/自证/排障，也是"用户信任"的来源 | 直接采用它那张键表（`reference/myraderpro/metrics_blob.txt`，46 个键）+ `最近 UDP/镜像/代理异常` 分组；Swift 侧 `LabeledContent` + `Form/Section` | 小 |
| 10 | **多马甲开关（`DRProductName` / `DRProMode` / DisplayName）** | 小。一个代码出多品牌 | 已有 r39 的 `BattleBrandVariant` 机制，扩成字典即可 | 小 |
| 11 | **`Base64` + `GET /api/v1/app/announcement` 远程公告 + 已读去重** | 小。运营触达 | 已有 `announcement` 思路，成本极低 | 小 |

### 反向：**我们有、它没有**（不要为了对齐而丢掉）

| 我们的能力 | 它的状态 |
|---|---|
| **完全离线可用**（本地解析全部在本机）| 断网/服务端挂 = 完全不可用 |
| `channel_map.json` + `loot_ids.json` 本地知识库 | 无（知识在服务端）|
| 本地 Web 前端（`web/` + Leaflet 风格） | 无（云页面）|
| Hiddify / sing-box 配置分享 | 只有 Shadowrocket 一种 |
| 物资/开箱内容解析（`loot_parsing_enabled`）| 无本地实现 |
| 本地卡密激活页（无账号体系，无隐私采集）| 强制登录 + 设备登记 + 服务端下发代理凭据 |

---

## 10. 证据索引 / 复现步骤

```bash
# 1. 解包（.ipa 是 zip）
copy MyRader6Pro.ipa dist/mp/mp.zip && tar -xf dist/mp/mp.zip -C dist/mp/x
# 2. 结构
python tools/macho_scan.py dist/mp/x/Payload/MyRaderPro.app/DeltaRadarIOS
python tools/macho_dump.py dist/mp/x/Payload/MyRaderPro.app/DeltaRadarIOS dist/mp/out/dump.json
# 3. 本报告用到的临时脚本（都在 dist/mp/，可删）
#    scan.py        分节字符串（UTF-8，含 CJK）
#    cjk.py         全库中文串 + 偏移
#    offsets.py     指定字面量的精确偏移
#    region.py / hexd.py / dumpat.py   定点内容查看
#    fixup.py / refs.py                指针/引用查找（chained fixups，未命中，可删）
#    extract.py / allsym.py / mangle.py / rust_syms.py / hexscan.py / dh.py
#    build_ref.py   生成 reference/myraderpro/
```

产物：`reference/myraderpro/` 共 9 个纯文本文件（`binary_facts.json` / `cstring_all.txt` /
`ui_strings_zh.txt` / `swift_surface.txt` / `swift_type_names.txt` / `symbols.txt` /
`rust_strings.txt` / `metrics_blob.txt` / `api_surface.txt`）。

**明确标为"未确认"的项**：
1. `shadowrocket://add/` 后载荷的具体来源与 scheme（§6.3）——只能排除"App 硬编码 socks5://"。
2. 镜像帧的二进制封装格式（§4.4）。
3. TGCP mode-1/2/3 的具体算法、DH 公钥长度、UDP_C 旗标位序（§8.2/§8.3）。
4. `socksPort` 的具体数值/区间（§3）。
5. 用户提到的行标签属于**哪个更晚的 build**（§2.1）——只能确定不在 5.1.58。
