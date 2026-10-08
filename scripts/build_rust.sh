#!/usr/bin/env bash
# build_rust.sh — 交叉编译 Rust 核心为 iOS 静态库
#
# 产物：
#   core/target/ios/libbattle_proxy.a   （link 进 app 主二进制）
#   core/target/ios/battle_proxy.h      （C ABI 头，供 Xcode 引用）
#
# 必须在 macOS 上运行（需要 Xcode 的 clang/SDK）。与样本一致：Rust 以 staticlib
# 形式**静态链接进 app 主二进制**，所以最终 IPA 里没有 Frameworks/ 目录、没有独立
# dylib，主程序是单一 MH_EXECUTE，Swift 与 Rust 符号共存。
#
# 用法：
#   ./scripts/build_rust.sh                 # 只编真机 arm64（默认；CI 与真机构建都用这个）
#   ./scripts/build_rust.sh --with-sim      # 额外把 x86_64 模拟器切片 lipo 进来（Intel Mac 调试用）
#
# 为什么默认不编模拟器切片：`aarch64-apple-ios`（真机）与 `aarch64-apple-ios-sim`
# 是**同一个 arm64 架构**，lipo 无法把两个 arm64 切片合并成一个 fat 文件（会直接报错）。
# 想在 Apple Silicon 的模拟器上跑，直接用 rustup 装 aarch64-apple-ios-sim 并让 Xcode
# 走那个 target；本脚本只负责产出真机库 + 可选的 x86_64 模拟器切片。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
CORE="$ROOT/core"
OUT="$CORE/target/ios"
PROFILE="${PROFILE:-release}"
WITH_SIM=0

for arg in "$@"; do
  case "$arg" in
    --with-sim) WITH_SIM=1 ;;
    --device-only) WITH_SIM=0 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "未知参数：$arg（--help 看用法）" >&2; exit 2 ;;
  esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "错误：iOS 静态库只能在 macOS 上构建（当前 $(uname -s)）。" >&2
  echo "提示：core 的单元测试不需要 macOS：cd core && cargo test" >&2
  exit 1
fi

command -v cargo >/dev/null || { echo "错误：找不到 cargo，请先安装 Rust（rustup）。" >&2; exit 1; }

mkdir -p "$OUT"

# 注意：这里刻意不用 `[[ ... ]] && VAR=...` 这种写法。
# 条件是假的时候整条语句返回非零，在 `set -e` 下会**静默退出脚本**（没有输出、看不出原因）。
PROFILE_FLAG=()
if [[ "$PROFILE" == "release" ]]; then
  PROFILE_FLAG=(--release)
fi

TARGETS=(aarch64-apple-ios)
if [[ "$WITH_SIM" == "1" ]]; then
  TARGETS+=(x86_64-apple-ios)
fi

echo "==> 安装 iOS target：${TARGETS[*]}"
rustup target add "${TARGETS[@]}" >/dev/null

for t in "${TARGETS[@]}"; do
  echo "==> cargo build --lib --target $t ($PROFILE)"
  # `--lib` 很关键：不加的话 cargo 会连 src/bin/*.rs 一起编（battle_receiver /
  # battle_replay 是**可执行程序**），在 iOS target 下去链接两个跑不起来的命令行程序，
  # 既浪费时间又容易因为链接器环境失败。iOS 侧只要那个 staticlib。
  ( cd "$CORE" && cargo build --lib --target "$t" "${PROFILE_FLAG[@]}" )
  ls -la "$CORE/target/$t/$PROFILE/libbattle_proxy.a" | awk '{print "    " $5 " bytes  " $9}'
done

cp "$CORE/target/aarch64-apple-ios/$PROFILE/libbattle_proxy.a" "$OUT/libbattle_proxy.a"

if [[ "$WITH_SIM" == "1" ]]; then
  echo "==> 合并真机 arm64 + 模拟器 x86_64（架构不同，可以 lipo）"
  lipo -create \
    "$CORE/target/aarch64-apple-ios/$PROFILE/libbattle_proxy.a" \
    "$CORE/target/x86_64-apple-ios/$PROFILE/libbattle_proxy.a" \
    -output "$OUT/libbattle_proxy.a"
fi

cp "$CORE/include/battle_proxy.h" "$OUT/battle_proxy.h"

echo "==> 结果"
lipo -info "$OUT/libbattle_proxy.a" 2>/dev/null || file "$OUT/libbattle_proxy.a"
echo "静态库: $OUT/libbattle_proxy.a"
echo "头文件: $OUT/battle_proxy.h"
echo
echo "下一步：cd ios && xcodegen generate && open BattleReceiverOpen.xcodeproj"
echo "链接所需的系统库（project.yml 已配）：-lc++ -lresolv -framework Security -framework Network -framework WebKit"
