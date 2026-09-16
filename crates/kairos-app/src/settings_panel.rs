//! 設定視窗：選單列「設定…」打開的原生視窗，頁籤分組、即時生效、自動儲存、每頁一顆「回復預設值」。
//! 給不會開文字編輯器的人用；主題檔仍是給會改檔的人的路。
//!
//! 控制項只負責顯示與收值，邏輯都在控制器：任何控制項一動就送 `settingChanged:`，
//! 控制器把整個視窗讀回一份主題、跟主題檔比對出覆寫、套用、存到 `settings.toml`。
//! 版面跟目標面板一樣用固定座標排；每一頁是一個 `Page`，一列一列往下加。

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly, sel};
use std::cell::RefCell;

use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSColorSpace, NSColorWell, NSControlStateValue,
    NSControlStateValueOff, NSControlStateValueOn, NSFloatingWindowLevel, NSFont, NSFontManager,
    NSLineBreakMode, NSPanel, NSPopUpButton, NSScreen, NSSlider, NSStepper, NSTabView,
    NSTabViewItem, NSTextAlignment, NSTextField, NSView, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::NSString;

use crate::settings_store::{MAX_OPACITY, MIN_OPACITY};
use crate::target_store::TargetFile;
use crate::theme::{Appearance, BeatRenderer, BeatStyle, Color, Material, Theme, Weight};

const WIDTH: f64 = 560.0;
const HEIGHT: f64 = 500.0;
const MARGIN: f64 = 16.0;
const LABEL_W: f64 = 150.0;
const ROW_H: f64 = 32.0;
const STATUS_H: f64 = 20.0;
const CONTROL_X: f64 = MARGIN + LABEL_W + 8.0;

/// 頁籤的順序，也是「回復預設值」按鈕的 `tag`。
pub const TAB_TIMELINE: isize = 0;
pub const TAB_BEAT: isize = 1;
pub const TAB_SOUND: isize = 2;
pub const TAB_PANEL: isize = 3;
pub const TAB_FONT: isize = 4;
pub const TAB_DISPLAY: isize = 5;
pub const TAB_SYNC: isize = 6;

const STYLES: [BeatStyle; 4] = [
    BeatStyle::Auto,
    BeatStyle::Ball,
    BeatStyle::Ring,
    BeatStyle::Pulse,
];
const STYLE_TITLES: [&str; 4] = ["自動（系統減少動態效果時改脈衝）", "球", "環", "脈衝"];
const RENDERERS: [BeatRenderer; 3] = [BeatRenderer::Auto, BeatRenderer::Metal, BeatRenderer::Layer];
const RENDERER_TITLES: [&str; 3] = ["自動（有 Metal 就用）", "Metal", "CALayer"];
const MATERIALS: [Material; 6] = [
    Material::Hud,
    Material::Popover,
    Material::Sidebar,
    Material::Menu,
    Material::UnderWindow,
    Material::WindowBackground,
];
const MATERIAL_TITLES: [&str; 6] = [
    "毛玻璃（HUD）",
    "彈出框",
    "側欄",
    "選單",
    "視窗底",
    "視窗背景",
];
const APPEARANCES: [Appearance; 3] = [Appearance::Dark, Appearance::Light, Appearance::System];
const APPEARANCE_TITLES: [&str; 3] = ["深色", "淺色", "跟著系統"];
const WEIGHTS: [Weight; 4] = [
    Weight::Regular,
    Weight::Medium,
    Weight::Semibold,
    Weight::Bold,
];
const WEIGHT_TITLES: [&str; 4] = ["一般", "中等", "半粗", "粗"];

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

fn ns(text: &str) -> Retained<NSString> {
    NSString::from_str(text)
}

/// 把控制項的 target 與 action 指到控制器。
fn wire(control: &objc2_app_kit::NSControl, target: &AnyObject, action: Sel) {
    // SAFETY: selector 是 Controller 在 define_class! 裡定義的方法；target 型別正確，活到程式結束。
    unsafe {
        control.setTarget(Some(target));
        control.setAction(Some(action));
    }
}

fn same_object(a: &AnyObject, b: &AnyObject) -> bool {
    std::ptr::eq(a, b)
}

/// 一列數字：文字欄加步進器。步進器的範圍就是這個值的範圍，打進去的數字也夾在裡面。
pub struct Number {
    field: Retained<NSTextField>,
    stepper: Retained<NSStepper>,
    decimals: usize,
}

impl Number {
    pub fn set(&self, v: f64) {
        self.field.setStringValue(&ns(&self.format(v)));
        self.stepper.setDoubleValue(v);
    }

    fn format(&self, v: f64) -> String {
        format!("{:.*}", self.decimals, v)
    }

    /// 讀欄位；不是數字就報錯，超出範圍夾回來並同步步進器。
    pub fn get(&self, name: &str) -> Result<f64, String> {
        let s = self.field.stringValue().to_string();
        let v: f64 = s
            .trim()
            .parse()
            .map_err(|_| format!("「{name}」要是數字，現在是 {s:?}"))?;
        let v = v.clamp(self.stepper.minValue(), self.stepper.maxValue());
        self.stepper.setDoubleValue(v);
        Ok(v)
    }

