//! 目標時刻面板：原生視窗，放日期選擇器、四個提前量欄位、歸零時刻、狀態，以及
//! 設定／解除／量測／校正的按鈕。這扇視窗要能成為 key window：欄位要打字，
//! 校正時的按鍵也要送到本程式。
//!
//! 版面用固定座標排，沒有 Auto Layout；控制項不多，算得清楚。

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSDatePicker, NSDatePickerElementFlags,
    NSDatePickerMode, NSDatePickerStyle, NSFloatingWindowLevel, NSFont, NSLineBreakMode, NSPanel,
    NSPopUpButton, NSTextAlignment, NSTextField, NSView, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSDate, NSString};

use kairos_core::target::LeadParams;

use crate::target_store::TargetFile;

const WIDTH: f64 = 420.0;
const HEIGHT: f64 = 400.0;
const MARGIN: f64 = 20.0;
const LABEL_W: f64 = 120.0;
const ROW_H: f64 = 32.0;
const CONTROL_X: f64 = MARGIN + LABEL_W + 8.0;
const CONTROL_W: f64 = WIDTH - CONTROL_X - MARGIN;

pub struct TargetPanel {
    pub panel: Retained<NSPanel>,
    picker: Retained<NSDatePicker>,
    _source: Retained<NSPopUpButton>,
    reaction: Retained<NSTextField>,
    browser: Retained<NSTextField>,
    one_way: Retained<NSTextField>,
    safety: Retained<NSTextField>,
    zero: Retained<NSTextField>,
    status: Retained<NSTextField>,
    calibration: Retained<NSTextField>,
    apply: Retained<NSButton>,
    clear: Retained<NSButton>,
    calibrate: Retained<NSButton>,
    adopt: Retained<NSButton>,
    centered: std::cell::Cell<bool>,
}

/// 使用者按「設定目標」時從面板讀到的值。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Inputs {
    pub target_unix_s: i64,
    pub lead: LeadParams,
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

/// 第 `i` 列的底邊 y（由上往下數，0 起算）。
fn row_y(i: usize) -> f64 {
    HEIGHT - MARGIN - ROW_H * (i as f64 + 1.0) + 4.0
}

fn label(mtm: MainThreadMarker, content: &NSView, text: &str, i: usize) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setAlignment(NSTextAlignment::Right);
    l.setFrame(rect(MARGIN, row_y(i), LABEL_W, 22.0));
    content.addSubview(&l);
    l
}

fn value_label(
    mtm: MainThreadMarker,
    content: &NSView,
    i: usize,
    lines: f64,
) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    l.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    let h = 22.0 * lines;
    l.setFrame(rect(CONTROL_X, row_y(i) - (h - 22.0), CONTROL_W, h));
    content.addSubview(&l);
    l
}

fn number_field(
    mtm: MainThreadMarker,
    content: &NSView,
    i: usize,
    hint: &str,
) -> Retained<NSTextField> {
    let f = NSTextField::textFieldWithString(&NSString::from_str(""), mtm);
    f.setFrame(rect(CONTROL_X, row_y(i), 90.0, 24.0));
    f.setAlignment(NSTextAlignment::Right);
    content.addSubview(&f);
    if !hint.is_empty() {
        let h = NSTextField::labelWithString(&NSString::from_str(hint), mtm);
        h.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        h.setFrame(rect(CONTROL_X + 98.0, row_y(i), CONTROL_W - 98.0, 22.0));
        content.addSubview(&h);
    }
    f
}

/// 第 `i` 列、從 `x` 起寬 `w` 的按鈕。
fn button_frame(x: f64, i: usize, w: f64) -> CGRect {
    rect(x, row_y(i) - 4.0, w, 30.0)
}

fn button(
    mtm: MainThreadMarker,
    content: &NSView,
    title: &str,
    target: &AnyObject,
    action: Sel,
    frame: CGRect,
) -> Retained<NSButton> {
    // SAFETY: selector 是 Controller 在 define_class! 裡定義的方法；target 型別正確，活到程式結束。
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(action),
            mtm,
        )
    };
    b.setFrame(frame);
    content.addSubview(&b);
    b
}

