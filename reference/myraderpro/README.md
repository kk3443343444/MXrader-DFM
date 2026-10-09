# `reference/myraderpro/` — MyRader Pro 5.1.58 提取物

来源：`MyRader6Pro.ipa`
→ `Payload/MyRaderPro.app/DeltaRadarIOS`（arm64 Mach-O，2,624,688 B，bundle `com.deltaradar.pro`）

**这里只有纯文本。没有任何二进制、没有任何 IPA 内容拷贝。**

| 文件 | 内容 |
|---|---|
| `binary_facts.json` | Mach-O/plist 结构化事实（header、dylibs、sections、大小、Info.plist 关键键）|
| `cstring_all.txt` | `__TEXT,__cstring` 全部 670 条（按首次使用顺序）|
| `ui_strings_zh.txt` | **全部 94 条中文用户可见文案 + 文件偏移**（这就是这台手机 App 会显示的文案全集）|
| `swift_surface.txt` | ObjC 类名 / Swift 反射字段名 / ObjC 方法名（决定 UI 与状态机的全部属性）|
| `swift_type_names.txt` | Swift 类型元数据区（0x1a7000–0x1ab600）里的 CamelCase 类型名 + 偏移 |
| `symbols.txt` | 符号表全部 933 条（含 SwiftUI / AVFAudio / URLComponents 等使用证据）|
| `rust_strings.txt` | Rust 侧协议/诊断字符串（crate `delta_radar_edge`）|
| `metrics_blob.txt` | 46 个统计键名（`socksAuthSuccesses`…`lastUdpError`）与原文窗口 |
| `api_surface.txt` | HTTP 端点 / WS / 头部 / Keychain / UserDefaults / 通知标识符 + 偏移 |

配套分析报告：`docs/REFERENCE_MYRADERPRO.md`（含机制结论、UI 还原、差距清单）。

## 关键结论速查

* 内嵌资产：**无**。没有 `channel_map.json`、`loot_ids.json`、`maps.json`、HTML、JSON、压缩资源、`Assets.car` 自绘资源。
  → 与本仓库 `core/assets/*.json`、`web/maps.json` **无字段可对比**。
* 它是**云化**版本：客户端只做 SOCKS5 服务端 + TGCP 拦截 + 镜像上报；解析/渲染在云端。
* 与本仓库 r39 的实现**零重叠**（协议、错误串、依赖树、构建机都不同）。
* 用户提到的「接收状态 / 内置雷达 / 连接节点」等标签**不在 5.1.58 这个 build 里**（详见报告 §2.1 对照表）。
