//! 面板的控制器：`CADisplayLink` 的 target、選單與目標面板按鈕的 target、主題與螢幕變動的
//! 處理、節拍程序（拍點表、節拍區、光暈、聲音）的生命週期，以及目標時刻模式的接線
//! （狀態機每格步進、鎖定時凍結取樣、反應時間校正）。
//!
//! 這是程式裡唯一的 `define_class!`：顯示連結、選單、按鈕都只能給 target 加 selector，
//! 沒有 block 版。所有狀態放在 ivars，以 `Cell`／`RefCell` 做內部可變性。

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use block2::RcBlock;
use notify::RecommendedWatcher;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBitmapImageFileType, NSButton, NSControlStateValueOff, NSControlStateValueOn, NSEvent,
    NSEventMask, NSEventPhase, NSEventType, NSMenuItem,
    NSWindowDidChangeBackingPropertiesNotification, NSWindowDidChangeScreenNotification,
    NSWindowDidEndLiveResizeNotification, NSWindowDidResizeNotification, NSWorkspace,
};
use objc2_foundation::{
    NSDictionary, NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSRunLoop,
    NSRunLoopCommonModes, NSString,
};
use objc2_quartz_core::{CADisplayLink, CAFrameRateRange, CATransaction};

use kairos_core::beat::visual::glow_level;
use kairos_core::beat::{BeatPlan, Phase, next_whole_second};
use kairos_core::display::{DASHES, DisplaySmoother, LocalTime, SmootherConfig, local_datetime};
use kairos_core::model::ClockModel;
use kairos_core::sync::SamplerHandle;
use kairos_core::target::{
    AbortReason, Action, CALIBRATION_ROUNDS, DEFAULT_LOCK_BEFORE, DEFAULT_MEASURE_BEFORE, Stage,
    TargetConfig, TargetMachine,
};
use kairos_core::time::{HostTime, system_theta_ns};

use crate::audio::AudioEngine;
use crate::beat_view::StripKind;
use crate::calibrate::CalibrationRun;
use crate::glow::Glow;
use crate::panel::{Face, PanelViews, build_panel};
use crate::settings_panel::{SettingsPanel, TAB_BEAT, TAB_SOUND, TAB_TIMELINE};
use crate::settings_store::{self, Settings, clamp_opacity, clamp_zoom};
use crate::target_panel::TargetPanel;
use crate::target_store::TargetFile;
use crate::text::{self, ms, remaining_text, status_word, system_clock_text};
use crate::theme::{self, Beat, BeatStyle, Theme};

/// 其他執行緒（檔案監看、通知）只能設旗標，主執行緒在下一格處理。
#[derive(Default)]
pub struct Flags {
    pub theme_dirty: AtomicBool,
    pub scale_dirty: AtomicBool,
    pub screen_dirty: AtomicBool,
    /// 視窗大小變了（含使用者拖邊緣）；主執行緒下一格看是不是拖曳中再決定要不要重建。
    pub resize_dirty: AtomicBool,
    /// 使用者放開邊緣。
    pub live_resize_ended: AtomicBool,
}

/// 滾輪與捏合的事件監聽只把量累加在這裡，主執行緒下一格再套用；監聽的 block 與控制器共享。
struct Gestures {
    /// 實體方向的滾動量：正值＝手指或滾輪往上。觸控板的精確位移以點計、滾輪以行計，分開累加。
    scroll_points: Cell<f64>,
    scroll_lines: Cell<f64>,
    /// 捏合累積的倍率，1.0 代表沒動。
    magnify: Cell<f64>,
    magnify_ended: Cell<bool>,
}

impl Gestures {
    fn new() -> Gestures {
        Gestures {
            scroll_points: Cell::new(0.0),
            scroll_lines: Cell::new(0.0),
            magnify: Cell::new(1.0),
            magnify_ended: Cell::new(false),
        }
    }
}

/// 存檔後等這麼久再讀，讓編輯器把檔案寫完。
const THEME_RELOAD_DELAY: Duration = Duration::from_millis(150);
/// 拖邊緣或捏合中最多多久重建一次面板。
const ZOOM_REBUILD_INTERVAL: Duration = Duration::from_millis(50);
/// 設定（大小、不透明度、設定視窗）改完後等這麼久沒再動才寫 settings.toml。
const SETTINGS_SAVE_DELAY: Duration = Duration::from_millis(300);
/// 說明列暫時顯示「大小 125% · 不透明度 70%」多久。
const TOAST_DURATION: Duration = Duration::from_millis(1500);
/// 選單「放大／縮小」一次乘除多少。
const ZOOM_STEP: f64 = 1.1;
/// 選單「更透明／更不透明」一次加減多少。
const OPACITY_STEP: f64 = 0.1;
/// 觸控板滾一點改多少不透明度（滿範圍約 200 點）；滾輪一行改 5%。
const OPACITY_PER_SCROLL_POINT: f64 = 0.004;
const OPACITY_PER_SCROLL_LINE: f64 = 0.05;
/// 前幾格印顯示連結的時序，確認提前量與週期。
const DIAG_FRAMES: usize = 300;
/// 設了 `KAIROS_SNAPSHOT_DIR` 時，在這幾格把面板離屏渲染成 PNG（沒有毛玻璃，只有內容）；
/// `KAIROS_SNAPSHOT_FRAMES="120,600"` 可改格數。開發用：沒有螢幕錄製權限也能看到版面。
const SNAPSHOT_FRAMES: &[u64] = &[120, 900];
/// 第一拍離現在至少要這麼久，音訊串流才來得及暖機。
const MIN_LEAD: Duration = Duration::from_millis(500);

fn snapshot_frames() -> Vec<u64> {
    std::env::var("KAIROS_SNAPSHOT_FRAMES")
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| SNAPSHOT_FRAMES.to_vec())
}

#[derive(Default)]
struct FrameDiag {
    /// targetTimestamp − 回呼開始時刻。
    leads_ms: Vec<f64>,
    /// 回呼開始時刻 − timestamp（上一次 vsync）：回呼被排程得多晚。
    lates_ms: Vec<f64>,
    /// 相鄰兩格 targetTimestamp 的差。
    gaps_ms: Vec<f64>,
    /// 整個回呼（含 CATransaction commit）花的時間。
    costs_ms: Vec<f64>,
    last_target_s: Option<f64>,
    reported: bool,
}

impl FrameDiag {
    fn record(&mut self, timestamp_s: f64, target_s: f64, started: HostTime, cost: Duration) {
        if self.reported {
            return;
        }
        let now_s = started.as_nanos() as f64 / 1e9;
        self.leads_ms.push((target_s - now_s) * 1e3);
        self.lates_ms.push((now_s - timestamp_s) * 1e3);
        self.costs_ms.push(cost.as_secs_f64() * 1e3);
        if let Some(prev) = self.last_target_s {
            self.gaps_ms.push((target_s - prev) * 1e3);
        }
        self.last_target_s = Some(target_s);
        if self.leads_ms.len() >= DIAG_FRAMES {
            self.reported = true;
            let (lead_lo, lead_mid, lead_hi) = stats(&mut self.leads_ms);
            let (late_lo, late_mid, late_hi) = stats(&mut self.lates_ms);
            let (gap_lo, gap_mid, gap_hi) = stats(&mut self.gaps_ms);
            let (cost_lo, cost_mid, cost_hi) = stats(&mut self.costs_ms);
            eprintln!(
                "顯示連結（前 {DIAG_FRAMES} 格）：targetTimestamp 提前 中位 {lead_mid:.2} ms（{lead_lo:.2}–{lead_hi:.2}）；回呼晚於 vsync 中位 {late_mid:.2} ms（{late_lo:.2}–{late_hi:.2}）；格間隔 中位 {gap_mid:.2} ms（{gap_lo:.2}–{gap_hi:.2}）；回呼耗時 中位 {cost_mid:.3} ms（{cost_lo:.3}–{cost_hi:.3}）"
            );
        }
    }
}

fn stats(v: &mut [f64]) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    v.sort_by(|a, b| a.total_cmp(b));
    (v[0], v[v.len() / 2], v[v.len() - 1])
}

pub struct MenuItems {
    pub panel: Retained<NSMenuItem>,
    pub click_through: Retained<NSMenuItem>,
    pub beat_start: Retained<NSMenuItem>,
    pub beat_stop: Retained<NSMenuItem>,
    pub sound: Retained<NSMenuItem>,
    pub glow: Retained<NSMenuItem>,
    /// 「節拍樣式」子選單：`None` 是「依主題檔」，其餘是強制的樣式。
    pub style: Vec<(Option<StripKind>, Retained<NSMenuItem>)>,
    pub clear_target: Retained<NSMenuItem>,
}

/// 一次節拍程序是為了什麼：試聽、目標時刻的倒數、反應時間校正的一輪。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    Demo,
    Countdown,
    Calibration,
}

impl Purpose {
    fn label(self) -> &'static str {
        match self {
            Purpose::Demo => "試聽",
            Purpose::Countdown => "倒數",
            Purpose::Calibration => "校正",
        }
    }
}

/// 一次節拍程序：從排程到歸零後收尾期結束。
struct BeatSession {
    plan: BeatPlan,
    purpose: Purpose,
    kind: StripKind,
    audio: Option<AudioEngine>,
    glow: Option<Glow>,
    /// 每一拍：離落地最近的一格（含提前量）差多少毫秒，正值代表那一格在落地之後。
    nearest_frame_ms: Vec<Option<f64>>,
    fps: f64,
    /// 節拍期間每一格「回呼開始 − 上一次 vsync」的毫秒數：顯示連結的相位，跟上屏延遲一起看。
    callback_late_ms: Vec<f64>,
}

fn abort_text(reason: AbortReason) -> &'static str {
    match reason {
        AbortReason::TargetPassed => "已錯過",
        AbortReason::TooLateToLock => "來不及鎖定",
        AbortReason::Cancelled => "已取消",
    }
}

/// Objective-C 執行期的 ivar 對齊上限是 8；`DisplaySmoother` 裡的 `i128` 對齊 16，
/// 所以狀態整包放進 `Box`，ivar 只存一個指標。
pub struct Ivars {
    state: Box<State>,
}

