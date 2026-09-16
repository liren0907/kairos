//! 滴答聲：cpal 輸出串流。只用回呼的 `callback` 時間戳（緩衝區第一個取樣點的呈現時刻，
//! 規劃文件第六節的 spike 驗過），再加 CoreAudio 的三個裝置延遲屬性，才是每個取樣點
//! 真正出喇叭的時刻。取樣點由 `kairos_core::beat::audio::render` 依拍點表填入。
//!
//! 回呼在即時執行緒：不配置、不阻塞、不印字。拍點表用 `try_lock` 讀一份副本
//! （`BeatPlan` 是 `Copy`），診斷列寫進預先配好容量的向量，滿了就丟。

use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{OutputCallbackInfo, StreamInstant};
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertyScope, AudioObjectPropertySelector,
    kAudioDevicePropertyLatency, kAudioDevicePropertySafetyOffset, kAudioDevicePropertyStreams,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    kAudioStreamPropertyLatency,
};

use kairos_core::beat::BeatPlan;
use kairos_core::beat::audio::{TickSound, render};
use kairos_core::time::HostTime;

type OSStatus = i32;

/// 裝置延遲的三個成分，單位是幀。出喇叭時刻 ＝ 回呼時間戳 ＋ 三者之和 ÷ 取樣率。
/// 緩衝區本身的延遲已經在 `callback` 時間戳裡（spike 量到領先約 1.14 個緩衝區），不再加。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Latency {
    pub device_frames: u32,
    pub stream_frames: u32,
    pub safety_frames: u32,
}

impl Latency {
    pub fn total_frames(&self) -> u32 {
        self.device_frames + self.stream_frames + self.safety_frames
    }

    pub fn duration(&self, rate: f64) -> Duration {
        Duration::from_secs_f64(self.total_frames() as f64 / rate)
    }
}

fn address(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn get_u32(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Result<u32, OSStatus> {
    let mut addr = address(selector, scope);
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: 三個指標都指向這個函式裡的合法變數；size 與 value 的大小一致。
    let rc = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut addr),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    if rc == 0 { Ok(value) } else { Err(rc) }
}

fn get_ids(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Result<Vec<AudioObjectID>, OSStatus> {
    let mut addr = address(selector, scope);
    let mut size: u32 = 0;
    // SAFETY: 指標都合法。
    let rc = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&mut addr),
            0,
            ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if rc != 0 {
        return Err(rc);
    }
    let mut ids = vec![0u32; size as usize / std::mem::size_of::<u32>()];
    if ids.is_empty() {
        return Ok(ids);
    }
    // SAFETY: 緩衝區大小正是剛問到的 size。
    let rc = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut addr),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(ids.as_mut_ptr().cast::<c_void>()).expect("Vec 指標非空"),
        )
    };
    if rc == 0 { Ok(ids) } else { Err(rc) }
}

/// 系統目前的預設輸出裝置。cpal 的 `default_output_device` 也是問這個屬性。
pub fn default_output_device() -> Result<AudioObjectID, OSStatus> {
    get_u32(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    )
}

/// 查輸出端的裝置延遲、第一條輸出串流的串流延遲、安全餘量。
pub fn output_latency(device: AudioObjectID) -> Result<Latency, OSStatus> {
    let device_frames = get_u32(
        device,
        kAudioDevicePropertyLatency,
        kAudioObjectPropertyScopeOutput,
    )?;
    let safety_frames = get_u32(
        device,
        kAudioDevicePropertySafetyOffset,
        kAudioObjectPropertyScopeOutput,
    )?;
    let streams = get_ids(
        device,
        kAudioDevicePropertyStreams,
        kAudioObjectPropertyScopeOutput,
    )?;
    let stream_frames = match streams.first() {
        Some(&s) => get_u32(
            s,
            kAudioStreamPropertyLatency,
            kAudioObjectPropertyScopeGlobal,
        )?,
        None => 0,
    };
    Ok(Latency {
        device_frames,
        stream_frames,
        safety_frames,
    })
}

/// 每次回呼記一列。`onsets` 是這個緩衝區裡起音的拍（位元遮罩）。
#[derive(Clone, Copy, Debug, Default)]
struct Row {
    /// `callback` 時間戳（奈秒，未加裝置延遲）。
    callback_ns: u64,
    frames: u32,
    /// `callback − now`：回呼進場時時間戳領先多少。
    lead_ns: i64,
    onsets: u32,
}

struct Shared {
    plan: Mutex<Option<BeatPlan>>,
    rows: Mutex<Vec<Row>>,
}

/// 60 秒、每 10.7 ms 一次回呼約 5600 列；多留一些。
const ROW_CAPACITY: usize = 8192;

/// 一條正在播的輸出串流。被 drop 就停。
pub struct AudioEngine {
    _stream: cpal::Stream,
    shared: Arc<Shared>,
    pub device_name: String,
    pub rate: f64,
    pub channels: usize,
    pub latency: Latency,
    latency_error: Option<OSStatus>,
}

