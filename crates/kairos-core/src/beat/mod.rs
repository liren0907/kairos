//! 拍點：把「標準時間的某一刻」換成以主機時間表示的拍點表，畫面、聲音、光暈共用同一份，
//! 三者天生對齊，因為根本沒有第二份時間。
//!
//! 一份 [`BeatPlan`] 的骨幹是三個數：歸零時刻、拍距、拍數。拍點在 `zero − k·period`；
//! 畫面比第一聲早 `lead_beats` 個拍距起跑（至少一拍，球要有整段軌跡才落地；想更早就多幾拍
//! 無聲的落地），歸零後留一段收尾期 `tail`，讓落地的閃光與光暈衰減完。
//!
//! 子模組 [`audio`] 是滴答聲的取樣渲染，[`visual`] 是節拍元件的幾何函數；
//! 兩者都是純函數，不碰任何框架。

pub mod audio;
pub mod visual;

use std::time::Duration;

use crate::model::ClockModel;
use crate::time::HostTime;

/// 預設的收尾期：閃光與光暈在這段時間內衰減完，之後 [`BeatPlan::phase_at`] 回 `None`。
pub const DEFAULT_TAIL: Duration = Duration::from_millis(1500);
/// 拍數上限：聲音渲染用 `u32` 位元遮罩回報「這個緩衝區裡有哪些拍起音」。
pub const MAX_TICKS: u32 = 32;
/// 畫面起跑最早在歸零前多少拍：一小時的秒數，再多沒有意義。
pub const MAX_LEAD_BEATS: u32 = 3600;

/// 以主機時間表示的拍點表。值型別，排好就不再變。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeatPlan {
    /// 歸零時刻：最後一拍落地。
    pub zero: HostTime,
    /// 拍距。
    pub period: Duration,
    /// 有聲的拍數，含歸零那一拍。
    pub ticks: u32,
    /// 畫面比第一聲早幾個拍距起跑，至少 1；多出來的那幾拍落地無聲。
    pub lead_beats: u32,
    /// 歸零後的收尾期。
    pub tail: Duration,
}

/// 某一刻在拍點表裡的位置。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Phase {
    /// 正在逼近（或剛落地）的拍，0 起算、含無聲拍；[`BeatPlan::total_beats`]` − 1` 是歸零拍。
    pub beat: u32,
    /// 這一拍週期內的相位：0 剛起跑、趨近 1 落地。歸零後固定為 1。
    pub phi: f64,
    /// 正在逼近的是歸零拍。
    pub is_final: bool,
    /// 距上一次落地多久；起跑後的第一個週期還沒有落地過，為 `None`。
    pub since_landing: Option<Duration>,
    /// 已經歸零，在收尾期。
    pub done: bool,
}

impl Phase {
    /// 起跑前的靜止狀態：跟第一個週期的 φ = 0 長得一樣，起跑時畫面才不會跳。
    pub fn idle(plan: &BeatPlan) -> Phase {
        Phase {
            beat: 0,
            phi: 0.0,
            is_final: plan.total_beats() == 1,
            since_landing: None,
            done: false,
        }
    }
}

impl BeatPlan {
    /// 拍距至少 1 ms，拍數夾在 1 到 [`MAX_TICKS`]；畫面提早一拍起跑、收尾 [`DEFAULT_TAIL`]。
    pub fn new(zero: HostTime, period: Duration, ticks: u32) -> BeatPlan {
        BeatPlan {
            zero,
            period: period.max(Duration::from_millis(1)),
            ticks: ticks.clamp(1, MAX_TICKS),
            lead_beats: 1,
            tail: DEFAULT_TAIL,
        }
    }

    /// 畫面比第一聲早 `lead_beats` 個拍距起跑（夾在 1 到 [`MAX_LEAD_BEATS`]）。
    pub fn with_lead(mut self, lead_beats: u32) -> BeatPlan {
        self.lead_beats = lead_beats.clamp(1, MAX_LEAD_BEATS);
        self
    }

    pub fn with_tail(mut self, tail: Duration) -> BeatPlan {
        self.tail = tail;
        self
    }

    /// 要讓畫面至少在歸零前 `lead` 起跑，第一聲前得留幾拍：往前取整到整拍，至少 1。
    /// 例：拍距 1 秒、4 拍、要 9.3 秒 → 共 10 拍 → 第一聲前 7 拍。
    pub fn lead_beats_for(lead: Duration, period: Duration, ticks: u32) -> u32 {
        let period = period.max(Duration::from_millis(1));
        let ticks = ticks.clamp(1, MAX_TICKS);
        let total = (lead.as_secs_f64() / period.as_secs_f64()).ceil();
        let total = if total.is_finite() {
            total.clamp(0.0, (MAX_LEAD_BEATS + ticks) as f64) as u32
        } else {
            0
        };
        (total + 1).saturating_sub(ticks).clamp(1, MAX_LEAD_BEATS)
    }

    /// 畫面上總共幾拍：無聲的前導拍加有聲拍（第一聲那一拍算在有聲裡）。
    pub fn total_beats(&self) -> u32 {
        self.ticks - 1 + self.lead_beats
    }

