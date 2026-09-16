//! kairos 的 macOS 殼。
//!
//! 階段零只做四件事：不佔 Dock、關 App Nap、選單列放一個圖示與 Quit、
//! 啟動時印出核心的時間基準證明兩層接上了。面板、畫面、聲音都在後面的階段。

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSActivityOptions, NSObjectProtocol, NSProcessInfo, NSString};

use kairos_core::time::{HostTime, Timebase};

/// App Nap 的活動 token。它被 drop 就等於結束活動、App Nap 重新生效，
/// 所以必須活到程式結束。
struct AppNapGuard {
    _activity: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

fn disable_app_nap() -> AppNapGuard {
    let reason = NSString::from_str("kairos 需要精確的計時器與網路時序");
    // UserInitiated 但允許系統閒置睡眠：我們要的是不被節流，不是阻止合蓋休眠。
    // LatencyCritical 額外告訴系統這個程式對排程延遲敏感。
    let options = NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
        | NSActivityOptions::LatencyCritical;
    let activity = NSProcessInfo::processInfo().beginActivityWithOptions_reason(options, &reason);
    AppNapGuard {
        _activity: activity,
    }
}

fn build_status_item(mtm: MainThreadMarker) -> Retained<NSStatusItem> {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        button.setTitle(&NSString::from_str("⏱"));
    }

    let menu = NSMenu::new(mtm);
    // SAFETY: `terminate:` 是 NSApplication 的公開 selector；
    // target 留空時沿 responder chain 送到 NSApp，由它結束程式。
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit kairos"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    menu.addItem(&quit);
    item.setMenu(Some(&menu));
    item
}

fn main() {
    let mtm = MainThreadMarker::new().expect("kairos 必須在主執行緒啟動");

    let tb = Timebase::get();
    let now = HostTime::now();
    eprintln!(
        "kairos 啟動：timebase {}/{}，HostTime::now() = {} tick（開機後 {:.3} 秒）",
        tb.numer(),
        tb.denom(),
        now.ticks(),
        now.as_nanos() as f64 / 1e9,
    );

    let app = NSApplication::sharedApplication(mtm);
    // 不佔 Dock、不出現在 Cmd-Tab。打包後由 Info.plist 的 LSUIElement 決定；
    // 這裡再設一次，讓 `cargo run` 不打包也有同樣行為。
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let _app_nap = disable_app_nap();
    eprintln!("App Nap 已關閉（UserInitiatedAllowingIdleSystemSleep | LatencyCritical）");

    let _status_item = build_status_item(mtm);
    eprintln!("選單列圖示已建立，從選單 Quit 或 Cmd-Q 結束");

    app.run();
}
