//! 呈現層要用、但不碰任何框架的純邏輯：顯示用偏移的平滑器，以及把遠端 Unix 時間
//! 拆成本地時分秒。
//!
//! 平滑器解決的是「模型每幾分鐘更新一次，數字不能跳」：顯示用的偏移以有上限的速率
//! 追向新模型的中點，追的期間把「顯示值離中點的殘差」加進回報的半寬，區間仍然誠實。

use crate::model::ClockModel;
use crate::time::HostTime;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SmootherConfig {
    /// 顯示用偏移每秒最多移動多少奈秒。10 ms/s 代表追的期間時間走速是 1 ± 1%，
    /// 人看不出來；模型最大漂移 500 ppm 遠在這個速率之內。
    pub max_slew_ns_per_s: f64,
}

impl Default for SmootherConfig {
    fn default() -> Self {
        SmootherConfig {
            max_slew_ns_per_s: 10_000_000.0,
        }
    }
}

/// 某一格要畫的遠端時間：中點與硬性半寬（已含平滑殘差）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shown {
    pub remote_unix_ns: i128,
    pub half_width_ns: u64,
    /// 顯示值還在往模型中點追。
    pub slewing: bool,
}

/// 顯示用偏移的平滑器。每一格呼叫一次 [`tick`](Self::tick)。
///
/// 規則只有三條：模型不可用就歸零；目標離目前顯示值超過模型半寬（顯示值本來就在
/// 區間外，例如第一筆模型或喚醒後）就直接跳過去；否則以上限速率追。
#[derive(Clone, Debug)]
pub struct DisplaySmoother {
    config: SmootherConfig,
    /// 目前顯示的 θ（遠端 − 主機，奈秒）。
    shown_theta_ns: Option<i128>,
    last_at: Option<HostTime>,
}

impl DisplaySmoother {
    pub fn new(config: SmootherConfig) -> DisplaySmoother {
        DisplaySmoother {
            config,
            shown_theta_ns: None,
            last_at: None,
        }
    }

    pub fn reset(&mut self) {
        self.shown_theta_ns = None;
        self.last_at = None;
    }

    /// 以模型算出主機時刻 `at` 要顯示的值。模型不可用時回傳 `None` 並歸零。
    pub fn tick(&mut self, model: &ClockModel, at: HostTime) -> Option<Shown> {
        if !model.is_usable() {
            self.reset();
            return None;
        }
        let est = model.estimate_at(at);
        let target = est.remote_unix_ns - at.as_nanos() as i128;

        let shown = match (self.shown_theta_ns, self.last_at) {
            (Some(prev), Some(last))
                if (target - prev).unsigned_abs() <= est.half_width_ns as u128 =>
            {
                let dt = at.saturating_duration_since(last).as_secs_f64();
                let max_step = (self.config.max_slew_ns_per_s * dt) as i128;
                prev + (target - prev).clamp(-max_step, max_step)
            }
            _ => target,
        };
        self.shown_theta_ns = Some(shown);
        self.last_at = Some(at);

        // 殘差 ≤ 半寬（否則上面會直接跳），所以回報的區間一定包住模型的區間。
        let residual = (target - shown).unsigned_abs() as u64;
        Some(Shown {
            remote_unix_ns: at.as_nanos() as i128 + shown,
            half_width_ns: est.half_width_ns.saturating_add(residual),
            slewing: residual != 0,
        })
    }
}

/// 本地牆上時間的一刻，只到毫秒。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WallClock {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub millis: u16,
}

/// 面板顯示的固定樣式 `HH:MM:SS.mmm`，十二格。
pub const DIGIT_CELLS: usize = 12;
/// 模型不可用時顯示的字樣。
pub const DASHES: [u8; DIGIT_CELLS] = *b"--:--:--.---";

impl WallClock {
    /// `HH:MM:SS.mmm` 的十二個 ASCII 字元。
    pub fn digits(&self) -> [u8; DIGIT_CELLS] {
        let d = |v: u16, div: u16| b'0' + ((v / div) % 10) as u8;
        [
            d(self.hour as u16, 10),
            d(self.hour as u16, 1),
            b':',
            d(self.minute as u16, 10),
            d(self.minute as u16, 1),
            b':',
            d(self.second as u16, 10),
            d(self.second as u16, 1),
            b'.',
            d(self.millis, 100),
            d(self.millis, 10),
            d(self.millis, 1),
        ]
    }
}