    fn is_stepper(&self, sender: &AnyObject) -> bool {
        same_object(&self.stepper, sender)
    }

    /// 步進器被按了：把它的值寫回文字欄。
    fn sync_from_stepper(&self) {
        let v = self.stepper.doubleValue();
        self.field.setStringValue(&ns(&self.format(v)));
    }
}

/// 一列滑桿，右邊一個小字顯示現在的值。
pub struct Slider {
    slider: Retained<NSSlider>,
    value: Retained<NSTextField>,
    percent: bool,
}

impl Slider {
    pub fn set(&self, v: f64) {
        self.slider.setDoubleValue(v);
        self.show(v);
    }

    pub fn get(&self) -> f64 {
        let v = self.slider.doubleValue();
        self.show(v);
        v
    }

    fn show(&self, v: f64) {
        let text = if self.percent {
            format!("{:.0}%", v * 100.0)
        } else {
            format!("{v:.2}")
        };
        self.value.setStringValue(&ns(&text));
    }
}

/// 一列彈出選單，以索引對應到一個列舉。
pub struct Popup {
    button: Retained<NSPopUpButton>,
}

impl Popup {
    pub fn set_index(&self, i: usize) {
        self.button.selectItemAtIndex(i as isize);
    }

    pub fn index(&self) -> usize {
        self.button.indexOfSelectedItem().max(0) as usize
    }
}

/// 可留空的數字列：空白代表「依預設」。步進器從預設值起跳。
pub struct OptionalNumber {
    inner: Number,
}

impl OptionalNumber {
    pub fn set(&self, v: Option<f64>, fallback: f64) {
        match v {
            Some(v) => self.inner.set(v),
            None => {
                self.inner.field.setStringValue(&ns(""));
                self.inner.stepper.setDoubleValue(fallback);
            }
        }
    }

    pub fn get(&self, name: &str) -> Result<Option<f64>, String> {
        if self.inner.field.stringValue().to_string().trim().is_empty() {
            return Ok(None);
        }
        self.inner.get(name).map(Some)
    }
}

/// 字型家族：第一項「系統字型」，其餘是 `NSFontManager` 的清單；主題檔填了清單裡沒有的名字，
/// 多列一項標「找不到」，讓視窗如實反映檔案。
pub struct FontPopup {
    button: Retained<NSPopUpButton>,
    families: Vec<String>,
    extra: RefCell<Option<String>>,
}

impl FontPopup {
    /// 空字串＝系統字型。
    pub fn set(&self, family: &str) {
        if family.is_empty() {
            self.button.selectItemAtIndex(0);
            return;
        }
        if let Some(i) = self.families.iter().position(|f| f == family) {
            self.button.selectItemAtIndex(i as isize + 1);
            return;
        }
        let mut extra = self.extra.borrow_mut();
        if extra.as_deref() != Some(family) {
            if extra.is_some() {
                self.button
                    .removeItemAtIndex(self.button.numberOfItems() - 1);
            }
            self.button
                .addItemWithTitle(&ns(&format!("{family}（找不到）")));
            *extra = Some(family.to_string());
        }
        self.button
            .selectItemAtIndex(self.button.numberOfItems() - 1);
    }

    pub fn get(&self) -> String {
        let i = self.button.indexOfSelectedItem();
        if i <= 0 {
            String::new()
        } else if (i as usize) <= self.families.len() {
            self.families[i as usize - 1].clone()
        } else {
            self.extra.borrow().clone().unwrap_or_default()
        }
    }
}

fn check_state(on: bool) -> NSControlStateValue {
    if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    }
}

fn color_of(well: &NSColorWell) -> Color {
    let c = well.color();
    let c = c
        .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
        .unwrap_or(c);
    Color::rgba(
        c.redComponent(),
        c.greenComponent(),
        c.blueComponent(),
        c.alphaComponent(),
    )
}

/// 一個頁籤的內容：固定大小的 view，列從上往下加。
struct Page {
    mtm: MainThreadMarker,
    view: Retained<NSView>,
    height: f64,
    width: f64,
    row: usize,
}

