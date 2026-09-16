//! kairos 的 macOS 殼。
//!
//! 階段零：不佔 Dock、關 App Nap、選單列圖示與 Quit。
//! 階段一：背景取樣執行緒、選單裡的狀態列每秒刷新、睡眠喚醒後立刻重新取樣。
//! 階段二：浮動面板——毛玻璃、顯示連結每格更新、字形圖集數字、主題熱重載、全域熱鍵。
//! 階段三：節拍——拍點表、面板節拍區、螢幕邊緣光暈、cpal 滴答聲、每螢幕的顯示提前量。
//! 階段四：目標時刻——原生目標面板、狀態機自動量測與鎖定、凍結取樣、反應時間校正。
//!
//! 這一層只做接線，邏輯都在 `kairos-core` 與各模組。

mod atlas;
mod audio;
mod beat_view;
mod calibrate;
mod controller;
mod glow;
mod hotkey;
mod panel;
mod target_panel;
mod target_store;
mod text;
mod theme;

use std::ptr::NonNull;

use block2::RcBlock;
use dispatch2::MainThreadBound;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSControlStateValueOn, NSMenu, NSMenuItem,
    NSScreen, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWorkspace,
    NSWorkspaceDidWakeNotification,
};
use objc2_foundation::{
    NSActivityOptions, NSNotification, NSObjectProtocol, NSProcessInfo, NSRunLoop,
    NSRunLoopCommonModes, NSString, NSTimer,
};

use kairos_core::sync::{SamplerConfig, SamplerHandle};
use kairos_core::time::{HostTime, Timebase, system_theta_ns};

use controller::{Controller, MenuItems};

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

/// 選單裡的兩列資訊：狀態列與細節列。停用的選單項是 macOS 資訊列的標準長相。
struct StatusRows {
    primary: Retained<NSMenuItem>,
    detail: Retained<NSMenuItem>,
}

impl StatusRows {
    fn set(&self, primary: &str, detail: &str) {
        self.primary.setTitle(&NSString::from_str(primary));
        self.detail.setTitle(&NSString::from_str(detail));
    }
}

fn info_row(mtm: MainThreadMarker, title: &str) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    item.setEnabled(false);
    item
}

/// 指向控制器的動作項。
fn action_item(
    mtm: MainThreadMarker,
    title: &str,
    action: objc2::runtime::Sel,
    target: &AnyObject,
) -> Retained<NSMenuItem> {
    // SAFETY: selector 是 Controller 在 define_class! 裡定義的方法；target 型別正確。
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(""),
        )
    };
    // SAFETY: target 活到程式結束（main 持有 Retained<Controller>）。
    unsafe { item.setTarget(Some(target)) };
    item
}

fn build_status_item(
    mtm: MainThreadMarker,
    controller: &Controller,
) -> (Retained<NSStatusItem>, StatusRows, MenuItems) {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        button.setTitle(&NSString::from_str("⏱"));
    }

    let menu = NSMenu::new(mtm);
    // 自己管啟用狀態：「停止節拍」只在節拍進行中可按。
    menu.setAutoenablesItems(false);
    let rows = StatusRows {
        primary: info_row(mtm, "標準時間：校時中…"),
        detail: info_row(mtm, ""),
    };
    menu.addItem(&rows.primary);
    menu.addItem(&rows.detail);
    menu.addItem(&NSMenuItem::separatorItem(mtm));

    let target: &AnyObject = controller;
    let items = MenuItems {
        panel: action_item(mtm, "隱藏面板", sel!(togglePanel:), target),
        click_through: action_item(mtm, "滑鼠穿透　⌃⌥⌘K", sel!(toggleClickThrough:), target),
        beat_start: action_item(mtm, "試聽節拍（5 秒後）", sel!(startDemoBeats:), target),
        beat_stop: action_item(mtm, "停止節拍", sel!(stopBeats:), target),
        sound: action_item(mtm, "節拍聲音", sel!(toggleSound:), target),
        glow: action_item(mtm, "邊緣光暈", sel!(toggleGlow:), target),
        clear_target: action_item(mtm, "解除目標", sel!(clearTarget:), target),
    };
    menu.addItem(&items.panel);
    menu.addItem(&items.click_through);
    menu.addItem(&action_item(
        mtm,
        "重新載入主題",
        sel!(reloadTheme:),
        target,
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));

    menu.addItem(&action_item(
        mtm,
        "目標時刻…",
        sel!(showTargetPanel:),
        target,
    ));
    items.clear_target.setEnabled(false);
    menu.addItem(&items.clear_target);
    menu.addItem(&action_item(mtm, "立刻量測", sel!(measureNow:), target));
    menu.addItem(&action_item(
        mtm,
        "校正反應時間…",
        sel!(startCalibration:),
        target,
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));

    items.beat_stop.setEnabled(false);
    items.sound.setState(NSControlStateValueOn);
    items.glow.setState(NSControlStateValueOn);
    menu.addItem(&items.beat_start);
    menu.addItem(&items.beat_stop);
    menu.addItem(&items.sound);
    menu.addItem(&items.glow);
    menu.addItem(&NSMenuItem::separatorItem(mtm));

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
    (item, rows, items)
}

