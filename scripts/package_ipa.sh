#!/usr/bin/env bash
# package_ipa.sh — 打包成与样本同结构的 IPA
#
# 参考样本结构（iPhone-MXrader-r39-3D转向修正.ipa）：
#
#   Payload/BattleReceiverOpen.app/
#       BattleReceiverOpen          主二进制（arm64 MH_EXECUTE，Swift+Rust 静态链接）
#       Info.plist                  CFBundleIdentifier=com.mxrader.monstervision
#       PkgInfo                     "APPL????"
#       MXIcon-*.png / Icon-*.png   图标
#       MXMark.png / BattleMark.png / battle-network-bg.png
#       （无 Frameworks/、无 PlugIns/、无 embedded.mobileprovision —— 由签名工具注入）
#
# 用法：
#   ./package_ipa.sh                       # 用 xcodebuild 构建并打包（需要签名凭据）
#   ./package_ipa.sh --app <path/to/App.app>  # 直接拿已有的 .app 打包
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
IOS="$ROOT/ios"
DIST="$ROOT/dist"
BUNDLE_ID="com.mxrader.monstervision"
APP_NAME="BattleReceiverOpen"
SCHEME="$APP_NAME"
CONFIG="${CONFIG:-Release}"
TEAM="${DEVELOPMENT_TEAM:-}"
IDENTITY="${CODESIGN_IDENTITY:-}"
PROFILE="${PROVISIONING_PROFILE:-}"

APP_PATH=""
if [[ "${1:-}" == "--app" ]]; then
  APP_PATH="${2:?--app 需要一个 .app 路径}"
fi

mkdir -p "$DIST"

if [[ -z "$APP_PATH" ]]; then
  [[ "$(uname -s)" == "Darwin" ]] || { echo "错误：构建 IPA 需要 macOS + Xcode。" >&2; exit 1; }
  command -v xcodebuild >/dev/null || { echo "错误：找不到 xcodebuild。" >&2; exit 1; }

  # 1) Rust 静态库（幂等）
  "$HERE/build_rust.sh"

  # 2) 生成 Xcode 工程（如果还没有）
  if [[ ! -d "$IOS/$APP_NAME.xcodeproj" ]]; then
    command -v xcodegen >/dev/null || { echo "错误：需要 xcodegen（brew install xcodegen）。" >&2; exit 1; }
    ( cd "$IOS" && xcodegen generate )
  fi

  # 3) 归档
  ARCHIVE="$DIST/$APP_NAME.xcarchive"
  echo "==> xcodebuild archive"
  xcodebuild \
    -project "$IOS/$APP_NAME.xcodeproj" \
    -scheme "$SCHEME" \
    -configuration "$CONFIG" \
    -sdk iphoneos \
    -archivePath "$ARCHIVE" \
    ${TEAM:+DEVELOPMENT_TEAM="$TEAM"} \
    ${IDENTITY:+CODE_SIGN_IDENTITY="$IDENTITY"} \
    ${PROFILE:+PROVISIONING_PROFILE_SPECIFIER="$PROFILE"} \
    CODE_SIGN_STYLE=Automatic \
    -quiet \
    archive

  APP_SRC="$ARCHIVE/Products/Applications/$APP_NAME.app"
  [[ -d "$APP_SRC" ]] || { echo "错误：归档里找不到 $APP_NAME.app" >&2; exit 1; }

  # 4) 组装 Payload
  STAGE="$DIST/stage"
  rm -rf "$STAGE"; mkdir -p "$STAGE/Payload"
  cp -R "$APP_SRC" "$STAGE/Payload/"
  APP_PATH="$STAGE/Payload/$APP_NAME.app"
fi

[[ -d "$APP_PATH" ]] || { echo "错误：$APP_PATH 不是 .app" >&2; exit 1; }
APP_DIR="$APP_PATH"

# 5) PkgInfo（样本里是 APPL????，很多打包脚本会漏）
printf 'APPL????' > "$APP_DIR/PkgInfo"

# 6) 补齐 entitlements（如果构建产物里没有）
if [[ ! -f "$APP_DIR/embedded.mobileprovision" && -n "$PROFILE" ]]; then
  echo "警告：没有 embedded.mobileprovision —— 侧载工具（AltStore/Sideloadly/TrollStore）会自行注入。" >&2
fi

# 7) 重签名（可选：给 side-load 前的临时签名）
if [[ -n "$IDENTITY" ]]; then
  echo "==> codesign ($IDENTITY)"
  codesign --force --deep --sign "$IDENTITY" \
    ${PROFILE:+--profile "$PROFILE"} \
    --entitlements "$IOS/$APP_NAME/$APP_NAME.entitlements" \
    "$APP_DIR"
fi

# 8) 打 zip 并把后缀改成 .ipa
OUT="$DIST/${APP_NAME}-mxrader-r39.ipa"
rm -f "$OUT"
( cd "$(dirname "$APP_DIR")/.." && zip -qry "$OUT" Payload )

echo "==> IPA: $OUT"
echo "==> 校验结构："
unzip -l "$OUT" | sed -n '1,40p'
echo
echo "==> 与样本结构对比（主二进制 / Info.plist / 无 Frameworks）："
"$HERE/verify_structure.sh" "$OUT" || true