impl Page {
    fn new(mtm: MainThreadMarker, size: CGSize) -> Page {
        let view =
            NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, size.width, size.height));
        Page {
            mtm,
            view,
            height: size.height,
            width: size.width,
            row: 0,
        }
    }

    /// 這一列的底邊 y。
    fn y(&self, i: usize) -> f64 {
        self.height - 10.0 - ROW_H * (i as f64 + 1.0) + 4.0
    }

    fn control_w(&self) -> f64 {
        self.width - CONTROL_X - MARGIN
    }

    /// 開一列：左邊放右對齊的標題，回傳這一列的索引。
    fn open_row(&mut self, title: &str) -> usize {
        let i = self.row;
        self.row += 1;
        if !title.is_empty() {
            let l = NSTextField::labelWithString(&ns(title), self.mtm);
            l.setAlignment(NSTextAlignment::Right);
            l.setFrame(rect(MARGIN, self.y(i), LABEL_W, 22.0));
            self.view.addSubview(&l);
        }
        i
    }

    /// 控制項右邊的小字提示。
    fn hint(&self, i: usize, x: f64, text: &str) {
        if text.is_empty() {
            return;
        }
        let h = NSTextField::labelWithString(&ns(text), self.mtm);
        h.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        h.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        h.setFrame(rect(x, self.y(i), self.width - x - MARGIN, 22.0));
        self.view.addSubview(&h);
    }

    #[allow(clippy::too_many_arguments)]
    fn number(
        &mut self,
        target: &AnyObject,
        title: &str,
        hint: &str,
        min: f64,
        max: f64,
        step: f64,
        decimals: usize,
    ) -> Number {
        self.number_with(target, title, hint, min, max, step, decimals, None)
    }

    /// 留空代表依預設的數字列。
    #[allow(clippy::too_many_arguments)]
    fn optional_number(
        &mut self,
        target: &AnyObject,
        title: &str,
        hint: &str,
        min: f64,
        max: f64,
        step: f64,
        decimals: usize,
    ) -> OptionalNumber {
        OptionalNumber {
            inner: self.number_with(target, title, hint, min, max, step, decimals, Some("預設")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn number_with(
        &mut self,
        target: &AnyObject,
        title: &str,
        hint: &str,
        min: f64,
        max: f64,
        step: f64,
        decimals: usize,
        placeholder: Option<&str>,
    ) -> Number {
        let i = self.open_row(title);
        let y = self.y(i);
        let field = NSTextField::textFieldWithString(&ns(""), self.mtm);
        field.setFrame(rect(CONTROL_X, y, 84.0, 24.0));
        field.setAlignment(NSTextAlignment::Right);
        if let Some(text) = placeholder {
            field.setPlaceholderString(Some(&ns(text)));
        }
        if let Some(cell) = field.cell() {
            // 按 Enter 或點到別處都算改完。
            cell.setSendsActionOnEndEditing(true);
        }
        wire(&field, target, sel!(settingChanged:));
        self.view.addSubview(&field);
        let stepper = NSStepper::new(self.mtm);
        stepper.setFrame(rect(CONTROL_X + 88.0, y - 2.0, 19.0, 27.0));
        stepper.setMinValue(min);
        stepper.setMaxValue(max);
        stepper.setIncrement(step);
        stepper.setValueWraps(false);
        stepper.setAutorepeat(true);
        wire(&stepper, target, sel!(stepperChanged:));
        self.view.addSubview(&stepper);
        self.hint(i, CONTROL_X + 116.0, hint);
        Number {
            field,
            stepper,
            decimals,
        }
    }

    fn slider(
        &mut self,
        target: &AnyObject,
        title: &str,
        min: f64,
        max: f64,
        percent: bool,
    ) -> Slider {
        let i = self.open_row(title);
        let y = self.y(i);
        let w = (self.control_w() - 60.0).min(220.0);
        let slider = NSSlider::new(self.mtm);
        slider.setFrame(rect(CONTROL_X, y, w, 24.0));
        slider.setMinValue(min);
        slider.setMaxValue(max);
        slider.setContinuous(true);
        wire(&slider, target, sel!(settingChanged:));
        self.view.addSubview(&slider);
        let value = NSTextField::labelWithString(&ns(""), self.mtm);
        value.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(
            12.0, 0.0,
        )));
        value.setFrame(rect(CONTROL_X + w + 8.0, y, 52.0, 22.0));
        self.view.addSubview(&value);
        Slider {
            slider,
            value,
            percent,
        }
    }

    /// 整列寬的文字欄（伺服器清單用）。
    fn text(&mut self, target: &AnyObject, title: &str) -> Retained<NSTextField> {
        let i = self.open_row(title);
        let field = NSTextField::textFieldWithString(&ns(""), self.mtm);
        field.setFrame(rect(CONTROL_X, self.y(i), self.control_w(), 24.0));
        if let Some(cell) = field.cell() {
            cell.setSendsActionOnEndEditing(true);
        }
        wire(&field, target, sel!(settingChanged:));
        self.view.addSubview(&field);
        field
    }

    fn font_popup(&mut self, target: &AnyObject, title: &str) -> FontPopup {
        let families: Vec<String> = NSFontManager::sharedFontManager(self.mtm)
            .availableFontFamilies()
            .iter()
            .map(|f| f.to_string())
            .collect();
        let mut titles: Vec<&str> = vec!["系統字型（數字等寬）"];
        titles.extend(families.iter().map(String::as_str));
        let popup = self.popup(target, title, &titles);
        FontPopup {
            button: popup.button,
            families,
            extra: RefCell::new(None),
        }
    }

    /// 一列裡的一顆動作按鈕，左欄留空。
    fn action_button(
        &mut self,
        target: &AnyObject,
        title: &str,
        action: Sel,
    ) -> Retained<NSButton> {
        let i = self.open_row("");
        // SAFETY: selector 是 Controller 的方法；target 型別正確，活到程式結束。
        let b = unsafe {
            NSButton::buttonWithTitle_target_action(
                &ns(title),
                Some(target),
                Some(action),
                self.mtm,
            )
        };
        b.setFrame(rect(CONTROL_X, self.y(i) - 4.0, 160.0, 30.0));
        self.view.addSubview(&b);
        b
    }

    fn popup(&mut self, target: &AnyObject, title: &str, titles: &[&str]) -> Popup {
        let i = self.open_row(title);
        let button = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(self.mtm),
            rect(
                CONTROL_X,
                self.y(i) - 2.0,
                self.control_w().min(300.0),
                26.0,
            ),
            false,
        );
        for t in titles {
            button.addItemWithTitle(&ns(t));
        }
        wire(&button, target, sel!(settingChanged:));
        self.view.addSubview(&button);
        Popup { button }
    }

    /// 核取方塊：標題在方塊右邊，左欄留空。
    fn check(&mut self, target: &AnyObject, title: &str) -> Retained<NSButton> {
        let i = self.open_row("");
        // SAFETY: selector 是 Controller 的方法；target 型別正確，活到程式結束。
        let b = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &ns(title),
                Some(target),
                Some(sel!(settingChanged:)),
                self.mtm,
            )
        };
        b.setFrame(rect(CONTROL_X, self.y(i), self.control_w(), 24.0));
        self.view.addSubview(&b);
        b
    }

    fn color(&mut self, target: &AnyObject, title: &str, hint: &str) -> Retained<NSColorWell> {
        let i = self.open_row(title);
        let well = NSColorWell::new(self.mtm);
        well.setFrame(rect(CONTROL_X, self.y(i) - 2.0, 64.0, 26.0));
        well.setSupportsAlpha(true);
        wire(&well, target, sel!(settingChanged:));
        self.view.addSubview(&well);
        self.hint(i, CONTROL_X + 72.0, hint);
        well
    }

    /// 整列寬、可換行的說明文字，佔 `lines` 列。
    fn info(&mut self, lines: usize) -> Retained<NSTextField> {
        let i = self.row;
        self.row += lines;
        let l = NSTextField::labelWithString(&ns(""), self.mtm);
        l.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        l.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        let h = ROW_H * lines as f64 - 6.0;
        l.setFrame(rect(
            MARGIN,
            self.y(i + lines - 1),
            self.width - 2.0 * MARGIN,
            h,
        ));
        self.view.addSubview(&l);
        l
    }

    /// 右下角的「回復預設值」，`tag` 是頁籤的索引。
    fn reset_button(&self, target: &AnyObject, tag: isize) -> Retained<NSButton> {
        // SAFETY: 同上。
        let b = unsafe {
            NSButton::buttonWithTitle_target_action(
                &ns("回復預設值"),
                Some(target),
                Some(sel!(resetSettingsTab:)),
                self.mtm,
            )
        };
        b.setTag(tag);
        b.setFrame(rect(self.width - MARGIN - 110.0, 8.0, 110.0, 30.0));
        self.view.addSubview(&b);
        b
    }

    fn finish(self, tabs: &NSTabView, label: &str) {
        // SAFETY: identifier 允許為 nil。
        let item = unsafe { NSTabViewItem::initWithIdentifier(NSTabViewItem::alloc(), None) };
        item.setLabel(&ns(label));
        item.setView(Some(&self.view));
        tabs.addTabViewItem(&item);
    }
}

