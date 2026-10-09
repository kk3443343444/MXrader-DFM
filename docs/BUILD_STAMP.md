# 构建版本戳（一次构建的唯一标识）

> 背景：真机调试时每次构建的 `CFBundleVersion` 都是 `32`，看不出来用户装的是哪一版，
> 已经导致过一次误判。本文件说明现在这套戳是怎么生成的、三处分别长什么样、
> 以及"对不上"时该看 CI 的哪一步。

## 1. 一次构建产出的戳

`<MARKETING_VERSION> (<run_number>)`，其中 `MARKETING_VERSION = 2.3.7-<short_sha7>`：

```
2.3.7-a1b2c3d (57)
```

- `<short_sha7>`：`git rev-parse --short=7 HEAD`
- `<run_number>`：GitHub Actions 的 `github.run_number`（单调递增，永远不同）

## 2. 三处必须互相印证

| # | 位置 | 形态 | 谁注入的 |
|---|---|---|---|
| 1 | Release 资产名 | `BattleReceiverOpen-2.3.7-a1b2c3d-r57-unsigned.ipa` | `.github/workflows/ios.yml` 的 "Compute build stamp" 步骤（`$BATTLE_IPA_NAME`） |
| 2 | 启动页文案 | `v2.3.7-a1b2c3d (57)` | `xcodebuild MARKETING_VERSION=... CURRENT_PROJECT_VERSION=57` → `Info.plist` → `BattleSplashView` 的 `v\(version) (\(build))` |
| 3 | Rust 状态 JSON 的 `version` | `"version": "2.3.7-a1b2c3d (57)"` | `core/build.rs` 把 `$BATTLE_BUILD_STAMP` 变成编译期常量 `crate::VERSION`（`status_json()` / `battle_proxy_version()` 都用它） |

诊断页「运行摘要」里同时显示 **App 构建**（第 2 处）与 **核心版本**（第 3 处），
两者不一致就说明注入断了。

Release 的 tag 仍然是固定的 `ipa-latest`，只有资产名带戳。
（注意：同一个 tag 下每次构建会新增一个资产，旧的不会自动删除。）

## 3. 本地构建

不带任何环境变量时 `core/build.rs` 落回 `2.3.7-r39`，而 `ios/project.yml` 里的
`MARKETING_VERSION` / `CURRENT_PROJECT_VERSION` 仍是 `2.3.7` / `32`
（`Info.plist` 写的是 `$(MARKETING_VERSION)` / `$(CURRENT_PROJECT_VERSION)`，
由 Xcode 在 Process Info.plist 阶段展开，所以本机包展开出来就是老的 `2.3.7` / `32`）。
也就是说本机 Xcode 直接跑出来的包**没有**戳 —— 这是刻意的：本机开发者不需要定位某一次构建，
而 CI 必须能。

## 4. CI 怎么保证对得上

`.github/workflows/ios.yml` 里有一个专门的 `Verify build stamp` 步骤，构建完立刻校验：

1. `dist/$BATTLE_IPA_NAME` 这个文件确实存在；
2. `Info.plist` 的 `CFBundleShortVersionString` / `CFBundleVersion` 等于戳的两半；
3. `core/target/ios/libbattle_proxy.a` 的字节里能找到完整的戳字符串。

任一条不成立就直接让这个 job 红掉，不会发出一个"看不出来是哪一版"的 IPA。

## 5. 出问题时看哪一步

| 症状 | 看哪一步 | 常见原因 |
|---|---|---|
| 三处都没戳（还是 `2.3.7` / `32` / `2.3.7-r39`） | `Compute build stamp` | 该步骤失败或没跑到（`$GITHUB_ENV` 没写进去） |
| IPA 名有戳、启动页没有 | `Build app (unsigned)` | `MARKETING_VERSION` / `CURRENT_PROJECT_VERSION` 没传给 `xcodebuild`，或 build setting 被 project.yml 覆盖 |
| 前两处有戳、状态 JSON 没有 | `Build Rust static library (device arm64)` | 脚本那一步没拿到 `$BATTLE_BUILD_STAMP`；或 **cargo 复用了旧 target 缓存**（`core/build.rs` 已声明 `rerun-if-env-changed`，若仍复现就清 `core/target`） |
| `Verify build stamp` 报静态库里找不到戳 | 同上一行 | 同上：多半是缓存 |
