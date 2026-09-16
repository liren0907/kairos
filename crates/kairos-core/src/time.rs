//! 單調時鐘。全程式只用 `mach_absolute_time` 當時間基準，因為
//! `CADisplayLink.targetTimestamp`、`NSEvent.timestamp`、CoreAudio 的 `mHostTime`
//! 與 `SCM_TIMESTAMP_MONOTONIC` 都以它為基準，程式內不需要任何時鐘換算。
//!
//! 單位是 mach tick，不是奈秒；Apple Silicon 上 timebase 是 125/3（1 tick ≈ 41.67 ns）。
//! 需要奈秒或 `Duration` 時透過 [`Timebase`] 換算，換算採四捨五入。

use std::ops::{Add, Sub};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `mach_timebase_info`：tick 與奈秒的換算比。程式生命週期內不變，讀一次快取。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timebase {
    numer: u32,
    denom: u32,
}

impl Timebase {
    /// 本機的 timebase，第一次呼叫時讀取，之後回傳快取值。
    pub fn get() -> Timebase {
        static TIMEBASE: OnceLock<Timebase> = OnceLock::new();
        *TIMEBASE.get_or_init(Self::read_from_kernel)
    }

    /// 以指定的比例建立，給測試與模擬用；正式程式碼一律用 [`Timebase::get`]。
    pub const fn new(numer: u32, denom: u32) -> Timebase {
        assert!(numer > 0 && denom > 0, "timebase 的分子與分母都必須為正");
        Timebase { numer, denom }
    }

    fn read_from_kernel() -> Timebase {
        let mut info = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: 傳入合法的可寫指標。
        let rc = unsafe { mach2::mach_time::mach_timebase_info(&mut info) };
        assert_eq!(rc, 0, "mach_timebase_info 失敗");
        Timebase::new(info.numer, info.denom)
    }

    pub const fn numer(&self) -> u32 {
        self.numer
    }

    pub const fn denom(&self) -> u32 {
        self.denom
    }

    /// tick 換奈秒，四捨五入。
    pub fn ticks_to_nanos(&self, ticks: u64) -> u64 {
        let n = ticks as u128 * self.numer as u128;
        ((n + self.denom as u128 / 2) / self.denom as u128) as u64
    }

    /// 奈秒換 tick，四捨五入。
    pub fn nanos_to_ticks(&self, nanos: u64) -> u64 {
        let n = nanos as u128 * self.denom as u128;
        ((n + self.numer as u128 / 2) / self.numer as u128) as u64
    }

    pub fn ticks_to_duration(&self, ticks: u64) -> Duration {
        Duration::from_nanos(self.ticks_to_nanos(ticks))
    }

    /// 超過 u64 奈秒範圍（約 584 年）的 `Duration` 會飽和。
    pub fn duration_to_ticks(&self, d: Duration) -> u64 {
        let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.nanos_to_ticks(nanos)
    }
}

/// 主機單調時間的一個瞬間，單位是 mach tick。
///
/// 只能與 `Duration` 加減、與另一個 `HostTime` 相減；不能轉成牆上時間——
/// 那是 [`crate::model::ClockModel`] 的工作。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostTime(u64);

impl HostTime {
    /// 現在的 `mach_absolute_time`。
    pub fn now() -> HostTime {
        // SAFETY: 無參數、無副作用的系統呼叫。
        HostTime(unsafe { mach2::mach_time::mach_absolute_time() })
    }

    pub const fn from_ticks(ticks: u64) -> HostTime {
        HostTime(ticks)
    }

    pub const fn ticks(self) -> u64 {
        self.0
    }

    /// 以本機 timebase 把奈秒換成 tick。
    pub fn from_nanos(nanos: u64) -> HostTime {
        HostTime(Timebase::get().nanos_to_ticks(nanos))
    }

    /// 以本機 timebase 換成奈秒。
    pub fn as_nanos(self) -> u64 {
        Timebase::get().ticks_to_nanos(self.0)
    }

    /// `self − earlier`；`earlier` 比較晚時回傳 `None`。
    pub fn checked_duration_since(self, earlier: HostTime) -> Option<Duration> {
        self.0
            .checked_sub(earlier.0)
            .map(|t| Timebase::get().ticks_to_duration(t))
    }

    /// `self − earlier`；`earlier` 比較晚時回傳零。
    pub fn saturating_duration_since(self, earlier: HostTime) -> Duration {
        self.checked_duration_since(earlier).unwrap_or_default()
    }

    /// `self − other` 的有號奈秒差，估計器做線性代數時用。
    pub fn signed_nanos_since(self, other: HostTime) -> i128 {
        let tb = Timebase::get();
        tb.ticks_to_nanos(self.0) as i128 - tb.ticks_to_nanos(other.0) as i128
    }

    pub fn elapsed(self) -> Duration {
        HostTime::now().saturating_duration_since(self)
    }
}

/// 現在的 `mach_continuous_time`，單位同樣是 tick，但**睡眠期間也會走**。
///
/// `mach_absolute_time` 在系統睡眠時停住，所以兩者的差在一次開機內只會因睡眠而增加；
/// [`crate::sync::SleepDetector`] 靠這個差偵測「中間睡過」。程式的時間基準仍然是
/// [`HostTime`]，這個值不拿來算任何時刻。
pub fn continuous_ticks() -> u64 {
    // SAFETY: 無參數、無副作用的系統呼叫。
    unsafe { mach2::mach_time::mach_continuous_time() }
}

