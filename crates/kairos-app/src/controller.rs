//! 面板的控制器：`CADisplayLink` 的 target、選單動作的 target、主題與螢幕變動的處理，
//! 以及節拍程序（拍點表、節拍區、光暈、聲音）的生命週期。
//!
//! 這是程式裡唯一的 `define_class!`：顯示連結與選單動作都只能給 target 加 selector，
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
use kairos_core::display::{DASHES, DisplaySmoother, LocalTime, SmootherConfig};
use kairos_core::sync::SamplerHandle;
use kairos_core::time::{HostTime, system_theta_ns};

use crate::audio::AudioEngine;
use crate::beat_view::StripKind;
use crate::glow::Glow;
use crate::panel::{Face, PanelViews, build_panel};
use crate::text::{ms, status_word, system_clock_text};
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

fn snapshot_frames() -> Vec<u64> {
    std::env::var("KAIROS_SNAPSHOT_FRAMES")
        .ok()
        .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| SNAPSHOT_FRAMES.to_vec())
}
/// 試聽節拍：歸零訂在「現在＋這麼久」之後的下一個整秒。
const DEMO_LEAD: Duration = Duration::from_secs(5);
/// 第一拍離現在至少要這麼久，音訊串流才來得及暖機。
const MIN_LEAD: Duration = Duration::from_millis(500);

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
}

/// 一次節拍程序：從排程到歸零後收尾期結束。
struct BeatSession {
    plan: BeatPlan,
    kind: StripKind,
    audio: Option<AudioEngine>,
    glow: Option<Glow>,
    /// 每一拍：離落地最近的一格（含提前量）差多少毫秒，正值代表那一格在落地之後。
    nearest_frame_ms: Vec<Option<f64>>,
    fps: f64,
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
    /// 面板目前所在的螢幕：名稱、這個螢幕的顯示提前量、最高刷新率。
    screen_name: RefCell<Option<String>>,
    lead_ms: Cell<f64>,
    screen_fps: Cell<f64>,
    beat: RefCell<Option<BeatSession>>,
    sound_on: Cell<bool>,
    glow_on: Cell<bool>,
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
    }
);

impl Controller {
    fn state(&self) -> &State {
        &self.ivars().state
    }

