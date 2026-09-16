//! Spike B：cpal 輸出回呼的時間戳，相對於 `mach_absolute_time` 是哪一刻。
//!
//! 從 cpal 0.16.0 原始碼已確認：`callback` 是 `AudioTimeStamp.mHostTime` 乘以
//! timebase 換成奈秒；`playback` 是 `callback` 加一個緩衝區長度（原始碼標注
//! TODO，是猜的）。所以剩下要量的是：`mHostTime` 是「回呼被叫的時刻」還是
//! 「這個緩衝區第一個取樣點送到裝置的時刻」。
//!
//! 量法：回呼進場立刻讀 `mach_absolute_time`，用同一條公式換成 `StreamInstant`，
//! 記錄 `callback − now`；另外記錄連續兩次 `callback` 的間隔，看它是精確等於
//! 緩衝區長度（取樣時鐘推算）還是帶抖動（回呼實際被叫的時刻）。
//!
//! 執行：`cargo run -p spike-cpal-hosttime`

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamInstant};

const CAPTURE_ROWS: usize = 200;
const CAPTURE_SECS: u64 = 2;
/// 串流剛啟動時 HAL 有暖機空檔，前幾筆不納入統計。
const WARMUP_ROWS: usize = 3;

#[derive(Clone, Copy)]
struct Timebase {
    numer: u32,
    denom: u32,
}

impl Timebase {
    fn read() -> Self {
        let mut info = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: 傳入合法的可寫指標。
        let rc = unsafe { mach2::mach_time::mach_timebase_info(&mut info) };
        assert_eq!(rc, 0, "mach_timebase_info 失敗");
        Self {
            numer: info.numer,
            denom: info.denom,
        }
    }

    /// 與 cpal `host_time_to_stream_instant` 完全相同的換算，確保可比較。
    fn now_as_stream_instant(&self) -> StreamInstant {
        // SAFETY: 無參數、無副作用的系統呼叫。
        let ticks = unsafe { mach2::mach_time::mach_absolute_time() };
        let nanos = ticks * self.numer as u64 / self.denom as u64;
        let secs = nanos / 1_000_000_000;
        let subsec = nanos - secs * 1_000_000_000;
        StreamInstant::new(secs as i64, subsec as u32)
    }
}

/// `a − b` 的有號奈秒差；`StreamInstant::duration_since` 只給單向，這裡補成雙向。
fn signed_diff_ns(a: &StreamInstant, b: &StreamInstant) -> i128 {
    match a.duration_since(b) {
        Some(d) => d.as_nanos() as i128,
        None => -(b.duration_since(a).map(|d| d.as_nanos() as i128).unwrap_or(0)),
    }
}

#[derive(Clone, Copy, Default)]
struct Row {
    /// callback − now（回呼進場時的 mach 時間），奈秒。正值代表 mHostTime 在未來。
    callback_minus_now_ns: i128,
    /// playback − callback，奈秒。預期等於一個緩衝區長度（cpal 自己加的）。
    playback_minus_callback_ns: i128,
    /// 這次 callback 與上一次 callback 的間隔，奈秒。
    callback_period_ns: i128,
    frames: u32,
}

fn main() {
    let tb = Timebase::read();
    println!(
        "mach_timebase_info: numer {} / denom {}（1 tick = {:.4} ns）",
        tb.numer,
        tb.denom,
        tb.numer as f64 / tb.denom as f64
    );

    let host = cpal::default_host();
    let device = host.default_output_device().expect("找不到預設輸出裝置");
    let supported = device.default_output_config().expect("讀不到預設輸出設定");
    println!(
        "裝置：{}；取樣率 {} Hz；聲道 {}；格式 {:?}；緩衝區範圍 {:?}",
        device.name().unwrap_or_else(|_| "?".into()),
        supported.sample_rate().0,
        supported.channels(),
        supported.sample_format(),
        supported.buffer_size(),
    );
    if supported.sample_format() != SampleFormat::F32 {
        println!("此 spike 只處理 F32 輸出格式，實際為 {:?}，結束。", supported.sample_format());
        return;
    }
    let config = supported.config();
    let sample_rate = config.sample_rate.0 as u128;
    let channels = config.channels as usize;

    let rows: Arc<Mutex<Vec<Row>>> = Arc::new(Mutex::new(Vec::with_capacity(CAPTURE_ROWS)));
    let rows_cb = Arc::clone(&rows);
    let mut last_callback: Option<StreamInstant> = None;

    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                // 第一件事：讀現在的 mach 時間，之後才做別的。
                let now = tb.now_as_stream_instant();
                let ts = info.timestamp();

                data.fill(0.0);

                let row = Row {
                    callback_minus_now_ns: signed_diff_ns(&ts.callback, &now),
                    playback_minus_callback_ns: signed_diff_ns(&ts.playback, &ts.callback),
                    callback_period_ns: last_callback
                        .map(|prev| signed_diff_ns(&ts.callback, &prev))
                        .unwrap_or(0),
                    frames: (data.len() / channels) as u32,
                };
                last_callback = Some(ts.callback);

                // 回呼裡不阻塞、不配置：try_lock，容量事先保留，滿了就丟。
                if let Ok(mut v) = rows_cb.try_lock()
                    && v.len() < v.capacity()
                {
                    v.push(row);
                }
            },
            |err| eprintln!("串流錯誤：{err}"),
            None,
        )
        .expect("建立輸出串流失敗");

    stream.play().expect("串流啟動失敗");
    thread::sleep(Duration::from_secs(CAPTURE_SECS));
    drop(stream);

    let rows = rows.lock().unwrap();
    report(&rows, sample_rate);
}

