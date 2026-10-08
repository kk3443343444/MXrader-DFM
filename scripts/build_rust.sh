#!/usr/bin/env bash
# build_rust.sh — 交叉编译 Rust 核心为 iOS 静态库
#
# 产物：
#   core/target/ios/libbattle_proxy.a   （真机 arm64 + 模拟器 arm64/x86_64 的 universal 库）
#   core/target/ios/battle_proxy.h      （C ABI 头文件，供 Xcode 引用）
#
# 必须在 macOS 上运行（需要 Xcode 的 clang/SDK）。
# 与样本一致：Rust 以 staticlib 形式**静态链接进 app 主二进制**，
# 因此最终 IPA 里没有 Frameworks/ 目录、没有独立 dylib，
# 主程序是单一 MH_EXECUTE，Swift 与 Rust 符号共存。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
CORE="$ROOT/core"
OUT="$CORE/target/ios"
PROFILE="${PROFILE:-release}"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "错误：iOS 静态库只能在 macOS 上构建（当前 $(uname -s)）。" >&2
  echo "提示：可以在 macOS CI 上跑本脚本，或仅在 PC 上开发 core 的单元测试：cd core && cargo test" >&2
  exit 1
fi

command -v cargo >/dev/null || { echo "错误：找不到 cargo，请先安装 Rust（rustup）。" >&2; exit 1; }

echo "==> 安装 iOS target（若缺失）"
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios >/dev/null

mkdir -p "$OUT"

TARGET_DIR_FLAG=()
if [[ "$PROFILE" == "release" ]]; then
  PROFILE_FLAG=(--release)
else
  PROFILE_FLAG=()
fi

build_one() {
  local target="$1"
  echo "==> cargo build --target $target ($PROFILE)"
  ( cd "$CORE" && cargo build --target "$target" "${PROFILE_FLAG[@]}" )
  echo "    $(ls -la "$CORE/target/$target/$PROFILE/libbattle_proxy.a" | awk '{print $5, $9}')"
}

build_one aarch64-apple-ios
build_one aarch64-apple-ios-sim
build_one x86_64-apple-ios

echo "==> 合并模拟器切片（arm64 + x86_64）"
SIM_LIB="$OUT/libbattle_proxy-sim.a"
lipo -create \
  "$CORE/target/aarch64-apple-ios-sim/$PROFILE/libbattle_proxy.a" \
  "$CORE/target/x86_64-apple-ios/$PROFILE/libbattle_proxy.a" \
  -output "$SIM_LIB"

echo "==> 合并真机 + 模拟器（universal）"
lipo -create \
  "$CORE/target/aarch64-apple-ios/$PROFILE/libbattle_proxy.a" \
  "$SIM_LIB" \
  -output "$OUT/libbattle_proxy.a"

cp "$CORE/include/battle_proxy.h" "$OUT/battle_proxy.h"

echo "==> 结果"
lipo -info "$OUT/libbattle_proxy.a"
echo "静态库: $OUT/libbattle_proxy.a"
echo "头文件: $OUT/battle_proxy.h"
echo
echo "下一步：cd ios && xcodegen generate && open BattleReceiverOpen.xcodeproj"
echo "提示：链接时需要这些系统库（project.yml 已配置）："
echo "      -lc++ -lresolv -framework Security -framework Network -framework SystemConfiguration"
