#!/usr/bin/env bash
# make_unsigned_ipa.sh — 把一个已构建的 .app 打成 IPA（不做签名）
#
# 为什么"不签名也能用"：AltStore / Sideloadly 这类侧载工具本来就是用你自己的
# Apple ID 在本地重新签名的，它们需要一个**结构完整但未签名**的 IPA 作为输入。
# 这样就不需要 Apple 开发者证书，也不需要把证书塞进 CI。
#
# 用法：
#   ./scripts/make_unsigned_ipa.sh <path/to/BattleReceiverOpen.app> [out.ipa]
#
# 产物结构与参考样本一致：
#   Payload/BattleReceiverOpen.app/{主二进制, Info.plist, PkgInfo, web/, 图标…}
set -euo pipefail

APP="${1:?用法: make_unsigned_ipa.sh <App.app> [out.ipa]}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

[[ -d "$APP" ]] || { echo "错误：$APP 不是目录/.app" >&2; exit 1; }
NAME="$(basename "$APP" .app)"
OUT="${2:-$ROOT/dist/${NAME}-unsigned.ipa}"

# 把 OUT 绝对化，再做任何 cd。
# 坑：脚本最后要 `( cd "$STAGE" && zip "$OUT" Payload )`；如果调用方传的是相对路径
# （CI 里我们传的就是 "dist/xxx.ipa"），这个相对路径会在 cd 之后被解析成
# "$STAGE/dist/xxx.ipa" —— 目录不存在，于是 zip 报
# "I/O error: No such file or directory / Could not create output file"。
mkdir -p "$(dirname "$OUT")"
OUT="$(cd "$(dirname "$OUT")" && pwd)/$(basename "$OUT")"

mkdir -p "$(dirname "$OUT")"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

echo "==> 拷进 Payload/"
mkdir -p "$STAGE/Payload"
cp -R "$APP" "$STAGE/Payload/$NAME.app"
APP_DIR="$STAGE/Payload/$NAME.app"

# 1) 清掉构建残留的签名（侧载工具会重新签；留着旧签名反而会冲突）
echo "==> 移除旧签名"
rm -rf "$APP_DIR/_CodeSignature" "$APP_DIR/CodeResources" "$APP_DIR/embedded.mobileprovision"
find "$APP_DIR" -name '.DS_Store' -delete 2>/dev/null || true
if command -v codesign >/dev/null 2>&1; then
  codesign --remove-signature "$APP_DIR" 2>/dev/null || true
fi

# 2) PkgInfo（样本里是 APPL????，很多打包脚本会漏，某些侧载工具会挑）
printf 'APPL????' > "$APP_DIR/PkgInfo"

# 3) 前端资源必须在，否则雷达页会 404（Rust 侧从 bundle 的 web/ 读文件）
if [[ -d "$APP_DIR/web" ]]; then
  html_count=$(find "$APP_DIR/web" -maxdepth 1 -name '*.html' | wc -l | tr -d ' ')
  echo "==> 前端资源：$html_count 个 html（$(du -sh "$APP_DIR/web" | cut -f1)）"
  [[ "$html_count" -ge 1 ]] || { echo "错误：web/ 里没有 index.html" >&2; exit 1; }
else
  echo "警告：app 里没有 web/ 目录 —— 雷达页面会 404。" >&2
  echo "      检查 ios/project.yml 里的 sources: ../web (type: folder, buildPhase: resources)" >&2
fi

# 4) 关键文件自检
for f in "$NAME" Info.plist PkgInfo; do
  [[ -e "$APP_DIR/$f" ]] || { echo "错误：app 里缺少 $f" >&2; exit 1; }
done
echo "==> 主二进制：$(du -h "$APP_DIR/$NAME" | cut -f1)"
if command -v lipo >/dev/null 2>&1; then
  echo "==> 架构：$(lipo -archs "$APP_DIR/$NAME")"
fi

# 5) 打包（zip 的目录顺序无所谓，但必须叫 Payload/）
echo "==> 打 zip"
rm -f "$OUT"
( cd "$STAGE" && zip -qry "$OUT" Payload )

echo
echo "IPA: $OUT  ($(du -h "$OUT" | cut -f1))"
echo
echo "下一步（在 Windows/macOS 上都可以）："
echo "  1) 用 Sideloadly / AltStore 载入这个 IPA，填你的 Apple ID，签完装到 iPhone；"
echo "  2) 手机上：设置 -> 通用 -> VPN与设备管理 -> 信任该开发者；"
echo "  3) 打开 app，务必允许「本地网络」权限 —— 否则 B 机连不上它的 SOCKS5。"
