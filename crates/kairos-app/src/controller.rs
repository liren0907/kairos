//! 面板的控制器：`CADisplayLink` 的 target、選單與目標面板按鈕的 target、主題與螢幕變動的
//! 處理、節拍程序（拍點表、節拍區、光暈、聲音）的生命週期，以及目標時刻模式的接線
//! （狀態機每格步進、鎖定時凍結取樣、反應時間校正）。
//!
//! 這是程式裡唯一的 `define_class!`：顯示連結、選單、按鈕都只能給 target 加 selector，
//! 沒有 block 版。所有狀態放在 ivars，以 `Cell`／`RefCell` 做內部可變性。

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use block2::RcBlock;
use notify::RecommendedWatcher;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBitmapImageFileType, NSControlStateValueOff, NSControlStateValueOn, NSMenuItem,
    NSWindowDidChangeBackingPropertiesNotification, NSWindowDidChangeScreenNotification,
    NSWorkspace,
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
    AbortReason, Action, CALIBRATION_ROUNDS, Stage, TargetConfig, TargetMachine,
};
use kairos_core::time::{HostTime, system_theta_ns};

use crate::audio::AudioEngine;
use crate::beat_view::StripKind;
use crate::calibrate::CalibrationRun;
use crate::glow::Glow;
use crate::panel::{Face, PanelViews, build_panel};
use crate::target_panel::TargetPanel;
use crate::target_store::TargetFile;
use crate::text::{self, ms, remaining_text, status_word, system_clock_text};
use crate::theme::{self, BeatStyle, Theme};

/// 其他執行緒（檔案監看、通知）只能設旗標，主執行緒在下一格處理。
#[derive(Default)]
pub struct Flags {
    pub theme_dirty: AtomicBool,
    pub scale_dirty: AtomicBool,
    pub screen_dirty: AtomicBool,
}

