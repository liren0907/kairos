//! 面板上的節拍區：數字上方一塊只在節拍期間出現的區域，放一個節拍元件。
//! 幾何都在 `kairos_core::beat::visual`，這裡只把數字塞進 `CALayer` 的屬性。
//!
//! 三種樣式：彈跳球（落地即拍點，地線閃一下）、收縮圓環（外環縮到與內環重合）、
//! pulse（不位移、只變亮，給「減少動態效果」用）。歸零拍換顏色並放大。

use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_quartz_core::CALayer;

use kairos_core::beat::Phase;
use kairos_core::beat::visual::{ball_height, landing_flash, pulse_opacity, ring_radius};

use crate::theme::{Color, Theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripKind {
    Ball,
    Ring,
    Pulse,
}

impl StripKind {
    pub fn label(self) -> &'static str {
        match self {
            StripKind::Ball => "ball",
            StripKind::Ring => "ring",
            StripKind::Pulse => "pulse",
        }
    }
}

pub(crate) const BALL_DIAMETER: f64 = 14.0;
pub(crate) const GROUND_WIDTH: f64 = 80.0;
pub(crate) const GROUND_HEIGHT: f64 = 2.0;
pub(crate) const GROUND_LIFT: f64 = 10.0;
pub(crate) const RING_INNER: f64 = 10.0;
pub(crate) const RING_OUTER_MAX: f64 = 28.0;
pub(crate) const RING_BORDER: f64 = 2.0;
pub(crate) const PULSE_DIAMETER: f64 = 18.0;
/// 歸零拍的放大倍率。
pub(crate) const FINAL_SCALE: f64 = 1.4;

pub struct BeatStrip {
    kind: StripKind,
    /// 主角：球、外環、或 pulse 的點。
    a: Retained<CALayer>,
    /// 配角：地線、內環；pulse 沒有。
    b: Option<Retained<CALayer>>,
    color: Retained<CGColor>,
    final_color: Retained<CGColor>,
    color_dim: Retained<CGColor>,
    center_x: f64,
    /// 球：地線上緣；環與點：中心 y。
    base_y: f64,
    max_height: f64,
    outer_radius: f64,
    showing_final: bool,
}

fn cg(c: &Color) -> Retained<CGColor> {
    c.nscolor().CGColor()
}

fn circle(diameter: f64) -> Retained<CALayer> {
    let l = CALayer::new();
    l.setBounds(CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(diameter, diameter),
    ));
    l.setCornerRadius(diameter / 2.0);
    l
}

impl BeatStrip {
    /// 在 `parent` 裡、左下角 `(x0, y0)`、大小 `width × height` 的區域建元件。
    pub fn build(
        parent: &CALayer,
        theme: &Theme,
        kind: StripKind,
        x0: f64,
        y0: f64,
        width: f64,
        height: f64,
    ) -> BeatStrip {
        let color = cg(&theme.beat.color);
        let final_color = cg(&theme.beat.final_color);
        let dim = theme.beat.color;
        let color_dim = cg(&Color::rgba(dim.r, dim.g, dim.b, dim.a * 0.35));
        let center_x = x0 + width / 2.0;

        let (a, b, base_y, max_height, outer_radius) = match kind {
            StripKind::Ball => {
                let ground_top = y0 + GROUND_LIFT + GROUND_HEIGHT;
                let ball = circle(BALL_DIAMETER);
                ball.setBackgroundColor(Some(&color));
                let ground = CALayer::new();
                ground.setFrame(CGRect::new(
                    CGPoint::new(center_x - GROUND_WIDTH / 2.0, y0 + GROUND_LIFT),
                    CGSize::new(GROUND_WIDTH, GROUND_HEIGHT),
                ));
                ground.setCornerRadius(GROUND_HEIGHT / 2.0);
                ground.setBackgroundColor(Some(&color));
                ground.setOpacity(0.35);
                let max_height =
                    (y0 + height - 4.0 - BALL_DIAMETER * FINAL_SCALE - ground_top).max(4.0);
                (ball, Some(ground), ground_top, max_height, 0.0)
            }
            StripKind::Ring => {
                let outer_r = RING_OUTER_MAX.min(height / 2.0 - 3.0).max(RING_INNER + 2.0);
                let outer = circle(outer_r * 2.0);
                outer.setBorderWidth(RING_BORDER);
                outer.setBorderColor(Some(&color));
                let inner = circle(RING_INNER * 2.0);
                inner.setBorderWidth(RING_BORDER);
                inner.setBorderColor(Some(&color));
                (outer, Some(inner), y0 + height / 2.0, 0.0, outer_r)
            }
            StripKind::Pulse => {
                let dot = circle(PULSE_DIAMETER);
                dot.setBackgroundColor(Some(&color));
                (dot, None, y0 + height / 2.0, 0.0, 0.0)
            }
        };
        a.setPosition(CGPoint::new(center_x, base_y));
        if let Some(b) = &b {
            if kind == StripKind::Ring {
                b.setPosition(CGPoint::new(center_x, base_y));
            }
            parent.addSublayer(b);
        }
        parent.addSublayer(&a);

        BeatStrip {
            kind,
            a,
            b,
            color,
            final_color,
            color_dim,
            center_x,
            base_y,
            max_height,
            outer_radius,
            showing_final: false,
        }
    }