/// 本機系統時鐘相對單調時鐘的偏移：Unix 奈秒 − 開機起奈秒。
///
/// 用它把模型的 θ（遠端 − 單調）換成人看得懂的「遠端 − 系統時鐘」，跟 `sntp` 的慣例一樣，
/// 正值代表本機系統時鐘慢。系統時鐘會被 timed 慢慢調，所以每次要用都重讀，不要存。
pub fn system_theta_ns() -> i128 {
    let host = HostTime::now();
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    unix - host.as_nanos() as i128
}

impl Add<Duration> for HostTime {
    type Output = HostTime;

    fn add(self, d: Duration) -> HostTime {
        HostTime(self.0.saturating_add(Timebase::get().duration_to_ticks(d)))
    }
}

impl Sub<Duration> for HostTime {
    type Output = HostTime;

    fn sub(self, d: Duration) -> HostTime {
        HostTime(self.0.saturating_sub(Timebase::get().duration_to_ticks(d)))
    }
}

/// 與 `std::time::Instant` 同語意：結果為負時飽和到零。
impl Sub<HostTime> for HostTime {
    type Output = Duration;

    fn sub(self, earlier: HostTime) -> Duration {
        self.saturating_duration_since(earlier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apple Silicon 的 timebase。
    const APPLE_SILICON: Timebase = Timebase::new(125, 3);
    /// Intel Mac 的 timebase，tick 即奈秒。
    const INTEL: Timebase = Timebase::new(1, 1);

    #[test]
    fn kernel_timebase_is_sane() {
        let tb = Timebase::get();
        assert!(tb.numer() > 0 && tb.denom() > 0);
        assert_eq!(tb, Timebase::get(), "快取值應穩定");
    }

    #[test]
    fn ticks_to_nanos_rounds() {
        assert_eq!(APPLE_SILICON.ticks_to_nanos(0), 0);
        assert_eq!(APPLE_SILICON.ticks_to_nanos(3), 125);
        // 1 tick = 41.666… ns → 42
        assert_eq!(APPLE_SILICON.ticks_to_nanos(1), 42);
        assert_eq!(INTEL.ticks_to_nanos(1_234_567), 1_234_567);
    }

    #[test]
    fn nanos_to_ticks_rounds() {
        assert_eq!(APPLE_SILICON.nanos_to_ticks(125), 3);
        // 42 ns × 3 / 125 = 1.008 → 1
        assert_eq!(APPLE_SILICON.nanos_to_ticks(42), 1);
        // 20 ns × 3 / 125 = 0.48 → 0；21 ns → 0.504 → 1
        assert_eq!(APPLE_SILICON.nanos_to_ticks(20), 0);
        assert_eq!(APPLE_SILICON.nanos_to_ticks(21), 1);
    }

    #[test]
    fn ticks_roundtrip_is_exact_when_tick_is_coarser_than_nanos() {
        // tick 比奈秒粗（numer > denom）時，tick → ns → tick 必須完全還原。
        for ticks in [0u64, 1, 2, 3, 7, 1_000, 123_456_789, u64::MAX / 200] {
            let back = APPLE_SILICON.nanos_to_ticks(APPLE_SILICON.ticks_to_nanos(ticks));
            assert_eq!(back, ticks, "ticks={ticks}");
        }
    }

    #[test]
    fn nanos_roundtrip_error_is_within_half_tick() {
        // ns → tick → ns 的誤差不超過半個 tick（約 20.8 ns）。
        let half_tick_ns = (125.0 / 3.0 / 2.0f64).ceil() as i128;
        for nanos in [0u64, 1, 20, 21, 42, 999, 1_000_000_007, u64::MAX / 200] {
            let back = APPLE_SILICON.ticks_to_nanos(APPLE_SILICON.nanos_to_ticks(nanos));
            let err = (back as i128 - nanos as i128).abs();
            assert!(err <= half_tick_ns, "nanos={nanos} back={back} err={err}");
        }
    }

    #[test]
    fn large_values_do_not_overflow() {
        // u64::MAX tick × 125 需要 u128 中間值。
        let ns = APPLE_SILICON.ticks_to_nanos(u64::MAX);
        assert!(ns > u64::MAX / 2);
        let _ = APPLE_SILICON.nanos_to_ticks(u64::MAX);
    }

    #[test]
    fn host_time_ordering_and_arithmetic() {
        let a = HostTime::from_ticks(1_000);
        let b = HostTime::from_ticks(2_000);
        assert!(a < b);
        assert_eq!(
            b.checked_duration_since(a),
            Some(Timebase::get().ticks_to_duration(1_000))
        );
        assert_eq!(a.checked_duration_since(b), None);
        assert_eq!(a.saturating_duration_since(b), Duration::ZERO);
        assert_eq!(a - b, Duration::ZERO);
        assert_eq!(b.signed_nanos_since(a), -a.signed_nanos_since(b));
    }

    #[test]
    fn host_time_add_sub_duration_roundtrip() {
        let t = HostTime::from_ticks(10_000_000);
        let d = Duration::from_millis(7);
        let later = t + d;
        assert!(later > t);
        // 加再減，誤差不超過一個 tick。
        let back = later - d;
        assert!(back.ticks().abs_diff(t.ticks()) <= 1);
    }

    #[test]
    fn now_is_monotonic() {
        let a = HostTime::now();
        let b = HostTime::now();
        assert!(b >= a);
        assert!(a.elapsed() < Duration::from_secs(1));
    }
}
