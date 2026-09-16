//! 區間估計器：把滑動視窗內的樣本交集成 (θ₀, ρ) 平面上的凸多邊形，
//! 解出當下 θ 與 ρ 的硬性上下界。
//!
//! 每筆樣本 `lo ≤ θ(m) ≤ hi`，代入 `θ(m) = θ₀ + ρ(m − m₀)` 就是 (θ₀, ρ) 上的一條帶；
//! 加上先驗 |ρ| ≤ ρmax 讓區域有界。交集為空代表有離群樣本：用最小平方擬合出共識線
//! （斜率夾在 ±ρmax），丟掉離它最遠的那筆，直到可行。
//!
//! 剔除**不改變狀態**：每次 `model()` 都從完整視窗重算。這樣早期樣本少、好壞分不出來
//! 時的誤判不會被記住；等好樣本成為多數，離群自然被踢掉。
//!
//! 數值上把 θ 減掉一個參考值、時間差用秒，讓 f64 只處理毫秒到秒等級的數字；
//! Unix 奈秒約 1.8×10¹⁸ 超過 f64 的精確整數範圍，不能直接算。

use std::cell::Cell;
use std::collections::VecDeque;
use std::time::Duration;

use crate::estimate::Estimator;
use crate::estimate::lp2::{HalfPlane, extremes, feasible_vertices};
use crate::model::{ClockModel, ModelStatus, SourceKind};
use crate::source::Sample;
use crate::time::HostTime;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EstimatorConfig {
    /// 滑動視窗長度，比這更舊的樣本丟掉。
    pub window: Duration,
    /// 視窗內最多保留幾筆，超過丟最舊的。
    pub max_samples: usize,
    /// 漂移率的先驗上界（百萬分之一）。NTP 的標準假設是 500。
    pub max_drift_ppm: f64,
    /// 至少幾筆樣本才算 Tracking，否則是 Converging。
    pub min_samples_for_tracking: usize,
    /// 樣本至少跨多長時間才算 Tracking。
    pub min_span_for_tracking: Duration,
    /// 回報半寬時額外加上的餘量，吃掉 f64 與 tick 換算的誤差。
    pub numeric_slack_ns: u64,
}

impl Default for EstimatorConfig {
    fn default() -> Self {
        EstimatorConfig {
            window: Duration::from_secs(30 * 60),
            max_samples: 160,
            max_drift_ppm: 500.0,
            min_samples_for_tracking: 4,
            min_span_for_tracking: Duration::from_secs(5 * 60),
            numeric_slack_ns: 1_000,
        }
    }
}

pub struct IntervalEstimator {
    source: SourceKind,
    config: EstimatorConfig,
    /// 依 `at` 排序，最舊在前。
    samples: VecDeque<Sample>,
    /// 最近一次 `model()` 為了可行而丟掉的樣本數。
    last_rejected: Cell<usize>,
}

/// 解出來的界，內部用。
struct Bounds {
    theta_lo: f64,
    theta_hi: f64,
    rho_lo: f64,
    rho_hi: f64,
}

impl IntervalEstimator {
    pub fn new(source: SourceKind, config: EstimatorConfig) -> IntervalEstimator {
        IntervalEstimator {
            source,
            config,
            samples: VecDeque::new(),
            last_rejected: Cell::new(0),
        }
    }

