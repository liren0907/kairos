//! 給人看的文字：狀態字樣、選單那兩列、面板的說明列。所有「遠端 − 系統時鐘」的正負
//! 慣例跟 `sntp` 一樣：正值代表本機系統時鐘慢。

use kairos_core::model::{ClockModel, ModelStatus};
use kairos_core::sync::{ServerStats, SyncStatus};
use kairos_core::time::HostTime;

pub fn ms(ns: i128) -> f64 {
    ns as f64 / 1e6
}

/// 「時間來源」子選單的標題列：「視窗 12 筆，丟 0 筆，第 3 輪」。
pub fn sync_header(status: &SyncStatus) -> String {
    if status.round == 0 {
        return "尚未取樣".to_string();
    }
    format!(
        "視窗 {} 筆，丟 {} 筆，第 {} 輪",
        status.samples, status.rejected, status.round
    )
}

/// 「時間來源」子選單的一列：主機、位址、最近一次的結果、成功／失敗次數、多久前。
pub fn server_row(s: &ServerStats, now: HostTime) -> String {
    let Some(last_at) = s.last_at else {
        return format!("{} · 尚未取樣", s.host);
    };
    let mut row = s.host.clone();
    if let Some(addr) = s.addr {
        row.push_str(&format!(" · {}", addr.ip()));
    }
    match &s.last_error {
        Some(e) => row.push_str(&format!(" · {e}")),
        None => {
            if let Some(rtt) = s.last_round_trip {
                row.push_str(&format!(" · 往返 {:.1} ms", rtt.as_secs_f64() * 1e3));
            }
            if let Some(hw) = s.last_half_width_ns {
                row.push_str(&format!(" · ± {:.1} ms", ms(hw as i128)));
            }
            if let Some(st) = s.last_stratum {
                row.push_str(&format!(" · s{st}"));
                if let Some(id) = &s.last_reference_id {
                    row.push_str(&format!(" {id}"));
                }
            }
        }
    }
    let ago = now.saturating_duration_since(last_at).as_secs();
    row.push_str(&format!(" · {} 成功 {} 失敗 · {ago} 秒前", s.ok, s.failed));
    row
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

/// 「還有 14:32」「還有 1:02:03」；已過就是「已過 0:05」。
pub fn remaining_text(remaining_ns: i128) -> String {
    let secs = remaining_ns.div_euclid(1_000_000_000);
    let (word, s) = if secs >= 0 {
        ("還有", secs)
    } else {
        ("已過", -secs)
    };
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{word} {h}:{m:02}:{sec:02}")
    } else {
        format!("{word} {m}:{sec:02}")
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
    fn remaining_text_formats_hours_minutes_and_past() {
        const S: i128 = 1_000_000_000;
        assert_eq!(remaining_text(872 * S), "還有 14:32");
        assert_eq!(remaining_text(3_723 * S + 500_000_000), "還有 1:02:03");
        assert_eq!(remaining_text(-5 * S), "已過 0:05");
        assert_eq!(remaining_text(0), "還有 0:00");
    }

    #[test]
    fn server_rows_cover_never_ok_and_failed() {
        let now = HostTime::from_nanos(5_000_000_000_000);
        let fresh = ServerStats::new("time.stdtime.gov.tw");
        assert_eq!(server_row(&fresh, now), "time.stdtime.gov.tw · 尚未取樣");

        let mut ok = ServerStats::new("time.google.com");
        ok.addr = Some("216.239.35.8:123".parse().unwrap());
        ok.ok = 12;
        ok.last_round_trip = Some(Duration::from_micros(13_120));
        ok.last_half_width_ns = Some(6_700_000);
        ok.last_stratum = Some(1);
        ok.last_reference_id = Some("GOOG".into());
        ok.last_at = Some(now - Duration::from_secs(41));
        ok.last_success_at = ok.last_at;
        assert_eq!(
            server_row(&ok, now),
            "time.google.com · 216.239.35.8 · 往返 13.1 ms · ± 6.7 ms · s1 GOOG · 12 成功 0 失敗 · 41 秒前"
        );

        let mut failed = ok.clone();
        failed.host = "time.cloudflare.com".into();
        failed.failed = 4;
        failed.last_error = Some("逾時".into());
        failed.last_at = Some(now - Duration::from_secs(37));
        assert_eq!(
            server_row(&failed, now),
            "time.cloudflare.com · 216.239.35.8 · 逾時 · 12 成功 4 失敗 · 37 秒前"
        );

        let status = SyncStatus {
            servers: vec![fresh],
            samples: 12,
            rejected: 0,
            round: 3,
            settings: None,
        };
        assert_eq!(sync_header(&status), "視窗 12 筆，丟 0 筆，第 3 輪");
        assert_eq!(sync_header(&SyncStatus::default()), "尚未取樣");
    }

    #[test]
    fn system_clock_text_switches_units() {
        assert_eq!(system_clock_text(87_000_000), "系統時鐘慢 87 ms");
        assert_eq!(system_clock_text(-87_000_000), "系統時鐘快 87 ms");
        assert_eq!(system_clock_text(2_981_000_000), "系統時鐘慢 2.98 s");
    }
}