/// 存檔後等這麼久再讀，讓編輯器把檔案寫完。
const THEME_RELOAD_DELAY: Duration = Duration::from_millis(150);
/// 前幾格印顯示連結的時序，確認提前量與週期。
const DIAG_FRAMES: usize = 300;
/// 設了 `KAIROS_SNAPSHOT_DIR` 時，在這幾格把面板離屏渲染成 PNG（沒有毛玻璃，只有內容）；
/// `KAIROS_SNAPSHOT_FRAMES="120,600"` 可改格數。開發用：沒有螢幕錄製權限也能看到版面。
const SNAPSHOT_FRAMES: &[u64] = &[120, 900];
/// 試聽節拍：歸零訂在「現在＋這麼久」之後的下一個整秒。
const DEMO_LEAD: Duration = Duration::from_secs(5);
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

        #[unsafe(method(showTargetPanel:))]
        fn show_target_panel_action(&self, _sender: Option<&AnyObject>) {
            self.show_target_panel();
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

    /// `theme` 由 main 先讀好（取樣執行緒的設定也從它來），這裡只接手。
    pub fn new(
        mtm: MainThreadMarker,
        sampler: SamplerHandle,
        theme: Theme,
        theme_path: PathBuf,
        store_path: PathBuf,
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

        iv.views.panel.orderFrontRegardless();
        self.refresh_screen(true);
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
        let theme = iv.theme.borrow();
        let scale = iv.views.scale();
        let face = Face::build(mtm, &iv.views, &theme, scale, strip, target_row);
        iv.views.apply_theme(&theme, face.height, keep_bottom);
        *iv.face.borrow_mut() = Some(face);
        iv.caption_second.set(None);
        if let Some(link) = iv.link.borrow().as_ref() {
            self.apply_frame_rate(link);
        }
    }

    /// 重讀主題檔。`verbose` 時連「沒變」也回報（選單手動觸發用）。
    pub fn reload_theme(&self, verbose: bool) {
        let iv = self.state();
        match std::fs::read_to_string(&iv.theme_path)
            .map_err(|e| e.to_string())
            .and_then(|t| Theme::parse(&t).map_err(|e| e.to_string()))
        {
            Ok(new) => {
                if *iv.theme.borrow() == new {
                    if verbose {
                        eprintln!("主題：沒有變動");
                    }
                    return;
                }
                let sync_changed = iv.theme.borrow().sync != new.sync;
                *iv.theme.borrow_mut() = new;
                self.rebuild_face(false);
                self.refresh_screen(false);
                eprintln!("主題：已重新載入 {}", iv.theme_path.display());
                if sync_changed {
                    match iv.theme.borrow().sync.settings() {
                        Ok(s) => {
                            eprintln!(
                                "主題：[sync] 有變，換成 {}{}",
                                s.servers.join("、"),
                                if iv.sampler.is_paused() {
                                    "（鎖定中，解凍後套用）"
                                } else {
                                    ""
                                }
                            );
                            iv.sampler.reconfigure(s);
                        }
                        Err(e) => eprintln!("主題：[sync] 不合法（{e}），時間來源沿用上一版"),
                    }
                }
            }
            Err(e) => eprintln!(
                "主題：{} 讀取失敗，沿用上一版：{e}",
                iv.theme_path.display()
            ),
        }
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

    /// 試聽：歸零訂在「現在＋5 秒」之後的下一個標準時間整秒，歸零拍會跟數字翻到 `.000`
    /// 同一瞬間，光看面板就能驗證球落地與翻頁是不是同一刻。
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
        let target = next_whole_second(&model, HostTime::now() + DEMO_LEAD);
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
        let theme = iv.theme.borrow();
        let plan = BeatPlan::from_target(
            &model,
            target_remote_unix_ns,
            theme.beat.period(),
            theme.beat.ticks,
        );
        drop(theme);
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
        let kind = match theme.beat.style {
            BeatStyle::Auto if reduce_motion => StripKind::Pulse,
            BeatStyle::Auto | BeatStyle::Ball => StripKind::Ball,
            BeatStyle::Ring => StripKind::Ring,
            BeatStyle::Pulse => StripKind::Pulse,
        };
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
            "節拍（{}）：歸零於本地時間 {zero_local}（{:.3} 秒後），{} 拍、拍距 {} ms、樣式 {}、聲音{}、光暈 {} 個螢幕、刷新率 {:.0} Hz{}{}",
            purpose.label(),
            plan.zero.saturating_duration_since(now).as_secs_f64(),
            plan.ticks,
            theme.beat.period_ms,
            kind.label(),
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
            menu.beat_start.setTitle(&NSString::from_str(if active {
                "重新排節拍（5 秒後）"
            } else {
                "試聽節拍（5 秒後）"
            }));
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
        let plan = {
            let theme = iv.theme.borrow();
            BeatPlan::new(
                model.host_at(zero_remote),
                theme.beat.period(),
                theme.beat.ticks,
            )
        };
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
        let period = theme.beat.period();
        let lead = period * theme.beat.ticks + Duration::from_millis(500);
        BeatPlan::new(HostTime::now() + lead, period, theme.beat.ticks)
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

        // 節拍：畫面的相位用「這一格上屏的時刻」加畫面相對聲音的提前量。
        let mut ended = false;
        if let Some(session) = iv.beat.borrow_mut().as_mut() {
            let (ahead, vlead) = theme.beat.visual_lead();
            let at_v = if ahead { at + vlead } else { at - vlead };
            if at_v >= session.plan.end() {
                ended = true;
            } else {
                let phase = session
                    .plan
                    .phase_at(at_v)
                    .unwrap_or_else(|| Phase::idle(&session.plan));
                if let Some(strip) = face.strip.as_mut() {
                    strip.render(&phase, at);
                }
                if let Some(glow) = session.glow.as_mut() {
                    glow.set(glow_level(&phase), phase.is_final);
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

/// 鎖定至少要在歸零前多久：畫面提早一拍起跑（`period × ticks`）加音訊暖機。
fn min_lock_margin(theme: &Theme) -> Duration {
    theme.beat.period() * theme.beat.ticks + Duration::from_millis(500)
}