    pub fn source(&self) -> SourceKind {
        self.source
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 最近一次 `model()` 為了讓交集非空而丟掉的樣本數。
    pub fn rejected(&self) -> usize {
        self.last_rejected.get()
    }

    /// 視窗內最舊到最新樣本的時間跨度。
    pub fn span(&self) -> Duration {
        match (self.samples.front(), self.samples.back()) {
            (Some(a), Some(b)) => b.at.saturating_duration_since(a.at),
            _ => Duration::ZERO,
        }
    }

    fn rho_max_ns_per_s(&self) -> f64 {
        self.config.max_drift_ppm * 1_000.0
    }

    fn insert_sorted(&mut self, sample: Sample) {
        let pos = self.samples.partition_point(|s| s.at <= sample.at);
        self.samples.insert(pos, sample);
    }

    fn evict_old(&mut self) {
        let Some(newest) = self.samples.back().map(|s| s.at) else { return };
        let cutoff = newest - self.config.window;
        while let Some(front) = self.samples.front() {
            if front.at < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        while self.samples.len() > self.config.max_samples {
            self.samples.pop_front();
        }
    }

    /// 以 `reference` 為 m₀、`theta_ref` 為 θ 的原點，建出所有半平面。
    /// 變數：x ＝ θ₀ − theta_ref（奈秒）、y ＝ ρ（奈秒／秒）。
    fn constraints(&self, samples: &[Sample], reference: HostTime, theta_ref: i128) -> Vec<HalfPlane> {
        let mut c = Vec::with_capacity(samples.len() * 2 + 2);
        for s in samples {
            let t = s.at.signed_nanos_since(reference) as f64 / 1e9;
            let lo = (s.lo_ns - theta_ref) as f64;
            let hi = (s.hi_ns - theta_ref) as f64;
            c.push(HalfPlane::new(1.0, t, hi));
            c.push(HalfPlane::new(-1.0, -t, -lo));
        }
        let rho_max = self.rho_max_ns_per_s();
        c.push(HalfPlane::new(0.0, 1.0, rho_max));
        c.push(HalfPlane::new(0.0, -1.0, rho_max));
        c
    }

    fn solve(&self, samples: &[Sample], reference: HostTime, theta_ref: i128) -> Option<Bounds> {
        let vertices = feasible_vertices(&self.constraints(samples, reference, theta_ref));
        let (theta_lo, theta_hi) = extremes(&vertices, 1.0, 0.0)?;
        let (rho_lo, rho_hi) = extremes(&vertices, 0.0, 1.0)?;
        Some(Bounds {
            theta_lo,
            theta_hi,
            rho_lo,
            rho_hi,
        })
    }

    fn theta_ref(samples: &[Sample]) -> i128 {
        samples.last().map(|s| s.midpoint_ns()).unwrap_or(0)
    }

    /// 交集為空時呼叫：丟掉離共識線最遠的那筆。
    ///
    /// 共識線是中點的不加權最小平方擬合，斜率夾在 ±ρmax。不加權是因為
    /// 「窄但錯」的樣本不該有更大的話語權；用原始殘差而不是用各自寬度正規化，
    /// 是因為離群的定義是「離大家遠」，跟它自己報多寬無關。
    fn remove_worst_fit(&self, working: &mut Vec<Sample>) {
        let n = working.len();
        if n <= 1 {
            working.clear();
            return;
        }
        let reference = working.last().unwrap().at;
        let theta_ref = Self::theta_ref(working);

        let pts: Vec<(f64, f64)> = working
            .iter()
            .map(|s| {
                let t = s.at.signed_nanos_since(reference) as f64 / 1e9;
                let mid = (s.midpoint_ns() - theta_ref) as f64;
                (t, mid)
            })
            .collect();

        let n_f = n as f64;
        let (mut st, mut sm, mut stt, mut stm) = (0.0, 0.0, 0.0, 0.0);
        for &(t, mid) in &pts {
            st += t;
            sm += mid;
            stt += t * t;
            stm += t * mid;
        }
        let det = n_f * stt - st * st;
        let rho_max = self.rho_max_ns_per_s();
        let b = if det.abs() > 1e-12 * n_f * stt.max(1.0) {
            ((n_f * stm - st * sm) / det).clamp(-rho_max, rho_max)
        } else {
            0.0
        };
        let a = (sm - b * st) / n_f;

        let worst = pts
            .iter()
            .enumerate()
            .map(|(i, &(t, mid))| (i, (mid - (a + b * t)).abs()))
            .max_by(|x, y| x.1.total_cmp(&y.1))
            .map(|(i, _)| i)
            .unwrap();
        working.remove(worst);
    }

    /// 從完整視窗出發，丟離群樣本直到交集非空。回傳可行子集與丟掉的數量。
    fn consensus(&self) -> (Vec<Sample>, usize) {
        let mut working: Vec<Sample> = self.samples.iter().copied().collect();
        let mut rejected = 0;
        while !working.is_empty() {
            let reference = working.last().unwrap().at;
            if self.solve(&working, reference, Self::theta_ref(&working)).is_some() {
                break;
            }
            self.remove_worst_fit(&mut working);
            rejected += 1;
        }
        (working, rejected)
    }
}

impl Estimator for IntervalEstimator {
    fn push(&mut self, sample: Sample) {
        debug_assert_eq!(sample.source, self.source, "樣本來源與估計器不符");
        self.insert_sorted(sample);
        self.evict_old();
    }

    fn model(&self, now: HostTime) -> ClockModel {
        let (working, rejected) = self.consensus();
        self.last_rejected.set(rejected);
        let theta_ref = Self::theta_ref(&working);
        let Some(bounds) = self.solve(&working, now, theta_ref) else {
            return ClockModel::uncalibrated(self.source, now);
        };

        let mid = (bounds.theta_lo + bounds.theta_hi) / 2.0;
        let half = (bounds.theta_hi - bounds.theta_lo) / 2.0;
        let rho_mid = (bounds.rho_lo + bounds.rho_hi) / 2.0;
        let rho_half = (bounds.rho_hi - bounds.rho_lo) / 2.0;

        let span = match (working.first(), working.last()) {
            (Some(a), Some(b)) => b.at.saturating_duration_since(a.at),
            _ => Duration::ZERO,
        };
        let status = if working.len() < self.config.min_samples_for_tracking
            || span < self.config.min_span_for_tracking
        {
            ModelStatus::Converging
        } else {
            ModelStatus::Tracking
        };

        ClockModel {
            source: self.source,
            status,
            reference: now,
            offset_ns: theta_ref + mid.round() as i128,
            drift: rho_mid / 1e9,
            half_width_ns: (half.ceil() as u64).saturating_add(self.config.numeric_slack_ns),
            half_width_growth_ns_per_s: rho_half,
        }
    }

    fn invalidate(&mut self) {
        self.samples.clear();
        self.last_rejected.set(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: i128 = 1_000_000_000;
    const MS: i128 = 1_000_000;
    /// 開機後 10⁶ 秒當測試的時間原點，避開 0 附近。
    const T0_NS: u64 = 1_000_000 * 1_000_000_000;

    fn at(secs_after_t0: f64) -> HostTime {
        HostTime::from_nanos(T0_NS + (secs_after_t0 * 1e9) as u64)
    }

    /// 真實模型：θ(t) = θ₀ + ρ·t，t 為離 T0 的秒數。
    fn truth(theta0_ns: i128, rho_ns_per_s: f64, host: HostTime) -> i128 {
        let t = (host.as_nanos() as i128 - T0_NS as i128) as f64 / 1e9;
        theta0_ns + (rho_ns_per_s * t).round() as i128
    }

    fn sample_around(theta: i128, host: HostTime, below: i128, above: i128) -> Sample {
        Sample::new(SourceKind::Standard, host, theta - below, theta + above).unwrap()
    }

    fn new_estimator() -> IntervalEstimator {
        IntervalEstimator::new(SourceKind::Standard, EstimatorConfig::default())
    }

    fn assert_contains(model: &ClockModel, host: HostTime, true_theta: i128) {
        let e = model.estimate_at(host);
        let true_remote = host.as_nanos() as i128 + true_theta;
        let lo = e.remote_unix_ns - e.half_width_ns as i128;
        let hi = e.remote_unix_ns + e.half_width_ns as i128;
        assert!(
            lo <= true_remote && true_remote <= hi,
            "真值掉出區間：true={true_remote} 區間=[{lo}, {hi}]（中點 {} ± {}）",
            e.remote_unix_ns,
            e.half_width_ns
        );
    }

    #[test]
    fn empty_is_uncalibrated() {
        let est = new_estimator();
        let m = est.model(at(0.0));
        assert_eq!(m.status, ModelStatus::Uncalibrated);
        assert!(!m.is_usable());
    }

    #[test]
    fn single_sample_bounds_theta_and_drift_is_prior() {
        let mut est = new_estimator();
        let theta0 = 1_700_000_000 * NS + 12 * MS;
        let h = at(0.0);
        est.push(sample_around(theta0, h, 3 * MS, 5 * MS));

        let m = est.model(h);
        assert_eq!(m.status, ModelStatus::Converging);
        assert_contains(&m, h, theta0);
        // 半寬應約等於區間半寬 4 ms（加餘量）。
        assert!((m.half_width_ns as i128 - 4 * MS).abs() < 10_000);
        // 只有一筆樣本，漂移只有先驗界：±500 ppm。
        assert!((m.half_width_growth_ns_per_s - 500_000.0).abs() < 1.0);
        assert!(m.drift.abs() < 1e-12);
    }

    #[test]
    fn many_samples_tighten_and_recover_drift() {
        let mut est = new_estimator();
        let theta0 = 1_700_000_000 * NS;
        let rho = 30_000.0; // 30 ppm
        for i in 0..40 {
            let h = at(i as f64 * 45.0); // 30 分鐘
            let theta = truth(theta0, rho, h);
            // 非對稱延遲：下界寬 2 到 20 ms、上界寬 1 到 15 ms。
            let below = (2 + (i * 7) % 19) as i128 * MS;
            let above = (1 + (i * 11) % 15) as i128 * MS;
            est.push(sample_around(theta, h, below, above));
        }
        let now = at(40.0 * 45.0);
        let m = est.model(now);
        assert_eq!(m.status, ModelStatus::Tracking);
        assert_contains(&m, now, truth(theta0, rho, now));
        // 漂移應被夾在真值附近，遠比先驗 500 ppm 窄。
        assert!(m.half_width_growth_ns_per_s < 100_000.0, "growth={}", m.half_width_growth_ns_per_s);
        let drift_ppm = m.drift * 1e6;
        assert!((drift_ppm - 30.0).abs() < m.half_width_growth_ns_per_s / 1_000.0 + 0.1);
        // 往前 60 秒的預測也要含真值。
        let ahead = now + Duration::from_secs(60);
        assert_contains(&m, ahead, truth(theta0, rho, ahead));
    }

    #[test]
    fn outliers_are_rejected() {
        let mut est = new_estimator();
        let theta0 = 1_700_000_000 * NS;
        for i in 0..20 {
            let h = at(i as f64 * 30.0);
            let theta = truth(theta0, 0.0, h);
            est.push(sample_around(theta, h, 5 * MS, 5 * MS));
            if i % 7 == 3 {
                // 離群：偏 +5 秒。
                est.push(sample_around(theta + 5 * NS, h + Duration::from_millis(500), 5 * MS, 5 * MS));
            }
        }
        let now = at(20.0 * 30.0);
        let m = est.model(now);
        assert_eq!(est.rejected(), 3);
        assert_contains(&m, now, theta0);
        assert!(m.half_width_ns < 10 * MS as u64);
    }

    #[test]
    fn persistent_bad_server_is_rejected() {
        // 兩台好伺服器 ±2 ms，一台壞伺服器固定 +30 ms 也 ±2 ms。
        let mut est = new_estimator();
        let theta0 = 1_700_000_000 * NS;
        for i in 0..30 {
            let h = at(i as f64 * 20.0);
            let theta = truth(theta0, 10_000.0, h);
            match i % 3 {
                0 | 1 => est.push(sample_around(theta, h, 2 * MS, 2 * MS)),
                _ => est.push(sample_around(theta + 30 * MS, h, 2 * MS, 2 * MS)),
            }
        }
        let now = at(30.0 * 20.0);
        let m = est.model(now);
        assert_contains(&m, now, truth(theta0, 10_000.0, now));
        // 壞伺服器的 +30 ms 不該在區間裡。
        let e = m.estimate_at(now);
        let bad = now.as_nanos() as i128 + truth(theta0, 10_000.0, now) + 30 * MS;
        assert!(bad > e.remote_unix_ns + e.half_width_ns as i128);
        assert!(est.rejected() >= 8, "rejected={}", est.rejected());
    }

    #[test]
    fn window_evicts_old_samples() {
        let mut est = IntervalEstimator::new(
            SourceKind::Standard,
            EstimatorConfig {
                window: Duration::from_secs(100),
                ..EstimatorConfig::default()
            },
        );
        let theta0 = 1_700_000_000 * NS;
        for i in 0..10 {
            est.push(sample_around(theta0, at(i as f64 * 30.0), MS, MS));
        }
        // 最新在 270 s，視窗 100 s → 只留 180、210、240、270 四筆。
        assert_eq!(est.len(), 4);
        assert_eq!(est.span(), Duration::from_secs(90));
    }

    #[test]
    fn invalidate_clears() {
        let mut est = new_estimator();
        est.push(sample_around(1_700_000_000 * NS, at(0.0), MS, MS));
        est.invalidate();
        assert!(est.is_empty());
        assert_eq!(est.model(at(1.0)).status, ModelStatus::Uncalibrated);
    }
}