impl TargetPanel {
    pub fn build(mtm: MainThreadMarker, target: &AnyObject) -> TargetPanel {
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect(0.0, 0.0, WIDTH, HEIGHT),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setTitle(&NSString::from_str("目標時刻"));
        panel.setFloatingPanel(true);
        panel.setLevel(NSFloatingWindowLevel);
        panel.setHidesOnDeactivate(false);
        panel.setBecomesKeyOnlyIfNeeded(false);
        // SAFETY: 我們用 Retained 持有、關閉只是 orderOut。
        unsafe { panel.setReleasedWhenClosed(false) };
        let content = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, WIDTH, HEIGHT));
        panel.setContentView(Some(&content));

        label(mtm, &content, "目標時刻", 0);
        let picker = NSDatePicker::new(mtm);
        picker.setDatePickerStyle(NSDatePickerStyle::TextFieldAndStepper);
        picker.setDatePickerMode(NSDatePickerMode::Single);
        picker.setDatePickerElements(
            NSDatePickerElementFlags::YearMonthDay | NSDatePickerElementFlags::HourMinuteSecond,
        );
        picker.setFrame(rect(CONTROL_X, row_y(0), 230.0, 26.0));
        content.addSubview(&picker);

        label(mtm, &content, "時間基準", 1);
        let source = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            rect(CONTROL_X, row_y(1) - 2.0, 200.0, 26.0),
            false,
        );
        source.addItemWithTitle(&NSString::from_str("標準時間（NTP）"));
        content.addSubview(&source);

        label(mtm, &content, "反應時間（ms）", 2);
        let reaction = number_field(mtm, &content, 2, "校正後按「採用」自動填入");
        label(mtm, &content, "瀏覽器處理（ms）", 3);
        let browser = number_field(mtm, &content, 3, "點擊到請求送出");
        label(mtm, &content, "單程延遲（ms）", 4);
        let one_way = number_field(mtm, &content, 4, "往返延遲的一半");
        label(mtm, &content, "安全餘量（ms）", 5);
        let safety = number_field(mtm, &content, 5, "正值：寧可晚一點點");

        label(mtm, &content, "歸零時刻", 6);
        let zero = value_label(mtm, &content, 6, 1.0);
        label(mtm, &content, "狀態", 7);
        let status = value_label(mtm, &content, 7, 2.0);

        let bw = (WIDTH - 2.0 * MARGIN - 16.0) / 3.0;
        let apply = button(
            mtm,
            &content,
            "設定目標",
            target,
            sel!(applyTarget:),
            button_frame(MARGIN, 9, bw),
        );
        let clear = button(
            mtm,
            &content,
            "解除目標",
            target,
            sel!(clearTarget:),
            button_frame(MARGIN + bw + 8.0, 9, bw),
        );
        button(
            mtm,
            &content,
            "立刻量測",
            target,
            sel!(measureNow:),
            button_frame(MARGIN + 2.0 * (bw + 8.0), 9, bw),
        );
        let bw2 = (WIDTH - 2.0 * MARGIN - 8.0) / 2.0;
        let calibrate = button(
            mtm,
            &content,
            "校正反應時間",
            target,
            sel!(startCalibration:),
            button_frame(MARGIN, 10, bw2),
        );
        let adopt = button(
            mtm,
            &content,
            "採用校正結果",
            target,
            sel!(adoptCalibration:),
            button_frame(MARGIN + bw2 + 8.0, 10, bw2),
        );
        adopt.setEnabled(false);

        let calibration = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        calibration.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        calibration.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        calibration.setFrame(rect(MARGIN, MARGIN - 8.0, WIDTH - 2.0 * MARGIN, 44.0));
        content.addSubview(&calibration);

        TargetPanel {
            panel,
            picker,
            _source: source,
            reaction,
            browser,
            one_way,
            safety,
            zero,
            status,
            calibration,
            apply,
            clear,
            calibrate,
            adopt,
            centered: std::cell::Cell::new(false),
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

    pub fn is_visible(&self) -> bool {
        self.panel.isVisible()
    }

    /// 用檔案裡的值填欄位；沒有目標就預設下一個整點。
    pub fn fill(&self, file: &TargetFile, now_unix_s: i64) {
        let secs = file
            .target_unix_s
            .unwrap_or_else(|| (now_unix_s / 3600 + 1) * 3600);
        self.picker
            .setDateValue(&NSDate::dateWithTimeIntervalSince1970(secs as f64));
        let set = |f: &NSTextField, v: f64| f.setStringValue(&NSString::from_str(&format!("{v}")));
        set(&self.reaction, file.reaction_ms);
        set(&self.browser, file.browser_ms);
        set(&self.one_way, file.one_way_ms);
        set(&self.safety, file.safety_ms);
    }

    pub fn read(&self) -> Result<Inputs, String> {
        let num = |f: &NSTextField, name: &str| -> Result<f64, String> {
            let s = f.stringValue().to_string();
            s.trim()
                .parse::<f64>()
                .map_err(|_| format!("{name}要是數字，現在是 {s:?}"))
        };
        let lead = LeadParams {
            reaction_ms: num(&self.reaction, "反應時間")?,
            browser_ms: num(&self.browser, "瀏覽器處理")?,
            one_way_ms: num(&self.one_way, "單程延遲")?,
            safety_ms: num(&self.safety, "安全餘量")?,
        };
        let target_unix_s = self.picker.dateValue().timeIntervalSince1970().round() as i64;
        Ok(Inputs {
            target_unix_s,
            lead,
        })
    }

    pub fn set_reaction(&self, ms: f64) {
        self.reaction
            .setStringValue(&NSString::from_str(&format!("{ms:.0}")));
    }

    pub fn set_zero(&self, text: &str) {
        self.zero.setStringValue(&NSString::from_str(text));
    }

    pub fn set_status(&self, text: &str) {
        self.status.setStringValue(&NSString::from_str(text));
    }

    pub fn set_calibration(&self, text: &str, adoptable: bool) {
        self.calibration.setStringValue(&NSString::from_str(text));
        self.adopt.setEnabled(adoptable);
    }

    /// 有目標時「設定」變「更新」、「解除」可按。
    pub fn set_armed(&self, armed: bool) {
        self.apply.setTitle(&NSString::from_str(if armed {
            "更新目標"
        } else {
            "設定目標"
        }));
        self.clear.setEnabled(armed);
    }

    pub fn set_calibrating(&self, on: bool) {
        self.calibrate.setTitle(&NSString::from_str(if on {
            "停止校正"
        } else {
            "校正反應時間"
        }));
    }
}
