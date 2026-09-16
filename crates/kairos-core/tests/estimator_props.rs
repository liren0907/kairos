//! 估計器的性質測試：隨機的真實偏移、漂移、非對稱延遲與離群樣本，
//! 真值在視窗內任何時刻（以及往前一段）都必須落在回報的區間內。
//!
//! 這條性質成立，「不確定度要誠實」才成立。

use std::time::Duration;

use proptest::prelude::*;

use kairos_core::estimate::Estimator;
use kairos_core::estimate::interval::{EstimatorConfig, IntervalEstimator};
use kairos_core::model::{ClockModel, SourceKind};
use kairos_core::source::Sample;
use kairos_core::time::HostTime;

const NS: i128 = 1_000_000_000;
/// 開機後 10⁶ 秒當時間原點。
const T0_NS: u64 = 1_000_000 * 1_000_000_000;
/// 視窗長度（秒），與 `EstimatorConfig::default` 一致。
const WINDOW_S: f64 = 1800.0;

fn host_at(secs: f64) -> HostTime {
    HostTime::from_nanos(T0_NS + (secs * 1e9) as u64)
}

/// θ(t) = θ₀ + ρ·t，t 用 `HostTime` 換回的奈秒算，與估計器的內部一致。
fn truth(theta0: i128, rho_ns_per_s: f64, host: HostTime) -> i128 {
    let t = (host.as_nanos() as i128 - T0_NS as i128) as f64 / 1e9;
    theta0 + (rho_ns_per_s * t).round() as i128
}

fn assert_contains(
    model: &ClockModel,
    host: HostTime,
    true_theta: i128,
    what: &str,
) -> Result<(), TestCaseError> {
    let e = model.estimate_at(host);
    let true_remote = host.as_nanos() as i128 + true_theta;
    let lo = e.remote_unix_ns - e.half_width_ns as i128;
    let hi = e.remote_unix_ns + e.half_width_ns as i128;
    prop_assert!(
        lo <= true_remote && true_remote <= hi,
        "{what}：真值掉出區間。true−mid = {} ns，half = {} ns",
        true_remote - e.remote_unix_ns,
        e.half_width_ns
    );
    Ok(())
}

#[derive(Debug, Clone)]
struct Scenario {
    theta0: i128,
    rho_ns_per_s: f64,
    /// (時間秒, 下界寬 ns, 上界寬 ns, 離群偏移 ns 或 0)
    samples: Vec<(f64, i128, i128, i128)>,
}

fn scenario() -> impl Strategy<Value = Scenario> {
    let theta0 = (-2_000i128..2_000).prop_map(|ms| 1_700_000_000 * NS + ms * 1_000_000);
    let rho = -400_000.0f64..400_000.0; // ±400 ppm，在先驗 500 ppm 之內
    let n = 1usize..=40;

    (theta0, rho, n).prop_flat_map(|(theta0, rho, n)| {
        let times = proptest::collection::vec(0.0f64..WINDOW_S, n);
        let widths = proptest::collection::vec((0i128..100_000_000, 0i128..100_000_000), n);
        // 離群：n ≥ 4 時最多 n/4 筆，偏移 ±2 到 ±10 秒。
        let max_outliers = if n >= 4 { n / 4 } else { 0 };
        let outliers = proptest::collection::vec(
            (any::<bool>(), 2i128..10).prop_map(|(neg, s)| if neg { -s * NS } else { s * NS }),
            0..=max_outliers,
        );
        (times, widths, outliers).prop_map(move |(mut times, widths, outliers)| {
            times.sort_by(|a, b| a.total_cmp(b));
            let mut samples: Vec<(f64, i128, i128, i128)> = times
                .into_iter()
                .zip(widths)
                .map(|(t, (below, above))| (t, below, above, 0))
                .collect();
            // 離群平均散在序列裡。
            let len = samples.len();
            for (k, shift) in outliers.iter().enumerate() {
                let idx = (k * len) / outliers.len().max(1);
                samples[idx.min(len - 1)].3 = *shift;
            }
            Scenario {
                theta0,
                rho_ns_per_s: rho,
                samples,
            }
        })
    })
}

/// 預設 300 筆；`PROPTEST_CASES=2000 cargo test --release` 可以加壓。
fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases(),
        ..ProptestConfig::default()
    })]

    #[test]
    fn truth_always_inside_reported_interval(sc in scenario()) {
        let mut est = IntervalEstimator::new(SourceKind::Standard, EstimatorConfig::default());
        let mut hosts = Vec::with_capacity(sc.samples.len());
        for &(t, below, above, shift) in &sc.samples {
            let h = host_at(t);
            let theta = truth(sc.theta0, sc.rho_ns_per_s, h) + shift;
            let s = Sample::new(SourceKind::Standard, h, theta - below, theta + above).unwrap();
            est.push(s);
            hosts.push(h);
        }

        let now = *hosts.last().unwrap();
        let model = est.model(now);
        prop_assert!(model.is_usable());

        // 視窗內每個樣本時刻。
        for h in &hosts {
            assert_contains(&model, *h, truth(sc.theta0, sc.rho_ns_per_s, *h), "樣本時刻")?;
        }
        // 現在、往前 60 秒、往前 10 分鐘。
        for ahead in [0u64, 60, 600] {
            let h = now + Duration::from_secs(ahead);
            assert_contains(&model, h, truth(sc.theta0, sc.rho_ns_per_s, h), "往前預測")?;
        }
        // 用另一個參考時刻再解一次，結果一樣要含真值。
        let other = host_at(WINDOW_S / 2.0);
        let model2 = est.model(other);
        assert_contains(&model2, other, truth(sc.theta0, sc.rho_ns_per_s, other), "換參考點")?;
    }

    #[test]
    fn no_outliers_means_no_rejections(
        theta0 in (-2_000i128..2_000).prop_map(|ms| 1_700_000_000 * NS + ms * 1_000_000),
        rho in -400_000.0f64..400_000.0,
        raw in proptest::collection::vec((0.0f64..WINDOW_S, 0i128..50_000_000, 0i128..50_000_000), 1..30),
    ) {
        let mut est = IntervalEstimator::new(SourceKind::Standard, EstimatorConfig::default());
        for (t, below, above) in raw {
            let h = host_at(t);
            let theta = truth(theta0, rho, h);
            est.push(Sample::new(SourceKind::Standard, h, theta - below, theta + above).unwrap());
        }
        // 所有樣本都來自同一條真實直線，交集一定非空，不該丟任何一筆。
        prop_assert_eq!(est.rejected(), 0);
    }
}
