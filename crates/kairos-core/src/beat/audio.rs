//! 滴答聲的取樣渲染。無狀態：每個取樣點自己算主機時刻，落在某拍的 `[onset, onset + dur)`
//! 就寫波形，跨緩衝區邊界不需要記任何東西，緩衝區大小變了也沒差。
//!
//! 聲音走廣播報時訊號的慣例：前導拍與歸零拍同音高，歸零拍拉長。同音高讓人比較「起點」
//! 而不是比較音色；長度不同讓歸零拍不會聽錯。起音與收音各留短短的斜坡避免爆音。

use std::f64::consts::TAU;
use std::time::Duration;

use super::BeatPlan;
use crate::time::HostTime;

/// 起音斜坡。
pub const ATTACK: Duration = Duration::from_millis(1);
/// 收音斜坡。
pub const RELEASE: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TickSound {
    pub hz: f64,
    /// 前導拍長度。
    pub tick: Duration,
    /// 歸零拍長度。
    pub final_tick: Duration,
    /// 0 到 1。
    pub volume: f32,
}

impl Default for TickSound {
    fn default() -> Self {
        TickSound {
            hz: 1000.0,
            tick: Duration::from_millis(30),
            final_tick: Duration::from_millis(200),
            volume: 0.5,
        }
    }
}

/// 梯形包絡：`t` 是離起音多久（秒），`dur` 是這一拍總長（秒）。範圍外為 0。
fn envelope(t: f64, dur: f64) -> f64 {
    if t < 0.0 || t >= dur {
        return 0.0;
    }
    let attack = (t / ATTACK.as_secs_f64()).min(1.0);
    let release = ((dur - t) / RELEASE.as_secs_f64()).min(1.0);
    attack.min(release).max(0.0)
}