struct State {
    sampler: SamplerHandle,
    views: PanelViews,
    face: RefCell<Option<Face>>,
    theme: RefCell<Theme>,
    theme_path: PathBuf,
    flags: Arc<Flags>,
    theme_dirty_since: Cell<Option<HostTime>>,
    _watcher: RefCell<Option<RecommendedWatcher>>,
    _backing_observer: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>>,
    _screen_observer: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>>,
    smoother: RefCell<DisplaySmoother>,
    last_slewing: Cell<bool>,
    /// 上一次節拍程序量到的「實際上屏 − 預測」中位數（毫秒），給選單細節列。
    last_present_ms: Cell<Option<f64>>,
    local: RefCell<LocalTime>,
    link: RefCell<Option<Retained<CADisplayLink>>>,
    click_through: Cell<bool>,
    menu: RefCell<Option<MenuItems>>,
    diag: RefCell<FrameDiag>,
    /// 說明列每秒算一次；記上次算的是哪一秒。
    caption_second: Cell<Option<i128>>,
    frames: Cell<u64>,
    snapshot_dir: Option<PathBuf>,
    snapshot_frames: Vec<u64>,
    /// 開發用：設了 `KAIROS_DEMO_BEATS` 就在模型第一次可用時自動試聽一次。
    demo_pending: Cell<bool>,
    /// 開發用：設了 `KAIROS_CALIBRATE` 就在啟動一秒後自動開始校正。
    calibrate_pending: Cell<bool>,
    /// 面板目前所在的螢幕：名稱、這個螢幕的顯示提前量、最高刷新率。
    screen_name: RefCell<Option<String>>,
    lead_ms: Cell<f64>,
    screen_fps: Cell<f64>,
    beat: RefCell<Option<BeatSession>>,
    sound_on: Cell<bool>,
    glow_on: Cell<bool>,
    /// 主題檔原樣（只讀）；`theme` 是蓋上設定的覆寫後的有效主題。
    base_theme: RefCell<Theme>,
    /// 設定視窗、選單、滾輪改過、跟主題檔不一樣的鍵，記在 `settings.toml` 的 `[theme]`。
    overrides: RefCell<toml::Table>,
    settings_path: PathBuf,
    settings_panel: RefCell<Option<SettingsPanel>>,
    /// 開發用：設了 `KAIROS_SHOW_SETTINGS` 就在啟動半秒後打開設定視窗（給快照看版面）。
    settings_pending: Cell<bool>,
    /// 面板的大小倍率，記在 `settings.toml` 的 `zoom`。
    zoom: Cell<f64>,
    /// 拖邊緣或捏合中還沒套用的倍率；每 `ZOOM_REBUILD_INTERVAL` 最多重建一次。
    zoom_pending: Cell<Option<f64>>,
    zoom_rebuilt_at: Cell<Option<HostTime>>,
    settings_dirty_since: Cell<Option<HostTime>>,
    /// 說明列暫時顯示的文字與開始時刻。
    toast: RefCell<Option<(String, HostTime)>>,
    gestures: Rc<Gestures>,
    _gesture_monitor: RefCell<Option<Retained<AnyObject>>>,
    _resize_observer: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>>,
    _live_resize_observer: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>>,
    /// 目標時刻：檔案、狀態機、面板、校正。
    store_path: PathBuf,
    store: RefCell<TargetFile>,
    target: RefCell<Option<TargetMachine>>,
    target_panel: RefCell<Option<TargetPanel>>,
    calibration: RefCell<Option<CalibrationRun>>,
    pending_reaction_ms: Cell<Option<f64>>,
}

define_class!(
    // SAFETY:
    // - NSObject 沒有子類別化的限制。
    // - Controller 沒有實作 Drop。
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KairosController"]
    #[ivars = Ivars]
    pub struct Controller;

    impl Controller {
        #[unsafe(method(frame:))]
        fn frame_action(&self, link: &CADisplayLink) {
            let started = HostTime::now();
            self.on_frame(link);
            self.state().diag.borrow_mut().record(
                link.timestamp(),
                link.targetTimestamp(),
                started,
                started.elapsed(),
            );
        }

        #[unsafe(method(togglePanel:))]
        fn toggle_panel_action(&self, _sender: Option<&AnyObject>) {
            self.toggle_panel();
        }

        #[unsafe(method(toggleClickThrough:))]
        fn toggle_click_through_action(&self, _sender: Option<&AnyObject>) {
            self.toggle_click_through();
        }

        #[unsafe(method(reloadTheme:))]
        fn reload_theme_action(&self, _sender: Option<&AnyObject>) {
            self.reload_theme(true);
        }

        #[unsafe(method(startDemoBeats:))]
        fn start_demo_beats_action(&self, _sender: Option<&AnyObject>) {
            self.start_demo_beats();
        }

        #[unsafe(method(stopBeats:))]
        fn stop_beats_action(&self, _sender: Option<&AnyObject>) {
            self.stop_beats("手動停止");
        }

        #[unsafe(method(toggleSound:))]
        fn toggle_sound_action(&self, _sender: Option<&AnyObject>) {
            self.toggle_sound();
        }

        #[unsafe(method(toggleGlow:))]
        fn toggle_glow_action(&self, _sender: Option<&AnyObject>) {
            self.toggle_glow();
        }

        /// 「節拍樣式」子選單的四個項目共用這個動作，靠 `tag` 分：0 依主題檔，1 球，2 環，3 脈衝。
        #[unsafe(method(setBeatStyle:))]
        fn set_beat_style_action(&self, sender: Option<&AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<NSMenuItem>())
                .map(|item| item.tag())
                .unwrap_or(0);
            let choice = match tag {
                1 => Some(StripKind::Ball),
                2 => Some(StripKind::Ring),
                3 => Some(StripKind::Pulse),
                _ => None,
            };
            self.set_beat_style(choice);
        }

        #[unsafe(method(zoomIn:))]
        fn zoom_in_action(&self, _sender: Option<&AnyObject>) {
            self.set_zoom(self.state().zoom.get() * ZOOM_STEP);
        }

        #[unsafe(method(zoomOut:))]
        fn zoom_out_action(&self, _sender: Option<&AnyObject>) {
            self.set_zoom(self.state().zoom.get() / ZOOM_STEP);
        }

        #[unsafe(method(resetZoom:))]
        fn reset_zoom_action(&self, _sender: Option<&AnyObject>) {
            self.set_zoom(1.0);
        }

        #[unsafe(method(moreTransparent:))]
        fn more_transparent_action(&self, _sender: Option<&AnyObject>) {
            self.set_opacity(Some(self.effective_opacity() - OPACITY_STEP));
        }

        #[unsafe(method(lessTransparent:))]
        fn less_transparent_action(&self, _sender: Option<&AnyObject>) {
            self.set_opacity(Some(self.effective_opacity() + OPACITY_STEP));
        }

        #[unsafe(method(resetOpacity:))]
        fn reset_opacity_action(&self, _sender: Option<&AnyObject>) {
            self.set_opacity(None);
        }

        #[unsafe(method(showTargetPanel:))]
        fn show_target_panel_action(&self, _sender: Option<&AnyObject>) {
            self.show_target_panel();
        }

        #[unsafe(method(showSettings:))]
        fn show_settings_action(&self, _sender: Option<&AnyObject>) {
            self.show_settings();
        }

        /// 設定視窗的任何控制項一動都來這裡。
        #[unsafe(method(settingChanged:))]
        fn setting_changed_action(&self, _sender: Option<&AnyObject>) {
            self.setting_changed();
        }

        /// 步進器：先把值寫回它的文字欄，再當一般改動處理。
        #[unsafe(method(stepperChanged:))]
        fn stepper_changed_action(&self, sender: Option<&AnyObject>) {
            if let Some(sender) = sender
                && let Some(p) = self.state().settings_panel.borrow().as_ref()
            {
                p.sync_stepper(sender);
            }
            self.setting_changed();
        }

        /// 每頁的「回復預設值」，靠 `tag` 分頁。
        #[unsafe(method(resetSettingsTab:))]
        fn reset_settings_tab_action(&self, sender: Option<&AnyObject>) {
            let tag = sender
                .and_then(|s| s.downcast_ref::<NSButton>())
                .map(|b| b.tag())
                .unwrap_or(-1);
            self.reset_settings_tab(tag);
        }

        #[unsafe(method(applyTarget:))]
        fn apply_target_action(&self, _sender: Option<&AnyObject>) {
            self.apply_target();
        }

        #[unsafe(method(clearTarget:))]
        fn clear_target_action(&self, _sender: Option<&AnyObject>) {
            self.clear_target();
        }

        #[unsafe(method(measureNow:))]
        fn measure_now_action(&self, _sender: Option<&AnyObject>) {
            self.measure_now();
        }

        #[unsafe(method(startCalibration:))]
        fn start_calibration_action(&self, _sender: Option<&AnyObject>) {
            self.start_calibration();
        }

        #[unsafe(method(adoptCalibration:))]
        fn adopt_calibration_action(&self, _sender: Option<&AnyObject>) {
            self.adopt_calibration();
        }
    }
);

impl Controller {
    fn state(&self) -> &State {
        &self.ivars().state
    }

