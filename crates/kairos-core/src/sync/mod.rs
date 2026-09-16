//! 取樣執行緒與模型發布：把 [`source`](crate::source) 與 [`estimate`](crate::estimate)
//! 放到背景執行緒定期跑，算好的 [`ClockModel`] 放進 [`ModelSlot`] 讓呈現層讀。
//!
//! ```text
//! 取樣執行緒                                     主執行緒
//! ─────────────────────────────                  ──────────────────────────
//! 每輪：偵測睡眠 → DNS 重解 → 每台幾筆           slot.get() → estimate_at(now)
//!       → estimator.push() → model() → slot.set()
//!                        ◄── resample_now() ──   喚醒通知、使用者要求
//! ```
//!
//! O(n³) 的 LP 只在這條執行緒每輪算一次；主執行緒每次讀到的是值型別的模型，
//! `estimate_at` 是 O(1)，面板每一格讀都不會有負擔。
//!
//! 睡眠的處理由 [`SleepDetector`] 全權負責：`mach_absolute_time` 在睡眠期間停住，
//! 睡前的樣本在醒來後全部失效，一定要清掉。`NSWorkspace` 的喚醒通知只是
//! 「現在立刻取樣」的排程提示，不是唯一防線——通知到達的時間不受我們控制，
//! 執行緒可能在通知之前就已經醒來跑了一輪。

use std::fmt;
use std::io;
use std::net::SocketAddrV4;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::estimate::Estimator;
use crate::estimate::interval::{EstimatorConfig, IntervalEstimator};
use crate::model::{ClockModel, ModelStatus, SourceKind};
use crate::source::client::{QueryError, query_burst};
use crate::source::ntp::Exchange;
use crate::source::udp::{TimestampedUdpSocket, resolve_ipv4};
use crate::time::{HostTime, Timebase, continuous_ticks, system_theta_ns};

/// 取樣執行緒寫、呈現層讀的模型槽。`ClockModel` 是 `Copy`，讀出來就是一份快照。
#[derive(Clone, Debug)]
pub struct ModelSlot(Arc<Mutex<ClockModel>>);

impl ModelSlot {
    pub fn new(initial: ClockModel) -> ModelSlot {
        ModelSlot(Arc::new(Mutex::new(initial)))
    }

    pub fn get(&self) -> ClockModel {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set(&self, model: ClockModel) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = model;
    }
}

/// 用 `mach_continuous_time − mach_absolute_time` 的增量偵測「中間睡過」。
///
/// 兩個時鐘同單位、同起點，差值就是開機以來睡掉的總時間；差值比上次多，就是又睡了一次。
/// 兩次讀取之間有幾微秒的抖動，所以要有門檻；真正的睡眠最短也是秒級，門檻設幾百毫秒
/// 既不會誤報也不會漏報。
#[derive(Clone, Debug)]
pub struct SleepDetector {
    /// 上次看到的差值（tick）。
    last_gap: u64,
    threshold_ticks: u64,
}

impl SleepDetector {
    /// 以現在的時鐘讀數當基準。
    pub fn new(threshold: Duration) -> SleepDetector {
        let tb = Timebase::get();
        SleepDetector::from_readings(
            HostTime::now().ticks(),
            continuous_ticks(),
            tb.duration_to_ticks(threshold),
        )
    }

    /// 以指定讀數建立，給測試用。
    pub fn from_readings(absolute: u64, continuous: u64, threshold_ticks: u64) -> SleepDetector {
        SleepDetector {
            last_gap: continuous.saturating_sub(absolute),
            threshold_ticks,
        }
    }

    /// 讀現在的時鐘，回傳自上次檢查以來睡掉的時間；沒睡回傳 `None`。
    pub fn check(&mut self) -> Option<Duration> {
        self.check_readings(HostTime::now().ticks(), continuous_ticks())
            .map(|t| Timebase::get().ticks_to_duration(t))
    }