/// 把 Unix 奈秒加上時區偏移拆成本地時分秒毫秒。純函數，給測試與 [`LocalTime`] 用。
pub fn split_local(unix_ns: i128, gmtoff_secs: i64) -> WallClock {
    const NS: i128 = 1_000_000_000;
    let secs = unix_ns.div_euclid(NS) + gmtoff_secs as i128;
    let sub_ns = unix_ns.rem_euclid(NS);
    let of_day = secs.rem_euclid(86_400) as u32;
    WallClock {
        hour: (of_day / 3_600) as u8,
        minute: ((of_day / 60) % 60) as u8,
        second: (of_day % 60) as u8,
        millis: (sub_ns / 1_000_000) as u16,
    }
}

/// 本地時區的換算，時區偏移每分鐘重查一次（日光節約的切換都在整點，一分鐘內會跟上）。
#[derive(Clone, Debug)]
pub struct LocalTime {
    cached_minute: Option<i64>,
    gmtoff_secs: i64,
}

impl Default for LocalTime {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalTime {
    pub fn new() -> LocalTime {
        // libc crate 沒有收錄 tzset，自己宣告。
        unsafe extern "C" {
            fn tzset();
        }
        // SAFETY: 無參數的 C 函數，讀環境變數 TZ 與 /etc/localtime。
        unsafe { tzset() };
        LocalTime {
            cached_minute: None,
            gmtoff_secs: 0,
        }
    }

    pub fn wall(&mut self, unix_ns: i128) -> WallClock {
        let secs = unix_ns.div_euclid(1_000_000_000) as i64;
        let minute = secs.div_euclid(60);
        if self.cached_minute != Some(minute) {
            self.gmtoff_secs = gmtoff_for(secs);
            self.cached_minute = Some(minute);
        }
        split_local(unix_ns, self.gmtoff_secs)
    }

    pub fn gmtoff_secs(&self) -> i64 {
        self.gmtoff_secs
    }
}

fn gmtoff_for(unix_secs: i64) -> i64 {
    let t: libc::time_t = unix_secs as libc::time_t;
    // SAFETY: `tm` 全零是合法的初始值；`localtime_r` 只寫入我們提供的結構。
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::localtime_r(&t, &mut tm) };
    if r.is_null() { 0 } else { tm.tm_gmtoff as i64 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelStatus, SourceKind};
    use std::time::Duration;

    const MS: i128 = 1_000_000;

    fn model(reference: HostTime, theta_ns: i128, half_ms: u64) -> ClockModel {
        ClockModel {
            source: SourceKind::Standard,
            status: ModelStatus::Tracking,
            reference,
            offset_ns: theta_ns,
            drift: 0.0,
            half_width_ns: half_ms * 1_000_000,
            half_width_growth_ns_per_s: 0.0,
        }
    }

    fn contains(shown: &Shown, model: &ClockModel, at: HostTime) -> bool {
        let e = model.estimate_at(at);
        let lo = shown.remote_unix_ns - shown.half_width_ns as i128;
        let hi = shown.remote_unix_ns + shown.half_width_ns as i128;
        lo <= e.remote_unix_ns - e.half_width_ns as i128
            && e.remote_unix_ns + e.half_width_ns as i128 <= hi
    }

    #[test]
    fn first_tick_shows_the_model_midpoint() {
        let t0 = HostTime::from_nanos(1_000_000_000_000);
        let m = model(t0, 1_700_000_000 * 1_000 * MS, 8);
        let mut s = DisplaySmoother::new(SmootherConfig::default());
        let shown = s.tick(&m, t0).unwrap();
        assert_eq!(shown.remote_unix_ns, m.estimate_at(t0).remote_unix_ns);
        assert_eq!(shown.half_width_ns, m.half_width_ns);
        assert!(!shown.slewing);
    }