    /// `base` 是主題檔原樣、`theme` 是蓋上 `settings` 的覆寫後的有效主題，都由 main 先算好
    /// （取樣執行緒的設定也從有效主題來），這裡只接手。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mtm: MainThreadMarker,
        sampler: SamplerHandle,
        base: Theme,
        theme: Theme,
        settings: Settings,
        theme_path: PathBuf,
        store_path: PathBuf,
        settings_path: PathBuf,
    ) -> Retained<Self> {
        let views = build_panel(mtm, &theme);
        let flags = Arc::new(Flags::default());
        let watcher = {
            let flags = flags.clone();
            match theme::watch(&theme_path, move || {
                flags.theme_dirty.store(true, Ordering::SeqCst)
            }) {
                Ok(w) => Some(w),
                Err(e) => {
                    eprintln!(
                        "主題：無法監看 {}（{e}），改動後請從選單重新載入",
                        theme_path.display()
                    );
                    None
                }
            }
        };

        let store = TargetFile::load(&store_path);
        let target = store.config().map(|cfg| {
            let m = TargetMachine::new(cfg, min_lock_margin(&theme));
            eprintln!(
                "目標：從 {} 載入，目標 {}，歸零 {}（提前 {:.0} ms）",
                store_path.display(),
                local_datetime(cfg.target_unix_ns).date_time_string(),
                local_datetime(cfg.zero_unix_ns()).date_time_string(),
                cfg.lead.total_ms()
            );
            m
        });

        let idle_fps = theme.display.idle_fps;
        let state = Box::new(State {
            sampler,
            views,
            face: RefCell::new(None),
            theme: RefCell::new(theme),
            theme_path,
            flags,
            theme_dirty_since: Cell::new(None),
            _watcher: RefCell::new(watcher),
            _backing_observer: RefCell::new(None),
            _screen_observer: RefCell::new(None),
            smoother: RefCell::new(DisplaySmoother::new(SmootherConfig::default())),
            last_slewing: Cell::new(false),
            last_present_ms: Cell::new(None),
            local: RefCell::new(LocalTime::new()),
            link: RefCell::new(None),
            click_through: Cell::new(false),
            menu: RefCell::new(None),
            diag: RefCell::new(FrameDiag::default()),
            caption_second: Cell::new(None),
            frames: Cell::new(0),
            snapshot_dir: std::env::var_os("KAIROS_SNAPSHOT_DIR").map(PathBuf::from),
            snapshot_frames: snapshot_frames(),
            demo_pending: Cell::new(std::env::var_os("KAIROS_DEMO_BEATS").is_some()),
            calibrate_pending: Cell::new(std::env::var_os("KAIROS_CALIBRATE").is_some()),
            screen_name: RefCell::new(None),
            lead_ms: Cell::new(0.0),
            screen_fps: Cell::new(idle_fps),
            beat: RefCell::new(None),
            sound_on: Cell::new(true),
            glow_on: Cell::new(true),
            base_theme: RefCell::new(base),
            overrides: RefCell::new(settings.theme),
            settings_path,
            settings_panel: RefCell::new(None),
            settings_pending: Cell::new(std::env::var_os("KAIROS_SHOW_SETTINGS").is_some()),
            zoom: Cell::new(settings.zoom),
            zoom_pending: Cell::new(None),
            zoom_rebuilt_at: Cell::new(None),
            settings_dirty_since: Cell::new(None),
            toast: RefCell::new(None),
            gestures: Rc::new(Gestures::new()),
            _gesture_monitor: RefCell::new(None),
            _resize_observer: RefCell::new(None),
            _live_resize_observer: RefCell::new(None),
            store_path,
            store: RefCell::new(store),
            target: RefCell::new(target),
            target_panel: RefCell::new(None),
            calibration: RefCell::new(None),
            pending_reaction_ms: Cell::new(None),
        });
        let this = Self::alloc(mtm).set_ivars(Ivars { state });
        // SAFETY: NSObject 的 init。
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        this.rebuild_face(false);
        this
    }

    /// 上一次節拍程序量到的「實際上屏 − 預測」中位數（毫秒）；還沒量過是 `None`。
    pub fn last_present_ms(&self) -> Option<f64> {
        self.state().last_present_ms.get()
    }

    pub fn set_menu_items(&self, items: MenuItems) {
        *self.state().menu.borrow_mut() = Some(items);
        self.refresh_menu();
        self.refresh_style_menu();
    }

    /// 建立顯示連結、訂閱螢幕縮放與換螢幕的通知、把面板秀出來。
    pub fn start(&self) {
        let iv = self.state();
        let target: &AnyObject = self;
        // SAFETY: `frame:` 是上面 define_class! 定義的方法；target 型別正確。
        // 顯示連結會 retain target、我們也 retain 顯示連結——這個環跟程式同壽命，故意的。
        let link = unsafe {
            iv.views
                .effect
                .displayLinkWithTarget_selector(target, sel!(frame:))
        };
        // SAFETY: 主執行緒的 run loop 與標準 mode。common modes 讓選單打開時也繼續跑。
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
        *iv.link.borrow_mut() = Some(link);

        let center = NSNotificationCenter::defaultCenter();
        let flags = iv.flags.clone();
        let block = RcBlock::new(move |_n: NonNull<NSNotification>| {
            flags.scale_dirty.store(true, Ordering::SeqCst);
        });
        // SAFETY: 通知名是 AppKit 公開常數；只看我們自己的面板；閉包只捕捉 Arc<Flags>，是 Send。
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWindowDidChangeBackingPropertiesNotification),
                Some(&iv.views.panel),
                None,
                &block,
            )
        };
        *iv._backing_observer.borrow_mut() = Some(token);

        let flags = iv.flags.clone();
        let block = RcBlock::new(move |_n: NonNull<NSNotification>| {
            flags.screen_dirty.store(true, Ordering::SeqCst);
        });
        // SAFETY: 同上。
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWindowDidChangeScreenNotification),
                Some(&iv.views.panel),
                None,
                &block,
            )
        };
        *iv._screen_observer.borrow_mut() = Some(token);

        let flags = iv.flags.clone();
        let block = RcBlock::new(move |_n: NonNull<NSNotification>| {
            flags.resize_dirty.store(true, Ordering::SeqCst);
        });
        // SAFETY: 同上。
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWindowDidResizeNotification),
                Some(&iv.views.panel),
                None,
                &block,
            )
        };
        *iv._resize_observer.borrow_mut() = Some(token);

        let flags = iv.flags.clone();
        let block = RcBlock::new(move |_n: NonNull<NSNotification>| {
            flags.live_resize_ended.store(true, Ordering::SeqCst);
        });
        // SAFETY: 同上。
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWindowDidEndLiveResizeNotification),
                Some(&iv.views.panel),
                None,
                &block,
            )
        };
        *iv._live_resize_observer.borrow_mut() = Some(token);

        self.install_gesture_monitor();
        iv.views.panel.orderFrontRegardless();
        self.refresh_screen(true);
    }

    /// 面板上的滾輪（不透明度）與捏合（大小）。本地事件監聽只看本程式的事件、不需要權限；
    /// 只吃時鐘面板上的事件（目標面板的日期選擇器照常滾），吃掉的事件不再往下送。
    fn install_gesture_monitor(&self) {
        let iv = self.state();
        let gestures = Rc::clone(&iv.gestures);
        let panel_number = iv.views.panel.windowNumber();
        let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit 交來的事件指標在 block 執行期間有效。
            let e = unsafe { event.as_ref() };
            if e.windowNumber() != panel_number {
                return event.as_ptr();
            }
            match e.r#type() {
                NSEventType::ScrollWheel => {
                    // 慣性滾動不算，免得一甩就滑到底。
                    if e.momentumPhase() != NSEventPhase::None {
                        return std::ptr::null_mut();
                    }
                    // 換算成實體方向：手指或滾輪往上是正。
                    let dy = e.scrollingDeltaY();
                    let physical = if e.isDirectionInvertedFromDevice() {
                        -dy
                    } else {
                        dy
                    };
                    let cell = if e.hasPreciseScrollingDeltas() {
                        &gestures.scroll_points
                    } else {
                        &gestures.scroll_lines
                    };
                    cell.set(cell.get() + physical);
                    std::ptr::null_mut()
                }
                NSEventType::Magnify => {
                    gestures
                        .magnify
                        .set(gestures.magnify.get() * (1.0 + e.magnification()));
                    if e.phase()
                        .intersects(NSEventPhase::Ended | NSEventPhase::Cancelled)
                    {
                        gestures.magnify_ended.set(true);
                    }
                    std::ptr::null_mut()
                }
                _ => event.as_ptr(),
            }
        });
        let mask = NSEventMask::ScrollWheel | NSEventMask::Magnify;
        // SAFETY: block 的簽名跟 AppKit 要的一樣；回傳原事件代表放行、null 代表吃掉。
        match unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) } {
            Some(monitor) => *iv._gesture_monitor.borrow_mut() = Some(monitor),
            None => eprintln!("外觀：裝不上滾輪與捏合的監聽，面板上只能拖邊緣改大小"),
        }
    }

    /// 重查面板在哪個螢幕：名稱決定顯示提前量，最高刷新率決定節拍期間要多快。
    fn refresh_screen(&self, verbose: bool) {
        let iv = self.state();
        let (name, fps) = match iv.views.panel.screen() {
            Some(s) => (
                Some(s.localizedName().to_string()),
                s.maximumFramesPerSecond() as f64,
            ),
            None => (None, iv.theme.borrow().display.idle_fps),
        };
        let lead = iv.theme.borrow().display.lead_ms_for(name.as_deref());
        let changed = *iv.screen_name.borrow() != name
            || iv.lead_ms.get() != lead
            || iv.screen_fps.get() != fps;
        *iv.screen_name.borrow_mut() = name.clone();
        iv.lead_ms.set(lead);
        iv.screen_fps.set(fps);
        if changed || verbose {
            eprintln!(
                "螢幕：{}（最高 {fps:.0} Hz），顯示提前量 {lead} ms",
                name.as_deref().unwrap_or("?")
            );
        }
        if changed && let Some(link) = iv.link.borrow().as_ref() {
            self.apply_frame_rate(link);
        }
    }

    /// 平常鎖在 `idle_fps`；節拍期間鎖在面板所在螢幕的最高刷新率。
    fn apply_frame_rate(&self, link: &CADisplayLink) {
        let iv = self.state();
        let fps = if iv.beat.borrow().is_some() {
            iv.screen_fps.get()
        } else {
            iv.theme.borrow().display.idle_fps
        };
        let fps = fps.clamp(10.0, 240.0) as f32;
        link.setPreferredFrameRateRange(CAFrameRateRange {
            minimum: fps,
            maximum: fps,
            preferred: fps,
        });
    }

    /// 重建面板內容。有節拍程序在跑就帶節拍區，有目標就帶目標列。
    fn rebuild_face(&self, keep_bottom: bool) {
        let iv = self.state();
        let mtm = self.mtm();
        if let Some(old) = iv.face.borrow_mut().take() {
            old.teardown();
        }
        let strip = iv.beat.borrow().as_ref().map(|s| s.kind);
        let target_row = iv.target.borrow().is_some();
        let zoom = iv.zoom.get();
        let theme = iv.theme.borrow().scaled(zoom);
        let scale = iv.views.scale();
        let face = Face::build(mtm, &iv.views, &theme, scale, zoom, strip, target_row);
        iv.views.apply_theme(&theme, face.height, zoom, keep_bottom);
        iv.views.set_opacity(self.effective_opacity());
        *iv.face.borrow_mut() = Some(face);
        iv.caption_second.set(None);
        if let Some(link) = iv.link.borrow().as_ref() {
            self.apply_frame_rate(link);
        }
    }

    /// 重讀主題檔、蓋上設定的覆寫、套用差異。`verbose` 時連「沒變」也回報（選單手動觸發用）。
    /// 設定視窗改東西不走這裡（見 `commit_theme`），但倒數期間延後的改動會在倒數結束後從這裡補套。
    pub fn reload_theme(&self, verbose: bool) {
        let iv = self.state();
        let new_base = match std::fs::read_to_string(&iv.theme_path)
            .map_err(|e| e.to_string())
            .and_then(|t| Theme::parse(&t).map_err(|e| e.to_string()))
        {
            Ok(t) => t,
            Err(e) => {
                eprintln!(
                    "主題：{} 讀取失敗，沿用上一版：{e}",
                    iv.theme_path.display()
                );
                return;
            }
        };
        let base_changed = *iv.base_theme.borrow() != new_base;
        if base_changed {
            // 最後一次動作為準：主題檔改了哪些鍵，設定裡那些鍵的覆寫就作廢。
            let removed = settings_store::prune(
                &mut iv.overrides.borrow_mut(),
                &iv.base_theme.borrow().to_table(),
                &new_base.to_table(),
            );
            *iv.base_theme.borrow_mut() = new_base;
            if !removed.is_empty() {
                eprintln!(
                    "主題：{} 有變，設定裡這些鍵的覆寫作廢，回到依主題檔",
                    removed.join("、")
                );
                self.mark_settings_dirty();
            }
        }
        let effective =
            match settings_store::effective(&iv.base_theme.borrow(), &iv.overrides.borrow()) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("設定：[theme] 的覆寫套不上（{e}），這次全部作廢");
                    iv.overrides.borrow_mut().clear();
                    self.mark_settings_dirty();
                    iv.base_theme.borrow().clone()
                }
            };
        if effective == *iv.theme.borrow() {
            if verbose {
                eprintln!("主題：沒有變動");
            }
            return;
        }
        self.apply_theme_change(effective, if base_changed { "主題" } else { "設定" });
        if base_changed {
            eprintln!("主題：已重新載入 {}", iv.theme_path.display());
        }
    }

    /// 設定視窗、選單、滾輪改了有效主題：算出跟主題檔的差當覆寫、記下來、套用。
    /// 差是對「現在」算的，再疊到「主題檔＋既有覆寫」上，所以倒數期間延後套用的改動不會被蓋掉。
    fn commit_theme(&self, new: Theme, who: &str) {
        let iv = self.state();
        let base_table = iv.base_theme.borrow().to_table();
        let delta = settings_store::diff(&iv.theme.borrow().to_table(), &new.to_table());
        let pending = settings_store::merge(
            &settings_store::merge(&base_table, &iv.overrides.borrow()),
            &delta,
        );
        let overrides = settings_store::diff(&base_table, &pending);
        let effective = match Theme::from_table(pending) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{who}：改動套不上（{e}），不動");
                return;
            }
        };
        *iv.overrides.borrow_mut() = overrides;
        self.mark_settings_dirty();
        self.apply_theme_change(effective, who);
    }

    /// 換上一份新的有效主題，只動有變的部分。倒數期間只套不透明度，其餘留旗標等倒數結束再補。
    fn apply_theme_change(&self, new: Theme, who: &str) {
        let iv = self.state();
        let old = iv.theme.borrow().clone();
        let opacity_changed = old.panel.opacity != new.panel.opacity;
        if self.session_purpose() == Some(Purpose::Countdown) {
            if opacity_changed {
                iv.theme.borrow_mut().panel.opacity = new.panel.opacity;
                iv.views.set_opacity(new.panel.opacity);
                self.show_toast(self.describe_view());
            }
            let mut rest = new;
            rest.panel.opacity = old.panel.opacity;
            if rest != old {
                iv.flags.theme_dirty.store(true, Ordering::SeqCst);
                eprintln!("{who}：鎖定倒數中，除了不透明度以外的改動等倒數結束再套用");
                self.set_settings_status("鎖定倒數中，其他改動等倒數結束再套用");
            }
            return;
        }
        let sync_changed = old.sync != new.sync;
        let style_changed = old.beat.style != new.beat.style;
        let display_changed = old.display != new.display;
        let timeline_changed = old.beat.timeline() != new.beat.timeline()
            || old.beat.demo_lead_s != new.beat.demo_lead_s;
        let rebuild = needs_rebuild(&old, &new);
        *iv.theme.borrow_mut() = new;

        let mut retarget = false;
        if style_changed {
            let reduce_motion =
                NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
            let kind = self.resolve_kind(&iv.theme.borrow(), reduce_motion);
            if let Some(session) = iv.beat.borrow_mut().as_mut()
                && session.kind != kind
            {
                // 進行中的節拍也換過去；下面重建節拍區，Metal 的上屏統計從那一刻起重算。
                session.kind = kind;
                retarget = true;
            }
            self.refresh_style_menu();
            eprintln!(
                "{who}：節拍樣式改為{}{}",
                kind.title(),
                if retarget {
                    "，進行中的節拍立刻換"
                } else {
                    ""
                }
            );
        }
        if opacity_changed {
            iv.views.set_opacity(self.effective_opacity());
            self.show_toast(self.describe_view());
        }
        if rebuild || retarget {
            self.rebuild_face(false);
        }
        if display_changed {
            self.refresh_screen(false);
            if let Some(link) = iv.link.borrow().as_ref() {
                self.apply_frame_rate(link);
            }
        }
        if timeline_changed {
            eprintln!(
                "{who}：時間軸改為 {}",
                iv.theme.borrow().beat.timeline().describe()
            );
            self.rearm_if_pending();
        }
        if sync_changed {
            match iv.theme.borrow().sync.settings() {
                Ok(s) => {
                    eprintln!(
                        "{who}：[sync] 有變，換成 {}{}",
                        s.servers.join("、"),
                        if iv.sampler.is_paused() {
                            "（鎖定中，解凍後套用）"
                        } else {
                            ""
                        }
                    );
                    iv.sampler.reconfigure(s);
                }
                Err(e) => eprintln!("{who}：[sync] 不合法（{e}），時間來源沿用上一版"),
            }
        }
        self.refresh_menu();
        self.refresh_settings_panel();
    }

    pub fn toggle_panel(&self) {
        let iv = self.state();
        let visible = iv.views.panel.isVisible();
        if visible {
            iv.views.panel.orderOut(None);
        } else {
            iv.views.panel.orderFrontRegardless();
        }
        if let Some(link) = iv.link.borrow().as_ref() {
            link.setPaused(visible);
        }
        if let Some(menu) = iv.menu.borrow().as_ref() {
            menu.panel.setTitle(&NSString::from_str(if visible {
                "顯示面板"
            } else {
                "隱藏面板"
            }));
        }
    }

    pub fn toggle_click_through(&self) {
        let iv = self.state();
        let on = !iv.click_through.get();
        iv.click_through.set(on);
        iv.views.panel.setIgnoresMouseEvents(on);
        if let Some(menu) = iv.menu.borrow().as_ref() {
            menu.click_through.setState(if on {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
        eprintln!("滑鼠穿透：{}", if on { "開" } else { "關" });
    }

    // ---------- 節拍程序 ----------

    /// 試聽：歸零訂在「現在＋試聽提前（`demo_lead_s`，不夠畫面起跑就拉長）」之後的下一個
    /// 標準時間整秒，歸零拍會跟數字翻到 `.000` 同一瞬間，光看面板就能驗證球落地與翻頁是不是同一刻。
    pub fn start_demo_beats(&self) {
        let iv = self.state();
        let model = iv.sampler.model();
        if !model.is_usable() {
            eprintln!(
                "節拍：標準時間模型還不能用（{}），等校時完成再試",
                status_word(&model).err().unwrap_or("未就緒")
            );
            return;
        }
        let (demo_lead, needed) = {
            let b = &iv.theme.borrow().beat;
            (b.demo_lead(), b.timeline().visual_lead + MIN_LEAD)
        };
        let lead = demo_lead.max(needed);
        if lead > demo_lead {
            eprintln!(
                "節拍：試聽提前 {:.0} 秒不夠畫面起跑，改成 {:.1} 秒後歸零",
                demo_lead.as_secs_f64(),
                lead.as_secs_f64()
            );
        }
        let target = next_whole_second(&model, HostTime::now() + lead);
        self.start_beats(target);
    }

    /// 排一次試聽，歸零在標準時間 `target_remote_unix_ns`。
    pub fn start_beats(&self, target_remote_unix_ns: i128) -> bool {
        let iv = self.state();
        let model = iv.sampler.model();
        if !model.is_usable() {
            eprintln!("節拍：標準時間模型還不能用，無法排程");
            return false;
        }
        let plan = iv
            .theme
            .borrow()
            .beat
            .plan(model.host_at(target_remote_unix_ns));
        self.start_plan(plan, Purpose::Demo)
    }

    /// 用一份已經算好的拍點表開一次節拍程序：節拍區、光暈、聲音、刷新率。
    fn start_plan(&self, plan: BeatPlan, purpose: Purpose) -> bool {
        let iv = self.state();
        if iv.beat.borrow().is_some() {
            self.stop_beats("重新排程");
        }
        let theme = iv.theme.borrow().clone();
        let now = HostTime::now();
        if plan.first_tick() < now + MIN_LEAD {
            eprintln!(
                "節拍：第一拍離現在不到 {} ms，來不及，放棄",
                MIN_LEAD.as_millis()
            );
            return false;
        }

        let reduce_motion = NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
        let kind = self.resolve_kind(&theme, reduce_motion);
        let glow = (theme.beat.glow && iv.glow_on.get() && !reduce_motion)
            .then(|| Glow::build(self.mtm(), &theme));
        let audio = if iv.sound_on.get() {
            match AudioEngine::start(plan, theme.beat.sound()) {
                Ok(a) => {
                    eprintln!("{}", a.describe());
                    Some(a)
                }
                Err(e) => {
                    eprintln!("節拍：聲音無法啟動（{e}），只有畫面");
                    None
                }
            }
        } else {
            None
        };

        let zero_local = {
            let model = iv.sampler.model();
            if model.is_usable() {
                let digits = iv
                    .local
                    .borrow_mut()
                    .wall(model.estimate_at(plan.zero).remote_unix_ns)
                    .digits();
                String::from_utf8_lossy(&digits).into_owned()
            } else {
                "（模型不可用，只有主機時間）".to_string()
            }
        };
        let summary = format!(
            "節拍（{}）：歸零於本地時間 {zero_local}（{:.3} 秒後），{} 拍、拍距 {} ms、畫面歸零前 {:.1} 秒起跑（共 {} 拍{}）、樣式 {}{}、聲音{}、光暈 {} 個螢幕、刷新率 {:.0} Hz{}{}",
            purpose.label(),
            plan.zero.saturating_duration_since(now).as_secs_f64(),
            plan.ticks,
            theme.beat.period_ms,
            plan.visual_lead().as_secs_f64(),
            plan.total_beats(),
            match plan.silent_landings() {
                0 => String::new(),
                n => format!("，前 {n} 次落地無聲"),
            },
            kind.title(),
            if settings_store::has_leaf(&iv.overrides.borrow(), "beat.style") {
                "（設定指定）"
            } else {
                ""
            },
            if audio.is_some() { "開" } else { "關" },
            glow.as_ref().map(|g| g.screen_count()).unwrap_or(0),
            iv.screen_fps.get(),
            if reduce_motion {
                "；系統開著「減少動態效果」"
            } else {
                ""
            },
            if iv.last_slewing.get() {
                "；注意：數字還在往模型中點追，落地與翻頁可能差幾毫秒"
            } else {
                ""
            },
        );
        *iv.beat.borrow_mut() = Some(BeatSession {
            plan,
            purpose,
            kind,
            audio,
            glow,
            nearest_frame_ms: vec![None; plan.ticks as usize],
            fps: iv.screen_fps.get(),
            callback_late_ms: Vec::with_capacity(512),
        });
        if purpose == Purpose::Countdown {
            // 倒數期間不重建面板：邊緣拉不動，捏合與選單也擋掉（見 set_zoom）。
            iv.views.set_resizable(false);
            iv.zoom_pending.set(None);
        }
        self.rebuild_face(true);
        self.refresh_menu();
        eprintln!("{summary}");
        true
    }

    /// 結束節拍程序：印診斷、拆光暈、關聲音、收掉節拍區，然後依用途收尾
    /// （倒數→解凍取樣；校正→記這一輪、開下一輪）。
    pub fn stop_beats(&self, reason: &str) {
        let iv = self.state();
        let taken = iv.beat.borrow_mut().take();
        let Some(session) = taken else {
            eprintln!("節拍：目前沒有在跑");
            return;
        };
        let completed = reason == "結束";
        eprintln!("節拍（{}）：{reason}", session.purpose.label());
        if let Some(a) = &session.audio {
            eprintln!("{}", a.report(&session.plan));
        }
        let mut line = format!(
            "節拍畫面：{:.0} Hz，半格 {:.1} ms",
            session.fps,
            500.0 / session.fps.max(1.0)
        );
        if !session.callback_late_ms.is_empty() {
            let mut lates = session.callback_late_ms.clone();
            lates.sort_by(|a, b| a.total_cmp(b));
            line.push_str(&format!(
                "，回呼晚於 vsync 中位 {:.2} ms（{:.2}–{:.2}）",
                lates[lates.len() / 2],
                lates[0],
                lates[lates.len() - 1]
            ));
        }
        for (k, d) in session.nearest_frame_ms.iter().enumerate() {
            match d {
                Some(d) => line.push_str(&format!("；第 {k} 拍最近一格差 {d:+.1} ms")),
                None => line.push_str(&format!("；第 {k} 拍沒畫到")),
            }
        }
        eprintln!("{line}");
        // Metal 節拍區：在重建面板（會拆掉圖層）之前把上屏統計拿走。
        {
            let (ahead, vlead) = iv.theme.borrow().beat.visual_lead();
            let ticks: Vec<HostTime> = session
                .plan
                .tick_times()
                .map(|(_, t)| if ahead { t - vlead } else { t + vlead })
                .collect();
            let summary = iv
                .face
                .borrow_mut()
                .as_mut()
                .and_then(|f| f.strip.as_mut())
                .and_then(|s| s.metal_mut())
                .map(|m| m.take_summary(&ticks));
            if let Some(summary) = summary {
                let (first, second) = summary.lines();
                eprintln!("{first}");
                eprintln!("{second}");
                iv.last_present_ms
                    .set(summary.offset_ms.map(|(mid, _, _)| mid));
            }
        }
        if let Some(g) = &session.glow {
            g.teardown();
        }
        let purpose = session.purpose;
        let plan = session.plan;
        drop(session);
        iv.views.set_resizable(true);
        self.rebuild_face(true);

        match purpose {
            Purpose::Demo => {}
            Purpose::Countdown => {
                if let Some(m) = iv.target.borrow_mut().as_mut() {
                    if completed {
                        m.step(None, true);
                    } else {
                        m.abort(AbortReason::Cancelled);
                    }
                }
                iv.sampler.resume();
                eprintln!(
                    "目標：倒數{}，恢復取樣",
                    if completed { "結束" } else { "取消" }
                );
            }
            Purpose::Calibration => self.calibration_round_ended(&plan, completed),
        }
        self.refresh_menu();
        self.refresh_target_ui();
    }

    fn refresh_menu(&self) {
        let iv = self.state();
        if let Some(menu) = iv.menu.borrow().as_ref() {
            let active = iv.beat.borrow().is_some();
            menu.beat_stop.setEnabled(active);
            let secs = iv.theme.borrow().beat.demo_lead().as_secs_f64();
            menu.beat_start.setTitle(&NSString::from_str(&format!(
                "{}（{secs:.0} 秒後）",
                if active {
                    "重新排節拍"
                } else {
                    "試聽節拍"
                }
            )));
            menu.clear_target.setEnabled(iv.target.borrow().is_some());
        }
    }

    /// 節拍聲音開關。程序進行中切換會立刻開／關串流。
    pub fn toggle_sound(&self) {
        let iv = self.state();
        let on = !iv.sound_on.get();
        iv.sound_on.set(on);
        if let Some(menu) = iv.menu.borrow().as_ref() {
            menu.sound.setState(if on {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
        if let Some(session) = iv.beat.borrow_mut().as_mut() {
            if on && session.audio.is_none() {
                match AudioEngine::start(session.plan, iv.theme.borrow().beat.sound()) {
                    Ok(a) => {
                        eprintln!("{}", a.describe());
                        session.audio = Some(a);
                    }
                    Err(e) => eprintln!("節拍：聲音無法啟動（{e}）"),
                }
            } else if !on {
                session.audio = None;
            }
        }
        eprintln!("節拍聲音：{}", if on { "開" } else { "關" });
    }

    /// 邊緣光暈開關。程序進行中切換會立刻建／拆光暈面板。
    pub fn toggle_glow(&self) {
        let iv = self.state();
        let on = !iv.glow_on.get();
        iv.glow_on.set(on);
        if let Some(menu) = iv.menu.borrow().as_ref() {
            menu.glow.setState(if on {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
        if let Some(session) = iv.beat.borrow_mut().as_mut() {
            if on && session.glow.is_none() {
                let theme = iv.theme.borrow();
                let reduce =
                    NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion();
                if theme.beat.glow && !reduce {
                    session.glow = Some(Glow::build(self.mtm(), &theme));
                }
            } else if !on && let Some(g) = session.glow.take() {
                g.teardown();
            }
        }
        eprintln!("邊緣光暈：{}", if on { "開" } else { "關" });
    }

    /// 這一次節拍區該畫什麼：照有效主題的 `style`（`auto` 看「減少動態效果」）。
    fn resolve_kind(&self, theme: &Theme, reduce_motion: bool) -> StripKind {
        match theme.beat.style {
            BeatStyle::Auto if reduce_motion => StripKind::Pulse,
            BeatStyle::Auto | BeatStyle::Ball => StripKind::Ball,
            BeatStyle::Ring => StripKind::Ring,
            BeatStyle::Pulse => StripKind::Pulse,
        }
    }

    /// 選單「節拍樣式」：改的是有效主題的 `[beat] style`，跟設定視窗同一份覆寫、會記住；
    /// `None` 回到依主題檔。程序進行中切換會立刻重建節拍區（Metal 的上屏統計從那一刻起重算）。
    pub fn set_beat_style(&self, choice: Option<StripKind>) {
        let iv = self.state();
        let mut t = iv.theme.borrow().clone();
        t.beat.style = match choice {
            None => iv.base_theme.borrow().beat.style,
            Some(StripKind::Ball) => BeatStyle::Ball,
            Some(StripKind::Ring) => BeatStyle::Ring,
            Some(StripKind::Pulse) => BeatStyle::Pulse,
        };
        if t == *iv.theme.borrow() {
            eprintln!("節拍樣式：沒有變（已經是這個）");
            self.refresh_style_menu();
            return;
        }
        self.commit_theme(t, "節拍樣式");
    }

    /// 讓「節拍樣式」子選單只勾目前的選擇：設定有指定就勾那個樣式，否則勾「依主題檔」。
    fn refresh_style_menu(&self) {
        let iv = self.state();
        if let Some(menu) = iv.menu.borrow().as_ref() {
            let overridden = settings_store::has_leaf(&iv.overrides.borrow(), "beat.style");
            let current = match iv.theme.borrow().beat.style {
                _ if !overridden => None,
                BeatStyle::Ball => Some(StripKind::Ball),
                BeatStyle::Ring => Some(StripKind::Ring),
                BeatStyle::Pulse => Some(StripKind::Pulse),
                BeatStyle::Auto => None,
            };
            for (choice, item) in &menu.style {
                item.setState(if *choice == current {
                    NSControlStateValueOn
                } else {
                    NSControlStateValueOff
                });
            }
        }
    }

    // ---------- 外觀：大小與不透明度 ----------

    fn settings(&self) -> Settings {
        let iv = self.state();
        Settings {
            zoom: iv.zoom.get(),
            theme: iv.overrides.borrow().clone(),
        }
    }

    /// 現在該套的不透明度：有效主題的值（設定改過就是設定的，否則主題檔的）。
    fn effective_opacity(&self) -> f64 {
        self.state().theme.borrow().panel.opacity
    }

    fn describe_view(&self) -> String {
        let iv = self.state();
        settings_store::describe_view(
            iv.zoom.get(),
            self.effective_opacity(),
            settings_store::has_leaf(&iv.overrides.borrow(), "panel.opacity"),
        )
    }

    /// 說明列暫時顯示一句話，蓋過平常的「標準時間（NTP）· …」。
    fn show_toast(&self, text: String) {
        *self.state().toast.borrow_mut() = Some((text, HostTime::now()));
    }

    fn mark_settings_dirty(&self) {
        self.state().settings_dirty_since.set(Some(HostTime::now()));
    }

    /// 選單或程式指定倍率；拖邊緣與捏合走 `zoom_pending`。鎖定倒數期間不改。
    pub fn set_zoom(&self, zoom: f64) {
        let iv = self.state();
        if self.session_purpose() == Some(Purpose::Countdown) {
            self.show_toast("鎖定中不改大小".to_string());
            return;
        }
        iv.zoom_pending.set(Some(clamp_zoom(zoom)));
        self.service_zoom(true);
    }

    /// 不透明度：改的是有效主題的 `[panel] opacity`（記進設定）；`None` 回到依主題檔。
    /// 只是一行 alpha，節拍與鎖定期間都照改。
    pub fn set_opacity(&self, opacity: Option<f64>) {
        let iv = self.state();
        let mut t = iv.theme.borrow().clone();
        t.panel.opacity = match opacity {
            Some(o) => clamp_opacity(o),
            None => iv.base_theme.borrow().panel.opacity,
        };
        if t == *iv.theme.borrow() {
            self.show_toast(self.describe_view());
            return;
        }
        self.commit_theme(t, "外觀");
    }

    /// 把累加的滾動量換成不透明度、捏合量換成待套用的倍率。
    fn service_gestures(&self) {
        let iv = self.state();
        let points = iv.gestures.scroll_points.replace(0.0);
        let lines = iv.gestures.scroll_lines.replace(0.0);
        let delta = points * OPACITY_PER_SCROLL_POINT + lines * OPACITY_PER_SCROLL_LINE;
        if delta != 0.0 {
            self.set_opacity(Some(self.effective_opacity() + delta));
        }
        let magnify = iv.gestures.magnify.replace(1.0);
        if magnify != 1.0 {
            if self.session_purpose() == Some(Purpose::Countdown) {
                self.show_toast("鎖定中不改大小".to_string());
            } else {
                let base = iv.zoom_pending.get().unwrap_or(iv.zoom.get());
                iv.zoom_pending.set(Some(clamp_zoom(base * magnify)));
            }
        }
    }

    /// 套用待套用的倍率：拖曳或捏合中每 `ZOOM_REBUILD_INTERVAL` 最多重建一次；
    /// `settle`（放手）時立刻套、把視窗大小校準到內容、夾回螢幕、排存檔。
    fn service_zoom(&self, settle: bool) {
        let iv = self.state();
        let Some(zoom) = iv.zoom_pending.get() else {
            return;
        };
        let due = settle
            || iv
                .zoom_rebuilt_at
                .get()
                .is_none_or(|t| t.elapsed() >= ZOOM_REBUILD_INTERVAL);
        if !due {
            return;
        }
        iv.zoom_pending.set(None);
        let changed = (zoom - iv.zoom.get()).abs() > 1e-4;
        if changed {
            iv.zoom.set(zoom);
            self.rebuild_face(false);
            iv.zoom_rebuilt_at.set(Some(HostTime::now()));
            self.show_toast(self.describe_view());
        }
        if settle {
            if !changed && !iv.views.in_live_resize() {
                // 拖曳中重建時沒動視窗大小；放手後把大小校準到內容。
                let height = iv.face.borrow().as_ref().map(|f| f.height);
                if let Some(height) = height {
                    let theme = iv.theme.borrow().scaled(zoom);
                    iv.views.apply_theme(&theme, height, zoom, false);
                }
            }
            iv.views.constrain_to_screen();
            self.mark_settings_dirty();
        }
    }

    /// 設定改完、沒再動 `SETTINGS_SAVE_DELAY` 後寫 settings.toml。
    fn service_settings_save(&self) {
        let iv = self.state();
        let Some(since) = iv.settings_dirty_since.get() else {
            return;
        };
        if since.elapsed() < SETTINGS_SAVE_DELAY {
            return;
        }
        iv.settings_dirty_since.set(None);
        let settings = self.settings();
        match settings.save(&iv.settings_path) {
            Ok(()) => eprintln!(
                "設定：已寫入 settings.toml（{}；{} 個覆寫：{}）",
                self.describe_view(),
                settings_store::leaf_count(&settings.theme),
                settings_store::describe(&settings.theme)
            ),
            Err(e) => eprintln!("設定：{} 寫入失敗：{e}", iv.settings_path.display()),
        }
    }

    // ---------- 設定視窗 ----------

    pub fn show_settings(&self) {
        let iv = self.state();
        let mtm = self.mtm();
        if iv.settings_panel.borrow().is_none() {
            let target: &AnyObject = self;
            *iv.settings_panel.borrow_mut() = Some(SettingsPanel::build(mtm, target));
        }
        self.refresh_settings_panel();
        self.set_settings_status("改了就生效、自動記住；「回復預設值」回到主題檔的值");
        if let Some(p) = iv.settings_panel.borrow().as_ref() {
            p.show(mtm);
        }
    }

    /// 用有效主題與目標檔填設定視窗（存在時），並更新時間軸那兩行。
    fn refresh_settings_panel(&self) {
        let iv = self.state();
        let guard = iv.settings_panel.borrow();
        let Some(p) = guard.as_ref() else { return };
        let theme = iv.theme.borrow();
        p.fill(&theme, &iv.store.borrow());
        let margin = min_lock_margin(&theme);
        p.set_timeline_info(&format!(
            "畫面{}。鎖定至少要提前 {:.1} 秒（起跑加暖機再加 1 秒），填得更晚會自動提早。",
            theme.beat.timeline().describe(),
            margin.as_secs_f64() + 1.0
        ));
    }

    fn set_settings_status(&self, text: &str) {
        if let Some(p) = self.state().settings_panel.borrow().as_ref() {
            p.set_status(text);
        }
    }

    /// 設定視窗的控制項一動：整個視窗讀回來、套用、存檔。欄位不是數字就只顯示錯誤、什麼都不動。
    fn setting_changed(&self) {
        let iv = self.state();
        let mut theme = iv.theme.borrow().clone();
        let mut store = iv.store.borrow().clone();
        {
            let guard = iv.settings_panel.borrow();
            let Some(p) = guard.as_ref() else { return };
            if let Err(e) = p.read(&mut theme, &mut store) {
                p.set_status(&e);
                return;
            }
        }
        self.set_settings_status("已套用");
        if store != *iv.store.borrow() {
            *iv.store.borrow_mut() = store.clone();
            if let Err(e) = store.save(&iv.store_path) {
                eprintln!("目標：寫入 {} 失敗：{e}", iv.store_path.display());
            }
            eprintln!(
                "設定：歸零前 {} 秒量測、{} 秒鎖定，已記在 target.toml",
                store.measure_before_s, store.lock_before_s
            );
            self.rearm_if_pending();
        }
        if theme != *iv.theme.borrow() {
            self.commit_theme(theme, "設定");
        }
        if self.session_purpose() != Some(Purpose::Countdown) {
            self.refresh_settings_panel();
        }
    }

    /// 「回復預設值」：把那一頁的鍵全部回到主題檔的值（對一般人來說就是預設值）。
    fn reset_settings_tab(&self, tag: isize) {
        let iv = self.state();
        let base = iv.base_theme.borrow().clone();
        let mut t = iv.theme.borrow().clone();
        match tag {
            TAB_TIMELINE => {
                t.beat.period_ms = base.beat.period_ms;
                t.beat.ticks = base.beat.ticks;
                t.beat.visual_lead_s = base.beat.visual_lead_s;
                t.beat.idle_ball = base.beat.idle_ball;
                t.beat.tail_ms = base.beat.tail_ms;
                t.beat.demo_lead_s = base.beat.demo_lead_s;
                let mut store = iv.store.borrow().clone();
                store.measure_before_s = DEFAULT_MEASURE_BEFORE.as_secs();
                store.lock_before_s = DEFAULT_LOCK_BEFORE.as_secs();
                if store != *iv.store.borrow() {
                    *iv.store.borrow_mut() = store.clone();
                    if let Err(e) = store.save(&iv.store_path) {
                        eprintln!("目標：寫入 {} 失敗：{e}", iv.store_path.display());
                    }
                    self.rearm_if_pending();
                }
            }
            TAB_BEAT => {
                t.beat.style = base.beat.style;
                t.beat.renderer = base.beat.renderer;
                t.beat.strip_height = base.beat.strip_height;
                t.beat.color = base.beat.color;
                t.beat.final_color = base.beat.final_color;
                t.beat.glow = base.beat.glow;
                t.beat.glow_width = base.beat.glow_width;
                t.beat.glow_opacity = base.beat.glow_opacity;
            }
            TAB_SOUND => {
                t.beat.volume = base.beat.volume;
                t.beat.tick_hz = base.beat.tick_hz;
                t.beat.tick_ms = base.beat.tick_ms;
                t.beat.final_ms = base.beat.final_ms;
                t.beat.visual_lead_ms = base.beat.visual_lead_ms;
            }
            _ => return,
        }
        self.set_settings_status("這一頁已回復預設值");
        if t != *iv.theme.borrow() {
            self.commit_theme(t, "設定");
        }
        if self.session_purpose() != Some(Purpose::Countdown) {
            self.refresh_settings_panel();
        }
    }

    // ---------- 目標時刻 ----------

    fn session_purpose(&self) -> Option<Purpose> {
        self.state().beat.borrow().as_ref().map(|s| s.purpose)
    }

    /// 現在的標準時間估計；模型不可用時退回系統時鐘（只給「目標是否已過」這種粗判斷用）。
    fn now_unix_ns(&self, model: &ClockModel) -> (i128, bool) {
        let now = HostTime::now();
        if model.is_usable() {
            (model.estimate_at(now).remote_unix_ns, true)
        } else {
            (now.as_nanos() as i128 + system_theta_ns(), false)
        }
    }

    pub fn show_target_panel(&self) {
        let iv = self.state();
        let mtm = self.mtm();
        if iv.target_panel.borrow().is_none() {
            let target: &AnyObject = self;
            let panel = TargetPanel::build(mtm, target);
            let model = iv.sampler.model();
            let (now_ns, _) = self.now_unix_ns(&model);
            panel.fill(&iv.store.borrow(), (now_ns / 1_000_000_000) as i64);
            panel.set_armed(iv.target.borrow().is_some());
            *iv.target_panel.borrow_mut() = Some(panel);
        }
        self.refresh_target_ui();
        if let Some(p) = iv.target_panel.borrow().as_ref() {
            p.show(mtm);
        }
    }

    /// 面板按「設定目標」：讀欄位、存檔、建狀態機（已在倒數就先停）。
    pub fn apply_target(&self) {
        let iv = self.state();
        let inputs = {
            let guard = iv.target_panel.borrow();
            let Some(p) = guard.as_ref() else { return };
            match p.read() {
                Ok(i) => i,
                Err(e) => {
                    p.set_status(&e);
                    return;
                }
            }
        };
        let model = iv.sampler.model();
        let (now_ns, from_model) = self.now_unix_ns(&model);
        let target_ns = inputs.target_unix_s as i128 * 1_000_000_000;
        if target_ns <= now_ns {
            let msg = format!(
                "目標時刻已過（依{}）",
                if from_model {
                    "標準時間"
                } else {
                    "系統時鐘"
                }
            );
            if let Some(p) = iv.target_panel.borrow().as_ref() {
                p.set_status(&msg);
            }
            return;
        }
        if self.session_purpose() == Some(Purpose::Countdown) {
            self.stop_beats("重新設定目標");
        }
        {
            let mut store = iv.store.borrow_mut();
            store.target_unix_s = Some(inputs.target_unix_s);
            store.set_lead(inputs.lead);
            if let Err(e) = store.save(&iv.store_path) {
                eprintln!("目標：寫入 {} 失敗：{e}", iv.store_path.display());
            }
        }
        let cfg = iv.store.borrow().config().expect("剛設了目標");
        self.arm(cfg);
    }

    fn arm(&self, cfg: TargetConfig) {
        let iv = self.state();
        let margin = min_lock_margin(&iv.theme.borrow());
        *iv.target.borrow_mut() = Some(TargetMachine::new(cfg, margin));
        eprintln!(
            "目標：設定為 {}，歸零 {}（提前 {:.0} ms），歸零前 {} 秒量測、{} 秒鎖定",
            local_datetime(cfg.target_unix_ns).date_time_string(),
            local_datetime(cfg.zero_unix_ns()).date_time_string(),
            cfg.lead.total_ms(),
            cfg.measure_before.as_secs(),
            cfg.lock_before.as_secs()
        );
        if !iv
            .face
            .borrow()
            .as_ref()
            .is_some_and(|f| f.has_target_row())
        {
            self.rebuild_face(true);
        }
        if let Some(p) = iv.target_panel.borrow().as_ref() {
            p.set_armed(true);
        }
        self.refresh_menu();
        self.refresh_target_ui();
    }

    /// 時間軸或量測／鎖定提前改了、而目標還在待命或量測：用新的設定重建狀態機（鎖定後不動）。
    fn rearm_if_pending(&self) {
        let iv = self.state();
        let pending = matches!(
            iv.target.borrow().as_ref().map(|m| m.stage()),
            Some(Stage::Armed | Stage::Measuring)
        );
        if !pending {
            return;
        }
        let Some(cfg) = iv.store.borrow().config() else {
            return;
        };
        let margin = min_lock_margin(&iv.theme.borrow());
        let m = TargetMachine::new(cfg, margin);
        eprintln!(
            "目標：重排時間軸，歸零前 {} 秒量測、{} 秒鎖定（至少 {:.1} 秒）",
            m.config().measure_before.as_secs(),
            m.config().lock_before.as_secs(),
            margin.as_secs_f64() + 1.0
        );
        *iv.target.borrow_mut() = Some(m);
        self.refresh_target_ui();
    }

    pub fn clear_target(&self) {
        let iv = self.state();
        if self.session_purpose() == Some(Purpose::Countdown) {
            self.stop_beats("解除目標");
        }
        if iv.target.borrow().is_none() {
            eprintln!("目標：目前沒有目標");
            return;
        }
        *iv.target.borrow_mut() = None;
        {
            let mut store = iv.store.borrow_mut();
            store.target_unix_s = None;
            if let Err(e) = store.save(&iv.store_path) {
                eprintln!("目標：寫入 {} 失敗：{e}", iv.store_path.display());
            }
        }
        if iv.sampler.is_paused() {
            iv.sampler.resume();
        }
        self.rebuild_face(true);
        if let Some(p) = iv.target_panel.borrow().as_ref() {
            p.set_armed(false);
            p.set_status("沒有目標");
            p.set_zero("");
        }
        self.refresh_menu();
        eprintln!("目標：已解除");
    }

    pub fn measure_now(&self) {
        let iv = self.state();
        iv.sampler.resample_now();
        if let Some(p) = iv.target_panel.borrow().as_ref() {
            p.set_status("已要求標準時間立刻重新校時（一輪約 12 秒）");
        }
        eprintln!("目標：手動量測，已要求重新校時");
    }

    /// 狀態機走一步：量測、鎖定、中止。每格呼叫，成本可忽略。
    fn step_target(&self, model: &ClockModel) {
        let iv = self.state();
        let action = {
            let mut guard = iv.target.borrow_mut();
            let Some(m) = guard.as_mut() else { return };
            if m.is_over() || m.is_locked() {
                return;
            }
            let now_remote = model
                .is_usable()
                .then(|| model.estimate_at(HostTime::now()).remote_unix_ns);
            m.step(now_remote, false)
        };
        match action {
            None | Some(Action::Finish) => {}
            Some(Action::Measure) => {
                iv.sampler.resample_now();
                eprintln!("目標：進入量測，已要求標準時間重新校時");
                self.refresh_target_ui();
            }
            Some(Action::Lock) => self.lock_target(model),
            Some(Action::Abort(reason)) => {
                eprintln!("目標：中止（{}）", abort_text(reason));
                self.refresh_target_ui();
            }
        }
    }

    /// 鎖定：凍結模型、用凍結的模型反解歸零的主機時刻、開倒數的節拍程序。
    fn lock_target(&self, model: &ClockModel) {
        let iv = self.state();
        let zero_remote = match iv.target.borrow().as_ref() {
            Some(m) => m.config().zero_unix_ns(),
            None => return,
        };
        let plan = iv.theme.borrow().beat.plan(model.host_at(zero_remote));
        iv.sampler.pause();
        let est = model.estimate_at(HostTime::now());
        eprintln!(
            "目標：鎖定，模型凍結（± {:.1} ms，{}），歸零於本地 {}，第一拍 {:.1} 秒後",
            ms(est.half_width_ns as i128),
            status_word(model).unwrap_or("?"),
            local_datetime(zero_remote).date_time_string(),
            plan.first_tick()
                .saturating_duration_since(HostTime::now())
                .as_secs_f64()
        );
        if !self.start_plan(plan, Purpose::Countdown) {
            iv.sampler.resume();
            if let Some(m) = iv.target.borrow_mut().as_mut() {
                m.abort(AbortReason::TooLateToLock);
            }
            eprintln!("目標：排不進拍點表，中止並解凍");
        }
        self.refresh_target_ui();
    }

    /// 目標列與面板狀態的文字。沒有目標回 `None`。
    fn target_texts(&self, model: &ClockModel) -> Option<(String, String)> {
        let iv = self.state();
        let guard = iv.target.borrow();
        let m = guard.as_ref()?;
        let cfg = m.config();
        let target_clock = local_datetime(cfg.target_unix_ns).clock;
        let head = format!(
            "目標 {:02}:{:02}:{:02}",
            target_clock.hour, target_clock.minute, target_clock.second
        );
        let now = HostTime::now();
        let tail = match m.stage() {
            Stage::Armed | Stage::Measuring => {
                let word = if m.stage() == Stage::Armed {
                    "待命"
                } else {
                    "量測中"
                };
                if model.is_usable() {
                    let remaining = cfg.target_unix_ns - model.estimate_at(now).remote_unix_ns;
                    format!(
                        " · {} · 提前 {:.0} ms · {word}",
                        remaining_text(remaining),
                        cfg.lead.total_ms()
                    )
                } else {
                    " · 等待校時".to_string()
                }
            }
            Stage::Locked => {
                let z = local_datetime(cfg.zero_unix_ns()).clock;
                format!(
                    " · 已鎖定 · 歸零 {:02}:{:02}:{:02}.{:03}",
                    z.hour, z.minute, z.second, z.millis
                )
            }
            Stage::Done => " · 已結束".to_string(),
            Stage::Aborted(reason) => format!(" · {}", abort_text(reason)),
        };
        let row = format!("{head}{tail}");
        let status = format!(
            "{row}\n{}",
            text::menu_rows(model, now, system_theta_ns()).0
        );
        Some((row, status))
    }

    /// 更新目標面板上的狀態與歸零時刻（面板存在時），以及時鐘面板的目標列。
    fn refresh_target_ui(&self) {
        let iv = self.state();
        let model = iv.sampler.model();
        let texts = self.target_texts(&model);
        if let Some(face) = iv.face.borrow_mut().as_mut()
            && let Some((row, _)) = &texts
        {
            face.set_target(row);
        }
        // 校正進行中，狀態列歸校正流程管。
        if iv.calibration.borrow().is_some() {
            return;
        }
        if let Some(p) = iv.target_panel.borrow().as_ref() {
            match &texts {
                Some((_, status)) => {
                    p.set_status(status);
                    if let Some(m) = iv.target.borrow().as_ref() {
                        let cfg = m.config();
                        p.set_zero(&format!(
                            "{}（提前 {:.0} ms）",
                            local_datetime(cfg.zero_unix_ns()).date_time_string(),
                            cfg.lead.total_ms()
                        ));
                    }
                }
                None => {
                    p.set_status("沒有目標");
                    p.set_zero("");
                }
            }
        }
    }

    // ---------- 反應時間校正 ----------

    /// 校正的每一輪：歸零在「現在＋畫面起跑所需時間＋半秒」。不需要模型，純主機時間。
    fn calibration_plan(&self) -> BeatPlan {
        let theme = self.state().theme.borrow();
        let lead = theme.beat.timeline().visual_lead + Duration::from_millis(500);
        theme.beat.plan(HostTime::now() + lead)
    }

    pub fn start_calibration(&self) {
        let iv = self.state();
        if iv.calibration.borrow().is_some() {
            self.stop_beats("停止校正");
            return;
        }
        self.show_target_panel();
        if let Some(p) = self.session_purpose() {
            let msg = format!("{}進行中，先停止節拍再校正", p.label());
            if let Some(panel) = iv.target_panel.borrow().as_ref() {
                panel.set_status(&msg);
            }
            eprintln!("校正：{msg}");
            return;
        }
        let Some(run) = CalibrationRun::new(CALIBRATION_ROUNDS) else {
            eprintln!("校正：無法安裝事件監聽");
            return;
        };
        let total = run.total();
        *iv.calibration.borrow_mut() = Some(run);
        iv.pending_reaction_ms.set(None);
        let plan = self.calibration_plan();
        if !self.start_plan(plan, Purpose::Calibration) {
            *iv.calibration.borrow_mut() = None;
            return;
        }
        if let Some(p) = iv.target_panel.borrow().as_ref() {
            p.set_calibrating(true);
            p.set_calibration("", false);
            p.set_status(&format!(
                "校正中：第 1/{total} 輪。在歸零那一拍按下搶票用的鍵，或點 kairos 的任一視窗"
            ));
        }
        eprintln!(
            "校正：開始 {total} 輪，提示是目前開著的（聲音{}、光暈{}）；只開聲音或只開畫面各跑一次，兩者的差就是 visual_lead_ms 該填的值",
            if iv.sound_on.get() { "開" } else { "關" },
            if iv.glow_on.get() { "開" } else { "關" }
        );
    }

    /// 一輪跑完（或被中途停止）。
    fn calibration_round_ended(&self, plan: &BeatPlan, completed: bool) {
        let iv = self.state();
        if !completed {
            *iv.calibration.borrow_mut() = None;
            if let Some(p) = iv.target_panel.borrow().as_ref() {
                p.set_calibrating(false);
                p.set_status("校正已取消");
            }
            eprintln!("校正：取消");
            return;
        }
        let (finished, next_round, total) = {
            let mut guard = iv.calibration.borrow_mut();
            let Some(run) = guard.as_mut() else { return };
            let r = run.finish_round(plan.zero);
            eprintln!(
                "校正：第 {} 輪 {}",
                run.rounds.len(),
                r.map_or("沒按到".to_string(), |ms| format!("{ms:+.0} ms"))
            );
            (run.is_complete(), run.next_round(), run.total())
        };
        if finished {
            let run = iv.calibration.borrow_mut().take().expect("剛才還在");
            let summary = run.summary();
            let median = run.median_ms();
            iv.pending_reaction_ms.set(median);
            {
                let mut store = iv.store.borrow_mut();
                store.calibration_ms = run.samples_ms();
                if let Err(e) = store.save(&iv.store_path) {
                    eprintln!("目標：寫入 {} 失敗：{e}", iv.store_path.display());
                }
            }
            if let Some(p) = iv.target_panel.borrow().as_ref() {
                p.set_calibrating(false);
                p.set_calibration(&summary, median.is_some());
                p.set_status(
                    match median {
                        Some(m) => {
                            format!("校正完成：中位 {m:+.0} ms。按「採用校正結果」寫進反應時間")
                        }
                        None => "校正完成，但沒有任何一輪按到".to_string(),
                    }
                    .as_str(),
                );
            }
            eprintln!("校正：完成，{summary}");
            return;
        }
        if let Some(run) = iv.calibration.borrow().as_ref() {
            run.clear_events();
        }
        let plan = self.calibration_plan();
        if !self.start_plan(plan, Purpose::Calibration) {
            *iv.calibration.borrow_mut() = None;
            return;
        }
        if let Some(p) = iv.target_panel.borrow().as_ref() {
            p.set_status(&format!("校正中：第 {next_round}/{total} 輪"));
        }
    }

    pub fn adopt_calibration(&self) {
        let iv = self.state();
        let Some(ms) = iv.pending_reaction_ms.get() else {
            eprintln!("校正：沒有可採用的結果");
            return;
        };
        {
            let mut store = iv.store.borrow_mut();
            store.reaction_ms = ms.round();
            if let Err(e) = store.save(&iv.store_path) {
                eprintln!("目標：寫入 {} 失敗：{e}", iv.store_path.display());
            }
        }
        if let Some(p) = iv.target_panel.borrow().as_ref() {
            p.set_reaction(ms.round());
            p.set_calibration("", false);
            p.set_status(&format!(
                "反應時間已設為 {:.0} ms；有目標的話按「更新目標」套用",
                ms.round()
            ));
        }
        iv.pending_reaction_ms.set(None);
        eprintln!("校正：採用 {:.0} ms 為反應時間", ms.round());
    }

    // ---------- 每格 ----------

    /// 旗標都在這裡處理，保證在主執行緒、每格最多一次。倒數期間不重建面板。
    fn service_flags(&self) {
        let iv = self.state();
        if iv.flags.screen_dirty.swap(false, Ordering::SeqCst) {
            self.refresh_screen(false);
        }
        self.service_gestures();
        // 拖邊緣：視窗大小由 AppKit 改，我們只反推倍率；放手時再算一次最終寬度。
        let ended = iv.flags.live_resize_ended.swap(false, Ordering::SeqCst);
        if (iv.flags.resize_dirty.swap(false, Ordering::SeqCst) && iv.views.in_live_resize())
            || ended
        {
            let base_width = iv.theme.borrow().panel.width;
            iv.zoom_pending
                .set(Some(clamp_zoom(iv.views.content_width() / base_width)));
        }
        let settle = ended || iv.gestures.magnify_ended.replace(false);
        self.service_zoom(settle);
        self.service_settings_save();
        if self.session_purpose() == Some(Purpose::Countdown) {
            return;
        }
        if iv.flags.scale_dirty.swap(false, Ordering::SeqCst) {
            eprintln!("螢幕縮放變成 {}x，重建字形圖集", iv.views.scale());
            self.rebuild_face(false);
        }
        if iv.flags.theme_dirty.swap(false, Ordering::SeqCst)
            && iv.theme_dirty_since.get().is_none()
        {
            iv.theme_dirty_since.set(Some(HostTime::now()));
        }
        if let Some(since) = iv.theme_dirty_since.get()
            && since.elapsed() >= THEME_RELOAD_DELAY
        {
            iv.theme_dirty_since.set(None);
            self.reload_theme(false);
        }
    }

    /// 把時鐘面板（與目標面板，若已建立）離屏渲染成 PNG。毛玻璃由視窗伺服器合成，不會出現在圖裡。
    fn snapshot(&self, dir: &std::path::Path, frame: u64) {
        let iv = self.state();
        snapshot_view(&iv.views.effect, &dir.join(format!("panel-{frame}.png")));
        if let Some(p) = iv.target_panel.borrow().as_ref()
            && let Some(view) = p.panel.contentView()
        {
            snapshot_view(&view, &dir.join(format!("target-{frame}.png")));
        }
        if let Some(p) = iv.settings_panel.borrow().as_ref()
            && let Some(view) = p.panel.contentView()
        {
            snapshot_view(&view, &dir.join(format!("settings-{frame}.png")));
        }
    }

    fn on_frame(&self, link: &CADisplayLink) {
        self.service_flags();
        let iv = self.state();
        let frame = iv.frames.get() + 1;
        iv.frames.set(frame);
        if let Some(dir) = &iv.snapshot_dir
            && iv.snapshot_frames.contains(&frame)
        {
            self.snapshot(dir, frame);
        }

        let target_s = link.targetTimestamp();
        let lead = Duration::from_secs_f64(iv.lead_ms.get().max(0.0) / 1e3);
        // targetTimestamp 是 CACurrentMediaTime 基準的秒數，也就是 mach_absolute_time。
        let at = HostTime::from_nanos((target_s * 1e9) as u64) + lead;
        if let Some(session) = iv.beat.borrow_mut().as_mut()
            && session.callback_late_ms.len() < 4_096
        {
            let now_s = HostTime::now().as_nanos() as f64 / 1e9;
            session
                .callback_late_ms
                .push((now_s - link.timestamp()) * 1e3);
        }

        let model = iv.sampler.model();
        if iv.demo_pending.get() && model.is_usable() {
            iv.demo_pending.set(false);
            self.start_demo_beats();
        }
        if iv.calibrate_pending.get() && frame > 60 {
            iv.calibrate_pending.set(false);
            self.start_calibration();
        }
        if iv.settings_pending.get() && frame > 30 {
            iv.settings_pending.set(false);
            self.show_settings();
        }
        self.step_target(&model);
        let shown = iv.smoother.borrow_mut().tick(&model, at);
        iv.last_slewing.set(shown.is_some_and(|s| s.slewing));
        let mut face_slot = iv.face.borrow_mut();
        let Some(face) = face_slot.as_mut() else {
            return;
        };
        let theme = iv.theme.borrow();
        let bar_scale = theme.layout.bar_full_scale_ms;

        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let second = match shown {
            Some(s) => {
                let wall = iv.local.borrow_mut().wall(s.remote_unix_ns);
                face.set_digits(&wall.digits());
                let hw_ms = ms(s.half_width_ns as i128);
                face.set_detail(&format!(
                    "± {hw_ms:.1} ms · {}",
                    status_word(&model).unwrap_or("")
                ));
                face.set_bar(Some(hw_ms / bar_scale));
                Some(s.remote_unix_ns.div_euclid(1_000_000_000))
            }
            None => {
                face.set_digits(&DASHES);
                face.set_detail(status_word(&model).err().unwrap_or("校時中…"));
                face.set_bar(None);
                face.set_caption("標準時間（NTP）");
                // 沒有模型時也要每秒更新目標列，用主機秒數當節拍。
                Some(-(at.as_nanos() as i128 / 1_000_000_000))
            }
        };
        if iv.caption_second.get() != second {
            iv.caption_second.set(second);
            if let Some(s) = shown {
                let diff = s.remote_unix_ns - (at.as_nanos() as i128 + system_theta_ns());
                face.set_caption(&format!("標準時間（NTP）· {}", system_clock_text(diff)));
            }
            if let Some((row, status)) = self.target_texts(&model) {
                face.set_target(&row);
                if let Some(p) = iv.target_panel.borrow().as_ref()
                    && p.is_visible()
                    && iv.calibration.borrow().is_none()
                {
                    p.set_status(&status);
                }
            }
        }
        // 暫時文字蓋過說明列；到期就讓下一格重算平常的說明。
        let toast = iv.toast.borrow().clone();
        if let Some((text, since)) = toast {
            if since.elapsed() < TOAST_DURATION {
                face.set_caption(&text);
            } else {
                *iv.toast.borrow_mut() = None;
                iv.caption_second.set(None);
            }
        }

        // 節拍：畫面的相位用「這一格上屏的時刻」加畫面相對聲音的提前量。
        let mut ended = false;
        if let Some(session) = iv.beat.borrow_mut().as_mut() {
            let (ahead, vlead) = theme.beat.visual_lead();
            let at_v = if ahead { at + vlead } else { at - vlead };
            if at_v >= session.plan.end() {
                ended = true;
            } else {
                // 起跑前：要嘛畫靜止的球（`Phase::idle`），要嘛整個藏起來。
                let phase = session
                    .plan
                    .phase_at(at_v)
                    .or_else(|| theme.beat.idle_ball.then(|| Phase::idle(&session.plan)));
                match (face.strip.as_mut(), &phase) {
                    (Some(strip), Some(p)) => strip.render(p, at),
                    (Some(strip), None) => strip.set_hidden(true),
                    (None, _) => {}
                }
                if let Some(glow) = session.glow.as_mut() {
                    match &phase {
                        Some(p) => glow.set(glow_level(p), p.is_final),
                        None => glow.set(0.0, false),
                    }
                }
                for (k, t) in session.plan.tick_times() {
                    let d = at_v.signed_nanos_since(t) as f64 / 1e6;
                    let slot = &mut session.nearest_frame_ms[k as usize];
                    if slot.is_none_or(|prev| d.abs() < prev.abs()) {
                        *slot = Some(d);
                    }
                }
            }
        }
        CATransaction::commit();
        drop(theme);
        drop(face_slot);
        if ended {
            self.stop_beats("結束");
        }
    }
}

fn snapshot_view(view: &objc2_app_kit::NSView, path: &std::path::Path) {
    let rect = view.bounds();
    let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(rect) else {
        return;
    };
    // 先鋪一層深灰：視窗背景不在 view 裡，深色外觀的白字才看得到。
    if let Some(ctx) = objc2_app_kit::NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep) {
        objc2_app_kit::NSGraphicsContext::saveGraphicsState_class();
        objc2_app_kit::NSGraphicsContext::setCurrentContext(Some(&ctx));
        objc2_app_kit::NSColor::colorWithSRGBRed_green_blue_alpha(0.16, 0.16, 0.16, 1.0).setFill();
        objc2_app_kit::NSRectFill(rect);
        objc2_app_kit::NSGraphicsContext::restoreGraphicsState_class();
    }
    view.cacheDisplayInRect_toBitmapImageRep(rect, &rep);
    // SAFETY: 空的屬性字典是合法輸入。
    let data = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    };
    if let Some(data) = data {
        let ok = data.writeToFile_atomically(&NSString::from_str(&path.to_string_lossy()), true);
        eprintln!(
            "快照 {}：{}",
            path.display(),
            if ok { "已寫入" } else { "寫入失敗" }
        );
    }
}

/// 鎖定至少要在歸零前多久：畫面起跑所需的時間加音訊暖機。
fn min_lock_margin(theme: &Theme) -> Duration {
    theme.beat.timeline().visual_lead + Duration::from_millis(500)
}

/// 哪些改動要重建面板：幾何、字型、顏色、節拍區的畫法、高度與顏色。
/// 不透明度、時序、聲音、光暈、顯示提前量、時間來源都各有更輕的套法。
fn needs_rebuild(old: &Theme, new: &Theme) -> bool {
    let mut probe = new.clone();
    probe.panel.opacity = old.panel.opacity;
    probe.sync = old.sync.clone();
    probe.display = old.display.clone();
    probe.beat = Beat {
        renderer: new.beat.renderer,
        strip_height: new.beat.strip_height,
        color: new.beat.color,
        final_color: new.beat.final_color,
        ..old.beat.clone()
    };
    probe != *old
}
