//! 用 Metal 畫節拍區，並記錄每一格**真正上屏**的時刻。
//!
//! 節拍區只有圓與圓角矩形，一個鋪滿的三角形加 SDF 片段著色器就夠；每格只換 uniform。
//! 重點不在畫得多好，而在 `MTLDrawable` 的 `presentedTime`：這是 macOS 公開 API 裡唯一會
//! 告訴你「這一格實際什麼時候顯示」的地方，跟 `mach_absolute_time` 同基準。顯示連結的
//! `targetTimestamp` 只是預測，兩者的差就是我們一直看不到的那一格延遲。
//!
//! 上屏方式預設是 Metal 自己排（`presentsWithTransaction` 關）：GPU 畫完的下一個 vsync 顯示，
//! `presentedTime` 是標準語意，跟數字最多差一個 vsync。實測綁 CA transaction 的版本會跳掉
//! 一成以上的格、回報的時刻還比不綁的早兩格，語意可疑，所以只留作 A/B（`KAIROS_METAL_SYNC`）。
//! 每格 Metal 這一段的耗時會記下來一起回報，超過一兩毫秒就是有東西在等（drawable 不夠、GPU 忙）。
//!
//! 幾何跟 [`beat_view`](crate::beat_view) 用同一組常數，兩種畫法長得一樣。

use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBlendFactor, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLLibrary, MTLLoadAction,
    MTLPixelFormat, MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLStoreAction,
};
use objc2_quartz_core::{CALayer, CAMetalDrawable, CAMetalLayer};

use kairos_core::beat::Phase;
use kairos_core::beat::visual::{ball_height, landing_flash, pulse_opacity, ring_radius};
use kairos_core::time::HostTime;

use crate::beat_view::{FINAL_SCALE, StripKind, StripMetrics};
use crate::theme::{Color, Theme};

/// 片段著色器：`u.a` 是主角圓（球、外環、點），`u.b` 是配角（地線或內環）。
/// 座標是像素、y 向下（`[[position]]` 的慣例）；`cov` 以半像素做抗鋸齒。輸出預乘 alpha。
const SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct U {
    float4 size_mode;
    float4 a;
    float4 a_color;
    float4 b;
    float4 b_color;
    float4 b_fill;
};

struct V { float4 pos [[position]]; };

vertex V strip_vertex(uint vid [[vertex_id]]) {
    float2 p = float2(vid == 1 ? 3.0 : -1.0, vid == 2 ? 3.0 : -1.0);
    V o;
    o.pos = float4(p, 0.0, 1.0);
    return o;
}

static float cov(float d) { return 1.0 - smoothstep(-0.5, 0.5, d); }
static float circle(float2 p, float2 c, float r) { return length(p - c) - r; }
static float rrect(float2 p, float2 c, float2 h) {
    float r = min(h.x, h.y);
    float2 q = abs(p - c) - h + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
static float ring(float d, float border) { return cov(abs(d + border * 0.5) - border * 0.5); }
static float4 over(float4 dst, float4 color, float c) {
    float a = color.a * c;
    return float4(color.rgb * a, a) + dst * (1.0 - a);
}

fragment float4 strip_fragment(V in [[stage_in]], constant U& u [[buffer(0)]]) {
    float2 p = in.pos.xy;
    int mode = int(u.size_mode.z);
    float4 out = float4(0.0);
    if (mode == 0) {
        out = over(out, u.b_color, cov(rrect(p, u.b.xy, u.b.zw)));
    } else if (mode == 1) {
        float d = circle(p, u.b.xy, u.b.z);
        out = over(out, u.b_fill, cov(d + u.b.w));
        out = over(out, u.b_color, ring(d, u.b.w));
    }
    float d = circle(p, u.a.xy, u.a.z);
    float c = u.a.w > 0.0 ? ring(d, u.a.w) : cov(d);
    return over(out, u.a_color, c);
}
"#;

/// 節拍區的尺寸（點）與螢幕縮放。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StripGeometry {
    pub width: f64,
    pub height: f64,
    /// 螢幕的 backing scale（點到像素）。
    pub scale: f64,
    /// 面板的大小倍率，決定球、環、點的尺寸。
    pub zoom: f64,
}

