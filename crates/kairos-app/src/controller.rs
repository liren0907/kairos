//! 面板的控制器：`CADisplayLink` 的 target、選單動作的 target、主題與螢幕縮放變動的處理。
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
    NSWindowDidChangeBackingPropertiesNotification,
};
use objc2_foundation::{
    NSDictionary, NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSRunLoop,
    NSRunLoopCommonModes, NSString,
};
use objc2_quartz_core::{CADisplayLink, CAFrameRateRange, CATransaction};

use kairos_core::display::{DASHES, DisplaySmoother, LocalTime, SmootherConfig};
use kairos_core::sync::SamplerHandle;
use kairos_core::time::{HostTime, system_theta_ns};

use crate::panel::{Face, PanelViews, build_panel};
use crate::text::{ms, status_word, system_clock_text};
use crate::theme::{self, Theme};

/// 其他執行緒（檔案監看、通知）只能設旗標，主執行緒在下一格處理。
#[derive(Default)]
pub struct Flags {
    pub theme_dirty: AtomicBool,
    pub scale_dirty: AtomicBool,
}

/// 存檔後等這麼久再讀，讓編輯器把檔案寫完。
const THEME_RELOAD_DELAY: Duration = Duration::from_millis(150);
/// 前幾格印顯示連結的時序，確認提前量與週期。
const DIAG_FRAMES: usize = 300;
/// 設了 `KAIROS_SNAPSHOT_DIR` 時，在這幾格把面板離屏渲染成 PNG（沒有毛玻璃，只有內容）。
/// 開發用：沒有螢幕錄製權限也能看到版面。
const SNAPSHOT_FRAMES: &[u64] = &[120, 900];

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
    smoother: RefCell<DisplaySmoother>,
    local: RefCell<LocalTime>,
    link: RefCell<Option<Retained<CADisplayLink>>>,
    click_through: Cell<bool>,
    menu: RefCell<Option<MenuItems>>,
    diag: RefCell<FrameDiag>,
    /// 說明列每秒算一次；記上次算的是哪一秒。
    caption_second: Cell<Option<i128>>,
    frames: Cell<u64>,
    snapshot_dir: Option<PathBuf>,
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
            smoother: RefCell::new(DisplaySmoother::new(SmootherConfig::default())),
            local: RefCell::new(LocalTime::new()),
            link: RefCell::new(None),
            click_through: Cell::new(false),
            menu: RefCell::new(None),
            diag: RefCell::new(FrameDiag::default()),
            caption_second: Cell::new(None),
            frames: Cell::new(0),
            snapshot_dir: std::env::var_os("KAIROS_SNAPSHOT_DIR").map(PathBuf::from),
        });
        let this = Self::alloc(mtm).set_ivars(Ivars { state });
        // SAFETY: NSObject 的 init。
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        this.rebuild_face();
        this
    }

    pub fn set_menu_items(&self, items: MenuItems) {
        *self.state().menu.borrow_mut() = Some(items);
    }

    /// 建立顯示連結、訂閱螢幕縮放變動、把面板秀出來。
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
        self.apply_frame_rate(&link);
        *iv.link.borrow_mut() = Some(link);

        let flags = iv.flags.clone();
        let block = RcBlock::new(move |_n: NonNull<NSNotification>| {
            flags.scale_dirty.store(true, Ordering::SeqCst);
        });
        // SAFETY: 通知名是 AppKit 公開常數；只看我們自己的面板；閉包只捕捉 Arc<Flags>，是 Send。
        let token = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSWindowDidChangeBackingPropertiesNotification),
                Some(&iv.views.panel),
                None,
                &block,
            )
        };
        *iv._backing_observer.borrow_mut() = Some(token);

        iv.views.panel.orderFrontRegardless();
    }

    fn apply_frame_rate(&self, link: &CADisplayLink) {
        let fps = self
            .state()
            .theme
            .borrow()
            .display
            .idle_fps
            .clamp(10.0, 240.0) as f32;
        link.setPreferredFrameRateRange(CAFrameRateRange {
            minimum: fps,
            maximum: fps,
            preferred: fps,
        });
    }

    fn rebuild_face(&self) {
        let iv = self.state();
        let mtm = self.mtm();
        if let Some(old) = iv.face.borrow_mut().take() {
            old.teardown();
        }
        let theme = iv.theme.borrow();
        let scale = iv.views.scale();
        let face = Face::build(mtm, &iv.views, &theme, scale);
        iv.views.apply_theme(&theme, face.height);
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
                self.rebuild_face();
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

    /// 旗標都在這裡處理，保證在主執行緒、每格最多一次。
    fn service_flags(&self) {
        let iv = self.state();
        if iv.flags.scale_dirty.swap(false, Ordering::SeqCst) {
            eprintln!("螢幕縮放變成 {}x，重建字形圖集", iv.views.scale());
            self.rebuild_face();
        }
        if iv.flags.theme_dirty.swap(false, Ordering::SeqCst)
            && iv.theme_dirty_since.get().is_none()
        {
            iv.theme_dirty_since.set(Some(HostTime::now()));
        }
        if let Some(since) = iv.theme_dirty_since.get() {
            if since.elapsed() >= THEME_RELOAD_DELAY {
                iv.theme_dirty_since.set(None);
                self.reload_theme(false);
            }
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
        if let Some(dir) = &iv.snapshot_dir {
            if SNAPSHOT_FRAMES.contains(&frame) {
                self.snapshot(&dir.join(format!("panel-{frame}.png")));
            }
        }

        let target_s = link.targetTimestamp();
        let lead = Duration::from_secs_f64(iv.theme.borrow().display.lead_ms.max(0.0) / 1e3);
        // targetTimestamp 是 CACurrentMediaTime 基準的秒數，也就是 mach_absolute_time。
        let at = HostTime::from_nanos((target_s * 1e9) as u64) + lead;

        let model = iv.sampler.model();
        let shown = iv.smoother.borrow_mut().tick(&model, at);
        let mut face_slot = iv.face.borrow_mut();
        let Some(face) = face_slot.as_mut() else {
            return;
        };
        let bar_scale = iv.theme.borrow().layout.bar_full_scale_ms;

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
        CATransaction::commit();
    }
}
