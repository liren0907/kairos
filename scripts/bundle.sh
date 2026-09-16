#!/bin/sh
# 把 kairos-app 組成 target/Kairos.app。
#
# 用法：scripts/bundle.sh [debug|release]（預設 release）
# 之後 `open target/Kairos.app` 即可啟動；Dock 不會出現圖示，由 Info.plist 的 LSUIElement 決定。
# 版本號從根目錄 Cargo.toml 的 [workspace.package] 讀，build 號用 git 的 commit 數。
# 要做給別人下載的 zip，用 scripts/release.sh（它會先呼叫這支）。

set -eu

profile="${1:-release}"
root="$(cd "$(dirname "$0")/.." && pwd)"
app="$root/target/Kairos.app"
contents="$app/Contents"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
build="$(git -C "$root" rev-list --count HEAD 2>/dev/null || echo 1)"
[ -n "$version" ] || { echo "Cargo.toml 裡讀不到 version" >&2; exit 2; }

case "$profile" in
    release) cargo build --release -p kairos-app ;;
    debug)   cargo build -p kairos-app ;;
    *) echo "未知的 profile：$profile（用 debug 或 release）" >&2; exit 2 ;;
esac

rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"
cp "$root/target/$profile/kairos" "$contents/MacOS/kairos"
cp "$root/crates/kairos-app/Info.plist" "$contents/Info.plist"
cp "$root/crates/kairos-app/Kairos.icns" "$contents/Resources/Kairos.icns"
printf 'APPL????' > "$contents/PkgInfo"
/usr/libexec/PlistBuddy \
    -c "Set :CFBundleShortVersionString $version" \
    -c "Set :CFBundleVersion $build" \
    "$contents/Info.plist"

# ad-hoc 簽章：本機執行不需要開發者憑證，但 Apple Silicon 上完全沒簽的 binary 會被系統直接擋掉。
codesign --force --sign - "$app" >/dev/null 2>&1

echo "已建立 $app（$version，build $build）"