    pub fn new(
        mtm: MainThreadMarker,
        sampler: SamplerHandle,
        theme_path: PathBuf,
    ) -> Retained<Self> {
        let theme = match Theme::load_or_create(&theme_path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("主題：讀取 {} 失敗（{e}），用預設值", theme_path.display());
                Theme::default()
            }
        };
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
            screen_name: RefCell::new(None),
            lead_ms: Cell::new(0.0),
            screen_fps: Cell::new(idle_fps),
            beat: RefCell::new(None),
            sound_on: Cell::new(true),
            glow_on: Cell::new(true),
        });
        let this = Self::alloc(mtm).set_ivars(Ivars { state });
        // SAFETY: NSObject 的 init。
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        this.rebuild_face(false);
        this
    }

    pub fn set_menu_items(&self, items: MenuItems) {
        *self.state().menu.borrow_mut() = Some(items);
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

    /// 重建面板內容。有節拍程序在跑就帶節拍區。
    fn rebuild_face(&self, keep_bottom: bool) {
        let iv = self.state();
        let mtm = self.mtm();
        if let Some(old) = iv.face.borrow_mut().take() {
            old.teardown();
        }
        let strip = iv.beat.borrow().as_ref().map(|s| s.kind);
        let theme = iv.theme.borrow();
        let scale = iv.views.scale();
        let face = Face::build(mtm, &iv.views, &theme, scale, strip);
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
                *iv.theme.borrow_mut() = new;
                self.rebuild_face(false);
                self.refresh_screen(false);
                eprintln!("主題：已重新載入 {}", iv.theme_path.display());
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

    /// 排一次節拍程序，歸零在標準時間 `target_remote_unix_ns`。階段四的倒數也走這裡。
    pub fn start_beats(&self, target_remote_unix_ns: i128) {
        let iv = self.state();
        if iv.beat.borrow().is_some() {
            self.stop_beats("重新排程");
        }
        let model = iv.sampler.model();
        if !model.is_usable() {
            eprintln!("節拍：標準時間模型還不能用，無法排程");
            return;
        }
        let theme = iv.theme.borrow().clone();
        let plan = BeatPlan::from_target(
            &model,
            target_remote_unix_ns,
            theme.beat.period(),
            theme.beat.ticks,
        );
        let now = HostTime::now();
        if plan.first_tick() < now + MIN_LEAD {
            eprintln!(
                "節拍：第一拍離現在不到 {} ms，來不及，放棄",
                MIN_LEAD.as_millis()
            );
            return;
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

        let wall = iv.local.borrow_mut().wall(target_remote_unix_ns);
        let digits = wall.digits();
        let summary = format!(
            "節拍：歸零於本地時間 {}（{:.3} 秒後），{} 拍、拍距 {} ms、樣式 {}、聲音{}、光暈 {} 個螢幕、刷新率 {:.0} Hz{}{}",
            String::from_utf8_lossy(&digits),
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
            kind,
            audio,
            glow,
            nearest_frame_ms: vec![None; plan.ticks as usize],
            fps: iv.screen_fps.get(),
        });
        self.rebuild_face(true);
        self.set_beat_menu(true);
        eprintln!("{summary}");
    }

    /// 結束節拍程序：印診斷、拆光暈、關聲音、收掉節拍區。
    pub fn stop_beats(&self, reason: &str) {
        let iv = self.state();
        let taken = iv.beat.borrow_mut().take();
        let Some(session) = taken else {
            eprintln!("節拍：目前沒有在跑");
            return;
        };
        eprintln!("節拍：{reason}");
        if let Some(a) = &session.audio {
            eprintln!("{}", a.report(&session.plan));
        }
        let mut line = format!(
            "節拍畫面：{:.0} Hz，半格 {:.1} ms",
            session.fps,
            500.0 / session.fps.max(1.0)
        );
        for (k, d) in session.nearest_frame_ms.iter().enumerate() {
            match d {
                Some(d) => line.push_str(&format!("；第 {k} 拍最近一格差 {d:+.1} ms")),
                None => line.push_str(&format!("；第 {k} 拍沒畫到")),
            }
        }
        eprintln!("{line}");
        if let Some(g) = &session.glow {
            g.teardown();
        }
        drop(session);
        self.rebuild_face(true);
        self.set_beat_menu(false);
    }

    fn set_beat_menu(&self, active: bool) {
        if let Some(menu) = self.state().menu.borrow().as_ref() {
            menu.beat_stop.setEnabled(active);
            menu.beat_start.setTitle(&NSString::from_str(if active {
                "重新排節拍（5 秒後）"
            } else {
                "試聽節拍（5 秒後）"
            }));
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

    /// 旗標都在這裡處理，保證在主執行緒、每格最多一次。
    fn service_flags(&self) {
        let iv = self.state();
        if iv.flags.scale_dirty.swap(false, Ordering::SeqCst) {
            eprintln!("螢幕縮放變成 {}x，重建字形圖集", iv.views.scale());
            self.rebuild_face(false);
        }
        if iv.flags.screen_dirty.swap(false, Ordering::SeqCst) {
            self.refresh_screen(false);
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

    /// 把面板內容區離屏渲染成 PNG。毛玻璃由視窗伺服器合成，不會出現在圖裡。
    fn snapshot(&self, path: &std::path::Path) {
        let view = &self.state().views.effect;
        let rect = view.bounds();
        let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(rect) else {
            return;
        };
        view.cacheDisplayInRect_toBitmapImageRep(rect, &rep);
        // SAFETY: 空的屬性字典是合法輸入。
        let data = unsafe {
            rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
        };
        if let Some(data) = data {
            let ok =
                data.writeToFile_atomically(&NSString::from_str(&path.to_string_lossy()), true);
            eprintln!(
                "快照 {}：{}",
                path.display(),
                if ok { "已寫入" } else { "寫入失敗" }
            );
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
            self.snapshot(&dir.join(format!("panel-{frame}.png")));
        }

        let target_s = link.targetTimestamp();
        let lead = Duration::from_secs_f64(iv.lead_ms.get().max(0.0) / 1e3);
        // targetTimestamp 是 CACurrentMediaTime 基準的秒數，也就是 mach_absolute_time。
        let at = HostTime::from_nanos((target_s * 1e9) as u64) + lead;

        let model = iv.sampler.model();
        if iv.demo_pending.get() && model.is_usable() {
            iv.demo_pending.set(false);
            self.start_demo_beats();
        }
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
        match shown {
            Some(s) => {
                let wall = iv.local.borrow_mut().wall(s.remote_unix_ns);
                face.set_digits(&wall.digits());
                let hw_ms = ms(s.half_width_ns as i128);
                face.set_detail(&format!(
                    "± {hw_ms:.1} ms · {}",
                    status_word(&model).unwrap_or("")
                ));
                face.set_bar(Some(hw_ms / bar_scale));

                let second = s.remote_unix_ns.div_euclid(1_000_000_000);
                if iv.caption_second.get() != Some(second) {
                    iv.caption_second.set(Some(second));
                    let diff = s.remote_unix_ns - (at.as_nanos() as i128 + system_theta_ns());
                    face.set_caption(&format!("標準時間（NTP）· {}", system_clock_text(diff)));
                }
            }
            None => {
                face.set_digits(&DASHES);
                face.set_detail(status_word(&model).err().unwrap_or("校時中…"));
                face.set_bar(None);
                face.set_caption("標準時間（NTP）");
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
                let phase = session
                    .plan
                    .phase_at(at_v)
                    .unwrap_or_else(|| Phase::idle(&session.plan));
                if let Some(strip) = face.strip.as_mut() {
                    strip.render(&phase);
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
