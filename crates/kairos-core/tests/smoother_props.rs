//! 平滑器的性質：不管模型怎麼換、格與格間隔多長，回報的區間永遠包住模型的區間，
//! 而且在模型區間內移動時速率不超過上限。

use std::time::Duration;

use proptest::prelude::*;

use kairos_core::display::{DisplaySmoother, SmootherConfig};
use kairos_core::model::{ClockModel, ModelStatus, SourceKind};
use kairos_core::time::HostTime;

const T0_NS: u64 = 1_000_000 * 1_000_000_000;

fn model(reference: HostTime, theta_ns: i128, half_ns: u64, usable: bool) -> ClockModel {
    if usable {
        ClockModel {
            source: SourceKind::Standard,
            status: ModelStatus::Tracking,
            reference,
            offset_ns: theta_ns,
            drift: 0.0,
            half_width_ns: half_ns,
            half_width_growth_ns_per_s: 0.0,
        }
    } else {
        ClockModel::uncalibrated(SourceKind::Standard, reference)
    }
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]

    #[test]
    fn shown_interval_contains_model_interval(
        theta0 in -2_000_000_000i128..2_000_000_000,
        steps in proptest::collection::vec(
            (-100_000_000i128..100_000_000, 0u64..200_000_000, 1_000u64..2_000_000_000, 0u8..10),
            1..60,
        ),
    ) {
        let mut s = DisplaySmoother::new(SmootherConfig::default());
        let mut at = HostTime::from_nanos(T0_NS);
        let mut theta = 1_700_000_000i128 * 1_000_000_000 + theta0;
        let mut prev: Option<(i128, HostTime)> = None;
        for (delta, half, dt_ns, unusable_roll) in steps {
            theta += delta;
            at = at + Duration::from_nanos(dt_ns);
            let m = model(at, theta, half, unusable_roll != 0);
            match s.tick(&m, at) {
                None => {
                    prop_assert!(!m.is_usable());
                    prev = None;
                }
                Some(shown) => {
                    let e = m.estimate_at(at);
                    let lo = shown.remote_unix_ns - shown.half_width_ns as i128;
                    let hi = shown.remote_unix_ns + shown.half_width_ns as i128;
                    prop_assert!(lo <= e.remote_unix_ns - e.half_width_ns as i128);
                    prop_assert!(e.remote_unix_ns + e.half_width_ns as i128 <= hi);

                    let shown_theta = shown.remote_unix_ns - at.as_nanos() as i128;
                    if let Some((p_theta, p_at)) = prev {
                        // 目標在上一格顯示值的半寬內 ⇒ 這一格只能以上限速率移動。
                        let target = e.remote_unix_ns - at.as_nanos() as i128;
                        if (target - p_theta).unsigned_abs() <= e.half_width_ns as u128 {
                            let dt = at.saturating_duration_since(p_at).as_secs_f64();
                            let max_step = (10_000_000.0 * dt) as i128 + 1_000;
                            prop_assert!((shown_theta - p_theta).abs() <= max_step);
                        }
                    }
                    prev = Some((shown_theta, at));
                }
            }
        }
    }
}