pub struct SettingsPanel {
    pub panel: Retained<NSPanel>,
    status: Retained<NSTextField>,
    // 時間軸
    period_ms: Number,
    ticks: Number,
    visual_lead_s: Number,
    idle_ball: Retained<NSButton>,
    tail_ms: Number,
    demo_lead_s: Number,
    measure_s: Number,
    lock_s: Number,
    timeline_info: Retained<NSTextField>,
    // 節拍
    style: Popup,
    renderer: Popup,
    strip_height: Number,
    color: Retained<NSColorWell>,
    final_color: Retained<NSColorWell>,
    glow: Retained<NSButton>,
    glow_width: Number,
    glow_opacity: Slider,
    // 聲音
    volume: Slider,
    tick_hz: Number,
    tick_ms: Number,
    final_ms: Number,
    visual_lead_ms: Number,
    // 面板
    width: Number,
    corner_radius: Number,
    material: Popup,
    appearance: Popup,
    opacity: Slider,
    padding: Number,
    line_gap: Number,
    bar_height: Number,
    bar_full_scale_ms: Number,
    // 字型與顏色
    family: FontPopup,
    weight: Popup,
    time_size: Number,
    detail_size: Number,
    caption_size: Number,
    color_time: Retained<NSColorWell>,
    color_detail: Retained<NSColorWell>,
    color_caption: Retained<NSColorWell>,
    color_bar: Retained<NSColorWell>,
    color_bar_track: Retained<NSColorWell>,
    // 顯示
    lead_ms: Number,
    idle_fps: Number,
    /// 打開視窗時接著的每台螢幕一列；沒接著的螢幕在檔案裡的覆寫不動。
    screens: Vec<(String, OptionalNumber)>,
    present_info: Retained<NSTextField>,
    // 時間來源
    servers: Retained<NSTextField>,
    burst: Number,
    spacing_ms: Number,
    cycle_s: Number,
    tabs: Retained<NSTabView>,
    centered: std::cell::Cell<bool>,
}