/// 每秒刷新選單文字。
///
/// 計時器要加進 `NSRunLoopCommonModes`：選單打開時 run loop 在事件追蹤模式，
/// 只排在 default mode 的計時器不會觸發，正好是使用者在看的時候凍住。
fn start_menu_refresh(
    mtm: MainThreadMarker,
    rows: StatusRows,
    sampler: SamplerHandle,
) -> Retained<NSTimer> {
    let rows = MainThreadBound::new(rows, mtm);
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let mtm = MainThreadMarker::new().expect("主執行緒 run loop 的計時器只會在主執行緒觸發");
        let (primary, detail) =
            text::menu_rows(&sampler.model(), HostTime::now(), system_theta_ns());
        rows.get(mtm).set(&primary, &detail);
    });
    // SAFETY: 閉包只捕捉 `MainThreadBound` 與 `SamplerHandle`，兩者都是 Send，
    // 滿足「block 必須 sendable」的要求。
    let timer = unsafe { NSTimer::timerWithTimeInterval_repeats_block(1.0, true, &block) };
    // SAFETY: 主執行緒的 run loop 與標準的 mode 常數。
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    // 不等第一秒，馬上填一次。
    timer.fire();
    timer
}

/// 訂閱系統喚醒通知：叫取樣執行緒立刻跑一輪。清樣本的判斷在核心的 `SleepDetector`，
/// 這裡只是加速。回傳的觀察者 token 被 drop 就等於取消訂閱，要活到程式結束。
fn observe_wake(sampler: SamplerHandle) -> Retained<ProtocolObject<dyn NSObjectProtocol>> {
    let block = RcBlock::new(move |_note: NonNull<NSNotification>| {
        eprintln!("收到 NSWorkspaceDidWakeNotification，要求立刻取樣");
        sampler.resample_now();
    });
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    // SAFETY: 通知名是 AppKit 的公開常數；object 與 queue 傳 None 代表任何來源、
    // 在通知送出的執行緒上執行；閉包只捕捉 `SamplerHandle`，是 Send。
    unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceDidWakeNotification),
            None,
            None,
            &block,
        )
    }
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

    let config = SamplerConfig::default();
    eprintln!(
        "取樣：{} 台伺服器，每台 {} 筆間隔 {:?}，每 {:?} 一輪",
        config.servers.len(),
        config.burst,
        config.spacing,
        config.cycle
    );
    let sampler = kairos_core::sync::spawn(config, Box::new(|event| eprintln!("[取樣] {event}")))
        .expect("取樣執行緒啟動失敗");

    let theme_path = theme::Theme::default_path();
    eprintln!("主題檔：{}", theme_path.display());
    for screen in NSScreen::screens(mtm).iter() {
        let f = screen.frame();
        eprintln!(
            "螢幕清單：{}（{:.0}×{:.0}，最高 {} Hz）——主題檔 [display.screens] 用這個名稱覆寫提前量",
            screen.localizedName(),
            f.size.width,
            f.size.height,
            screen.maximumFramesPerSecond()
        );
    }
    let store_path = target_store::TargetFile::default_path();
    eprintln!("目標檔：{}", store_path.display());
    let controller = Controller::new(mtm, sampler.clone(), theme_path, store_path);

    let (_status_item, rows, items) = build_status_item(mtm, &controller);
    controller.set_menu_items(items);
    let _refresh = start_menu_refresh(mtm, rows, sampler.clone());
    let _wake_observer = observe_wake(sampler);

    {
        let controller = controller.clone();
        match hotkey::register(
            hotkey::KEY_K,
            hotkey::modifiers::CONTROL | hotkey::modifiers::OPTION | hotkey::modifiers::CMD,
            Box::new(move || controller.toggle_click_through()),
        ) {
            Ok(()) => eprintln!("熱鍵：⌃⌥⌘K 切換滑鼠穿透"),
            Err(rc) => eprintln!("熱鍵註冊失敗（OSStatus {rc}），請用選單切換"),
        }
    }

    controller.start();
    eprintln!(
        "面板已顯示；選單列 ⏱ 可隱藏面板、切換滑鼠穿透、重新載入主題、設目標時刻、校正反應時間、試聽節拍，Quit 或 Cmd-Q 結束"
    );

    app.run();
}
