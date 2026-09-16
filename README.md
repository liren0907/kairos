# kairos

macOS 專用的精準浮動時鐘：浮在所有視窗之上，誠實顯示 ± 不確定度，並以可預測的視聽節拍協助對準目標時刻。

Rust 加 objc2，最低支援 macOS 14，Apple Silicon。

## 下載與安裝

1. 到 [最新版本](https://github.com/liren0907/kairos/releases/latest) 下載 `Kairos-<版本>-macos.zip`。
2. Safari 會自動解壓縮；沒有的話對 zip 檔按兩下。得到一個 `Kairos.app`。
3. 把 `Kairos.app` 拖進「應用程式」資料夾，然後按兩下打開。

打開後 **Dock 上不會有圖示**，請看螢幕右上角選單列的碼錶圖示 ⏱，時鐘面板會浮在桌面上。所有設定都在那個圖示的「設定…」裡。

### 第一次打開被擋下來？

這個 app 沒有經過 Apple 公證，第一次打開 macOS 會說「Apple 無法驗證 Kairos 是否含有惡意軟體」。這是正常的，照下面做一次就好，之後不會再問：

1. 在那個對話框按「完成」（不要按「移到垃圾桶」）。
2. 打開「系統設定」，進「隱私權與安全性」，拉到最下面。
3. 會看到一行「已阻擋 Kairos 以保護你的 Mac」，按旁邊的「仍要打開」。
4. 再確認一次「打開」，輸入你的 Mac 密碼或用 Touch ID。

macOS 14 的話比較簡單：對 `Kairos.app` 按右鍵，選「打開」，再按一次「打開」就行。

每次下載新版本都要重做一次上面的步驟。

## 更新

到 [最新版本](https://github.com/liren0907/kairos/releases/latest) 下載新的 zip，先從選單列碼錶圖示選「結束」，把新的 `Kairos.app` 拖進「應用程式」覆蓋舊的，再打開。你的設定會保留。

## 移除

從選單列碼錶圖示選「結束」，把「應用程式」裡的 `Kairos.app` 丟到垃圾桶。設定檔在 `~/Library/Application Support/kairos/`，要清乾淨就連這個資料夾一起刪。

## 開發

```bash
cargo test                # 跑測試
cargo run -p kairos-app   # 直接跑，不打包
scripts/bundle.sh         # 組成 target/Kairos.app（release），open 它即可
scripts/release.sh        # 打包成 target/dist/Kairos-<版本>-macos.zip，最後印出 gh release 指令
scripts/make-icon.sh      # 重畫 app 圖示（改了 scripts/make-icon.swift 才需要）
```

版本號只改根目錄 `Cargo.toml` 的 `[workspace.package] version`，bundle 時會填進 Info.plist。

主題檔 `~/Library/Application Support/kairos/theme.toml` 第一次啟動會自動建立，裡面每個鍵都有註解，存檔即時生效；「設定…」視窗改的值另外存在同資料夾的 `settings.toml`，只記跟主題檔不一樣的鍵。
