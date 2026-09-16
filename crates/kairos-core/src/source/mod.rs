//! 時間來源層。每種來源（NTP、指定網站的 Date 標頭）最終都產出同一種東西：
//! 偏移 θ 的硬性上下界，也就是 [`Sample`]。
//!
//! 來源不做任何統計，只誠實地把一次量測能推出的區間交出去。區間怎麼交集、
//! 離群怎麼剔除，是估計器的事。
//!
//! NTP 一次交換的推導：請求在本機 t₁ 送出、遠端 t₂ 收到；回應在遠端 t₃ 送出、
//! 本機 t₄ 收到。單程延遲非負，所以 θ ∈ [t₃ − t₄, t₂ − t₁]，寬度等於往返延遲
//! 減去遠端處理時間。t₄ 取核心接收時間戳記（與 `mach_absolute_time` 同基準，
//! 已由 spike 驗證）；t₁ 只能在 `sendto` 前於使用者空間讀，區間因此略寬，但仍安全。

pub mod client;
pub mod ntp;
pub mod udp;

use crate::model::SourceKind;
use crate::time::HostTime;

/// 一筆量測：在本機時間 `at` 附近，θ 落在 `[lo_ns, hi_ns]`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub source: SourceKind,
    /// 量測的本機時間，取送出與收到的中點。
    pub at: HostTime,
    /// θ 的下界（奈秒）。
    pub lo_ns: i128,
    /// θ 的上界（奈秒）。
    pub hi_ns: i128,
}

impl Sample {
    /// 建構時保證 `lo ≤ hi`，否則這筆量測本身就矛盾，回傳 `None`。
    pub fn new(source: SourceKind, at: HostTime, lo_ns: i128, hi_ns: i128) -> Option<Sample> {
        (lo_ns <= hi_ns).then_some(Sample {
            source,
            at,
            lo_ns,
            hi_ns,
        })
    }

    pub fn midpoint_ns(&self) -> i128 {
        (self.lo_ns + self.hi_ns) / 2
    }

    pub fn half_width_ns(&self) -> u64 {
        ((self.hi_ns - self.lo_ns) / 2) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_inverted_interval() {
        let at = HostTime::from_ticks(0);
        assert!(Sample::new(SourceKind::Standard, at, 10, 5).is_none());
        assert!(Sample::new(SourceKind::Standard, at, 5, 5).is_some());
    }

    #[test]
    fn midpoint_and_half_width() {
        let s = Sample::new(SourceKind::Standard, HostTime::from_ticks(0), -4, 10).unwrap();
        assert_eq!(s.midpoint_ns(), 3);
        assert_eq!(s.half_width_ns(), 7);
    }
}