    pub fn teardown(&self) {
        self.a.removeFromSuperlayer();
        if let Some(b) = &self.b {
            b.removeFromSuperlayer();
        }
    }

    /// 呼叫端要包在關掉隱式動畫的 `CATransaction` 裡。
    pub fn render(&mut self, p: &Phase) {
        if p.is_final != self.showing_final {
            self.showing_final = p.is_final;
            self.apply_final(p.is_final);
        }
        let flash = landing_flash(p);
        match self.kind {
            StripKind::Ball => {
                let d = self.a.bounds().size.height;
                let h = ball_height(p, self.max_height);
                self.a
                    .setPosition(CGPoint::new(self.center_x, self.base_y + h + d / 2.0));
                if let Some(ground) = &self.b {
                    ground.setOpacity((0.35 + 0.65 * flash) as f32);
                }
            }
            StripKind::Ring => {
                let r = ring_radius(p, self.outer_radius, self.inner_radius());
                self.a.setBounds(CGRect::new(
                    CGPoint::new(0.0, 0.0),
                    CGSize::new(r * 2.0, r * 2.0),
                ));
                self.a.setCornerRadius(r);
                if let Some(inner) = &self.b {
                    inner.setBackgroundColor(Some(if flash > 0.02 {
                        if p.is_final {
                            &self.final_color
                        } else {
                            &self.color
                        }
                    } else {
                        &self.color_dim
                    }));
                    inner.setOpacity(if flash > 0.02 { flash as f32 } else { 1.0 });
                }
            }
            StripKind::Pulse => {
                self.a.setOpacity(pulse_opacity(p) as f32);
            }
        }
    }

    fn inner_radius(&self) -> f64 {
        if self.showing_final {
            RING_INNER * 1.3
        } else {
            RING_INNER
        }
    }

    fn apply_final(&self, is_final: bool) {
        let color = if is_final {
            &self.final_color
        } else {
            &self.color
        };
        let scale = if is_final { FINAL_SCALE } else { 1.0 };
        match self.kind {
            StripKind::Ball => {
                let d = BALL_DIAMETER * scale;
                self.a
                    .setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(d, d)));
                self.a.setCornerRadius(d / 2.0);
                self.a.setBackgroundColor(Some(color));
                if let Some(ground) = &self.b {
                    ground.setBackgroundColor(Some(color));
                }
            }
            StripKind::Ring => {
                self.a.setBorderColor(Some(color));
                if let Some(inner) = &self.b {
                    let r = self.inner_radius();
                    inner.setBounds(CGRect::new(
                        CGPoint::new(0.0, 0.0),
                        CGSize::new(r * 2.0, r * 2.0),
                    ));
                    inner.setCornerRadius(r);
                    inner.setBorderColor(Some(color));
                }
            }
            StripKind::Pulse => {
                let d = PULSE_DIAMETER * scale;
                self.a
                    .setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(d, d)));
                self.a.setCornerRadius(d / 2.0);
                self.a.setBackgroundColor(Some(color));
            }
        }
    }
}