    /// 以指定讀數檢查，回傳睡掉的 tick 數。偵測到就把基準移到新的差值。
    pub fn check_readings(&mut self, absolute: u64, continuous: u64) -> Option<u64> {
        let gap = continuous.saturating_sub(absolute);
        let slept = gap.saturating_sub(self.last_gap);
        if slept >= self.threshold_ticks {
            self.last_gap = gap;
            Some(slept)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug)]
pub struct SamplerConfig {
    /// 每輪都會重新解析的伺服器主機名。
    pub servers: Vec<String>,
    pub port: u16,
    /// 每台每輪幾筆。
    pub burst: usize,
    /// 同一台連續兩筆的間隔。
    pub spacing: Duration,
    /// 兩輪之間的間隔（從上一輪結束算）。
    pub cycle: Duration,
    pub socket_timeout: Duration,
    /// 超過這麼久沒有任何成功樣本，模型標成 `Stale`（數字照舊，半寬會隨漂移界變寬）。
    pub stale_after: Duration,
    /// 睡眠偵測的門檻。
    pub sleep_threshold: Duration,
    pub estimator: EstimatorConfig,
}

impl Default for SamplerConfig {
    /// 3 分鐘一輪、三台各 4 筆：30 分鐘視窗裝 10 輪 120 筆，在估計器 160 筆的上限內；
    /// 對每台伺服器平均 45 秒一筆，夠客氣。
    fn default() -> Self {
        SamplerConfig {
            servers: vec![
                "time.stdtime.gov.tw".into(),
                "time.google.com".into(),
                "time.cloudflare.com".into(),
            ],
            port: 123,
            burst: 4,
            spacing: Duration::from_secs(1),
            cycle: Duration::from_secs(180),
            socket_timeout: Duration::from_secs(2),
            stale_after: Duration::from_secs(10 * 60),
            sleep_threshold: Duration::from_millis(500),
            estimator: EstimatorConfig::default(),
        }
    }
}

/// 執行中可以改的那幾個設定：伺服器清單與取樣節奏。估計器、逾時、睡眠門檻不開放。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncSettings {
    pub servers: Vec<String>,
    pub burst: usize,
    pub spacing: Duration,
    pub cycle: Duration,
}

impl SyncSettings {
    pub const MAX_SERVERS: usize = 8;
    pub const MAX_BURST: usize = 8;
    pub const MIN_SPACING: Duration = Duration::from_millis(200);
    pub const MIN_CYCLE: Duration = Duration::from_secs(60);

    /// 建構時驗證：1 到 8 台、每輪 1 到 8 筆、間隔 ≥ 200 ms、週期 ≥ 60 秒，主機名不能空白。
    pub fn new(
        servers: Vec<String>,
        burst: usize,
        spacing: Duration,
        cycle: Duration,
    ) -> Result<SyncSettings, String> {
        let servers: Vec<String> = servers.into_iter().map(|s| s.trim().to_string()).collect();
        if servers.is_empty() || servers.len() > Self::MAX_SERVERS {
            return Err(format!(
                "伺服器要 1 到 {} 台，現在 {} 台",
                Self::MAX_SERVERS,
                servers.len()
            ));
        }
        if let Some(bad) = servers
            .iter()
            .find(|s| s.is_empty() || s.chars().any(char::is_whitespace))
        {
            return Err(format!("伺服器主機名 {bad:?} 不合法"));
        }
        if burst == 0 || burst > Self::MAX_BURST {
            return Err(format!("每輪筆數要 1 到 {}，現在 {burst}", Self::MAX_BURST));
        }
        if spacing < Self::MIN_SPACING {
            return Err(format!(
                "同一台兩筆的間隔至少 {} ms，現在 {} ms",
                Self::MIN_SPACING.as_millis(),
                spacing.as_millis()
            ));
        }
        if cycle < Self::MIN_CYCLE {
            return Err(format!(
                "兩輪的間隔至少 {} 秒，現在 {} 秒",
                Self::MIN_CYCLE.as_secs(),
                cycle.as_secs()
            ));
        }
        Ok(SyncSettings {
            servers,
            burst,
            spacing,
            cycle,
        })
    }

    pub fn from_config(config: &SamplerConfig) -> SyncSettings {
        SyncSettings {
            servers: config.servers.clone(),
            burst: config.burst,
            spacing: config.spacing,
            cycle: config.cycle,
        }
    }
}

impl SamplerConfig {
    pub fn apply(&mut self, settings: &SyncSettings) {
        self.servers = settings.servers.clone();
        self.burst = settings.burst;
        self.spacing = settings.spacing;
        self.cycle = settings.cycle;
    }
}

/// 一台伺服器的近況，給選單的「時間來源」子選單看。
#[derive(Clone, Debug, PartialEq)]
pub struct ServerStats {
    pub host: String,
    /// 最近一次解析到的位址。
    pub addr: Option<SocketAddrV4>,
    pub ok: u64,
    pub failed: u64,
    pub last_round_trip: Option<Duration>,
    pub last_half_width_ns: Option<u64>,
    pub last_stratum: Option<u8>,
    pub last_reference_id: Option<String>,
    /// 最近一次失敗的原因；成功之後清掉。
    pub last_error: Option<String>,
    /// 最近一次成功或失敗的時刻。
    pub last_at: Option<HostTime>,
    pub last_success_at: Option<HostTime>,
}

