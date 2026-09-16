//! 浮動面板本體與它的「臉」：面板視窗、毛玻璃、一個自己管圖層樹的 view，
//! 以及十二格數字、不確定度長條、兩行說明文字。
//!
//! 座標都是 AppKit 的非翻轉座標，原點在左下。

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearanceCustomization, NSAutoresizingMaskOptions, NSBackingStoreType, NSColor,
    NSLineBreakMode, NSPanel, NSScreen, NSStatusWindowLevel, NSTextAlignment, NSTextField, NSView,
    NSVisualEffectBlendingMode, NSVisualEffectState, NSVisualEffectView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::NSString;
use objc2_quartz_core::CALayer;

use kairos_core::display::DIGIT_CELLS;

use crate::atlas::GlyphAtlas;
use crate::beat_view::{BeatStrip, StripKind};
use crate::theme::Theme;

/// 面板視窗與兩層 view。程式生命週期內不重建；主題變了只改屬性與大小。
pub struct PanelViews {
    pub panel: Retained<NSPanel>,
    pub effect: Retained<NSVisualEffectView>,
    /// layer-hosting view：`setLayer:` 在 `setWantsLayer:` 之前呼叫，圖層樹由我們管，
    /// AppKit 不會動它的子圖層。view 本身只要活著就好。
    _host: Retained<NSView>,
    pub root_layer: Retained<CALayer>,
}

const FRAME_NAME: &str = "kairos.panel";
const SCREEN_MARGIN: f64 = 24.0;

pub fn build_panel(mtm: MainThreadMarker, theme: &Theme) -> PanelViews {
    let rect = CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(theme.panel.width, 100.0),
    );
    let mask = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    // `defer` 為 false 讓視窗立刻有 backing store。
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        rect,
        mask,
        NSBackingStoreType::Buffered,
        false,
    );
    // 浮在狀態列之上；別的程式全螢幕時也看得到、每個 Space 都在。
    panel.setLevel(NSStatusWindowLevel + 1);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    panel.setHidesOnDeactivate(false);
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(true);
    panel.setMovableByWindowBackground(true);
    // SAFETY: 我們用 Retained 持有面板、從不呼叫 close，關掉「關閉即釋放」避免雙重釋放。
    unsafe { panel.setReleasedWhenClosed(false) };

    let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), rect);
    effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    // 面板永遠不是前景視窗；不釘成 Active 的話毛玻璃會一直是失焦的灰。
    effect.setState(NSVisualEffectState::Active);
    effect.setWantsLayer(true);
    effect.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    panel.setContentView(Some(&effect));

    let root_layer = CALayer::new();
    let host = NSView::initWithFrame(NSView::alloc(mtm), effect.bounds());
    host.setLayer(Some(&root_layer));
    host.setWantsLayer(true);
    host.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    effect.addSubview(&host);

    // 上次的位置有存就用，沒有就放主螢幕右下角。
    let name = NSString::from_str(FRAME_NAME);
    if !panel.setFrameUsingName(&name) {
        if let Some(screen) = NSScreen::mainScreen(mtm) {
            let v = screen.visibleFrame();
            panel.setFrameOrigin(CGPoint::new(
                v.origin.x + v.size.width - theme.panel.width - SCREEN_MARGIN,
                v.origin.y + SCREEN_MARGIN,
            ));
        }
    }
    panel.setFrameAutosaveName(&name);

    PanelViews {
        panel,
        effect,
        _host: host,
        root_layer,
    }
}

