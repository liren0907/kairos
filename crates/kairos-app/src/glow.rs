//! 螢幕邊緣光暈：每個螢幕一扇蓋滿整個螢幕、滑鼠穿透、不搶焦點的透明面板，
//! 四條邊各一層從邊緣往內漸透明的漸層，拍點時整體漸亮漸暗。
//!
//! 給視線不在時鐘面板上的人看的：周邊視覺看不清數字，但抓得到亮度變化。
//! 排程時建立、歸零後拆掉，跟音訊串流同壽命；中途換螢幕的情況不處理。

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSPanel, NSScreen, NSStatusWindowLevel, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_foundation::NSArray;
use objc2_quartz_core::{CAGradientLayer, CALayer};

use crate::theme::{Color, Theme};

struct GlowScreen {
    panel: Retained<NSPanel>,
    _host: Retained<NSView>,
    container: Retained<CALayer>,
    edges: Vec<Retained<CAGradientLayer>>,
}

pub struct Glow {
    screens: Vec<GlowScreen>,
    color: Color,
    final_color: Color,
    max_opacity: f64,
    showing_final: bool,
    last_opacity: f32,
}

/// 漸層的兩個顏色：邊緣是主題色，往內到全透明。
///
/// `CGColor` 是 Core Foundation 型別，直接當 `NSArray<CGColor>` 的元素會被 objc2 的
/// 編碼檢查擋下（它不是 `id`）；走 toll-free bridging 轉成 `AnyObject` 再建陣列。
fn gradient_colors(color: &Color) -> Retained<NSArray> {
    let solid = color.nscolor().CGColor();
    let clear = Color::rgba(color.r, color.g, color.b, 0.0)
        .nscolor()
        .CGColor();
    NSArray::from_slice(&[as_object(&solid), as_object(&clear)])
}

fn as_object(c: &CGColor) -> &AnyObject {
    let cf: &CFType = c;
    cf.as_ref()
}

impl Glow {
    pub fn build(mtm: MainThreadMarker, theme: &Theme) -> Glow {
        let width = theme.beat.glow_width.max(1.0);
        let colors = gradient_colors(&theme.beat.color);
        let screens = NSScreen::screens(mtm)
            .iter()
            .map(|screen| build_screen(mtm, &screen, width, &colors))
            .collect();
        Glow {
            screens,
            color: theme.beat.color,
            final_color: theme.beat.final_color,
            max_opacity: theme.beat.glow_opacity.clamp(0.0, 1.0),
            showing_final: false,
            last_opacity: 0.0,
        }
    }

    pub fn screen_count(&self) -> usize {
        self.screens.len()
    }

    /// `level` 0..1 是亮度；`is_final` 時換成歸零拍的顏色。呼叫端要包在關掉隱式動畫的
    /// `CATransaction` 裡。
    pub fn set(&mut self, level: f64, is_final: bool) {
        if is_final != self.showing_final {
            self.showing_final = is_final;
            let colors = gradient_colors(if is_final {
                &self.final_color
            } else {
                &self.color
            });
            for s in &self.screens {
                for e in &s.edges {
                    // SAFETY: 陣列元素是 CGColor，正是這個屬性要的。
                    unsafe { e.setColors(Some(&colors)) };
                }
            }
        }
        let opacity = (level.clamp(0.0, 1.0) * self.max_opacity) as f32;
        if opacity != self.last_opacity {
            self.last_opacity = opacity;
            for s in &self.screens {
                s.container.setOpacity(opacity);
            }
        }
    }

    pub fn teardown(&self) {
        for s in &self.screens {
            s.panel.orderOut(None);
        }
    }
}

fn build_screen(
    mtm: MainThreadMarker,
    screen: &NSScreen,
    width: f64,
    colors: &NSArray,
) -> GlowScreen {
    let frame = screen.frame();
    let mask = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        mask,
        NSBackingStoreType::Buffered,
        false,
    );
    // 比時鐘面板低一級，時鐘永遠在光暈上面。
    panel.setLevel(NSStatusWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel.setIgnoresMouseEvents(true);
    panel.setHidesOnDeactivate(false);
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(false);
    // SAFETY: 我們用 Retained 持有、從不呼叫 close。
    unsafe { panel.setReleasedWhenClosed(false) };

    let bounds = CGRect::new(CGPoint::new(0.0, 0.0), frame.size);
    let root = CALayer::new();
    let host = NSView::initWithFrame(NSView::alloc(mtm), bounds);
    host.setLayer(Some(&root));
    host.setWantsLayer(true);
    panel.setContentView(Some(&host));

    let container = CALayer::new();
    container.setFrame(bounds);
    container.setOpacity(0.0);
    root.addSublayer(&container);

    let (w, h) = (frame.size.width, frame.size.height);
    let g = width.min(w / 2.0).min(h / 2.0);
    // （框、起點、終點）；座標原點在左下，起點是主題色那一端。
    let specs = [
        (
            CGRect::new(CGPoint::new(0.0, h - g), CGSize::new(w, g)),
            CGPoint::new(0.5, 1.0),
            CGPoint::new(0.5, 0.0),
        ),
        (
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w, g)),
            CGPoint::new(0.5, 0.0),
            CGPoint::new(0.5, 1.0),
        ),
        (
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(g, h)),
            CGPoint::new(0.0, 0.5),
            CGPoint::new(1.0, 0.5),
        ),
        (
            CGRect::new(CGPoint::new(w - g, 0.0), CGSize::new(g, h)),
            CGPoint::new(1.0, 0.5),
            CGPoint::new(0.0, 0.5),
        ),
    ];
    let edges = specs
        .iter()
        .map(|(rect, start, end)| {
            let layer = CAGradientLayer::new();
            layer.setFrame(*rect);
            layer.setStartPoint(*start);
            layer.setEndPoint(*end);
            // SAFETY: 陣列元素是 CGColor。
            unsafe { layer.setColors(Some(colors)) };
            container.addSublayer(&layer);
            layer
        })
        .collect();

    panel.orderFrontRegardless();
    GlowScreen {
        panel,
        _host: host,
        container,
        edges,
    }
}
