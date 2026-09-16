//! 節拍元件的幾何：都是相位的純函數，畫面每一格用 `targetTimestamp` 算出相位再來查，
//! 位置永遠連續，不量化到拍點上。
//!
//! 設計原則是「每一拍都是某個東西抵達某個位置的瞬間」：抵達前要有夠長、夠穩定的運動，
//! 大腦才能外插軌跡、預測抵達的時刻。

use std::time::Duration;

use super::Phase;

/// 落地閃光的時間常數。
pub const FLASH_TAU: Duration = Duration::from_millis(40);
/// 光暈衰減的時間常數。
pub const GLOW_TAU: Duration = Duration::from_millis(80);
/// pulse 樣式的底亮度。
pub const PULSE_FLOOR: f64 = 0.2;

/// 彈跳球離地高度：h ＝ 4·max·φ(1 − φ)。φ = 0 與 1 在地上、0.5 最高；
/// 落地瞬間速度最大，撞擊點最銳利。歸零後躺在地上。
pub fn ball_height(p: &Phase, max: f64) -> f64 {
    if p.done {
        return 0.0;
    }
    4.0 * max * p.phi * (1.0 - p.phi)
}

/// 收縮圓環的外環半徑：從 `outer` 線性縮到 `inner`，等速逼近最好預測；φ = 1 與內環重合。
pub fn ring_radius(p: &Phase, outer: f64, inner: f64) -> f64 {
    if p.done {
        return inner;
    }
    outer - (outer - inner) * p.phi
}

fn decay(since: Option<Duration>, tau: Duration) -> f64 {
    match since {
        Some(d) => (-d.as_secs_f64() / tau.as_secs_f64()).exp().clamp(0.0, 1.0),
        None => 0.0,
    }
}

/// 落地閃光 0..1：落地瞬間 1，之後以 [`FLASH_TAU`] 指數衰減。
pub fn landing_flash(p: &Phase) -> f64 {
    decay(p.since_landing, FLASH_TAU)
}

/// 螢幕邊緣光暈的亮度 0..1：φ³ 漸亮到拍點，落地後以 [`GLOW_TAU`] 衰減，兩者取大。
/// 漸亮漸暗、每秒一次，遠低於每秒三次的光敏感上限。
pub fn glow_level(p: &Phase) -> f64 {
    let after = decay(p.since_landing, GLOW_TAU);
    if p.done {
        return after;
    }
    after.max(p.phi * p.phi * p.phi)
}

/// pulse 樣式（減少動態效果時用）的不透明度：不位移，只從底亮度以 φ² 漸亮到拍點，
/// 落地後掉回底亮度並帶一點衰減。
pub fn pulse_opacity(p: &Phase) -> f64 {
    let after = decay(p.since_landing, GLOW_TAU);
    let rise = if p.done { 0.0 } else { p.phi * p.phi };
    PULSE_FLOOR + (1.0 - PULSE_FLOOR) * rise.max(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(phi: f64, since_ms: Option<u64>, done: bool) -> Phase {
        Phase {
            beat: 1,
            phi,
            is_final: false,
            since_landing: since_ms.map(Duration::from_millis),
            done,
        }
    }

    #[test]
    fn ball_is_on_the_ground_at_both_ends_and_highest_in_the_middle() {
        assert_eq!(ball_height(&phase(0.0, None, false), 40.0), 0.0);
        assert_eq!(ball_height(&phase(1.0, None, false), 40.0), 0.0);
        assert_eq!(ball_height(&phase(0.5, None, false), 40.0), 40.0);
        let a = ball_height(&phase(0.3, None, false), 40.0);
        let b = ball_height(&phase(0.7, None, false), 40.0);
        assert!((a - b).abs() < 1e-9);
        assert_eq!(ball_height(&phase(1.0, Some(10), true), 40.0), 0.0);
    }

    #[test]
    fn ring_shrinks_linearly_to_the_inner_radius() {
        assert_eq!(ring_radius(&phase(0.0, None, false), 28.0, 10.0), 28.0);
        assert_eq!(ring_radius(&phase(0.5, None, false), 28.0, 10.0), 19.0);
        assert_eq!(ring_radius(&phase(1.0, None, false), 28.0, 10.0), 10.0);
        assert_eq!(ring_radius(&phase(1.0, Some(0), true), 28.0, 10.0), 10.0);
    }

    #[test]
    fn flash_and_glow_peak_at_landing_then_decay() {
        assert_eq!(landing_flash(&phase(0.0, None, false)), 0.0);
        assert_eq!(landing_flash(&phase(0.0, Some(0), false)), 1.0);
        let f40 = landing_flash(&phase(0.04, Some(40), false));
        assert!((f40 - (-1.0f64).exp()).abs() < 1e-9);

        assert!(glow_level(&phase(0.0, None, false)) == 0.0);
        assert!((glow_level(&phase(1.0, None, false)) - 1.0).abs() < 1e-9);
        // 剛落地：衰減項主導；一秒後：φ³ 主導。
        assert!(glow_level(&phase(0.01, Some(0), false)) == 1.0);
        let late = glow_level(&phase(0.9, Some(900), false));
        assert!((late - 0.729).abs() < 1e-6, "{late}");
        // 歸零後只剩衰減。
        assert!(glow_level(&phase(1.0, Some(400), true)) < 0.01);
    }

    #[test]
    fn pulse_never_goes_below_the_floor() {
        for phi in [0.0, 0.3, 0.9, 1.0] {
            let o = pulse_opacity(&phase(phi, None, false));
            assert!((PULSE_FLOOR..=1.0).contains(&o), "{phi} → {o}");
        }
        assert!((pulse_opacity(&phase(1.0, None, false)) - 1.0).abs() < 1e-9);
        assert!((pulse_opacity(&phase(1.0, Some(2_000), true)) - PULSE_FLOOR).abs() < 1e-6);
    }
}