/// 傳給著色器的常數，跟 MSL 的 `struct U` 一模一樣：六個 float4，96 位元組。
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Uniforms {
    /// 寬、高（像素）、模式（0 球、1 環、2 點）、未用。
    pub size_mode: [f32; 4],
    /// 主角：中心 x、中心 y（像素、y 向下）、半徑、邊框寬（0＝實心）。
    pub a: [f32; 4],
    pub a_color: [f32; 4],
    /// 配角。球：地線中心 x、y、半寬、半高。環：內環中心 x、y、半徑、邊框寬。點：不用。
    pub b: [f32; 4],
    /// 配角的邊框（環）或整體（地線）顏色。
    pub b_color: [f32; 4],
    /// 內環的填色（只有環用）。
    pub b_fill: [f32; 4],
}

fn rgba(c: &Color, alpha: f64) -> [f32; 4] {
    [c.r as f32, c.g as f32, c.b as f32, (c.a * alpha) as f32]
}

/// 純函數：某一格的 uniform。座標先以「點、y 向上、原點在節拍區左下」算，跟 `beat_view` 一樣，
/// 最後換成像素、y 向下。
pub fn uniforms_for(
    kind: StripKind,
    geo: StripGeometry,
    phase: &Phase,
    color: &Color,
    final_color: &Color,
) -> Uniforms {
    let s = geo.scale;
    let m = StripMetrics::scaled(geo.zoom);
    let px = |x: f64| (x * s) as f32;
    let py = |y: f64| ((geo.height - y) * s) as f32;
    let main = if phase.is_final { final_color } else { color };
    let flash = landing_flash(phase);
    let center_x = geo.width / 2.0;
    let mode = match kind {
        StripKind::Ball => 0.0,
        StripKind::Ring => 1.0,
        StripKind::Pulse => 2.0,
    };
    let mut u = Uniforms {
        size_mode: [px(geo.width), px(geo.height), mode, 0.0],
        a: [0.0; 4],
        a_color: [0.0; 4],
        b: [0.0; 4],
        b_color: [0.0; 4],
        b_fill: [0.0; 4],
    };
    match kind {
        StripKind::Ball => {
            let ground_top = m.ground_lift + m.ground_height;
            let max_height =
                (geo.height - 4.0 - m.ball_diameter * FINAL_SCALE - ground_top).max(4.0);
            let d = m.ball_diameter * if phase.is_final { FINAL_SCALE } else { 1.0 };
            let h = ball_height(phase, max_height);
            u.a = [px(center_x), py(ground_top + h + d / 2.0), px(d / 2.0), 0.0];
            u.a_color = rgba(main, 1.0);
            u.b = [
                px(center_x),
                py(m.ground_lift + m.ground_height / 2.0),
                px(m.ground_width / 2.0),
                px(m.ground_height / 2.0),
            ];
            u.b_color = rgba(main, 0.35 + 0.65 * flash);
        }
        StripKind::Ring => {
            let outer_r = m
                .ring_outer_max
                .min(geo.height / 2.0 - 3.0)
                .max(m.ring_inner + 2.0);
            let inner_r = if phase.is_final {
                m.ring_inner * 1.3
            } else {
                m.ring_inner
            };
            let r = ring_radius(phase, outer_r, inner_r);
            let cy = geo.height / 2.0;
            u.a = [px(center_x), py(cy), px(r), px(m.ring_border)];
            u.a_color = rgba(main, 1.0);
            u.b = [px(center_x), py(cy), px(inner_r), px(m.ring_border)];
            let flashing = flash > 0.02;
            u.b_color = rgba(main, if flashing { flash } else { 1.0 });
            u.b_fill = rgba(main, if flashing { flash } else { 0.35 });
        }
        StripKind::Pulse => {
            let d = m.pulse_diameter * if phase.is_final { FINAL_SCALE } else { 1.0 };
            u.a = [px(center_x), py(geo.height / 2.0), px(d / 2.0), 0.0];
            u.a_color = rgba(main, pulse_opacity(phase));
        }
    }
    u
}

/// 一格的上屏紀錄：顯示連結給的目標時刻，與 Metal 回報的實際上屏時刻（跳過的格是 `None`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentRecord {
    pub frame: u64,
    pub target: HostTime,
    pub presented: Option<HostTime>,
}