impl ServerStats {
    pub fn new(host: &str) -> ServerStats {
        ServerStats {
            host: host.to_string(),
            addr: None,
            ok: 0,
            failed: 0,
            last_round_trip: None,
            last_half_width_ns: None,
            last_stratum: None,
            last_reference_id: None,
            last_error: None,
            last_at: None,
            last_success_at: None,
        }
    }

    fn record_ok(&mut self, ex: &Exchange, now: HostTime) {
        self.ok += 1;
        self.last_round_trip = Some(ex.round_trip);
        self.last_half_width_ns = Some(ex.sample.half_width_ns());
        self.last_stratum = Some(ex.response.stratum);
        self.last_reference_id = Some(ex.response.reference_id_string());
        self.last_error = None;
        self.last_at = Some(now);
        self.last_success_at = Some(now);
    }

    fn record_failure(&mut self, error: String, now: HostTime) {
        self.failed += 1;
        self.last_error = Some(error);
        self.last_at = Some(now);
    }
}

/// 取樣執行緒的整體近況：每台伺服器、視窗裡的樣本數、丟掉幾筆、第幾輪。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SyncStatus {
    pub servers: Vec<ServerStats>,
    pub samples: usize,
    pub rejected: usize,
    pub round: u64,
    pub settings: Option<SyncSettings>,
}

/// 取樣執行緒寫、選單讀的近況槽。
#[derive(Clone, Debug, Default)]
pub struct StatusSlot(Arc<Mutex<SyncStatus>>);

impl StatusSlot {
    pub fn get(&self) -> SyncStatus {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, status: SyncStatus) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = status;
    }
}

/// 取樣執行緒對外的事件，給記錄與日後的設定視窗用。
#[derive(Debug)]
pub enum SamplerEvent {
    CycleStarted {
        round: u64,
    },
    /// 收到新的設定，已換上、接著立刻跑一輪。
    Reconfigured {
        servers: Vec<String>,
    },
    ResolveFailed {
        host: String,
        error: io::Error,
    },
    Exchange {
        host: String,
        addr: SocketAddrV4,
        result: Result<Exchange, QueryError>,
        /// 事件發生時的系統時鐘偏移，讓 `Display` 能印「遠端 − 系統時鐘」而不用自己讀時鐘。
        system_theta_ns: i128,
    },
    /// 偵測到睡眠：樣本已清空、模型已標成不可用的 `Stale`。
    SleepDetected {
        slept: Duration,
    },
    /// 收到 `pause`：模型已標成 `Frozen`，不再碰網路。
    Paused,
    /// 收到 `resume`：狀態已還原，立刻跑一輪。
    Resumed,
    Published {
        model: ClockModel,
        samples: usize,
        rejected: usize,
        system_theta_ns: i128,
    },
}

fn ms(ns: i128) -> f64 {
    ns as f64 / 1e6
}

impl fmt::Display for SamplerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SamplerEvent::CycleStarted { round } => write!(f, "第 {round} 輪取樣"),
            SamplerEvent::Reconfigured { servers } => {
                write!(f, "換上新設定：{}，立刻跑一輪", servers.join("、"))
            }
            SamplerEvent::ResolveFailed { host, error } => write!(f, "{host}：解析失敗：{error}"),
            SamplerEvent::Exchange {
                host,
                addr,
                result: Ok(ex),
                system_theta_ns,
            } => write!(
                f,
                "{host}（{}）：往返 {:.2} ms，中點 {:+.3} ms ± {:.3} ms，stratum {} / {}",
                addr.ip(),
                ex.round_trip.as_secs_f64() * 1e3,
                ms(ex.sample.midpoint_ns() - system_theta_ns),
                ms(ex.sample.half_width_ns() as i128),
                ex.response.stratum,
                ex.response.reference_id_string(),
            ),
            SamplerEvent::Exchange {
                host,
                result: Err(QueryError::NoMatchingResponse),
                ..
            } => write!(f, "{host}：逾時"),
            SamplerEvent::Exchange {
                host,
                result: Err(e),
                ..
            } => write!(f, "{host}：失敗：{e}"),
            SamplerEvent::Paused => write!(f, "暫停取樣，模型凍結"),
            SamplerEvent::Resumed => write!(f, "恢復取樣，立刻跑一輪"),
            SamplerEvent::SleepDetected { slept } => {
                write!(
                    f,
                    "偵測到睡眠 {:.1} 秒，樣本清空、重新校時",
                    slept.as_secs_f64()
                )
            }
            SamplerEvent::Published {
                model,
                samples,
                rejected,
                system_theta_ns,
            } => {
                if !model.is_usable() {
                    return write!(f, "模型 {:?}：沒有可用樣本", model.status);
                }
                let e = model.estimate_at(model.reference);
                write!(
                    f,
                    "模型 {:?}：{:+.3} ms ± {:.3} ms，漂移 {:+.1} ± {:.1} ppm，{samples} 筆，丟 {rejected} 筆",
                    model.status,
                    ms(e.remote_unix_ns - (model.reference.as_nanos() as i128 + system_theta_ns)),
                    ms(e.half_width_ns as i128),
                    model.drift * 1e6,
                    model.half_width_growth_ns_per_s / 1_000.0,
                )
            }
        }
    }
}

