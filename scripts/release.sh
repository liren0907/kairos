#!/bin/sh
# 做一個可以丟上 GitHub Releases 的 zip。
#
# 用法：scripts/release.sh
# 流程：release build → target/Kairos.app → 簽章 → 壓成 target/dist/Kairos-<版本>-macos.zip → 印出發佈指令。
#
# 簽章看鑰匙圈：有「Developer ID Application」憑證就用它簽（hardened runtime 加時間戳），
# 再看 notarytool 有沒有存好的設定檔（預設名稱 kairos，可用 KAIROS_NOTARY_PROFILE 改）
# 有就順便公證加 staple；什麼都沒有就維持 bundle.sh 的 ad-hoc 簽章，
# 下載的人第一次打開要到「系統設定 › 隱私權與安全性」按「仍要打開」（README 有寫）。
#
# 這支不碰 git 也不碰 GitHub；最後印出的 gh 指令由你自己下。

set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
app="$root/target/Kairos.app"
dist="$root/target/dist"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
zip="$dist/Kairos-$version-macos.zip"
notes="$dist/release-notes.md"

"$root/scripts/bundle.sh" release

identity="$(security find-identity -v -p codesigning 2>/dev/null \
    | awk -F'"' '/Developer ID Application/ { print $2; exit }')"
notarized=no
if [ -n "$identity" ]; then
    echo "用 Developer ID 簽章：$identity"
    codesign --force --options runtime --timestamp --sign "$identity" "$app"
    profile="${KAIROS_NOTARY_PROFILE:-kairos}"
    if xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1; then
        echo "公證中（設定檔 $profile）…"
        tmp="$dist/notarize-$version.zip"
        mkdir -p "$dist"
        rm -f "$tmp"
        ditto -c -k --keepParent "$app" "$tmp"
        xcrun notarytool submit "$tmp" --keychain-profile "$profile" --wait
        xcrun stapler staple "$app"
        rm -f "$tmp"
        notarized=yes
    else
        echo "鑰匙圈沒有 notarytool 設定檔「$profile」，跳過公證。要做就先跑一次："
        echo "  xcrun notarytool store-credentials $profile --apple-id <Apple ID> --team-id <Team ID>"
    fi
else
    echo "鑰匙圈沒有 Developer ID，維持 ad-hoc 簽章（下載的人第一次要按「仍要打開」）。"
fi

codesign --verify --deep --strict "$app"
# spctl 對 ad-hoc 一定說 rejected，只是印出來讓你知道 Gatekeeper 會怎麼看它。
spctl --assess --type execute "$app" 2>&1 | sed 's/^/Gatekeeper：/' || true

mkdir -p "$dist"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"

if [ "$notarized" = yes ]; then
    open_note="下載後解壓縮，把 Kairos 拖進「應用程式」，雙擊就能開。"
else
    open_note="下載後解壓縮，把 Kairos 拖進「應用程式」。第一次打開會被 macOS 擋下：按「完成」，到「系統設定 › 隱私權與安全性」拉到最下面按「仍要打開」，再打開一次即可（macOS 14 可改用右鍵 › 打開）。"
fi
cat > "$notes" <<NOTES
Kairos $version（macOS 14 以上，Apple Silicon）。

$open_note

打開後 Dock 不會有圖示，看選單列右上角的碼錶圖示；設定在那個圖示的「設定…」裡。
完整說明見 README 的「下載與安裝」。
NOTES

size="$(du -h "$zip" | cut -f1)"
sha="$(shasum -a 256 "$zip" | cut -d' ' -f1)"
echo
echo "已建立 $zip（$size）"
echo "SHA-256 $sha"
echo
echo "發佈到 GitHub（tag 不存在會由 gh 建在目前的 HEAD 上）："
echo "  gh release create v$version \"$zip\" --title \"Kairos $version\" --notes-file \"$notes\""
echo "給她的網址：https://github.com/liren0907/kairos/releases/latest"