impl AudioEngine {
    /// 開預設輸出裝置、查延遲、立刻開始播（沒到拍點前都是靜音）。
    pub fn start(plan: BeatPlan, sound: TickSound) -> Result<AudioEngine, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "找不到預設輸出裝置".to_string())?;
        let device_name = device.name().unwrap_or_else(|_| "?".into());
        let supported = device
            .default_output_config()
            .map_err(|e| format!("讀不到預設輸出設定：{e}"))?;
        let config = supported.config();
        let rate = config.sample_rate.0 as f64;
        let channels = config.channels as usize;

        let (latency, latency_error) = match default_output_device().and_then(output_latency) {
            Ok(l) => (l, None),
            Err(rc) => (Latency::default(), Some(rc)),
        };
        let latency_dur = latency.duration(rate);

        let shared = Arc::new(Shared {
            plan: Mutex::new(Some(plan)),
            rows: Mutex::new(Vec::with_capacity(ROW_CAPACITY)),
        });
        let shared_cb = Arc::clone(&shared);
        let epoch = StreamInstant::new(0, 0);
        let mut local: Option<BeatPlan> = Some(plan);

        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], info: &OutputCallbackInfo| {
                    // 第一件事：讀現在，之後才做別的。
                    let now = HostTime::now();
                    let callback_ns = info
                        .timestamp()
                        .callback
                        .duration_since(&epoch)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    let start = HostTime::from_nanos(callback_ns) + latency_dur;
                    if let Ok(p) = shared_cb.plan.try_lock() {
                        local = *p;
                    }
                    let onsets = match local {
                        Some(plan) => render(&plan, &sound, start, rate, channels, data),
                        None => {
                            data.fill(0.0);
                            0
                        }
                    };
                    if let Ok(mut rows) = shared_cb.rows.try_lock()
                        && rows.len() < rows.capacity()
                    {
                        rows.push(Row {
                            callback_ns,
                            frames: (data.len() / channels.max(1)) as u32,
                            lead_ns: callback_ns as i64 - now.as_nanos() as i64,
                            onsets,
                        });
                    }
                },
                |err| eprintln!("音訊串流錯誤：{err}"),
                None,
            )
            .map_err(|e| format!("建立輸出串流失敗：{e}"))?;
        stream.play().map_err(|e| format!("串流啟動失敗：{e}"))?;

        Ok(AudioEngine {
            _stream: stream,
            shared,
            device_name,
            rate,
            channels,
            latency,
            latency_error,
        })
    }

    /// 啟動時印的一行。
    pub fn describe(&self) -> String {
        let l = self.latency;
        let latency = match self.latency_error {
            None => format!(
                "裝置延遲 {} + 串流 {} + 安全餘量 {} 幀 ＝ {:.2} ms",
                l.device_frames,
                l.stream_frames,
                l.safety_frames,
                l.duration(self.rate).as_secs_f64() * 1e3
            ),
            Some(rc) => format!("裝置延遲查詢失敗（OSStatus {rc}），當 0"),
        };
        format!(
            "聲音：{}，{} Hz，{} 聲道，{latency}",
            self.device_name, self.rate as u32, self.channels
        )
    }

    /// 結束時印的診斷：回呼統計，以及每一拍實際落在哪個取樣點。
    pub fn report(&self, plan: &BeatPlan) -> String {
        let rows = self.shared.rows.lock().unwrap_or_else(|e| e.into_inner());
        if rows.is_empty() {
            return "聲音：沒有任何回呼".into();
        }
        let mut leads: Vec<i64> = rows.iter().skip(3).map(|r| r.lead_ns).collect();
        leads.sort_unstable();
        let lead_ms = |i: usize| leads.get(i).map(|v| *v as f64 / 1e6).unwrap_or(0.0);
        let frames = rows.iter().map(|r| r.frames).max().unwrap_or(0);
        let mut out = format!(
            "聲音：{} 次回呼，每次 {frames} 幀（{:.2} ms），callback 領先現在 中位 {:.2} ms（{:.2}–{:.2}）",
            rows.len(),
            frames as f64 / self.rate * 1e3,
            lead_ms(leads.len() / 2),
            lead_ms(0),
            lead_ms(leads.len().saturating_sub(1)),
        );
        let latency_ns = self.latency.duration(self.rate).as_nanos() as i128;
        for (k, planned) in plan.tick_times() {
            let row = rows.iter().find(|r| r.onsets & (1 << k) != 0);
            match row {
                Some(r) => {
                    let start_ns = r.callback_ns as i128 + latency_ns;
                    let rel_ns = planned.as_nanos() as i128 - start_ns;
                    let index = (rel_ns as f64 * self.rate / 1e9).ceil().max(0.0);
                    let late_us = (index / self.rate * 1e9 - rel_ns as f64) / 1e3;
                    out.push_str(&format!(
                        "；第 {k} 拍在緩衝區第 {index:.0} 個取樣點，晚 {late_us:.1} µs"
                    ));
                }
                None => out.push_str(&format!("；第 {k} 拍沒排到（串流開太晚或回呼沒跑）")),
            }
        }
        out
    }
}