enum Command {
    Resample,
    Reconfigure(SyncSettings),
}

/// 取樣執行緒的控制把手。可以複製；所有把手都丟掉後，執行緒在當輪結束時退出。
#[derive(Clone)]
pub struct SamplerHandle {
    slot: ModelSlot,
    status: StatusSlot,
    commands: mpsc::Sender<Command>,
    /// 凍結旗標：主執行緒設，執行緒在每一輪前與發布前看。
    paused: Arc<AtomicBool>,
}

impl SamplerHandle {
    /// 目前發布的模型快照。
    pub fn model(&self) -> ClockModel {
        self.slot.get()
    }

    pub fn slot(&self) -> &ModelSlot {
        &self.slot
    }

    /// 每台伺服器的近況快照。
    pub fn status(&self) -> SyncStatus {
        self.status.get()
    }

    /// 不等週期到，立刻跑下一輪。執行緒已退出時靜默忽略。
    pub fn resample_now(&self) {
        let _ = self.commands.send(Command::Resample);
    }

    /// 換伺服器清單與節奏：沒凍結就立刻換上並跑一輪；凍結中先記著，解凍那一輪前套用。
    /// 舊伺服器的樣本留著讓視窗自然老化。
    pub fn reconfigure(&self, settings: SyncSettings) {
        let _ = self.commands.send(Command::Reconfigure(settings));
    }

    /// 凍結：槽裡的模型**立刻**標成 `Frozen`（在呼叫端的執行緒上做，之後讀到的一定是
    /// 凍結的那一份），執行緒跑到一半的那一輪不發布、之後不碰網路，直到 [`resume`](Self::resume)。
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        let mut model = self.slot.get();
        if model.is_usable() {
            model.status = ModelStatus::Frozen;
            self.slot.set(model);
        }
        // 叫醒在等週期的執行緒，讓它進入等待、發出 Paused 事件。
        let _ = self.commands.send(Command::Resample);
    }

    /// 解凍並立刻跑一輪；模型狀態要等那一輪發布才會從 `Frozen` 變回來。
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        let _ = self.commands.send(Command::Resample);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
}

/// 啟動取樣執行緒。socket 在呼叫端建立，開不起來就立刻回錯。
pub fn spawn(
    config: SamplerConfig,
    on_event: Box<dyn FnMut(SamplerEvent) + Send>,
) -> io::Result<SamplerHandle> {
    let sock = TimestampedUdpSocket::new_ipv4(config.socket_timeout)?;
    let slot = ModelSlot::new(ClockModel::uncalibrated(
        SourceKind::Standard,
        HostTime::now(),
    ));
    let (tx, rx) = mpsc::channel();
    let paused = Arc::new(AtomicBool::new(false));
    let status = StatusSlot::default();
    let stats: Vec<ServerStats> = config.servers.iter().map(|h| ServerStats::new(h)).collect();
    status.set(SyncStatus {
        servers: stats.clone(),
        settings: Some(SyncSettings::from_config(&config)),
        ..SyncStatus::default()
    });

    let worker = Worker {
        estimator: IntervalEstimator::new(SourceKind::Standard, config.estimator),
        sleep: SleepDetector::new(config.sleep_threshold),
        config,
        sock,
        slot: slot.clone(),
        status: status.clone(),
        stats,
        pending: None,
        on_event,
        round: 0,
        last_success: None,
        woke_since_success: false,
        paused: paused.clone(),
    };
    thread::Builder::new()
        .name("kairos-sampler".into())
        .spawn(move || worker.run(rx))?;

    Ok(SamplerHandle {
        slot,
        status,
        commands: tx,
        paused,
    })
}

struct Worker {
    config: SamplerConfig,
    sock: TimestampedUdpSocket,
    estimator: IntervalEstimator,
    sleep: SleepDetector,
    slot: ModelSlot,
    status: StatusSlot,
    /// 與 `config.servers` 同順序。
    stats: Vec<ServerStats>,
    /// 收到但還沒套用的設定（凍結中收到的會等到解凍）。
    pending: Option<SyncSettings>,
    on_event: Box<dyn FnMut(SamplerEvent) + Send>,
    round: u64,
    /// 最近一筆成功樣本的時刻。
    last_success: Option<HostTime>,
    /// 睡眠清空之後還沒有拿到新樣本；這段期間發布的是「不可用的 Stale」而不是 Uncalibrated。
    woke_since_success: bool,
    paused: Arc<AtomicBool>,
}