impl SettingsPanel {
    pub fn build(mtm: MainThreadMarker, target: &AnyObject) -> SettingsPanel {
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect(0.0, 0.0, WIDTH, HEIGHT),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setTitle(&ns("kairos 設定"));
        panel.setFloatingPanel(true);
        panel.setLevel(NSFloatingWindowLevel);
        panel.setHidesOnDeactivate(false);
        panel.setBecomesKeyOnlyIfNeeded(false);
        // SAFETY: 我們用 Retained 持有、關閉只是 orderOut。
        unsafe { panel.setReleasedWhenClosed(false) };
        let content = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, WIDTH, HEIGHT));
        panel.setContentView(Some(&content));

        let status = NSTextField::labelWithString(&ns(""), mtm);
        status.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        status.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        status.setFrame(rect(MARGIN, MARGIN - 6.0, WIDTH - 2.0 * MARGIN, STATUS_H));
        content.addSubview(&status);

        let tabs_y = MARGIN + STATUS_H;
        let tabs = NSTabView::initWithFrame(
            NSTabView::alloc(mtm),
            rect(
                MARGIN,
                tabs_y,
                WIDTH - 2.0 * MARGIN,
                HEIGHT - tabs_y - MARGIN,
            ),
        );
        let page_size = tabs.contentRect().size;

        // ---- 時間軸 ----
        let mut p = Page::new(mtm, page_size);
        let period_ms = p.number(
            target,
            "拍距（ms）",
            "兩拍之間隔多久",
            100.0,
            10_000.0,
            100.0,
            0,
        );
        let ticks = p.number(target, "有聲的拍數", "含歸零那一拍", 1.0, 32.0, 1.0, 0);
        let visual_lead_s = p.number(
            target,
            "畫面起跑（歸零前秒）",
            "0＝自動（第一聲前一拍）；取整到整拍",
            0.0,
            3600.0,
            1.0,
            1,
        );
        let idle_ball = p.check(target, "起跑前先顯示靜止的球（鎖定到起跑之間）");
        let tail_ms = p.number(
            target,
            "歸零後停留（ms）",
            "讓閃光與光暈衰減完",
            0.0,
            60_000.0,
            100.0,
            0,
        );
        let demo_lead_s = p.number(
            target,
            "試聽提前（秒）",
            "「試聽節拍」幾秒後歸零",
            1.0,
            3600.0,
            1.0,
            0,
        );
        let measure_s = p.number(
            target,
            "量測提前（秒）",
            "歸零前多久重新校時",
            10.0,
            86_400.0,
            60.0,
            0,
        );
        let lock_s = p.number(
            target,
            "鎖定提前（秒）",
            "歸零前多久凍結時間、開始節拍",
            5.0,
            3600.0,
            10.0,
            0,
        );
        let timeline_info = p.info(2);
        p.reset_button(target, TAB_TIMELINE);
        p.finish(&tabs, "時間軸");

        // ---- 節拍 ----
        let mut p = Page::new(mtm, page_size);
        let style = p.popup(target, "樣式", &STYLE_TITLES);
        let renderer = p.popup(target, "畫法", &RENDERER_TITLES);
        let strip_height = p.number(target, "節拍區高度（點）", "", 24.0, 300.0, 4.0, 0);
        let color = p.color(target, "節拍顏色", "球、環、點與地線");
        let final_color = p.color(target, "歸零拍顏色", "最後一拍換這個色並放大");
        let glow = p.check(target, "螢幕邊緣光暈（每個螢幕一層）");
        let glow_width = p.number(target, "光暈寬度（點）", "", 1.0, 500.0, 4.0, 0);
        let glow_opacity = p.slider(target, "光暈亮度", 0.0, 1.0, true);
        p.reset_button(target, TAB_BEAT);
        p.finish(&tabs, "節拍");

        // ---- 聲音 ----
        let mut p = Page::new(mtm, page_size);
        let volume = p.slider(target, "音量", 0.0, 1.0, true);
        let tick_hz = p.number(
            target,
            "音高（Hz）",
            "前導拍與歸零拍同音高",
            20.0,
            20_000.0,
            50.0,
            0,
        );
        let tick_ms = p.number(target, "前導拍長度（ms）", "", 1.0, 1000.0, 5.0, 0);
        let final_ms = p.number(target, "歸零拍長度（ms）", "", 1.0, 5000.0, 10.0, 0);
        let visual_lead_ms = p.number(
            target,
            "畫面比聲音早（ms）",
            "覺得畫面慢就往正調；可為負",
            -500.0,
            500.0,
            5.0,
            0,
        );
        p.reset_button(target, TAB_SOUND);
        p.finish(&tabs, "聲音");

        // ---- 面板 ----
        let mut p = Page::new(mtm, page_size);
        let width = p.number(
            target,
            "寬（點）",
            "實際寬＝這個 × 面板上拖出來的倍率",
            200.0,
            1200.0,
            10.0,
            0,
        );
        let corner_radius = p.number(target, "圓角（點）", "", 0.0, 60.0, 1.0, 0);
        let material = p.popup(target, "材質", &MATERIAL_TITLES);
        let appearance = p.popup(target, "外觀", &APPEARANCE_TITLES);
        let opacity = p.slider(target, "不透明度", MIN_OPACITY, MAX_OPACITY, true);
        let padding = p.number(target, "內距（點）", "四邊留白", 4.0, 64.0, 1.0, 0);
        let line_gap = p.number(target, "行距（點）", "", 0.0, 30.0, 1.0, 0);
        let bar_height = p.number(target, "長條粗細（點）", "", 1.0, 20.0, 1.0, 0);
        let bar_full_scale_ms = p.number(
            target,
            "長條滿格（± ms）",
            "不確定度長條滿格對應的 ± 毫秒",
            5.0,
            1000.0,
            5.0,
            0,
        );
        p.reset_button(target, TAB_PANEL);
        p.finish(&tabs, "面板");

        // ---- 字型與顏色 ----
        let mut p = Page::new(mtm, page_size);
        let family = p.font_popup(target, "字型");
        let weight = p.popup(target, "粗細", &WEIGHT_TITLES);
        let time_size = p.number(target, "時間字級", "太大會超出面板寬", 16.0, 120.0, 1.0, 0);
        let detail_size = p.number(target, "細節字級", "± 與狀態那一列", 8.0, 40.0, 1.0, 0);
        let caption_size = p.number(target, "說明字級", "最底下那兩列", 8.0, 40.0, 1.0, 0);
        let color_time = p.color(target, "時間顏色", "");
        let color_detail = p.color(target, "細節顏色", "");
        let color_caption = p.color(target, "說明顏色", "");
        let color_bar = p.color(target, "長條顏色", "");
        let color_bar_track = p.color(target, "長條底色", "");
        p.reset_button(target, TAB_FONT);
        p.finish(&tabs, "字型與顏色");

        // ---- 顯示 ----
        let mut p = Page::new(mtm, page_size);
        let lead_ms = p.number(
            target,
            "預設提前量（ms）",
            "畫面實際上屏比預測晚多少",
            -50.0,
            200.0,
            1.0,
            0,
        );
        let idle_fps = p.number(
            target,
            "平常刷新率（Hz）",
            "節拍期間會拉到螢幕最高",
            10.0,
            240.0,
            10.0,
            0,
        );
        let mut screens = Vec::new();
        for screen in NSScreen::screens(mtm).iter() {
            let name = screen.localizedName().to_string();
            let field = p.optional_number(
                target,
                &name,
                "這台的提前量；留空＝依預設",
                -50.0,
                200.0,
                1.0,
                0,
            );
            screens.push((name, field));
        }
        let present_info = p.info(2);
        p.action_button(target, "填進這個螢幕", sel!(adoptPresentOffset:));
        p.reset_button(target, TAB_DISPLAY);
        p.finish(&tabs, "顯示");

        // ---- 時間來源 ----
        let mut p = Page::new(mtm, page_size);
        let servers = p.text(target, "伺服器");
        let sync_info = p.info(1);
        sync_info.setStringValue(&ns(
            "以空白或逗號分隔，1 到 8 台；存了就換上、立刻跑一輪，鎖定期間等解凍",
        ));
        let burst = p.number(target, "每台幾筆", "每一輪", 1.0, 8.0, 1.0, 0);
        let spacing_ms = p.number(
            target,
            "同一台間隔（ms）",
            "兩筆之間",
            200.0,
            10_000.0,
            100.0,
            0,
        );
        let cycle_s = p.number(target, "兩輪之間（秒）", "", 60.0, 3600.0, 30.0, 0);
        p.reset_button(target, TAB_SYNC);
        p.finish(&tabs, "時間來源");

        content.addSubview(&tabs);

        SettingsPanel {
            panel,
            status,
            period_ms,
            ticks,
            visual_lead_s,
            idle_ball,
            tail_ms,
            demo_lead_s,
            measure_s,
            lock_s,
            timeline_info,
            style,
            renderer,
            strip_height,
            color,
            final_color,
            glow,
            glow_width,
            glow_opacity,
            volume,
            tick_hz,
            tick_ms,
            final_ms,
            visual_lead_ms,
            width,
            corner_radius,
            material,
            appearance,
            opacity,
            padding,
            line_gap,
            bar_height,
            bar_full_scale_ms,
            family,
            weight,
            time_size,
            detail_size,
            caption_size,
            color_time,
            color_detail,
            color_caption,
            color_bar,
            color_bar_track,
            lead_ms,
            idle_fps,
            screens,
            present_info,
            servers,
            burst,
            spacing_ms,
            cycle_s,
            tabs,
            centered: std::cell::Cell::new(false),
        }
    }

    /// 切到第 `i` 頁（開發用：快照指定的頁）。
    pub fn select_tab(&self, i: isize) {
        if (0..self.tabs.numberOfTabViewItems()).contains(&i) {
            self.tabs.selectTabViewItemAtIndex(i);
        }
    }

    /// 秀出來並成為 key window。第一次置中。
    pub fn show(&self, mtm: MainThreadMarker) {
        if !self.centered.get() {
            self.centered.set(true);
            self.panel.center();
        }
        self.panel.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(mtm).activate();
    }

    fn numbers(&self) -> Vec<&Number> {
        let mut v = vec![
            &self.period_ms,
            &self.ticks,
            &self.visual_lead_s,
            &self.tail_ms,
            &self.demo_lead_s,
            &self.measure_s,
            &self.lock_s,
            &self.strip_height,
            &self.glow_width,
            &self.tick_hz,
            &self.tick_ms,
            &self.final_ms,
            &self.visual_lead_ms,
            &self.width,
            &self.corner_radius,
            &self.padding,
            &self.line_gap,
            &self.bar_height,
            &self.bar_full_scale_ms,
            &self.time_size,
            &self.detail_size,
            &self.caption_size,
            &self.lead_ms,
            &self.idle_fps,
            &self.burst,
            &self.spacing_ms,
            &self.cycle_s,
        ];
        v.extend(self.screens.iter().map(|(_, n)| &n.inner));
        v
    }

    /// 某個步進器被按了：把值寫回它的文字欄。不是這裡的步進器就回 `false`。
    pub fn sync_stepper(&self, sender: &AnyObject) -> bool {
        match self.numbers().iter().find(|n| n.is_stepper(sender)) {
            Some(n) => {
                n.sync_from_stepper();
                true
            }
            None => false,
        }
    }

    /// 用有效的主題與目標檔填所有控制項。不會觸發動作。
    pub fn fill(&self, theme: &Theme, store: &TargetFile) {
        let b = &theme.beat;
        self.period_ms.set(b.period_ms);
        self.ticks.set(b.ticks as f64);
        self.visual_lead_s.set(b.visual_lead_s);
        self.idle_ball.setState(check_state(b.idle_ball));
        self.tail_ms.set(b.tail_ms);
        self.demo_lead_s.set(b.demo_lead_s);
        self.measure_s.set(store.measure_before_s as f64);
        self.lock_s.set(store.lock_before_s as f64);

        self.style
            .set_index(STYLES.iter().position(|s| *s == b.style).unwrap_or(0));
        self.renderer
            .set_index(RENDERERS.iter().position(|r| *r == b.renderer).unwrap_or(0));
        self.strip_height.set(b.strip_height);
        self.color.setColor(&b.color.nscolor());
        self.final_color.setColor(&b.final_color.nscolor());
        self.glow.setState(check_state(b.glow));
        self.glow_width.set(b.glow_width);
        self.glow_opacity.set(b.glow_opacity);

        self.volume.set(b.volume);
        self.tick_hz.set(b.tick_hz);
        self.tick_ms.set(b.tick_ms);
        self.final_ms.set(b.final_ms);
        self.visual_lead_ms.set(b.visual_lead_ms);

        let pn = &theme.panel;
        self.width.set(pn.width);
        self.corner_radius.set(pn.corner_radius);
        self.material.set_index(
            MATERIALS
                .iter()
                .position(|m| *m == pn.material)
                .unwrap_or(0),
        );
        self.appearance.set_index(
            APPEARANCES
                .iter()
                .position(|a| *a == pn.appearance)
                .unwrap_or(0),
        );
        self.opacity.set(pn.opacity);
        let l = &theme.layout;
        self.padding.set(l.padding);
        self.line_gap.set(l.line_gap);
        self.bar_height.set(l.bar_height);
        self.bar_full_scale_ms.set(l.bar_full_scale_ms);

        let f = &theme.font;
        self.family.set(&f.family);
        self.weight
            .set_index(WEIGHTS.iter().position(|w| *w == f.weight).unwrap_or(0));
        self.time_size.set(f.time_size);
        self.detail_size.set(f.detail_size);
        self.caption_size.set(f.caption_size);
        let c = &theme.colors;
        self.color_time.setColor(&c.time.nscolor());
        self.color_detail.setColor(&c.detail.nscolor());
        self.color_caption.setColor(&c.caption.nscolor());
        self.color_bar.setColor(&c.bar.nscolor());
        self.color_bar_track.setColor(&c.bar_track.nscolor());

        let d = &theme.display;
        self.lead_ms.set(d.lead_ms);
        self.idle_fps.set(d.idle_fps);
        for (name, field) in &self.screens {
            field.set(d.screens.get(name).copied(), d.lead_ms);
        }

        let sy = &theme.sync;
        self.servers.setStringValue(&ns(&sy.servers.join(", ")));
        self.burst.set(sy.burst as f64);
        self.spacing_ms.set(sy.spacing_ms);
        self.cycle_s.set(sy.cycle_s);
    }

    /// 把控制項讀回主題與目標檔。有欄位不是數字就整個不動、回錯誤訊息。
    pub fn read(&self, theme: &mut Theme, store: &mut TargetFile) -> Result<(), String> {
        let b = &mut theme.beat;
        b.period_ms = self.period_ms.get("拍距")?;
        b.ticks = self.ticks.get("有聲的拍數")?.round() as u32;
        b.visual_lead_s = self.visual_lead_s.get("畫面起跑")?;
        b.idle_ball = self.idle_ball.state() == NSControlStateValueOn;
        b.tail_ms = self.tail_ms.get("歸零後停留")?;
        b.demo_lead_s = self.demo_lead_s.get("試聽提前")?;
        store.measure_before_s = self.measure_s.get("量測提前")?.round() as u64;
        store.lock_before_s = self.lock_s.get("鎖定提前")?.round() as u64;

        b.style = STYLES[self.style.index().min(STYLES.len() - 1)];
        b.renderer = RENDERERS[self.renderer.index().min(RENDERERS.len() - 1)];
        b.strip_height = self.strip_height.get("節拍區高度")?;
        b.color = color_of(&self.color);
        b.final_color = color_of(&self.final_color);
        b.glow = self.glow.state() == NSControlStateValueOn;
        b.glow_width = self.glow_width.get("光暈寬度")?;
        b.glow_opacity = self.glow_opacity.get();

        b.volume = self.volume.get();
        b.tick_hz = self.tick_hz.get("音高")?;
        b.tick_ms = self.tick_ms.get("前導拍長度")?;
        b.final_ms = self.final_ms.get("歸零拍長度")?;
        b.visual_lead_ms = self.visual_lead_ms.get("畫面比聲音早")?;

        let pn = &mut theme.panel;
        pn.width = self.width.get("寬")?;
        pn.corner_radius = self.corner_radius.get("圓角")?;
        pn.material = MATERIALS[self.material.index().min(MATERIALS.len() - 1)];
        pn.appearance = APPEARANCES[self.appearance.index().min(APPEARANCES.len() - 1)];
        pn.opacity = self.opacity.get();
        let l = &mut theme.layout;
        l.padding = self.padding.get("內距")?;
        l.line_gap = self.line_gap.get("行距")?;
        l.bar_height = self.bar_height.get("長條粗細")?;
        l.bar_full_scale_ms = self.bar_full_scale_ms.get("長條滿格")?;

        let f = &mut theme.font;
        f.family = self.family.get();
        f.weight = WEIGHTS[self.weight.index().min(WEIGHTS.len() - 1)];
        f.time_size = self.time_size.get("時間字級")?;
        f.detail_size = self.detail_size.get("細節字級")?;
        f.caption_size = self.caption_size.get("說明字級")?;
        let c = &mut theme.colors;
        c.time = color_of(&self.color_time);
        c.detail = color_of(&self.color_detail);
        c.caption = color_of(&self.color_caption);
        c.bar = color_of(&self.color_bar);
        c.bar_track = color_of(&self.color_bar_track);

        let d = &mut theme.display;
        d.lead_ms = self.lead_ms.get("預設提前量")?;
        d.idle_fps = self.idle_fps.get("平常刷新率")?;
        for (name, field) in &self.screens {
            match field.get(name)? {
                Some(v) => {
                    d.screens.insert(name.clone(), v);
                }
                None => {
                    d.screens.remove(name);
                }
            }
        }

        let sy = &mut theme.sync;
        sy.servers = self
            .servers
            .stringValue()
            .to_string()
            .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == '，' || ch == '、')
            .filter(|h| !h.is_empty())
            .map(str::to_string)
            .collect();
        sy.burst = self.burst.get("每台幾筆")?.round() as usize;
        sy.spacing_ms = self.spacing_ms.get("同一台間隔")?;
        sy.cycle_s = self.cycle_s.get("兩輪之間")?;
        sy.settings().map_err(|e| format!("時間來源：{e}"))?;
        Ok(())
    }

    /// 顯示頁那兩行：上次節拍量到的上屏差。
    pub fn set_present_info(&self, text: &str) {
        self.present_info.setStringValue(&ns(text));
    }

    /// 把某台螢幕的提前量欄填成 `ms`；那台不在畫面上就回 `false`。
    pub fn set_screen_lead(&self, name: &str, ms: f64) -> bool {
        match self.screens.iter().find(|(n, _)| n == name) {
            Some((_, field)) => {
                field.set(Some(ms), ms);
                true
            }
            None => false,
        }
    }

    /// 時間軸頁底下那兩行：實際起跑時刻、鎖定至少提前多久。
    pub fn set_timeline_info(&self, text: &str) {
        self.timeline_info.setStringValue(&ns(text));
    }

    pub fn set_status(&self, text: &str) {
        self.status.setStringValue(&ns(text));
    }
}