    /// 畫面起跑比歸零早多久。
    pub fn visual_lead(&self) -> Duration {
        self.period * self.total_beats()
    }

    /// 落地無聲的次數：前 `lead_beats − 1` 次落地沒有聲音。
    pub fn silent_landings(&self) -> u32 {
        self.lead_beats - 1
    }

    /// 以模型把遠端時刻反解成主機時刻當歸零點。之後模型再更新也不影響這份表。
    pub fn from_target(
        model: &ClockModel,
        target_remote_unix_ns: i128,
        period: Duration,
        ticks: u32,
    ) -> BeatPlan {
        BeatPlan::new(model.host_at(target_remote_unix_ns), period, ticks)
    }

    /// 第 `k` 拍的落地時刻；`k = ticks − 1` 就是歸零。超出範圍夾到最後一拍。
    pub fn tick_at(&self, k: u32) -> HostTime {
        let k = k.min(self.ticks - 1);
        self.zero - self.period * (self.ticks - 1 - k)
    }

    pub fn first_tick(&self) -> HostTime {
        self.tick_at(0)
    }

    /// 畫面起跑：第一聲前 `lead_beats` 個拍距。
    pub fn visual_start(&self) -> HostTime {
        self.zero - self.visual_lead()
    }

    /// 收尾期結束，整個節拍程序到此為止。
    pub fn end(&self) -> HostTime {
        self.zero + self.tail
    }

    pub fn tick_times(&self) -> impl Iterator<Item = (u32, HostTime)> + '_ {
        (0..self.ticks).map(move |k| (k, self.tick_at(k)))
    }

    /// 主機時刻 `at` 的相位。起跑前與收尾期結束後都是 `None`。
    pub fn phase_at(&self, at: HostTime) -> Option<Phase> {
        let start = self.visual_start();
        if at < start || at >= self.end() {
            return None;
        }
        let total = self.total_beats();
        if at >= self.zero {
            return Some(Phase {
                beat: total - 1,
                phi: 1.0,
                is_final: true,
                since_landing: Some(at - self.zero),
                done: true,
            });
        }
        let elapsed = (at - start).as_nanos();
        let period = self.period.as_nanos();
        let k = ((elapsed / period) as u32).min(total - 1);
        let into = elapsed - k as u128 * period;
        Some(Phase {
            beat: k,
            phi: (into as f64 / period as f64).clamp(0.0, 1.0),
            is_final: k == total - 1,
            since_landing: (k > 0).then(|| Duration::from_nanos(into as u64)),
            done: false,
        })
    }
}