impl PanelViews {
    /// 套用主題裡跟視窗本身有關的部分，並把內容區改成 `width × height`。
    /// 平常左上角不動；節拍區出現或消失時改成左下角不動，數字才不會跳。
    pub fn apply_theme(&self, theme: &Theme, height: f64, keep_bottom: bool) {
        self.panel.setAlphaValue(theme.panel.opacity);
        self.panel
            .setAppearance(theme.panel.appearance.ns().as_deref());
        self.effect.setMaterial(theme.panel.material.ns());
        if let Some(layer) = self.effect.layer() {
            layer.setCornerRadius(theme.panel.corner_radius);
            layer.setMasksToBounds(true);
        }

        let f = self.panel.frame();
        let top_left = CGPoint::new(f.origin.x, f.origin.y + f.size.height);
        self.panel
            .setContentSize(CGSize::new(theme.panel.width, height));
        if keep_bottom {
            self.panel.setFrameOrigin(f.origin);
        } else {
            self.panel.setFrameTopLeftPoint(top_left);
        }
    }

    pub fn scale(&self) -> f64 {
        self.panel.backingScaleFactor()
    }
}

/// 面板上會變的東西。主題、螢幕縮放變了，或節拍區出現／消失，就整個重建。
pub struct Face {
    pub atlas: GlyphAtlas,
    /// 面板內容區需要的高度（點）。
    pub height: f64,
    /// 節拍區，只在節拍期間有。
    pub strip: Option<BeatStrip>,
    digits: Vec<Retained<CALayer>>,
    shown: [u8; DIGIT_CELLS],
    bar_track: Retained<CALayer>,
    bar_fill: Retained<CALayer>,
    track_x: f64,
    track_w: f64,
    bar_y: f64,
    bar_h: f64,
    detail: Retained<NSTextField>,
    caption: Retained<NSTextField>,
    last_detail: String,
    last_caption: String,
}

impl Face {
    pub fn build(
        mtm: MainThreadMarker,
        views: &PanelViews,
        theme: &Theme,
        scale: f64,
        strip: Option<StripKind>,
    ) -> Face {
        let time_font_px = theme.font.nsfont(theme.font.time_size * scale);
        let atlas = GlyphAtlas::render(&time_font_px, &theme.colors.time.nscolor(), scale);

        let detail_font = theme.font.nsfont(theme.font.detail_size);
        let caption_font = theme.font.nsfont(theme.font.caption_size);
        let detail_h = (detail_font.ascender() - detail_font.descender() + 2.0).ceil();
        let caption_h = (caption_font.ascender() - caption_font.descender() + 2.0).ceil();

        let width = theme.panel.width;
        let p = theme.layout.padding;
        let gap = theme.layout.line_gap;
        let bar_h = theme.layout.bar_height;

        // 由下往上排。
        let caption_y = p;
        let bar_y = caption_y + caption_h + gap;
        let detail_y = bar_y + bar_h + gap;
        let digits_y = detail_y + detail_h + gap;
        let digits_top = digits_y + atlas.cell_height;
        // 節拍區在數字上方。
        let strip_h = theme.beat.strip_height.max(24.0);
        let height = match strip {
            Some(_) => digits_top + gap + strip_h + p,
            None => digits_top + p,
        };

        // 十二格數字，水平置中。
        let pattern = b"HH:MM:SS.mmm";
        let total_w: f64 = pattern.iter().map(|&c| atlas.width_of(c)).sum();
        if total_w > width - 2.0 * p {
            eprintln!(
                "主題：time_size {} 讓數字寬 {:.0} 點，超過面板可用寬 {:.0} 點",
                theme.font.time_size,
                total_w,
                width - 2.0 * p
            );
        }
        let mut x = ((width - total_w) / 2.0).floor();
        let mut digits = Vec::with_capacity(DIGIT_CELLS);
        for &c in pattern {
            let w = atlas.width_of(c);
            let layer = CALayer::new();
            layer.setContentsScale(scale);
            layer.setFrame(CGRect::new(
                CGPoint::new(x, digits_y),
                CGSize::new(w, atlas.cell_height),
            ));
            views.root_layer.addSublayer(&layer);
            digits.push(layer);
            x += w;
        }

        let track_x = p;
        let track_w = width - 2.0 * p;
        let bar_track = CALayer::new();
        bar_track.setFrame(CGRect::new(
            CGPoint::new(track_x, bar_y),
            CGSize::new(track_w, bar_h),
        ));
        bar_track.setCornerRadius(bar_h / 2.0);
        bar_track.setBackgroundColor(Some(&theme.colors.bar_track.nscolor().CGColor()));
        views.root_layer.addSublayer(&bar_track);
        let bar_fill = CALayer::new();
        bar_fill.setCornerRadius(bar_h / 2.0);
        bar_fill.setBackgroundColor(Some(&theme.colors.bar.nscolor().CGColor()));
        bar_fill.setFrame(CGRect::new(
            CGPoint::new(track_x, bar_y),
            CGSize::new(track_w, bar_h),
        ));
        views.root_layer.addSublayer(&bar_fill);

        let label = |font: &objc2_app_kit::NSFont, color: &NSColor, y: f64, h: f64| {
            let field = NSTextField::labelWithString(&NSString::from_str(""), mtm);
            field.setFont(Some(font));
            field.setTextColor(Some(color));
            field.setAlignment(NSTextAlignment::Left);
            field.setLineBreakMode(NSLineBreakMode::ByClipping);
            field.setFrame(CGRect::new(CGPoint::new(p, y), CGSize::new(track_w, h)));
            views.effect.addSubview(&field);
            field
        };
        let detail = label(
            &detail_font,
            &theme.colors.detail.nscolor(),
            detail_y,
            detail_h,
        );
        let caption = label(
            &caption_font,
            &theme.colors.caption.nscolor(),
            caption_y,
            caption_h,
        );

        let strip = strip.map(|kind| {
            BeatStrip::build(
                &views.root_layer,
                theme,
                kind,
                p,
                digits_top + gap,
                width - 2.0 * p,
                strip_h,
            )
        });

        Face {
            atlas,
            height,
            strip,
            digits,
            shown: [0; DIGIT_CELLS],
            bar_track,
            bar_fill,
            track_x,
            track_w,
            bar_y,
            bar_h,
            detail,
            caption,
            last_detail: String::new(),
            last_caption: String::new(),
        }
    }

