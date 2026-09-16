//! 命令列探針：對三台伺服器各打幾筆、印每筆區間、餵估計器、印模型。
//!
//! 偏移以「遠端時間 − 本機系統時鐘」呈現，跟 `sntp` 的慣例一樣：正值代表本機慢。
//! 本機系統時鐘平常由 timed 校正，所以這個數字應該在幾毫秒內；同時它也是
//! 用來跟 `sntp time.stdtime.gov.tw` 交叉比對的量。
//!
//! 執行：`cargo run --release -p kairos-core --example ntp-probe`

use std::time::Duration;

use kairos_core::estimate::Estimator;
use kairos_core::estimate::interval::{EstimatorConfig, IntervalEstimator};
use kairos_core::model::SourceKind;
use kairos_core::source::client::{QueryError, query_burst};
use kairos_core::source::udp::{TimestampedUdpSocket, resolve_ipv4};
use kairos_core::time::{HostTime, system_theta_ns};

const SERVERS: &[&str] = &[
    "time.stdtime.gov.tw",
    "time.google.com",
    "time.cloudflare.com",
];
const BURST: usize = 4;
const SPACING: Duration = Duration::from_secs(1);

fn ms(ns: i128) -> f64 {
    ns as f64 / 1e6
}

fn main() {
    let sys_theta = system_theta_ns();
    let sock = TimestampedUdpSocket::new_ipv4(Duration::from_secs(2)).expect("建立 socket 失敗");
    let mut est = IntervalEstimator::new(SourceKind::Standard, EstimatorConfig::default());

    println!(
        "{:<22} {:>8} {:>10} {:>10} {:>10}  stratum / ref",
        "伺服器", "往返 ms", "中點 ms", "±寬 ms", "伺服器誤差"
    );
    for host in SERVERS {
        let addrs = match resolve_ipv4(host, 123) {
            Ok(a) => a,
            Err(e) => {
                println!("{host:<22} 解析失敗：{e}");
                continue;
            }
        };
        let addr = addrs[0];
        for r in query_burst(&sock, addr, SourceKind::Standard, BURST, SPACING) {
            match r {
                Ok(ex) => {
                    let s = ex.sample;
                    println!(
                        "{:<22} {:>8.2} {:>+10.3} {:>10.3} {:>10.3}  {} / {}",
                        host,
                        ex.round_trip.as_secs_f64() * 1e3,
                        ms(s.midpoint_ns() - sys_theta),
                        ms(s.half_width_ns() as i128),
                        ms(ex.response.server_error_ns() as i128),
                        ex.response.stratum,
                        ex.response.reference_id_string(),
                    );
                    est.push(s);
                }
                Err(QueryError::NoMatchingResponse) => println!("{host:<22} 逾時"),
                Err(e) => println!("{host:<22} 失敗：{e}"),
            }
        }
    }

    let now = HostTime::now();
    let m = est.model(now);
    let e = m.estimate_at(now);
    println!();
    println!(
        "模型（{:?}，{} 筆樣本，丟掉 {} 筆，跨度 {:.0} 秒）",
        m.status,
        est.len(),
        est.rejected(),
        est.span().as_secs_f64()
    );
    println!(
        "  遠端 − 系統時鐘：{:+.3} ms ± {:.3} ms",
        ms(e.remote_unix_ns - (now.as_nanos() as i128 + sys_theta)),
        ms(e.half_width_ns as i128),
    );
    println!(
        "  漂移：{:+.1} ppm ± {:.1} ppm（樣本跨度短時只剩先驗界）",
        m.drift * 1e6,
        m.half_width_growth_ns_per_s / 1_000.0,
    );
    println!(
        "  60 秒後的半寬：{:.3} ms",
        ms(m.estimate_at(now + Duration::from_secs(60)).half_width_ns as i128)
    );
}