/// 不早於主機時刻 `not_before` 的下一個標準時間整秒（Unix 奈秒）。
/// 試聽節拍用它把歸零拍對到數字翻頁的瞬間。
pub fn next_whole_second(model: &ClockModel, not_before: HostTime) -> i128 {
    const S: i128 = 1_000_000_000;
    let r = model.estimate_at(not_before).remote_unix_ns;
    r.div_euclid(S) * S + if r.rem_euclid(S) == 0 { 0 } else { S }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelStatus, SourceKind};

    fn plan() -> BeatPlan {
        BeatPlan::new(
            HostTime::from_nanos(100 * 1_000_000_000),
            Duration::from_secs(1),
            4,
        )
    }

    fn ns(p: HostTime) -> i128 {
        p.as_nanos() as i128
    }

    #[test]
    fn ticks_are_spaced_one_period_back_from_zero() {
        let p = plan();
        let times: Vec<i128> = p.tick_times().map(|(_, t)| ns(t)).collect();
        let z = ns(p.zero);
        for (k, t) in times.iter().enumerate() {
            let expect = z - (3 - k as i128) * 1_000_000_000;
            assert!((t - expect).abs() <= 50, "第 {k} 拍 {t} ≠ {expect}");
        }
        assert!((ns(p.visual_start()) - (z - 4_000_000_000)).abs() <= 50);
        assert!((ns(p.end()) - (z + 1_500_000_000)).abs() <= 50);
    }

    #[test]
    fn phase_walks_through_the_plan() {
        let p = plan();
        let start = p.visual_start();
        assert_eq!(p.phase_at(start - Duration::from_millis(1)), None);

        let at_start = p.phase_at(start).unwrap();
        assert_eq!(at_start.beat, 0);
        assert_eq!(at_start.phi, 0.0);
        assert_eq!(at_start.since_landing, None);
        assert!(!at_start.is_final && !at_start.done);

        // 起跑後 1.25 秒：第 0 拍已落地 250 ms，正在逼近第 1 拍。
        let mid = p.phase_at(start + Duration::from_millis(1_250)).unwrap();
        assert_eq!(mid.beat, 1);
        assert!((mid.phi - 0.25).abs() < 1e-6, "{}", mid.phi);
        let since = mid.since_landing.unwrap().as_millis();
        assert!((249..=251).contains(&since), "{since}");
        assert!(!mid.is_final);

        // 歸零前 10 ms：最後一拍，φ 接近 1。
        let near = p.phase_at(p.zero - Duration::from_millis(10)).unwrap();
        assert_eq!(near.beat, 3);
        assert!(near.is_final && !near.done);
        assert!((near.phi - 0.99).abs() < 1e-6, "{}", near.phi);

        let done = p.phase_at(p.zero).unwrap();
        assert!(done.done && done.is_final && done.phi == 1.0);
        assert_eq!(done.since_landing, Some(Duration::ZERO));

        let tail = p.phase_at(p.zero + Duration::from_millis(1_499)).unwrap();
        assert!(tail.done);
        assert_eq!(p.phase_at(p.end()), None);
    }

    #[test]
    fn lead_beats_push_the_visual_start_earlier_and_land_silently() {
        let p = plan().with_lead(3).with_tail(Duration::from_millis(500));
        let z = ns(p.zero);
        assert_eq!(p.total_beats(), 6);
        assert_eq!(p.silent_landings(), 2);
        assert_eq!(p.visual_lead(), Duration::from_secs(6));
        assert!((ns(p.visual_start()) - (z - 6_000_000_000)).abs() <= 50);
        assert!((ns(p.end()) - (z + 500_000_000)).abs() <= 50);
        // 有聲拍不受影響。
        assert!((ns(p.first_tick()) - (z - 3_000_000_000)).abs() <= 50);
        assert_eq!(p.tick_times().count(), 4);

        // 起跑後 2.5 秒：第 2 拍（0 起算）飛到一半，前兩次落地都無聲；第 2 拍落地就是第一聲。
        let start = p.visual_start();
        let mid = p.phase_at(start + Duration::from_millis(2_500)).unwrap();
        assert_eq!(mid.beat, 2);
        assert!((mid.phi - 0.5).abs() < 1e-6);
        assert!(!mid.is_final);
        // 最後一拍是第 5 拍。
        let near = p.phase_at(p.zero - Duration::from_millis(10)).unwrap();
        assert_eq!(near.beat, 5);
        assert!(near.is_final);
        assert_eq!(p.phase_at(p.zero + Duration::from_millis(500)), None);
        assert!(!Phase::idle(&p).is_final);
    }

    #[test]
    fn lead_beats_for_rounds_up_to_whole_beats_and_never_below_one() {
        let s = Duration::from_secs;
        let one = Duration::from_secs(1);
        assert_eq!(BeatPlan::lead_beats_for(Duration::ZERO, one, 4), 1);
        assert_eq!(BeatPlan::lead_beats_for(s(4), one, 4), 1);
        assert_eq!(
            BeatPlan::lead_beats_for(Duration::from_millis(4_001), one, 4),
            2
        );
        assert_eq!(
            BeatPlan::lead_beats_for(Duration::from_millis(9_300), one, 4),
            7
        );
        assert_eq!(BeatPlan::lead_beats_for(s(2), one, 4), 1);
        // 拍距 500 ms、1 拍、要 3 秒 → 共 6 拍 → 第一聲前 6 拍。
        assert_eq!(
            BeatPlan::lead_beats_for(s(3), Duration::from_millis(500), 1),
            6
        );
        assert_eq!(BeatPlan::lead_beats_for(s(100_000), one, 4), MAX_LEAD_BEATS);
        assert_eq!(plan().with_lead(0).lead_beats, 1);
        assert_eq!(plan().with_lead(u32::MAX).lead_beats, MAX_LEAD_BEATS);
    }

    #[test]
    fn new_clamps_period_and_ticks() {
        let p = BeatPlan::new(HostTime::from_ticks(0), Duration::ZERO, 0);
        assert_eq!(p.ticks, 1);
        assert_eq!(p.period, Duration::from_millis(1));
        let p = BeatPlan::new(HostTime::from_ticks(0), Duration::from_secs(1), 99);
        assert_eq!(p.ticks, MAX_TICKS);
        assert!(
            Phase::idle(&BeatPlan::new(
                HostTime::from_ticks(0),
                Duration::from_secs(1),
                1
            ))
            .is_final
        );
    }

    #[test]
    fn next_whole_second_rounds_up_and_from_target_inverts() {
        let m0 = HostTime::from_nanos(50 * 1_000_000_000);
        let model = ClockModel {
            source: SourceKind::Standard,
            status: ModelStatus::Tracking,
            reference: m0,
            offset_ns: 1_700_000_000_000_000_000 + 250_000_000,
            drift: 30e-6,
            half_width_ns: 5_000_000,
            half_width_growth_ns_per_s: 0.0,
        };
        let target = next_whole_second(&model, m0);
        assert_eq!(target % 1_000_000_000, 0);
        let now_remote = model.estimate_at(m0).remote_unix_ns;
        assert!(target > now_remote && target - now_remote <= 1_000_000_000);

        let plan = BeatPlan::from_target(&model, target, Duration::from_secs(1), 4);
        let back = model.estimate_at(plan.zero).remote_unix_ns;
        assert!((back - target).abs() <= 100, "{}", back - target);
    }
}
