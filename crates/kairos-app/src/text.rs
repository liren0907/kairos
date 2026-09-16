//! 給人看的文字：狀態字樣、選單那兩列、面板的說明列。所有「遠端 − 系統時鐘」的正負
//! 慣例跟 `sntp` 一樣：正值代表本機系統時鐘慢。

use kairos_core::model::{ClockModel, ModelStatus};
use kairos_core::time::HostTime;

pub fn ms(ns: i128) -> f64 {
    ns as f64 / 1e6
}

/// 可用狀態的字樣；不可用的狀態回傳 `Err(說明)`。
pub fn status_word(model: &ClockModel) -> Result<&'static str, &'static str> {
    match model.status {
        ModelStatus::Uncalibrated => Err("校時中…"),
        ModelStatus::Stale if !model.is_usable() => Err("喚醒後重新校時中"),
        ModelStatus::Converging => Ok("收斂中"),
        ModelStatus::Tracking => Ok("追蹤中"),
        ModelStatus::Stale => Ok("過時"),
        ModelStatus::Frozen => Ok("已凍結"),
    }
}

/// 「系統時鐘慢 2.98 s」這種說法，給面板的說明列。
pub fn system_clock_text(remote_minus_system_ns: i128) -> String {
    let d = ms(remote_minus_system_ns);
    let word = if d >= 0.0 { "慢" } else { "快" };
    let a = d.abs();
    if a < 1_000.0 {
        format!("系統時鐘{word} {a:.0} ms")
    } else {
        format!("系統時鐘{word} {:.2} s", a / 1_000.0)
    }
}

/// 選單裡的兩列：狀態列與細節列。
pub fn menu_rows(model: &ClockModel, now: HostTime, system_theta_ns: i128) -> (String, String) {
    let status_word = match status_word(model) {
        Ok(w) => w,
        Err(why) => return (format!("標準時間：{why}"), String::new()),
    };
    let e = model.estimate_at(now);
    let offset_ms = ms(e.remote_unix_ns - (now.as_nanos() as i128 + system_theta_ns));
    let primary = format!(
        "標準時間 {offset_ms:+.1} ms ± {:.1} ms（{status_word}）",
        ms(e.half_width_ns as i128)
    );
    let since = now.saturating_duration_since(model.reference).as_secs();
    let detail = format!(
        "漂移 {:+.1} ± {:.0} ppm，上次取樣 {since} 秒前",
        model.drift * 1e6,
        model.half_width_growth_ns_per_s / 1_000.0
    );
    (primary, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kairos_core::model::SourceKind;
    use std::time::Duration;

    fn model(status: ModelStatus, reference: HostTime) -> ClockModel {
        ClockModel {
            source: SourceKind::Standard,
            status,
            reference,
            // 讓「遠端 − 系統時鐘」剛好是 +88 ms：θ = θ_sys + 88 ms。
            offset_ns: 1_000 + 88_000_000,
            drift: 12.3e-6,
            half_width_ns: 8_000_000,
            half_width_growth_ns_per_s: 480_000.0,
        }
    }

    #[test]
    fn menu_rows_tracking_shows_offset_and_width() {
        let now = HostTime::from_nanos(5_000_000_000_000);
        let (primary, detail) = menu_rows(&model(ModelStatus::Tracking, now), now, 1_000);
        assert_eq!(primary, "標準時間 +88.0 ms ± 8.0 ms（追蹤中）");
        assert_eq!(detail, "漂移 +12.3 ± 480 ppm，上次取樣 0 秒前");
    }

    #[test]
    fn menu_rows_grow_width_and_age_with_time() {
        let reference = HostTime::from_nanos(5_000_000_000_000);
        let now = reference + Duration::from_secs(10);
        let (primary, detail) = menu_rows(&model(ModelStatus::Converging, reference), now, 1_000);
        // 8 ms + 10 s × 480 µs/s = 12.8 ms；偏移多了 12.3 ppm × 10 s ≈ 0.12 ms。
        assert_eq!(primary, "標準時間 +88.1 ms ± 12.8 ms（收斂中）");
        assert!(detail.ends_with("上次取樣 10 秒前"), "{detail}");
    }

    #[test]
    fn menu_rows_special_states() {
        let now = HostTime::from_nanos(5_000_000_000_000);
        let uncal = ClockModel::uncalibrated(SourceKind::Standard, now);
        assert_eq!(menu_rows(&uncal, now, 0).0, "標準時間：校時中…");

        let mut woke = uncal;
        woke.status = ModelStatus::Stale;
        assert_eq!(menu_rows(&woke, now, 0).0, "標準時間：喚醒後重新校時中");

        let stale = model(ModelStatus::Stale, now);
        assert_eq!(
            menu_rows(&stale, now, 1_000).0,
            "標準時間 +88.0 ms ± 8.0 ms（過時）"
        );
    }

    #[test]
    fn system_clock_text_switches_units() {
        assert_eq!(system_clock_text(87_000_000), "系統時鐘慢 87 ms");
        assert_eq!(system_clock_text(-87_000_000), "系統時鐘快 87 ms");
        assert_eq!(system_clock_text(2_981_000_000), "系統時鐘慢 2.98 s");
    }
}