    /// 從 view 與圖層樹拆掉，準備重建。
    pub fn teardown(&self) {
        for l in &self.digits {
            l.removeFromSuperlayer();
        }
        self.bar_track.removeFromSuperlayer();
        self.bar_fill.removeFromSuperlayer();
        self.detail.removeFromSuperview();
        self.caption.removeFromSuperview();
        if let Some(strip) = &self.strip {
            strip.teardown();
        }
    }

    /// 只換有變的格子。呼叫端要包在關掉隱式動畫的 `CATransaction` 裡。
    pub fn set_digits(&mut self, digits: &[u8; DIGIT_CELLS]) {
        for (i, (&new, old)) in digits.iter().zip(self.shown.iter_mut()).enumerate() {
            if new != *old {
                // SAFETY: contents 是 `layerContentsForContentsScale:` 回傳的物件，正是給這個屬性用的。
                unsafe { self.digits[i].setContents(Some(self.atlas.contents(new))) };
                *old = new;
            }
        }
    }

    /// `Some(比例)` 畫成置中的長條，`None` 代表模型不可用，整條淡淡地亮。
    pub fn set_bar(&self, fraction: Option<f64>) {
        let (w, opacity) = match fraction {
            Some(f) => ((f.clamp(0.0, 1.0) * self.track_w).max(2.0), 1.0),
            None => (self.track_w, 0.35),
        };
        self.bar_fill.setOpacity(opacity);
        self.bar_fill.setFrame(CGRect::new(
            CGPoint::new(self.track_x + (self.track_w - w) / 2.0, self.bar_y),
            CGSize::new(w, self.bar_h),
        ));
    }

    pub fn set_detail(&mut self, text: &str) {
        if self.last_detail != text {
            self.detail.setStringValue(&NSString::from_str(text));
            self.last_detail = text.to_owned();
        }
    }

    pub fn set_caption(&mut self, text: &str) {
        if self.last_caption != text {
            self.caption.setStringValue(&NSString::from_str(text));
            self.last_caption = text.to_owned();
        }
    }
}