fn report(rows: &[Row], sample_rate: u128) {
    println!("\n收到 {} 筆回呼樣本（略過前 {WARMUP_ROWS} 筆暖機）", rows.len());
    if rows.len() < WARMUP_ROWS + 2 {
        println!("樣本太少，無法判讀。");
        return;
    }
    let rows = &rows[WARMUP_ROWS..];

    let frames = rows[0].frames;
    let buffer_ns = frames as u128 * 1_000_000_000 / sample_rate;
    println!("每次回呼 {frames} 幀，緩衝區長度 {:.1} µs", buffer_ns as f64 / 1_000.0);

    let us = |ns: i128| ns as f64 / 1_000.0;
    let stats = |xs: &mut Vec<i128>| {
        xs.sort_unstable();
        (xs[0], xs[xs.len() / 2], xs[xs.len() - 1])
    };

    let mut cmn: Vec<i128> = rows.iter().map(|r| r.callback_minus_now_ns).collect();
    let mut pmc: Vec<i128> = rows.iter().map(|r| r.playback_minus_callback_ns).collect();
    let mut period: Vec<i128> = rows.iter().map(|r| r.callback_period_ns).collect();
    let (cmn_min, cmn_med, cmn_max) = stats(&mut cmn);
    let (pmc_min, pmc_med, pmc_max) = stats(&mut pmc);
    let (p_min, p_med, p_max) = stats(&mut period);

    println!(
        "callback − now：min {:.1} / median {:.1} / max {:.1} µs（÷ 緩衝區 = {:.2} / {:.2} / {:.2}）",
        us(cmn_min),
        us(cmn_med),
        us(cmn_max),
        cmn_min as f64 / buffer_ns as f64,
        cmn_med as f64 / buffer_ns as f64,
        cmn_max as f64 / buffer_ns as f64,
    );
    println!(
        "playback − callback：min {:.1} / median {:.1} / max {:.1} µs",
        us(pmc_min),
        us(pmc_med),
        us(pmc_max),
    );
    println!(
        "callback 間隔：min {:.1} / median {:.1} / max {:.1} µs（抖動 {:.1} µs）",
        us(p_min),
        us(p_med),
        us(p_max),
        us(p_max - p_min),
    );

    println!("\n前 10 筆逐筆（µs）：");
    println!("{:>6} {:>16} {:>20} {:>14}", "幀", "callback−now", "playback−callback", "間隔");
    for r in rows.iter().take(10) {
        println!(
            "{:>6} {:>16.1} {:>20.1} {:>14.1}",
            r.frames,
            us(r.callback_minus_now_ns),
            us(r.playback_minus_callback_ns),
            us(r.callback_period_ns),
        );
    }

    // 判讀
    let period_jitter_us = us(p_max - p_min);
    let ratio_med = cmn_med as f64 / buffer_ns as f64;
    println!();
    if cmn_min > 0 && ratio_med > 0.5 {
        println!(
            "判讀：callback 穩定領先「現在」約 {ratio_med:.2} 個緩衝區，mHostTime 是呈現時刻（尚未含裝置延遲）；"
        );
        println!("　　　cpal 的時間基準可用，裝置延遲另查 kAudioDevicePropertyLatency 等屬性補上。");
    } else if cmn_min.abs() < buffer_ns as i128 / 4 && cmn_max.abs() < buffer_ns as i128 / 4 {
        println!("判讀：callback 與「現在」幾乎重合，mHostTime 是回呼被叫的時刻；");
        println!("　　　cpal 資訊不足以做取樣點等級的排程，改走 coreaudio-rs 直接拿完整 AudioTimeStamp。");
    } else {
        println!("判讀：模式不在預期的兩種之內，需人工檢視上面的數字。");
    }
    if period_jitter_us < 50.0 {
        println!("附註：callback 間隔抖動 {period_jitter_us:.1} µs，幾乎是取樣時鐘推算出來的，不是排程時刻。");
    } else {
        println!("附註：callback 間隔抖動 {period_jitter_us:.1} µs，帶有排程抖動。");
    }
}