/// 一次節拍程序的上屏統計。
#[derive(Clone, Debug, PartialEq)]
pub struct PresentSummary {
    /// 有回報實際上屏時刻的格數。
    pub reported: usize,
    /// 系統回報「沒顯示」或拿不到 drawable 的格數。
    pub skipped: usize,
    /// 實際上屏 − targetTimestamp（毫秒）：中位、最小、最大。
    pub offset_ms: Option<(f64, f64, f64)>,
    /// 每一拍：離拍點最近的一格「實際上屏」差幾毫秒，正值代表在拍點之後。
    pub nearest_presented_ms: Vec<Option<f64>>,
    /// 每格 Metal 這一段（拿 drawable 到 present）花的毫秒數：中位、最大。
    pub cost_ms: Option<(f64, f64)>,
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// 純函數：把紀錄整理成摘要。`ticks` 是拍點的主機時刻（含畫面提前量）。
pub fn summarize(
    records: &[PresentRecord],
    skipped_without_drawable: usize,
    costs_ms: &[f64],
    ticks: &[HostTime],
) -> PresentSummary {
    let mut offsets: Vec<f64> = records
        .iter()
        .filter_map(|r| {
            r.presented
                .map(|p| p.signed_nanos_since(r.target) as f64 / 1e6)
        })
        .collect();
    let skipped =
        skipped_without_drawable + records.iter().filter(|r| r.presented.is_none()).count();
    offsets.sort_by(|a, b| a.total_cmp(b));
    let offset_ms =
        (!offsets.is_empty()).then(|| (median(&offsets), offsets[0], offsets[offsets.len() - 1]));
    let nearest_presented_ms = ticks
        .iter()
        .map(|&t| {
            records
                .iter()
                .filter_map(|r| r.presented)
                .map(|p| p.signed_nanos_since(t) as f64 / 1e6)
                .min_by(|a, b| a.abs().total_cmp(&b.abs()))
        })
        .collect();
    let mut costs: Vec<f64> = costs_ms.to_vec();
    costs.sort_by(|a, b| a.total_cmp(b));
    let cost_ms = (!costs.is_empty()).then(|| (median(&costs), costs[costs.len() - 1]));
    PresentSummary {
        reported: offsets.len(),
        skipped,
        offset_ms,
        nearest_presented_ms,
        cost_ms,
    }
}

impl PresentSummary {
    /// 給紀錄檔的兩行。
    pub fn lines(&self) -> (String, String) {
        let first = match self.offset_ms {
            Some((mid, lo, hi)) => format!(
                "節拍上屏：{} 格有回報，實際上屏 − 預測 中位 {mid:+.2} ms（{lo:+.2}–{hi:+.2}），跳過 {} 格{}",
                self.reported,
                self.skipped,
                match self.cost_ms {
                    Some((m, max)) => format!("；Metal 每格 中位 {m:.3} ms（最大 {max:.3}）"),
                    None => String::new(),
                }
            ),
            None => format!(
                "節拍上屏：沒有任何一格回報實際上屏時刻，跳過 {} 格",
                self.skipped
            ),
        };
        let mut second = String::from("節拍上屏");
        for (k, d) in self.nearest_presented_ms.iter().enumerate() {
            match d {
                Some(d) => second.push_str(&format!("；第 {k} 拍最近實際上屏差 {d:+.1} ms")),
                None => second.push_str(&format!("；第 {k} 拍沒有上屏紀錄")),
            }
        }
        (first, second)
    }
}

/// 節拍區的 Metal 版本。建立失敗就回 `Err(原因)`，呼叫端退回 CALayer 版本。
pub struct MetalStrip {
    layer: Retained<CAMetalLayer>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    kind: StripKind,
    geometry: StripGeometry,
    color: Color,
    final_color: Color,
    records: Arc<Mutex<Vec<PresentRecord>>>,
    frame: u64,
    skipped: usize,
    costs_ms: Vec<f64>,
    with_transaction: bool,
}

const MAX_RECORDS: usize = 4_096;

impl MetalStrip {
    /// 在 `parent` 下建一個蓋住節拍區的 `CAMetalLayer`。`x0, y0` 是節拍區在父圖層的左下角（點）。
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        parent: &CALayer,
        theme: &Theme,
        kind: StripKind,
        x0: f64,
        y0: f64,
        width: f64,
        height: f64,
        scale: f64,
        zoom: f64,
    ) -> Result<MetalStrip, String> {
        let device = MTLCreateSystemDefaultDevice().ok_or("沒有 Metal 裝置")?;
        let queue = device
            .newCommandQueue()
            .ok_or("建不了 Metal command queue")?;
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADER), None)
            .map_err(|e| format!("著色器編譯失敗：{e}"))?;
        let vertex = library
            .newFunctionWithName(&NSString::from_str("strip_vertex"))
            .ok_or("著色器裡沒有 strip_vertex")?;
        let fragment = library
            .newFunctionWithName(&NSString::from_str("strip_fragment"))
            .ok_or("著色器裡沒有 strip_fragment")?;

        let desc = MTLRenderPipelineDescriptor::new();
        desc.setVertexFunction(Some(&vertex));
        desc.setFragmentFunction(Some(&fragment));
        // SAFETY: 索引 0 一定在色彩附件陣列的範圍內。
        let attachment = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        attachment.setBlendingEnabled(true);
        attachment.setSourceRGBBlendFactor(MTLBlendFactor::One);
        attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
        attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        let pipeline = device
            .newRenderPipelineStateWithDescriptor_error(&desc)
            .map_err(|e| format!("建不了 render pipeline：{e}"))?;

        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setOpaque(false);
        layer.setFramebufferOnly(true);
        layer.setContentsScale(scale);
        layer.setFrame(CGRect::new(
            CGPoint::new(x0, y0),
            CGSize::new(width, height),
        ));
        layer.setDrawableSize(CGSize::new(
            (width * scale).round(),
            (height * scale).round(),
        ));
        // 預設讓 Metal 自己排上屏（GPU 畫完的下一個 vsync），`presentedTime` 是標準語意；
        // 實測綁 CA transaction 會跳掉一成以上的格、回報的時刻也比不綁的早兩格，語意可疑。
        // 開發輔助：`KAIROS_METAL_SYNC` 改成綁 transaction，拿來 A/B。
        let with_transaction = std::env::var_os("KAIROS_METAL_SYNC").is_some();
        layer.setPresentsWithTransaction(with_transaction);
        layer.setDisplaySyncEnabled(true);
        // 三個 drawable：上屏延遲兩格時，兩個會讓 nextDrawable 等前一格釋放，整格卡住、
        // 提交變晚、延遲再多一格，惡性循環。
        layer.setMaximumDrawableCount(3);
        parent.addSublayer(&layer);

        Ok(MetalStrip {
            layer,
            queue,
            pipeline,
            kind,
            geometry: StripGeometry {
                width,
                height,
                scale,
                zoom,
            },
            color: theme.beat.color,
            final_color: theme.beat.final_color,
            records: Arc::new(Mutex::new(Vec::with_capacity(512))),
            frame: 0,
            skipped: 0,
            costs_ms: Vec::with_capacity(512),
            with_transaction,
        })
    }

    pub fn teardown(&self) {
        self.layer.removeFromSuperlayer();
    }

    /// 畫這一格並排上屏。`target` 是顯示連結說這一格會上屏的時刻，跟實際上屏一起記下來。
    /// 呼叫端要包在關掉隱式動畫的 `CATransaction` 裡。
    pub fn render(&mut self, phase: &Phase, target: HostTime) {
        let started = Instant::now();
        let uniforms = uniforms_for(
            self.kind,
            self.geometry,
            phase,
            &self.color,
            &self.final_color,
        );
        let Some(drawable) = self.layer.nextDrawable() else {
            self.skipped += 1;
            return;
        };
        let Some(buffer) = self.queue.commandBuffer() else {
            self.skipped += 1;
            return;
        };
        let pass = MTLRenderPassDescriptor::renderPassDescriptor();
        // SAFETY: 索引 0 一定在色彩附件陣列的範圍內。
        let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(&drawable.texture()));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::Store);
        attachment.setClearColor(MTLClearColor {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 0.0,
        });
        let Some(encoder) = buffer.renderCommandEncoderWithDescriptor(&pass) else {
            self.skipped += 1;
            return;
        };
        encoder.setRenderPipelineState(&self.pipeline);
        // SAFETY: `uniforms` 在這個函式活著，長度就是它的大小；buffer(0) 對應著色器的宣告。
        unsafe {
            encoder.setFragmentBytes_length_atIndex(
                NonNull::from(&uniforms).cast::<c_void>(),
                size_of::<Uniforms>(),
                0,
            );
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
        }
        encoder.endEncoding();

        let frame = self.frame;
        self.frame += 1;
        let records = Arc::clone(&self.records);
        let handler = RcBlock::new(move |d: NonNull<ProtocolObject<dyn MTLDrawable>>| {
            // SAFETY: Metal 在呼叫 handler 時保證 drawable 活著。
            let seconds = unsafe { d.as_ref() }.presentedTime();
            let presented =
                (seconds > 0.0).then(|| HostTime::from_nanos((seconds * 1e9).round() as u64));
            if let Ok(mut r) = records.lock()
                && r.len() < MAX_RECORDS
            {
                r.push(PresentRecord {
                    frame,
                    target,
                    presented,
                });
            }
        });
        // SAFETY: block 由 Metal 複製持有，這裡的指標在呼叫期間有效。
        unsafe { drawable.addPresentedHandler(RcBlock::as_ptr(&handler)) };

        if self.with_transaction {
            // presentsWithTransaction 的規定順序：commit → 等排程 → 在 transaction 裡 present。
            buffer.commit();
            buffer.waitUntilScheduled();
            drawable.present();
        } else {
            buffer.presentDrawable(ProtocolObject::from_ref(&*drawable));
            buffer.commit();
        }
        if self.costs_ms.len() < MAX_RECORDS {
            self.costs_ms.push(started.elapsed().as_secs_f64() * 1e3);
        }
    }

    /// 節拍結束時拿走統計；`ticks` 是拍點的主機時刻（含畫面提前量）。
    pub fn take_summary(&mut self, ticks: &[HostTime]) -> PresentSummary {
        let records = std::mem::take(&mut *self.records.lock().unwrap_or_else(|e| e.into_inner()));
        let costs = std::mem::take(&mut self.costs_ms);
        let skipped = std::mem::take(&mut self.skipped);
        summarize(&records, skipped, &costs, ticks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn geo() -> StripGeometry {
        StripGeometry {
            width: 288.0,
            height: 64.0,
            scale: 2.0,
            zoom: 1.0,
        }
    }

    const M: StripMetrics = StripMetrics::BASE;

    fn phase(phi: f64, is_final: bool, since_ms: Option<u64>) -> Phase {
        Phase {
            beat: 0,
            phi,
            is_final,
            since_landing: since_ms.map(Duration::from_millis),
            done: false,
        }
    }

    const BLUE: Color = Color::rgba(0.0, 0.5, 1.0, 1.0);
    const ORANGE: Color = Color::rgba(1.0, 0.7, 0.3, 1.0);

    #[test]
    fn uniforms_layout_matches_the_shader_struct() {
        assert_eq!(size_of::<Uniforms>(), 6 * 16);
        assert_eq!(std::mem::align_of::<Uniforms>(), 4);
    }

    #[test]
    fn ball_sits_on_the_ground_at_landing_and_flies_mid_beat() {
        let g = geo();
        let ground_top = M.ground_lift + M.ground_height;
        let landed = uniforms_for(StripKind::Ball, g, &phase(0.0, false, None), &BLUE, &ORANGE);
        assert_eq!(landed.size_mode, [576.0, 128.0, 0.0, 0.0]);
        // 中心 x 在正中間；y 換成像素、向下：(64 − (12 + 7)) × 2 = 90。
        assert_eq!(landed.a[0], 288.0);
        assert_eq!(
            landed.a[1],
            ((g.height - (ground_top + M.ball_diameter / 2.0)) * 2.0) as f32
        );
        assert_eq!(landed.a[2], (M.ball_diameter / 2.0 * 2.0) as f32);
        assert_eq!(landed.a[3], 0.0);
        assert_eq!(landed.a_color, [0.0, 0.5, 1.0, 1.0]);
        // 地線：沒閃時 35%。
        assert!((landed.b_color[3] - 0.35).abs() < 1e-6);
        assert_eq!(landed.b[2], (M.ground_width / 2.0 * 2.0) as f32);

        let flying = uniforms_for(StripKind::Ball, g, &phase(0.5, false, None), &BLUE, &ORANGE);
        assert!(flying.a[1] < landed.a[1], "半拍時球在上面（像素 y 較小）");

        let flashing = uniforms_for(
            StripKind::Ball,
            g,
            &phase(0.0, false, Some(0)),
            &BLUE,
            &ORANGE,
        );
        assert!((flashing.b_color[3] - 1.0).abs() < 1e-6, "落地瞬間地線全亮");

        let final_beat = uniforms_for(StripKind::Ball, g, &phase(0.0, true, None), &BLUE, &ORANGE);
        assert_eq!(final_beat.a_color, [1.0, 0.7, 0.3, 1.0]);
        assert_eq!(
            final_beat.a[2],
            (M.ball_diameter * FINAL_SCALE / 2.0 * 2.0) as f32
        );
    }

    #[test]
    fn ring_shrinks_and_inner_ring_flashes() {
        let g = geo();
        let start = uniforms_for(StripKind::Ring, g, &phase(0.0, false, None), &BLUE, &ORANGE);
        let end = uniforms_for(StripKind::Ring, g, &phase(1.0, false, None), &BLUE, &ORANGE);
        assert_eq!(start.size_mode[2], 1.0);
        assert!(start.a[2] > end.a[2], "外環從大縮到小");
        assert_eq!(end.a[2], (M.ring_inner * 2.0) as f32);
        assert_eq!(start.a[3], (M.ring_border * 2.0) as f32);
        assert!((start.b_fill[3] - 0.35).abs() < 1e-6);
        assert_eq!(start.b_color[3], 1.0);

        let flash = uniforms_for(
            StripKind::Ring,
            g,
            &phase(0.0, false, Some(0)),
            &BLUE,
            &ORANGE,
        );
        assert_eq!(flash.b_fill[3], 1.0);
        assert_eq!(flash.b_color[3], 1.0);

        let final_beat = uniforms_for(StripKind::Ring, g, &phase(0.0, true, None), &BLUE, &ORANGE);
        assert_eq!(final_beat.b[2], (M.ring_inner * 1.3 * 2.0) as f32);
    }

    #[test]
    fn pulse_uses_opacity_only() {
        let g = geo();
        let dim = uniforms_for(
            StripKind::Pulse,
            g,
            &phase(0.0, false, None),
            &BLUE,
            &ORANGE,
        );
        let bright = uniforms_for(
            StripKind::Pulse,
            g,
            &phase(1.0, false, None),
            &BLUE,
            &ORANGE,
        );
        assert_eq!(dim.size_mode[2], 2.0);
        assert!((dim.a_color[3] - 0.2).abs() < 1e-6);
        assert!((bright.a_color[3] - 1.0).abs() < 1e-6);
        assert_eq!(dim.a[2], (M.pulse_diameter / 2.0 * 2.0) as f32);
        assert_eq!(dim.b_color[3], 0.0);
    }

    #[test]
    fn zoom_scales_the_shapes() {
        let mut g = geo();
        g.zoom = 2.0;
        let landed = uniforms_for(StripKind::Ball, g, &phase(0.0, false, None), &BLUE, &ORANGE);
        assert_eq!(landed.a[2], (M.ball_diameter * 2.0 / 2.0 * 2.0) as f32);
        assert_eq!(landed.b[2], (M.ground_width * 2.0 / 2.0 * 2.0) as f32);
        let dot = uniforms_for(
            StripKind::Pulse,
            g,
            &phase(0.0, false, None),
            &BLUE,
            &ORANGE,
        );
        assert_eq!(dot.a[2], (M.pulse_diameter * 2.0 / 2.0 * 2.0) as f32);
    }

    #[test]
    fn summary_reports_offsets_skips_and_nearest_frames() {
        let t0 = HostTime::from_nanos(1_000_000_000_000);
        let ms = |m: u64| Duration::from_millis(m);
        let records = vec![
            PresentRecord {
                frame: 0,
                target: t0,
                presented: Some(t0),
            },
            PresentRecord {
                frame: 1,
                target: t0 + ms(16),
                presented: Some(t0 + ms(33)),
            },
            PresentRecord {
                frame: 2,
                target: t0 + ms(33),
                presented: None,
            },
            PresentRecord {
                frame: 3,
                target: t0 + ms(50),
                presented: Some(t0 + ms(50)),
            },
        ];
        let s = summarize(&records, 1, &[0.2, 0.1, 0.3], &[t0 + ms(40), t0 + ms(500)]);
        assert_eq!(s.reported, 3);
        assert_eq!(s.skipped, 2);
        let (mid, lo, hi) = s.offset_ms.unwrap();
        assert_eq!((mid, lo, hi), (0.0, 0.0, 17.0));
        // 離 40 ms 最近的實際上屏是 33 ms（−7）與 50 ms（+10）之中的 −7。
        assert_eq!(s.nearest_presented_ms, vec![Some(-7.0), Some(-450.0)]);
        assert_eq!(s.cost_ms, Some((0.2, 0.3)));
        let (first, second) = s.lines();
        assert!(
            first.contains("3 格有回報") && first.contains("跳過 2 格"),
            "{first}"
        );
        assert!(second.contains("第 0 拍最近實際上屏差 -7.0 ms"), "{second}");

        let empty = summarize(&[], 0, &[], &[t0]);
        assert_eq!(empty.offset_ms, None);
        assert_eq!(empty.nearest_presented_ms, vec![None]);
        assert!(empty.lines().0.contains("沒有任何一格"));
    }
}
