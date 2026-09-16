//! kairos 的 macOS 殼。
//!
//! 階段零做了：不佔 Dock、關 App Nap、選單列圖示與 Quit。
//! 階段一 1b 接上時間核心：背景取樣執行緒、選單裡的狀態列每秒刷新、
//! 睡眠喚醒後立刻重新取樣。面板、畫面、聲音都在後面的階段。
//!
//! 這一層只做接線，邏輯都在 `kairos-core`。

use std::ptr::NonNull;

use block2::RcBlock;
use dispatch2::MainThreadBound;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength, NSWorkspace, NSWorkspaceDidWakeNotification,
};
use objc2_foundation::{
    NSActivityOptions, NSNotification, NSObjectProtocol, NSProcessInfo, NSRunLoop,
    NSRunLoopCommonModes, NSString, NSTimer,
};

use kairos_core::model::{ClockModel, ModelStatus};
use kairos_core::sync::{SamplerConfig, SamplerHandle};
use kairos_core::time::{HostTime, Timebase, system_theta_ns};

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

fn build_status_item(mtm: MainThreadMarker) -> (Retained<NSStatusItem>, StatusRows) {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        button.setTitle(&NSString::from_str("⏱"));
    }

    let menu = NSMenu::new(mtm);
    let rows = StatusRows {
        primary: info_row(mtm, "標準時間：校時中…"),
        detail: info_row(mtm, ""),
    };
    menu.addItem(&rows.primary);
    menu.addItem(&rows.detail);
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
    (item, rows)
}

fn ms(ns: i128) -> f64 {
    ns as f64 / 1e6
}

/// 把模型換成兩列人看的文字。偏移是「遠端 − 系統時鐘」，正值代表本機系統時鐘慢。
fn describe(model: &ClockModel, now: HostTime, system_theta_ns: i128) -> (String, String) {
    let status_word = match model.status {
        ModelStatus::Uncalibrated => return ("標準時間：校時中…".into(), String::new()),
        ModelStatus::Stale if !model.is_usable() => {
            return ("標準時間：喚醒後重新校時中".into(), String::new());
        }
        ModelStatus::Converging => "收斂中",
        ModelStatus::Tracking => "追蹤中",
        ModelStatus::Stale => "過時",
        ModelStatus::Frozen => "已凍結",
    };
    let e = model.estimate_at(now);
    let offset_ms = ms(e.remote_unix_ns - (now.as_nanos() as i128 + system_theta_ns));
    let primary = format!(
        "標準時間 {offset_ms:+.1} ms ± {:.1} ms（{status_word}）",
        ms(e.half_width_ns as i128)
    );
    let since = now.saturating_duration_since(model.reference).as_secs();
    let detail = format!(
        "漂移 {:+.1} ± {:.0} ppm，上次取樣 {since} 秒前",
        model.drift * 1e6,
        model.half_width_growth_ns_per_s / 1_000.0
    );
    (primary, detail)
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
        let (primary, detail) = describe(&sampler.model(), HostTime::now(), system_theta_ns());
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

    let (_status_item, rows) = build_status_item(mtm);
    let _refresh = start_menu_refresh(mtm, rows, sampler.clone());
    let _wake_observer = observe_wake(sampler);
    eprintln!("選單列圖示已建立，從選單 Quit 或 Cmd-Q 結束");

    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use kairos_core::model::SourceKind;
    use std::time::Duration;

    fn model(status: ModelStatus, reference: HostTime) -> ClockModel {
        ClockModel {
            source: SourceKind::Standard,
            status,
            reference,
            // 讓「遠端 − 系統時鐘」剛好是 +88 ms：θ = θ_sys + 88 ms。
            offset_ns: 1_000 + 88_000_000,
            drift: 12.3e-6,
            half_width_ns: 8_000_000,
            half_width_growth_ns_per_s: 480_000.0,
        }
    }

    #[test]
    fn describe_tracking_shows_offset_and_width() {
        let now = HostTime::from_nanos(5_000_000_000_000);
        let (primary, detail) = describe(&model(ModelStatus::Tracking, now), now, 1_000);
        assert_eq!(primary, "標準時間 +88.0 ms ± 8.0 ms（追蹤中）");
        assert_eq!(detail, "漂移 +12.3 ± 480 ppm，上次取樣 0 秒前");
    }

    #[test]
    fn describe_grows_width_and_age_with_time() {
        let reference = HostTime::from_nanos(5_000_000_000_000);
        let now = reference + Duration::from_secs(10);
        let (primary, detail) = describe(&model(ModelStatus::Converging, reference), now, 1_000);
        // 8 ms + 10 s × 480 µs/s = 12.8 ms；偏移多了 12.3 ppm × 10 s ≈ 0.12 ms。
        assert_eq!(primary, "標準時間 +88.1 ms ± 12.8 ms（收斂中）");
        assert!(detail.ends_with("上次取樣 10 秒前"), "{detail}");
    }

    #[test]
    fn describe_special_states() {
        let now = HostTime::from_nanos(5_000_000_000_000);
        let uncal = ClockModel::uncalibrated(SourceKind::Standard, now);
        assert_eq!(describe(&uncal, now, 0).0, "標準時間：校時中…");

        let mut woke = uncal;
        woke.status = ModelStatus::Stale;
        assert_eq!(describe(&woke, now, 0).0, "標準時間：喚醒後重新校時中");

        let stale = model(ModelStatus::Stale, now);
        assert_eq!(describe(&stale, now, 1_000).0, "標準時間 +88.0 ms ± 8.0 ms（過時）");
    }
}
