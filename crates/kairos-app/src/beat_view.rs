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
    pub const ALL: [StripKind; 3] = [StripKind::Ball, StripKind::Ring, StripKind::Pulse];

    /// 選單與紀錄用的中文名。
    pub fn title(self) -> &'static str {
        match self {
            StripKind::Ball => "球",
            StripKind::Ring => "環",
            StripKind::Pulse => "脈衝",
        }
    }
}

/// 節拍元件的尺寸（點）。兩種畫法共用同一組，乘上面板的大小倍率。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StripMetrics {
    pub ball_diameter: f64,
    pub ground_width: f64,
    pub ground_height: f64,
    pub ground_lift: f64,
    pub ring_inner: f64,
    pub ring_outer_max: f64,
    pub ring_border: f64,
    pub pulse_diameter: f64,
}

impl StripMetrics {
    /// 倍率 1.0 的尺寸。
    pub const BASE: StripMetrics = StripMetrics {
        ball_diameter: 14.0,
        ground_width: 80.0,
        ground_height: 2.0,
        ground_lift: 10.0,
        ring_inner: 10.0,
        ring_outer_max: 28.0,
        ring_border: 2.0,
        pulse_diameter: 18.0,
    };

    pub fn scaled(zoom: f64) -> StripMetrics {
        let b = StripMetrics::BASE;
        StripMetrics {
            ball_diameter: b.ball_diameter * zoom,
            ground_width: b.ground_width * zoom,
            ground_height: b.ground_height * zoom,
            ground_lift: b.ground_lift * zoom,
            ring_inner: b.ring_inner * zoom,
            ring_outer_max: b.ring_outer_max * zoom,
            ring_border: b.ring_border * zoom,
            pulse_diameter: b.pulse_diameter * zoom,
        }
    }
}

/// 歸零拍的放大倍率。
pub(crate) const FINAL_SCALE: f64 = 1.4;

pub struct BeatStrip {
    kind: StripKind,
    m: StripMetrics,
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
    /// 在 `parent` 裡、左下角 `(x0, y0)`、大小 `width × height` 的區域建元件；`zoom` 是面板的大小倍率。
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        parent: &CALayer,
        theme: &Theme,
        kind: StripKind,
        x0: f64,
        y0: f64,
        width: f64,
        height: f64,
        zoom: f64,
    ) -> BeatStrip {
        let m = StripMetrics::scaled(zoom);
        let color = cg(&theme.beat.color);
        let final_color = cg(&theme.beat.final_color);
        let dim = theme.beat.color;
        let color_dim = cg(&Color::rgba(dim.r, dim.g, dim.b, dim.a * 0.35));
        let center_x = x0 + width / 2.0;

        let (a, b, base_y, max_height, outer_radius) = match kind {
            StripKind::Ball => {
                let ground_top = y0 + m.ground_lift + m.ground_height;
                let ball = circle(m.ball_diameter);
                ball.setBackgroundColor(Some(&color));
                let ground = CALayer::new();
                ground.setFrame(CGRect::new(
                    CGPoint::new(center_x - m.ground_width / 2.0, y0 + m.ground_lift),
                    CGSize::new(m.ground_width, m.ground_height),
                ));
                ground.setCornerRadius(m.ground_height / 2.0);
                ground.setBackgroundColor(Some(&color));
                ground.setOpacity(0.35);
                let max_height =
                    (y0 + height - 4.0 - m.ball_diameter * FINAL_SCALE - ground_top).max(4.0);
                (ball, Some(ground), ground_top, max_height, 0.0)
            }
            StripKind::Ring => {
                let outer_r = m
                    .ring_outer_max
                    .min(height / 2.0 - 3.0)
                    .max(m.ring_inner + 2.0);
                let outer = circle(outer_r * 2.0);
                outer.setBorderWidth(m.ring_border);
                outer.setBorderColor(Some(&color));
                let inner = circle(m.ring_inner * 2.0);
                inner.setBorderWidth(m.ring_border);
                inner.setBorderColor(Some(&color));
                (outer, Some(inner), y0 + height / 2.0, 0.0, outer_r)
            }
            StripKind::Pulse => {
                let dot = circle(m.pulse_diameter);
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
            m,
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

    /// 起跑前不想看到靜止的球就整個藏起來。
    pub fn set_hidden(&self, hidden: bool) {
        self.a.setHidden(hidden);
        if let Some(b) = &self.b {
            b.setHidden(hidden);
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
            self.m.ring_inner * 1.3
        } else {
            self.m.ring_inner
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
                let d = self.m.ball_diameter * scale;
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
                let d = self.m.pulse_diameter * scale;
                self.a
                    .setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(d, d)));
                self.a.setCornerRadius(d / 2.0);
                self.a.setBackgroundColor(Some(color));
            }
        }
    }
}
