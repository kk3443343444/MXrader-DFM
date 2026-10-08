#!/usr/bin/env bash
# fetch_tiles.sh — 准备雷达底图瓦片
#
# 雷达前端从 /tiles/{map}/{z}/{x}/{y}.png 取图（见 web/tiles/README.md）。
# 本脚本给出两条合法路径，**默认不下载任何东西**：
#
#   A. 自建瓦片：用你自己的地图截图/卫星图切片，放进 web/tiles/<map>/…
#   B. 使用你所在地区法律与授权允许的公开瓦片源，自行决定并承担使用条款。
#
# 用法：
#   ./fetch_tiles.sh --check                     # 只检查本地瓦片目录是否齐全
#   ./fetch_tiles.sh --from <模板URL> --map ZeroDam --z 2 3 4
#                                                # 按模板抓取（模板含 {z}/{x}/{y}）
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
TILES="$ROOT/web/tiles"
MAPS="ZeroDam Layali Bakesh SpaceCity"

usage() { sed -n '2,14p' "$0"; exit 0; }

check() {
  echo "本地瓦片目录：$TILES"
  local missing=0
  for m in $MAPS; do
    local n
    n=$(find "$TILES/$m" -name '*.png' 2>/dev/null | wc -l | tr -d ' ')
    if [[ "$n" -gt 0 ]]; then
      echo "  [ok]   $m: $n 张"
    else
      echo "  [ -- ] $m: 缺失（雷达会用离线占位图，不影响运行）"
      missing=$((missing+1))
    fi
  done
  echo
  if [[ "$missing" -gt 0 ]]; then
    echo "提示：缺瓦片时雷达仍可工作（坐标/朝向/弹道都能显示），只是没有建筑底图。"
    echo "      校准 maps.json 的 origin/scale/yaw_offset_deg 需要底图，建议至少准备一张。"
  fi
}

fetch() {
  local template="$1" map="$2"; shift 2
  [[ -n "$template" && -n "$map" ]] || usage
  [[ "$template" == *"{z}"* && "$template" == *"{x}"* && "$template" == *"{y}"* ]] || {
    echo "错误：模板必须包含 {z}/{x}/{y}" >&2; exit 1; }

  for z in "$@"; do
    local n=$((2 ** z))
    local dir="$TILES/$map/$z"
    mkdir -p "$dir"
    echo "==> $map zoom=$z（$((n*n)) 张）"
    for ((x=0; x<n; x++)); do
      for ((y=0; y<n; y++)); do
        local out="$dir/$x/$y.png"
        mkdir -p "$dir/$x"
        [[ -s "$out" ]] && continue
        local url="${template//\{z\}/$z}"
        url="${url//\{x\}/$x}"
        url="${url//\{y\}/$y}"
        curl -fsS --max-time 20 -o "$out" "$url" || { rm -f "$out"; }
      done
    done
  done
  echo "完成。建议接着校准 web/maps.json 的 origin_x/origin_y/scale/yaw_offset_deg。"
}

case "${1:---check}" in
  --check) check ;;
  --help|-h) usage ;;
  --from) shift; fetch "$@" ;;
  *) usage ;;
esac
