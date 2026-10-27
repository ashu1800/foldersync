#!/usr/bin/env bash
# 版本号一致性检查。
#
# 三处必须同步，否则会出现「界面显示 0.1.0、安装包叫 0.2.0、updater 认为自己是 0.1.0」
# 这种自相矛盾的局面，客户端会反复提示更新同一个版本。
#
#   Cargo.toml                        workspace version
#   apps/desktop/src-tauri/tauri.conf.json   product version（决定安装包名与 updater 比较基准）
#   apps/desktop/package.json         前端版本（展示用）
#
# 用法：
#   ./scripts/check-version.sh           只检查
#   ./scripts/check-version.sh 0.2.0     检查并同步写入（发版前用这个）
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

want="${1:-}"

read_cargo() { grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"(.*)".*/\1/'; }
read_tauri() { grep -m1 '"version"' apps/desktop/src-tauri/tauri.conf.json | sed -E 's/.*"(.*)".*/\1/'; }
read_pkg()   { grep -m1 '"version"' apps/desktop/package.json | sed -E 's/.*"(.*)".*/\1/'; }

if [[ -n "$want" ]]; then
  echo "把三处版本同步为 $want"
  sed -i -E "0,/^version = \".*\"/s//version = \"$want\"/" Cargo.toml
  sed -i -E "0,/\"version\": \".*\"/s//\"version\": \"$want\"/" apps/desktop/src-tauri/tauri.conf.json
  sed -i -E "0,/\"version\": \".*\"/s//\"version\": \"$want\"/" apps/desktop/package.json
  # 依赖 workspace version 的 crate 会跟着变，锁文件需要重新生成
  cargo update --workspace --offline >/dev/null 2>&1 || true
fi

c=$(read_cargo); t=$(read_tauri); p=$(read_pkg)
echo "Cargo.toml:  $c"
echo "tauri.conf:  $t"
echo "package.json:$p"

if [[ "$c" != "$t" || "$c" != "$p" ]]; then
  echo "错误：三处版本号不一致" >&2
  exit 1
fi
echo "版本一致：$c"
