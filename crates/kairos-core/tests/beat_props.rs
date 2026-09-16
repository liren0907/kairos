//! 拍點的性質：反解主機時刻與正向估計互為反函數；聲音渲染跟緩衝區怎麼切無關；
//! 相位在起跑到歸零之間永遠合法。

use std::time::Duration;

use proptest::prelude::*;

use kairos_core::beat::BeatPlan;
use kairos_core::beat::audio::{TickSound, render};
use kairos_core::model::{ClockModel, ModelStatus, SourceKind};
use kairos_core::time::HostTime;

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

fn model(reference_s: u64, offset_ns: i128, drift: f64) -> ClockModel {
    ClockModel {
        source: SourceKind::Standard,
        status: ModelStatus::Tracking,
        reference: HostTime::from_nanos(reference_s * 1_000_000_000),
        offset_ns,
        drift,
        half_width_ns: 5_000_000,
        half_width_growth_ns_per_s: 0.0,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]

    #[test]
    fn host_at_is_the_inverse_of_estimate_at(
        reference_s in 7_200u64..10_000_000,
        offset_ns in 1_600_000_000_000_000_000i128..1_900_000_000_000_000_000,
        drift in -500e-6f64..500e-6,
        ahead_ns in -3_600_000_000_000i128..3_600_000_000_000,
    ) {
        let m = model(reference_s, offset_ns, drift);
        let target = m.estimate_at(m.reference).remote_unix_ns + ahead_ns;
        let back = m.estimate_at(m.host_at(target)).remote_unix_ns;
        prop_assert!((back - target).abs() <= 100, "往返差 {} ns", back - target);
    }

    #[test]
    fn render_does_not_depend_on_buffer_boundaries(
        zero_offset_us in 0u64..300_000,
        ticks in 1u32..5,
        period_ms in 50u64..400,
        split in 1usize..2047,
        channels in 1usize..3,
    ) {
        let rate = 48_000.0;
        let start = HostTime::from_nanos(20 * 1_000_000_000);
        let plan = BeatPlan::new(
            start + Duration::from_micros(zero_offset_us),
            Duration::from_millis(period_ms),
            ticks,
        );
        let sound = TickSound::default();
        let frames = 2048;
        let mut whole = vec![0.0f32; frames * channels];
        render(&plan, &sound, start, rate, channels, &mut whole);
        let mut a = vec![0.0f32; split * channels];
        let mut b = vec![0.0f32; (frames - split) * channels];
        render(&plan, &sound, start, rate, channels, &mut a);
        let second = start + Duration::from_nanos((split as f64 / rate * 1e9).round() as u64);
        render(&plan, &sound, second, rate, channels, &mut b);
        for (j, (w, s)) in whole.iter().zip(a.iter().chain(b.iter())).enumerate() {
            prop_assert!((w - s).abs() < 1e-3, "取樣點 {j}：{w} vs {s}");
        }
    }

    #[test]
    fn phase_is_well_formed_between_start_and_zero(
        ticks in 1u32..8,
        period_ms in 100u64..2_000,
        frac in 0.0f64..1.0,
    ) {
        let zero = HostTime::from_nanos(100 * 1_000_000_000);
        let plan = BeatPlan::new(zero, Duration::from_millis(period_ms), ticks);
        let span = plan.zero - plan.visual_start();
        let at = plan.visual_start() + Duration::from_secs_f64(span.as_secs_f64() * frac);
        if at >= plan.zero {
            return Ok(());
        }
        let p = plan.phase_at(at).expect("起跑到歸零之間一定有相位");
        prop_assert!(p.beat < ticks);
        prop_assert!((0.0..=1.0).contains(&p.phi));
        prop_assert!(!p.done);
        prop_assert_eq!(p.is_final, p.beat == ticks - 1);
        prop_assert_eq!(p.since_landing.is_none(), p.beat == 0);
        if let Some(s) = p.since_landing {
            prop_assert!(s <= plan.period);
        }
    }
}