/// 一輪的結果：正常結束，或中途睡過、樣本已清空、應該立刻再跑一輪。
enum CycleOutcome {
    Published,
    SleptMidCycle,
}

impl Worker {
    fn run(mut self, rx: mpsc::Receiver<Command>) {
        loop {
            if self.paused.load(Ordering::SeqCst) && !self.wait_while_paused(&rx) {
                return;
            }
            self.apply_pending();
            if let CycleOutcome::SleptMidCycle = self.cycle() {
                continue;
            }
            match rx.recv_timeout(self.config.cycle) {
                Ok(command) => {
                    self.absorb(command);
                    // 排隊的多個要求合併成一輪；設定以最後一份為準。
                    while let Ok(c) = rx.try_recv() {
                        self.absorb(c);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn absorb(&mut self, command: Command) {
        match command {
            Command::Resample => {}
            Command::Reconfigure(settings) => self.pending = Some(settings),
        }
    }

    /// 凍結期間只等命令、不碰網路；旗標放開就回去立刻跑一輪。
    /// 回傳 false 代表所有把手都丟掉了，執行緒該退出。
    fn wait_while_paused(&mut self, rx: &mpsc::Receiver<Command>) -> bool {
        self.emit(SamplerEvent::Paused);
        while self.paused.load(Ordering::SeqCst) {
            match rx.recv() {
                Ok(command) => self.absorb(command),
                Err(_) => return false,
            }
        }
        self.emit(SamplerEvent::Resumed);
        true
    }

    /// 換上排隊中的設定：還在清單上的伺服器統計保留，新的從零開始。
    fn apply_pending(&mut self) {
        let Some(settings) = self.pending.take() else {
            return;
        };
        self.config.apply(&settings);
        let old = std::mem::take(&mut self.stats);
        self.stats = settings
            .servers
            .iter()
            .map(|host| {
                old.iter()
                    .find(|s| &s.host == host)
                    .cloned()
                    .unwrap_or_else(|| ServerStats::new(host))
            })
            .collect();
        self.publish_status();
        self.emit(SamplerEvent::Reconfigured {
            servers: settings.servers,
        });
    }

    fn publish_status(&self) {
        self.status.set(SyncStatus {
            servers: self.stats.clone(),
            samples: self.estimator.len(),
            rejected: self.estimator.rejected(),
            round: self.round,
            settings: Some(SyncSettings::from_config(&self.config)),
        });
    }

    fn emit(&mut self, event: SamplerEvent) {
        (self.on_event)(event);
    }

    fn cycle(&mut self) -> CycleOutcome {
        self.round += 1;
        if let Some(slept) = self.sleep.check() {
            self.invalidate_after_sleep(slept);
        }
        self.emit(SamplerEvent::CycleStarted { round: self.round });

        let system_theta = system_theta_ns();
        let servers = self.config.servers.clone();
        for (i, host) in servers.into_iter().enumerate() {
            let addr = match resolve_ipv4(&host, self.config.port) {
                Ok(addrs) => addrs[0],
                Err(error) => {
                    if let Some(s) = self.stats.get_mut(i) {
                        s.record_failure(format!("解析失敗：{error}"), HostTime::now());
                    }
                    self.publish_status();
                    self.emit(SamplerEvent::ResolveFailed { host, error });
                    continue;
                }
            };
            if let Some(s) = self.stats.get_mut(i) {
                s.addr = Some(addr);
            }
            let results = query_burst(
                &self.sock,
                addr,
                SourceKind::Standard,
                self.config.burst,
                self.config.spacing,
            );
            for result in results {
                let now = HostTime::now();
                match &result {
                    Ok(ex) => {
                        self.estimator.push(ex.sample);
                        self.last_success = Some(ex.sample.at);
                        self.woke_since_success = false;
                        if let Some(s) = self.stats.get_mut(i) {
                            s.record_ok(ex, now);
                        }
                    }
                    Err(QueryError::NoMatchingResponse) => {
                        if let Some(s) = self.stats.get_mut(i) {
                            s.record_failure("逾時".into(), now);
                        }
                    }
                    Err(e) => {
                        if let Some(s) = self.stats.get_mut(i) {
                            s.record_failure(e.to_string(), now);
                        }
                    }
                }
                self.emit(SamplerEvent::Exchange {
                    host: host.clone(),
                    addr,
                    result,
                    system_theta_ns: system_theta,
                });
            }
            // 每台跑完就更新，選單不用等整輪。
            self.publish_status();
        }

        // 這一輪跑到一半睡過：剛推進去的樣本混了睡前睡後，整個丟掉，不發布混合的模型。
        if let Some(slept) = self.sleep.check() {
            self.invalidate_after_sleep(slept);
            return CycleOutcome::SleptMidCycle;
        }

        // 這一輪跑到一半被凍結：樣本留著，但不發布，槽裡維持凍結的那一份。
        if self.paused.load(Ordering::SeqCst) {
            return CycleOutcome::Published;
        }

        let now = HostTime::now();
        let model = apply_staleness(
            self.estimator.model(now),
            now,
            self.last_success,
            self.config.stale_after,
            self.woke_since_success,
        );
        self.slot.set(model);
        // `model()` 才會重算剔除數，發布模型後再更新一次近況。
        self.publish_status();
        self.emit(SamplerEvent::Published {
            model,
            samples: self.estimator.len(),
            rejected: self.estimator.rejected(),
            system_theta_ns: system_theta,
        });
        CycleOutcome::Published
    }

    fn invalidate_after_sleep(&mut self, slept: Duration) {
        self.estimator.invalidate();
        self.last_success = None;
        self.woke_since_success = true;
        let mut model = ClockModel::uncalibrated(SourceKind::Standard, HostTime::now());
        model.status = ModelStatus::Stale;
        self.slot.set(model);
        self.emit(SamplerEvent::SleepDetected { slept });
    }
}

/// 在估計器算出的狀態上套兩條規則：喚醒後還沒拿到新樣本 → 不可用的 `Stale`；
/// 太久沒有成功樣本 → 數字照舊但標 `Stale`。
fn apply_staleness(
    mut model: ClockModel,
    now: HostTime,
    last_success: Option<HostTime>,
    stale_after: Duration,
    woke_since_success: bool,
) -> ClockModel {
    match last_success {
        None if woke_since_success => model.status = ModelStatus::Stale,
        None => {}
        Some(t) if now.saturating_duration_since(t) > stale_after => {
            model.status = ModelStatus::Stale;
        }
        Some(_) => {}
    }
    model
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_roundtrips_model() {
        let m0 = ClockModel::uncalibrated(SourceKind::Standard, HostTime::from_ticks(1));
        let slot = ModelSlot::new(m0);
        assert_eq!(slot.get(), m0);

        let mut m1 = m0;
        m1.offset_ns = 42;
        m1.status = ModelStatus::Tracking;
        let other = slot.clone();
        other.set(m1);
        assert_eq!(slot.get(), m1);
    }

    #[test]
    fn sleep_detector_ignores_jitter_below_threshold() {
        let mut d = SleepDetector::from_readings(1_000, 5_000, 100);
        // 差值 4_000 → 4_050：只多 50，低於門檻。
        assert_eq!(d.check_readings(2_000, 6_050), None);
        // 基準沒動，累積到 4_120 才報。
        assert_eq!(d.check_readings(3_000, 7_120), Some(120));
        // 之後以新的差值為基準。
        assert_eq!(d.check_readings(4_000, 8_120), None);
    }

    #[test]
    fn sleep_detector_reports_each_sleep_once() {
        let mut d = SleepDetector::from_readings(0, 0, 10);
        assert_eq!(d.check_readings(100, 100), None);
        assert_eq!(d.check_readings(200, 1_200), Some(1_000));
        assert_eq!(d.check_readings(300, 1_300), None);
        assert_eq!(d.check_readings(400, 3_400), Some(2_000));
    }

    #[test]
    fn sleep_detector_on_live_clocks_is_quiet_right_after_start() {
        let mut d = SleepDetector::new(Duration::from_millis(500));
        assert_eq!(d.check(), None);
        assert_eq!(d.check(), None);
    }

    #[test]
    fn sleep_detector_tolerates_continuous_read_before_absolute() {
        // continuous 比 absolute 先讀，差值可能短暫小於基準，不能 panic 也不能誤報。
        let mut d = SleepDetector::from_readings(1_000, 1_000, 10);
        assert_eq!(d.check_readings(2_005, 2_000), None);
    }

    /// 指向沒人在聽的 loopback 埠：每筆都逾時，但整條執行緒的流程跑得到。
    fn loopback_config() -> SamplerConfig {
        SamplerConfig {
            servers: vec!["127.0.0.1".into()],
            port: 9,
            burst: 1,
            spacing: Duration::ZERO,
            cycle: Duration::from_secs(60),
            socket_timeout: Duration::from_millis(50),
            ..SamplerConfig::default()
        }
    }

    #[test]
    fn resample_now_starts_a_new_cycle_before_the_period_ends() {
        let (tx, rx) = mpsc::channel();
        let handle = spawn(
            loopback_config(),
            Box::new(move |e| {
                let _ = tx.send(e);
            }),
        )
        .unwrap();

        let mut rounds = Vec::new();
        let deadline = Duration::from_secs(5);
        // 第一輪：開始、一筆逾時、發布 Uncalibrated。
        while rounds.is_empty() {
            match rx.recv_timeout(deadline).unwrap() {
                SamplerEvent::Published { model, .. } => {
                    assert_eq!(model.status, ModelStatus::Uncalibrated);
                    rounds.push(model);
                }
                SamplerEvent::Exchange { result, .. } => assert!(result.is_err()),
                _ => {}
            }
        }
        assert!(!handle.model().is_usable());

        // 週期是 60 秒，靠 resample_now 才會在幾秒內看到第二輪。
        handle.resample_now();
        loop {
            if let SamplerEvent::CycleStarted { round: 2 } = rx.recv_timeout(deadline).unwrap() {
                break;
            }
        }
    }

    #[test]
    fn pause_freezes_the_model_and_resume_runs_a_cycle() {
        let (tx, rx) = mpsc::channel();
        let handle = spawn(
            loopback_config(),
            Box::new(move |e| {
                let _ = tx.send(e);
            }),
        )
        .unwrap();
        let deadline = Duration::from_secs(5);
        loop {
            if let SamplerEvent::Published { .. } = rx.recv_timeout(deadline).unwrap() {
                break;
            }
        }
        // 放一個可用的模型進槽，暫停後它要立刻變成 Frozen、數值不變。
        let before = tracking(HostTime::now());
        handle.slot().set(before);
        handle.pause();
        let frozen = handle.model();
        assert_eq!(frozen.status, ModelStatus::Frozen);
        assert_eq!(frozen.offset_ns, before.offset_ns);
        assert!(frozen.is_usable());
        assert!(handle.is_paused());
        loop {
            if let SamplerEvent::Paused = rx.recv_timeout(deadline).unwrap() {
                break;
            }
        }
        // 凍結期間沒有新的一輪。
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(300)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        handle.resume();
        let mut resumed = false;
        loop {
            match rx.recv_timeout(deadline).unwrap() {
                SamplerEvent::Resumed => resumed = true,
                SamplerEvent::Published { model, .. } if resumed => {
                    assert_ne!(model.status, ModelStatus::Frozen);
                    break;
                }
                _ => {}
            }
        }
        assert_ne!(handle.model().status, ModelStatus::Frozen);
    }

    fn tracking(reference: HostTime) -> ClockModel {
        ClockModel {
            source: SourceKind::Standard,
            status: ModelStatus::Tracking,
            reference,
            offset_ns: 1_700_000_000_000_000_000,
            drift: 0.0,
            half_width_ns: 5_000_000,
            half_width_growth_ns_per_s: 100.0,
        }
    }

    #[test]
    fn sync_settings_are_validated() {
        let ok = SyncSettings::new(
            vec![" a.example ".into(), "b.example".into()],
            4,
            Duration::from_secs(1),
            Duration::from_secs(180),
        )
        .unwrap();
        assert_eq!(ok.servers, vec!["a.example", "b.example"]);
        let s = Duration::from_secs(1);
        let c = Duration::from_secs(180);
        assert!(SyncSettings::new(vec![], 4, s, c).is_err());
        assert!(SyncSettings::new(vec!["x".into(); 9], 4, s, c).is_err());
        assert!(SyncSettings::new(vec!["a b".into()], 4, s, c).is_err());
        assert!(SyncSettings::new(vec!["".into()], 4, s, c).is_err());
        assert!(SyncSettings::new(vec!["a".into()], 0, s, c).is_err());
        assert!(SyncSettings::new(vec!["a".into()], 9, s, c).is_err());
        assert!(SyncSettings::new(vec!["a".into()], 4, Duration::from_millis(100), c).is_err());
        assert!(SyncSettings::new(vec!["a".into()], 4, s, Duration::from_secs(30)).is_err());
        let from = SyncSettings::from_config(&SamplerConfig::default());
        assert_eq!(from.servers.len(), 3);
        assert_eq!(from.cycle, Duration::from_secs(180));
    }

    #[test]
    fn status_counts_failures_and_reconfigure_runs_a_cycle_immediately() {
        let (tx, rx) = mpsc::channel();
        let handle = spawn(
            loopback_config(),
            Box::new(move |e| {
                let _ = tx.send(e);
            }),
        )
        .unwrap();
        let deadline = Duration::from_secs(5);
        let initial = handle.status();
        assert_eq!(initial.servers.len(), 1);
        assert_eq!(initial.servers[0].host, "127.0.0.1");
        assert_eq!(initial.settings.as_ref().unwrap().burst, 1);
        loop {
            if let SamplerEvent::Published { .. } = rx.recv_timeout(deadline).unwrap() {
                break;
            }
        }
        let after = handle.status();
        assert_eq!(after.round, 1);
        let s = &after.servers[0];
        assert_eq!((s.ok, s.failed), (0, 1));
        assert_eq!(s.last_error.as_deref(), Some("逾時"));
        assert_eq!(s.addr.map(|a| a.port()), Some(9));
        assert!(s.last_at.is_some() && s.last_success_at.is_none());

        // 換成兩台（都指向沒人聽的 loopback）：立刻換上、第二輪馬上開始，舊的統計保留。
        let settings = SyncSettings::new(
            vec!["127.0.0.1".into(), "localhost".into()],
            1,
            Duration::from_millis(200),
            Duration::from_secs(60),
        )
        .unwrap();
        handle.reconfigure(settings.clone());
        let mut reconfigured = false;
        loop {
            match rx.recv_timeout(deadline).unwrap() {
                SamplerEvent::Reconfigured { servers } => {
                    assert_eq!(servers, settings.servers);
                    reconfigured = true;
                }
                SamplerEvent::CycleStarted { round: 2 } => {
                    assert!(reconfigured, "要先換設定再開新的一輪");
                    break;
                }
                _ => {}
            }
        }
        let status = handle.status();
        assert_eq!(status.settings.as_ref(), Some(&settings));
        assert_eq!(status.servers.len(), 2);
        assert_eq!(status.servers[0].failed, 1, "還在清單上的伺服器統計要保留");
        assert_eq!(status.servers[1].host, "localhost");
    }

    #[test]
    fn reconfigure_while_paused_waits_for_resume() {
        let (tx, rx) = mpsc::channel();
        let handle = spawn(
            loopback_config(),
            Box::new(move |e| {
                let _ = tx.send(e);
            }),
        )
        .unwrap();
        let deadline = Duration::from_secs(5);
        loop {
            if let SamplerEvent::Published { .. } = rx.recv_timeout(deadline).unwrap() {
                break;
            }
        }
        handle.pause();
        loop {
            if let SamplerEvent::Paused = rx.recv_timeout(deadline).unwrap() {
                break;
            }
        }
        let settings = SyncSettings::new(
            vec!["localhost".into()],
            1,
            Duration::from_millis(200),
            Duration::from_secs(60),
        )
        .unwrap();
        handle.reconfigure(settings.clone());
        // 凍結中不換、不跑。
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(300)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(handle.status().servers[0].host, "127.0.0.1");

        handle.resume();
        let mut seen = Vec::new();
        loop {
            match rx.recv_timeout(deadline).unwrap() {
                SamplerEvent::Resumed => seen.push("resumed"),
                SamplerEvent::Reconfigured { .. } => seen.push("reconfigured"),
                SamplerEvent::CycleStarted { round: 2 } => {
                    seen.push("cycle");
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(seen, ["resumed", "reconfigured", "cycle"]);
        assert_eq!(handle.status().servers[0].host, "localhost");
    }

    #[test]
    fn staleness_after_wake_without_samples_is_unusable_stale() {
        let now = HostTime::from_nanos(1_000_000_000_000);
        let m = apply_staleness(
            ClockModel::uncalibrated(SourceKind::Standard, now),
            now,
            None,
            Duration::from_secs(600),
            true,
        );
        assert_eq!(m.status, ModelStatus::Stale);
        assert!(!m.is_usable());
    }

    #[test]
    fn staleness_without_wake_and_without_samples_stays_uncalibrated() {
        let now = HostTime::from_nanos(1_000_000_000_000);
        let m = apply_staleness(
            ClockModel::uncalibrated(SourceKind::Standard, now),
            now,
            None,
            Duration::from_secs(600),
            false,
        );
        assert_eq!(m.status, ModelStatus::Uncalibrated);
    }

    #[test]
    fn staleness_keeps_numbers_when_samples_are_old() {
        let now = HostTime::from_nanos(1_000_000_000_000);
        let old = now - Duration::from_secs(601);
        let m = apply_staleness(
            tracking(now),
            now,
            Some(old),
            Duration::from_secs(600),
            false,
        );
        assert_eq!(m.status, ModelStatus::Stale);
        assert!(m.is_usable());
        assert_eq!(m.offset_ns, tracking(now).offset_ns);

        let fresh = now - Duration::from_secs(30);
        let m = apply_staleness(
            tracking(now),
            now,
            Some(fresh),
            Duration::from_secs(600),
            false,
        );
        assert_eq!(m.status, ModelStatus::Tracking);
    }
}
