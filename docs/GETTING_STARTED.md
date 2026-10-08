# 上手步骤（照顺序做，每步都有"怎么算通过"）

> 这份清单按**当前工程的真实状态**写：核心逻辑已写完但没编译过，前端已实测，
> 协议参数需要真实流量标定。别跳步 —— 第 1、2 步能在你今天的手边设备上完成。

## 步骤 0 · 搞清楚你手上有什么（1 分钟）

| 你需要的 | 用途 | 没有怎么办 |
|---|---|---|
| **一台 Mac**（Xcode 15+） | 编译 iOS 壳、出 IPA | 用云 Mac / GitHub Actions macOS runner；核心代码在 PC 上也能测（步骤 1） |
| **一台 iPhone**（iOS 16+）当 A 机 | 跑雷达 | — |
| **B 机**（另一台手机/平板/模拟器） | 跑游戏 | 与 A 机同局域网 |
| **代理工具**（小火箭 Shadowrocket / Hiddify） | 在 B 机上把 TCP+UDP 指到 A 机 | 小火箭需支持 UDP 转发；Hiddify 需 VPN/TUN 模式 |

## 步骤 1 · 先验证核心逻辑（PC 就能做，不需要 Mac）

**这台 Windows 机器已经装好了**：rustup 1.99 + GNU toolchain + WinLibs MinGW。不用 PowerShell 配环境（本机执行策略禁止跑 `.ps1`），直接用包装脚本：

```powershell
cd C:\Users\Administrator\Documents\deepseek-harness\default-workspace\MXrader-DFM
scripts\rust.cmd test                    # 等价于 cargo +stable-...-gnu test，并自动接好 PATH 与 linker
scripts\rust.cmd test --no-fail-fast
```

底层等价命令（换台机器时要自己接）：

```powershell
# Linux / macOS：装了 rustup 就能直接跑，不需要额外链接器
cd core && cargo test
# Windows 手工版（没装 VS Build Tools 时用 MinGW）：
$mingw = "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin"
$env:Path = "$env:USERPROFILE\.cargo\bin;$mingw;" + $env:Path
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = "$mingw\gcc.exe"
cd core; cargo +stable-x86_64-pc-windows-gnu test
```

**通过标志**：`test result: ok`，且 `battle::selftest` 的用例全绿（金标准向量 + 转向修正两档）。
**注意**：默认**不开** `license-net`，这样整棵依赖树是纯 Rust，不需要 cmake/nasm。
（这一步第一次跑就抓出了真 bug：`reqwest 0.13` 把特性名从 `rustls-tls` 改成了 `rustls` —— 见 `core/Cargo.toml` 的注释。）

## 步骤 2 · 看雷达界面长什么样（现在就能做）

```bash
python scripts/preview_web.py --open          # http://127.0.0.1:8770/battle.html
python scripts/preview_web.py --players 20 --map Layali
python scripts/preview_selftest.py            # 19 项断言：静态资源 + 真 WebSocket 握手 + 帧字段
```

它是个**假核心**：按 `docs/INTERFACES.md` §5 推合成对局（自己 + 队友 + 敌人 + 人机 +
死亡盒 + 物资 + 弹道 + 击杀条），全部在动。你可以用它：

* 确认三条就绪探针（`data-battle-ready` / `#app` / `.leaflet-container`）都对；
* 试左侧图例的 **2D北向上 / 跟随朝向 / 3D** 三种视角，看转向修正的效果；
* 把它当成 WS 契约的可执行文档 —— Rust 侧实际发什么，以它 + INTERFACES.md 为准。

## 步骤 3 · 编译 iOS 壳并跑起来（需要 Mac）

```bash
brew install xcodegen
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
./scripts/build_rust.sh          # 出 core/target/ios/libbattle_proxy.a（静态链接）
cd ios && xcodegen generate && open BattleReceiverOpen.xcodeproj
# Xcode 里选自己的 Team → 真机 Run
```

**通过标志**：启动页 5 秒内走到"端口已绑定，正在等待雷达页面"，随后出现**空的雷达地图**
（没有玩家，因为还没有游戏流量 —— 这是对的）。配对页二维码能扫出 Hiddify 配置。

## 步骤 4 · 先打通"纯转发"（这一步和解析无关，别混在一起调）

1. B 机装 Hiddify（或小火箭），把 A 机二维码扫进去（或手工填 `socks5://<A机IP>:<端口>`）；
2. **必须开 UDP 转发**；全局/TUN 模式时把 A 机 IP 加进直连/绕过列表；
3. B 机进游戏打一局；
4. 回 A 机看「诊断」页：