/// 把 `out`（交錯 `channels` 聲道）填成從主機時刻 `start` 起、每個取樣點相隔 `1/rate` 秒
/// 的聲音。`start` 要是這個緩衝區第一個取樣點**真正出喇叭**的時刻（回呼時間戳加裝置延遲）。
///
/// 回傳位元遮罩：第 `k` 位為 1 代表第 `k` 拍的起音落在這個緩衝區裡，給診斷用。
pub fn render(
    plan: &BeatPlan,
    sound: &TickSound,
    start: HostTime,
    rate: f64,
    channels: usize,
    out: &mut [f32],
) -> u32 {
    out.fill(0.0);
    if channels == 0 || rate <= 0.0 {
        return 0;
    }
    let frames = out.len() / channels;
    if frames == 0 {
        return 0;
    }
    let start_ns = start.as_nanos() as i128;
    let span_ns = (frames as f64 / rate * 1e9).ceil() as i128;
    let mut onsets = 0u32;

    for (k, onset) in plan.tick_times() {
        let dur = if k == plan.ticks - 1 {
            sound.final_tick
        } else {
            sound.tick
        };
        let dur_ns = dur.as_nanos() as i128;
        let rel_ns = onset.as_nanos() as i128 - start_ns;
        if rel_ns + dur_ns <= 0 || rel_ns >= span_ns {
            continue;
        }
        if rel_ns >= 0 {
            onsets |= 1 << k;
        }
        // 取樣點 j 離起音 t_j = j/rate − rel；要 0 ≤ t_j < dur。
        let rel_s = rel_ns as f64 / 1e9;
        let dur_s = dur.as_secs_f64();
        let first = (rel_s * rate).ceil().max(0.0) as usize;
        let last = ((rel_s + dur_s) * rate).ceil().max(0.0) as usize;
        for j in first..last.min(frames) {
            let t = j as f64 / rate - rel_s;
            let v = (sound.volume as f64 * envelope(t, dur_s) * (TAU * sound.hz * t).sin()) as f32;
            if v != 0.0 {
                for c in 0..channels {
                    out[j * channels + c] += v;
                }
            }
        }
    }
    onsets
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;

    fn plan_with_zero(zero: HostTime, ticks: u32) -> BeatPlan {
        BeatPlan::new(zero, Duration::from_secs(1), ticks)
    }

    #[test]
    fn onset_lands_on_the_right_sample() {
        let start = HostTime::from_nanos(10 * 1_000_000_000);
        // 唯一一拍在緩衝區開始後 10 ms ＝ 第 480 個取樣點。
        let plan = plan_with_zero(start + Duration::from_millis(10), 1);
        let mut out = vec![0.0f32; 1024];
        let mask = render(&plan, &TickSound::default(), start, RATE, 1, &mut out);
        assert_eq!(mask, 0b1);
        assert!(out[..480].iter().all(|&v| v == 0.0), "起音前不該有聲音");
        assert!(
            out[480..484].iter().any(|&v| v != 0.0),
            "起音後幾個取樣點內要有聲音"
        );
        // 起音從零開始，第一個非零取樣點很小（1 ms 斜坡的第一步）。
        let first_nonzero = out[480..].iter().find(|&&v| v != 0.0).copied().unwrap();
        assert!(first_nonzero.abs() < 0.05, "{first_nonzero}");
    }

    #[test]
    fn silence_when_no_tick_overlaps_the_buffer() {
        let start = HostTime::from_nanos(10 * 1_000_000_000);
        let plan = plan_with_zero(start + Duration::from_secs(5), 4);
        let mut out = vec![1.0f32; 512 * 2];
        let mask = render(&plan, &TickSound::default(), start, RATE, 2, &mut out);
        assert_eq!(mask, 0);
        assert!(out.iter().all(|&v| v == 0.0), "沒有拍時要清成靜音");
    }

    #[test]
    fn final_tick_is_longer_and_stereo_is_duplicated() {
        let start = HostTime::from_nanos(10 * 1_000_000_000);
        let sound = TickSound::default();
        let count = |plan: &BeatPlan| {
            let mut out = vec![0.0f32; 48_000 * 2];
            render(plan, &sound, start, RATE, 2, &mut out);
            for f in out.chunks(2) {
                assert_eq!(f[0], f[1]);
            }
            out.chunks(2).filter(|f| f[0] != 0.0).count()
        };
        // 一拍在 100 ms：ticks = 1 時它就是歸零拍（200 ms），ticks = 2 且歸零在 1.1 s 時它是前導拍。
        let lead = count(&plan_with_zero(start + Duration::from_millis(1_100), 2));
        let final_only = count(&plan_with_zero(start + Duration::from_millis(100), 1));
        // 30 ms ≈ 1440 個取樣點，200 ms ≈ 9600 個；正弦過零點會少幾個。
        assert!((1_300..=1_440).contains(&lead), "{lead}");
        assert!((9_400..=9_600).contains(&final_only), "{final_only}");
    }

    #[test]
    fn two_half_buffers_equal_one_whole_buffer() {
        let start = HostTime::from_nanos(10 * 1_000_000_000);
        // 起音橫跨兩個緩衝區的邊界（512 個取樣點 ＝ 10.667 ms）。
        let plan = plan_with_zero(start + Duration::from_micros(10_000), 1);
        let sound = TickSound::default();
        let mut whole = vec![0.0f32; 1024];
        render(&plan, &sound, start, RATE, 1, &mut whole);
        let mut a = vec![0.0f32; 512];
        let mut b = vec![0.0f32; 512];
        render(&plan, &sound, start, RATE, 1, &mut a);
        let second = start + Duration::from_nanos((512.0 / RATE * 1e9) as u64);
        render(&plan, &sound, second, RATE, 1, &mut b);
        for (j, (w, s)) in whole.iter().zip(a.iter().chain(b.iter())).enumerate() {
            assert!((w - s).abs() < 1e-4, "取樣點 {j}：{w} vs {s}");
        }
    }

    #[test]
    fn envelope_is_trapezoidal() {
        assert_eq!(envelope(-0.001, 0.03), 0.0);
        assert_eq!(envelope(0.0, 0.03), 0.0);
        assert!((envelope(0.0005, 0.03) - 0.5).abs() < 1e-9);
        assert_eq!(envelope(0.010, 0.03), 1.0);
        assert!((envelope(0.0275, 0.03) - 0.5).abs() < 1e-9);
        assert_eq!(envelope(0.03, 0.03), 0.0);
    }
}
