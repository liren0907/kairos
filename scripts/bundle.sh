#!/bin/sh
# 把 kairos-app 組成 target/Kairos.app。
#
# 用法：scripts/bundle.sh [debug|release]（預設 release）
# 之後 `open target/Kairos.app` 即可啟動；Dock 不會出現圖示，由 Info.plist 的 LSUIElement 決定。

set -eu

profile="${1:-release}"
root="$(cd "$(dirname "$0")/.." && pwd)"
app="$root/target/Kairos.app"
contents="$app/Contents"

case "$profile" in
    release) cargo build --release -p kairos-app ;;
    debug)   cargo build -p kairos-app ;;
    *) echo "未知的 profile：$profile（用 debug 或 release）" >&2; exit 2 ;;
esac

rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"
cp "$root/target/$profile/kairos" "$contents/MacOS/kairos"
cp "$root/crates/kairos-app/Info.plist" "$contents/Info.plist"
printf 'APPL????' > "$contents/PkgInfo"

# ad-hoc 簽章：本機執行不需要開發者憑證，但沒簽的話部分系統功能會拒絕。
codesign --force --sign - "$app" >/dev/null 2>&1

echo "已建立 $app"
