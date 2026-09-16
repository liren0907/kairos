#!/bin/sh
# 重畫 app 圖示並轉成 crates/kairos-app/Kairos.icns（這個 icns 進版控，bundle.sh 直接複製）。
# 平常不用跑；改了 make-icon.swift 才重跑，然後把 icns 一起 commit。
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
iconset="$root/target/icon/Kairos.iconset"
mkdir -p "$root/target/icon"
swift "$root/scripts/make-icon.swift" "$iconset"
iconutil -c icns "$iconset" -o "$root/crates/kairos-app/Kairos.icns"
echo "已建立 $root/crates/kairos-app/Kairos.icns"