    #[test]
    fn small_change_is_slewed_at_bounded_rate_and_stays_honest() {
        let t0 = HostTime::from_nanos(1_000_000_000_000);
        let theta = 1_700_000_000 * 1_000 * MS;
        let a = model(t0, theta, 10);
        let b = model(t0, theta + 5 * MS, 10);
        let mut s = DisplaySmoother::new(SmootherConfig::default());
        s.tick(&a, t0).unwrap();

        let frame = Duration::from_micros(16_667);
        let mut at = t0;
        let mut prev = theta;
        let mut converged_at = None;
        for i in 1..=120 {
            at = at + frame;
            let shown = s.tick(&b, at).unwrap();
            let now_theta = shown.remote_unix_ns - at.as_nanos() as i128;
            let step = (now_theta - prev).abs();
            // 10 ms/s × 16.667 ms ≈ 166.7 µs，容許 tick 換算誤差。
            assert!(step <= 167_000, "第 {i} 格移動了 {step} ns");
            assert!(contains(&shown, &b, at), "第 {i} 格區間不含模型區間");
            prev = now_theta;
            if !shown.slewing && converged_at.is_none() {
                converged_at = Some(i);
            }
        }
        // 5 ms 以 10 ms/s 追，約半秒（30 格）收斂。
        let n = converged_at.expect("兩秒內應收斂");
        assert!((28..=34).contains(&n), "在第 {n} 格收斂");
    }

    #[test]
    fn large_change_jumps_immediately() {
        let t0 = HostTime::from_nanos(1_000_000_000_000);
        let theta = 1_700_000_000 * 1_000 * MS;
        let a = model(t0, theta, 8);
        let b = model(t0, theta + 50 * MS, 8);
        let mut s = DisplaySmoother::new(SmootherConfig::default());
        s.tick(&a, t0).unwrap();
        let shown = s.tick(&b, t0 + Duration::from_millis(16)).unwrap();
        assert!(!shown.slewing);
        assert_eq!(shown.half_width_ns, b.half_width_ns);
    }

    #[test]
    fn unusable_model_resets_and_next_usable_jumps() {
        let t0 = HostTime::from_nanos(1_000_000_000_000);
        let theta = 1_700_000_000 * 1_000 * MS;
        let mut s = DisplaySmoother::new(SmootherConfig::default());
        s.tick(&model(t0, theta, 8), t0).unwrap();
        assert!(
            s.tick(&ClockModel::uncalibrated(SourceKind::Standard, t0), t0)
                .is_none()
        );
        let shown = s
            .tick(
                &model(t0, theta + 3 * MS, 8),
                t0 + Duration::from_millis(16),
            )
            .unwrap();
        assert!(!shown.slewing);
    }

    #[test]
    fn split_local_handles_offsets_and_rounding() {
        assert_eq!(
            split_local(0, 8 * 3_600),
            WallClock {
                hour: 8,
                minute: 0,
                second: 0,
                millis: 0
            }
        );
        // 1_700_000_000 = 2023-11-14 22:13:20 UTC
        let w = split_local(1_700_000_000 * 1_000_000_000 + 123_456_789, 0);
        assert_eq!(
            w,
            WallClock {
                hour: 22,
                minute: 13,
                second: 20,
                millis: 123
            }
        );
        assert_eq!(&w.digits(), b"22:13:20.123");
        // 負時區跨日。
        assert_eq!(
            split_local(1_700_000_000 * 1_000_000_000, -23 * 3_600).hour,
            23
        );
        assert_eq!(std::str::from_utf8(&DASHES).unwrap(), "--:--:--.---");
    }

    #[test]
    fn local_time_agrees_with_libc() {
        let mut lt = LocalTime::new();
        let unix_ns: i128 = 1_700_000_000 * 1_000_000_000 + 5 * 1_000_000;
        let w = lt.wall(unix_ns);
        let t: libc::time_t = 1_700_000_000;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&t, &mut tm) };
        assert_eq!(
            (w.hour, w.minute, w.second, w.millis),
            (tm.tm_hour as u8, tm.tm_min as u8, tm.tm_sec as u8, 5)
        );
    }
}
