//! 不可變的時鐘模型。估計器產出、呈現層讀取，層與層之間只傳這個值。
//!
//! 模型是遠端時間相對本機單調時間的線性關係：
//!
//! ```text
//! θ(m) = θ₀ + ρ · (m − m₀)        θ ＝ 遠端時間 − 本機時間
//! ```
//!
//! 呈現層每一格拿預計顯示的主機時間 `m` 呼叫 [`ClockModel::estimate_at`]，
//! 得到那一刻的遠端時間中點與半寬；半寬隨 `|m − m₀|` 增長，反映漂移的不確定。

use crate::time::HostTime;

/// 模型對齊的對象。兩種各跑一個估計器實例，不混合。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceKind {
    /// 國家標準時間伺服器。
    Standard,
    /// 使用者指定的網站伺服器認為的時間。
    Target,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelStatus {
    /// 還沒有任何樣本，估計值沒有意義。
    Uncalibrated,
    /// 有樣本但視窗還不夠長，區間偏寬。
    Converging,
    /// 正常運作中。
    Tracking,
    /// 睡眠喚醒或長時間沒有新樣本，等待重新取樣。
    Stale,
    /// 目標時刻前一分鐘凍結，不再更新。
    Frozen,
}

/// 遠端時間的線性模型，值型別、不可變。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClockModel {
    pub source: SourceKind,
    pub status: ModelStatus,
    /// 參考點 m₀。
    pub reference: HostTime,
    /// θ₀：在 m₀ 時「遠端 Unix 奈秒 − 本機奈秒」的偏移。
    pub offset_ns: i128,
    /// ρ：漂移率，秒／秒，無單位。
    pub drift: f64,
    /// 在 m₀ 的半寬（奈秒）。
    pub half_width_ns: u64,
    /// 半寬隨離開 m₀ 的時間增長的速率（奈秒／秒），是漂移不確定度的上界。
    pub half_width_growth_ns_per_s: f64,
}

/// 某個主機時刻的遠端時間估計：中點與硬性半寬。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// 遠端時間中點，Unix 紀元起的奈秒。
    pub remote_unix_ns: i128,
    /// 硬性半寬（奈秒）：真值落在 `remote_unix_ns ± half_width_ns`。
    pub half_width_ns: u64,
}

impl ClockModel {
    /// 沒有任何樣本時的模型：偏移為零、半寬為「無限大」，狀態為未校正。
    pub fn uncalibrated(source: SourceKind, now: HostTime) -> ClockModel {
        ClockModel {
            source,
            status: ModelStatus::Uncalibrated,
            reference: now,
            offset_ns: 0,
            drift: 0.0,
            half_width_ns: u64::MAX,
            half_width_growth_ns_per_s: 0.0,
        }
    }

    /// 在主機時間 `at` 的遠端時間估計。
    pub fn estimate_at(&self, at: HostTime) -> Estimate {
        let dt_ns = at.signed_nanos_since(self.reference);
        let dt_s = dt_ns as f64 / 1e9;
        let theta = self.offset_ns + (self.drift * dt_ns as f64) as i128;
        let remote_unix_ns = at.as_nanos() as i128 + theta;

        let growth = (self.half_width_growth_ns_per_s * dt_s.abs()) as u64;
        let half_width_ns = self.half_width_ns.saturating_add(growth);

        Estimate {
            remote_unix_ns,
            half_width_ns,
        }
    }

    pub fn is_usable(&self) -> bool {
        !matches!(self.status, ModelStatus::Uncalibrated) && self.half_width_ns != u64::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tracking(reference: HostTime) -> ClockModel {
        ClockModel {
            source: SourceKind::Standard,
            status: ModelStatus::Tracking,
            reference,
            offset_ns: 1_700_000_000_000_000_000, // 約 2023-11
            drift: 20e-6,                         // 20 ppm
            half_width_ns: 5_000_000,             // ±5 ms
            half_width_growth_ns_per_s: 50_000.0, // 每秒多 50 µs
        }
    }

    #[test]
    fn uncalibrated_is_not_usable() {
        let m = ClockModel::uncalibrated(SourceKind::Standard, HostTime::from_ticks(0));
        assert!(!m.is_usable());
        assert_eq!(
            m.estimate_at(HostTime::from_ticks(123)).half_width_ns,
            u64::MAX
        );
    }

    #[test]
    fn estimate_at_reference_is_offset_plus_host() {
        let m0 = HostTime::from_ticks(1_000_000);
        let m = tracking(m0);
        let e = m.estimate_at(m0);
        assert_eq!(e.remote_unix_ns, m0.as_nanos() as i128 + m.offset_ns);
        assert_eq!(e.half_width_ns, m.half_width_ns);
    }

    #[test]
    fn drift_and_growth_apply_with_elapsed_time() {
        let m0 = HostTime::from_ticks(1_000_000);
        let m = tracking(m0);
        let later = m0 + Duration::from_secs(10);
        let e = m.estimate_at(later);

        let dt_ns = later.signed_nanos_since(m0);
        // 20 ppm × 10 s = 200 µs 的額外偏移。
        let expected_theta = m.offset_ns + (20e-6 * dt_ns as f64) as i128;
        assert_eq!(e.remote_unix_ns, later.as_nanos() as i128 + expected_theta);
        // 5 ms + 10 s × 50 µs/s = 5.5 ms；允許 tick 換算的微小誤差。
        assert!((e.half_width_ns as i128 - 5_500_000).abs() < 1_000);
    }

    #[test]
    fn growth_is_symmetric_in_time() {
        let m0 = HostTime::from_ticks(10_000_000_000);
        let m = tracking(m0);
        let before = m.estimate_at(m0 - Duration::from_secs(3));
        let after = m.estimate_at(m0 + Duration::from_secs(3));
        assert!((before.half_width_ns as i128 - after.half_width_ns as i128).abs() < 1_000);
    }
}