**通过标志**：`udp_packets_up` / `udp_packets_down` 在涨，`milliseconds_since_last_server_transmission` < 3000
（诊断文案显示"对局中"）。此时游戏本身必须能正常玩 —— 转发不能影响手感。
如果这一步就不通，先解决代理配置（这一步不需要动代码）。

## 步骤 5 · 抓一份流量，然后标定协议（核心工作）

```bash
# A 机「诊断」→ 开始采集（或 admin 接口）→ 打一局 → 停止
# 把 battle-full-capture-<ts>.ndjson 拿到 PC 上
cd core
cargo run --bin battle_replay -- --capture capture.ndjson --sweep
```

`--sweep` 会遍历 `packet_id_bits × bunch_variant × channel_index_bits` 并打印一张
gate 通过率排名表，给出推荐档。然后：

```bash
cargo run --bin battle_replay -- --capture capture.ndjson \
    --packet-id-bits <推荐值> --bunch-variant <推荐值> --channel-index-bits <推荐值> --entities \
    --channel-map ../core/assets/channel_map.json
```

**通过标志**：gate 通过率显著上升（>30% 算有戏），通道直方图里出现 `ch3/ch4`（`BP_DFMCharacter_C` /
`BP_DFMPlayerState_C` 所在通道）。把推荐值写回 `ProtocolProfile::dfm_r39()`（`core/src/battle/udpxin.rs`）。

接着同一套流程标 `RepMovementProfile`（`core/src/battle/udpxin_move.rs`）：
`--entities` 报"有位置 0 个"就说明 `location_bits` / `location_scale` / `has_base_bit` 不对。

## 步骤 6 · 标地图与朝向

进图后站到两个已知地标（出生点、地图角落的固定建筑），记录雷达上的位置 vs 游戏里的位置，
解出 `origin_x/origin_y/scale`，写进 `web/maps.json`。朝向不对就改
`yaw_offset_deg` / `yaw_sign`（**三层只用改这一处** —— core 启动会读它，前端也从它读）。

**通过标志**：自己站在地图正确位置、朝向箭头与世界方向一致、队友/敌人点位置合理。

## 步骤 7 · 出 IPA 并侧载

```bash
./scripts/package_ipa.sh                              # 出 dist/BattleReceiverOpen-mxrader-r39.ipa
./scripts/verify_structure.sh dist/*.ipa "<样本.ipa>"  # 与样本逐项对比布局/Info.plist/Mach-O/符号
```

免费账号走 AltStore/Sideloadly（7 天），越狱机可以 TrollStore 直装。

## 常见卡点

| 现象 | 原因 | 处理 |
|---|---|---|
| 启动页停在"正在启动接收器" | 端口 2025–2045 全被占 | 杀掉旧版进程；或改 `endpoint.ports.range` |
| 雷达页空白但状态是 running | 前端资源没打进包 | `web/` 要随包分发；`embed::set_web_root()` 指向它 |
| 诊断页"长时间无数据" | B 机没开 UDP 转发，或 A 机 IP 不在直连列表 | 回步骤 4 |
| 有 UDP 但 `gate 通过 0%` | profile 档不对 | 步骤 5 的 `--sweep` |
| 有通道但没有位置 | `RepMovementProfile` 不对 | 步骤 5 的 `--entities` |
| 玩家点在原点堆成一坨 | 位置解析错位（护栏会拦一部分） | 看 `[combat] rejected packet growth` 日志 |
| 朝向整体偏 | `yaw_offset_deg`/`yaw_sign` | 步骤 6 |

## 语言/编码提示（Windows 上很坑）

用 Windows PowerShell 5.1 的 `Get-Content` 读这些文件会**显示成乱码**（它按 GBK 解码 UTF-8）。
那不代表文件坏了 —— 用 `Get-Content -Encoding UTF8`、VS Code，或本仓库的 `tools/find_double_encoded.py`
（它会告诉你文件里到底有没有真乱码，当前全库 0 处）。

同理，**批处理脚本（`.cmd`）必须保持纯 ASCII**：cmd.exe 按 OEM 代码页解析批处理文件，
中文注释会把行流切坏（`scripts\rust.cmd` 就是为此全英文注释的）。PowerShell 脚本（`.ps1`）
没有这个问题，但本机执行策略默认禁止运行——想用 `scripts\env.ps1` 得先
`Set-ExecutionPolicy -Scope Process Bypass`，否则就用 `scripts\rust.cmd`。
