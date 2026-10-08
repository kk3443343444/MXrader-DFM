# 怎么把这份工程打成 IPA

> 一句话前提：**iOS 可执行文件只能由 macOS + Xcode + iOS SDK 编译**。Windows/Linux
> 都做不到 —— 这是平台限制，不是配置问题。所以"打包"这件事有两条路，都不需要你
> 手上有 Mac 也能走通（A），或者半天内走完（B）。

---

## 路径 A · 用 GitHub 的 macOS runner 云编译（推荐，不需要 Mac）

免费额度足够：public 仓库无限，private 仓库每月 2000 分钟（一次构建约 8–15 分钟）。

### A1. 把仓库推上 GitHub

本仓库已经 `git init` + 完成首次提交（分支 `main`，123 个文件）。你只需要建远程并推：

```powershell
cd C:\Users\Administrator\Documents\deepseek-harness\default-workspace\MXrader-DFM

# 在 github.com 上新建一个仓库（空仓库，不要勾 README/.gitignore），然后：
git remote add origin https://github.com/<你的账号>/MXrader-DFM.git
git push -u origin main
```

推送时会要密码 —— GitHub 早就不能用账号密码了，**要填 Personal Access Token**：
GitHub → Settings → Developer settings → Personal access tokens → Fine-grained tokens
→ 勾 `Contents: Read and write`（或 classic token 勾 `repo`）→ 生成，把那一串当密码用。

### A2. 跑流水线

仓库页 → **Actions** → 左侧 **iOS IPA** → 右上 **Run workflow** → Run。

流水线（`.github/workflows/ios.yml`）做这些事：

```
macos-15 runner
 ├ 装 Rust + 三个 iOS target
 ├ ./scripts/build_rust.sh          → 交叉编译出 libbattle_proxy.a（含模拟器切片，lipo 成 universal）
 ├ brew install xcodegen && xcodegen generate
 ├ xcodebuild -sdk iphoneos Release CODE_SIGNING_ALLOWED=NO
 ├ ./scripts/make_unsigned_ipa.sh   → 组装 Payload/ + PkgInfo → zip 成 .ipa
 └ 上传 Artifacts: MXrader-unsigned-ipa
```

### A3. 下载并安装

Actions 页面 → 那次运行 → 底部 **Artifacts** → 下载 `MXrader-unsigned-ipa`（里面是 zip，解开得到 `.ipa`）。

然后按下面的「拿到 IPA 之后」一节做。

### A4. 第一次大概率会失败，这是预期的

* `core/` 那 17k 行 Rust **在本机只编译到一半**（依赖树全过，剩 30 个接口级错误正在修）；
* `ios/` 那 13 个 Swift 文件**从来没在 macOS 上编过** —— Swift 的类型错误只有在 Xcode 里才暴露；
* 也可能踩到 `xcodegen` 的 `sources: ../web` 是否被正确当成资源目录。

流水线失败时会把构建日志打包成 `build-logs` 一起上传。**把日志贴回来我继续修** ——
这一步本来就是我这边缺的"macOS 编译环"，CI 把这个环补上了。

---

## 路径 B · 你有 Mac（或租一台云 Mac）

一条命令：

```bash
git clone <你的仓库> && cd MXrader-DFM
brew install xcodegen
./scripts/build_rust.sh                 # 出 core/target/ios/libbattle_proxy.a
./scripts/package_ipa.sh                # 出 dist/BattleReceiverOpen-mxrader-r39.ipa（会自动调 make_unsigned_ipa）
./scripts/verify_structure.sh dist/*.ipa "<样本.ipa>"   # 可选：与样本逐项对比结构
```

`package_ipa.sh` 走 `xcodebuild archive` + `DEVELOPMENT_TEAM`/`CODE_SIGN_IDENTITY`
（如果你有付费开发者账号，直接出**已签名**的 IPA，1 年有效）；没填就走未签名路径。

云 Mac 按小时租（MacinCloud、MacStadium、AWS EC2 mac 实例）都行，装个 Xcode 就能编。

---

## 拿到 IPA 之后：侧载

未签名 IPA 必须由侧载工具用**你自己的 Apple ID** 重新签名：

1. PC 上装 **Sideloadly**（sideloadly.io）+ iTunes 驱动（它要跟 iPhone 通信）；
2. iPhone 用线连 PC，手机上点「信任此电脑」；
3. 把 `.ipa` 拖进 Sideloadly，填 Apple ID，点 Start；
4. 手机上：设置 → 通用 → VPN 与设备管理 → 信任该开发者；
5. 打开 app → **务必允许「本地网络」权限**（不给的话 B 机连不上它的 SOCKS5）；
6. 免费 Apple ID 签的 app **7 天过期**，到期重签一次；有开发者账号则 1 年。
   越狱设备用 **TrollStore** 可以免签名直装。

---

## 装好之后立刻能验证什么

app 起来后**不需要游戏流量**就该看到：

* 启动页 5 秒内走到「端口已绑定，正在等待雷达页面」；
* 出现一张**空的雷达地图**（说明 `web/` 资源打进去了、Rust 的静态服务在工作）；
* 配对页能生成二维码（Hiddify 配置）。

如果地图是白的 → `web/` 没进 bundle，检查 `ios/project.yml` 的
`sources: ../web (type: folder, buildPhase: resources)`；
如果卡在启动页 → 端口被占或 Rust 没链进去。

**真正出玩家数据**还要走 [GETTING_STARTED.md](GETTING_STARTED.md) 第 4–6 步
（B 机代理 → 抓包 → 标定 profile → 标定地图）。---

## 常见坑

| 现象 | 原因 |
|---|---|
| CI 里 `build_rust.sh: /bin/bash^M` | 仓库换行符又变成 CRLF 了 —— `.gitattributes` 已强制 LF，别删它 |
| `xcodebuild: error: SDK "iphoneos" cannot be located` | runner 镜像不对，把 `runs-on` 换成 `macos-15` / `macos-14` |
| `libbattle_proxy.a` 找不到 | `build_rust.sh` 只在 macOS 上跑；确认它在 `core/target/ios/` |
| 装上了但打开闪退 | 看 Xcode → Devices 的崩溃日志；多半是 `battle_proxy_start` 返回了 `failed`，把它的 JSON 打出来 |
| 雷达页面 404 | `web_root` 没传对：Swift 必须把 `Bundle.main.resourcePath + "/web"` 塞进配置 JSON 的 `web_root` 字段 |
