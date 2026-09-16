//! 目標時刻模式的純邏輯：提前量公式、狀態機、反應時間校正的算術。
//!
//! 不碰時鐘、網路、視窗。app 每一格拿「現在的標準時間估計」來問 [`TargetMachine::step`]，
//! 照吐出來的 [`Action`] 做：量測（叫取樣執行緒立刻跑一輪）、鎖定（凍結模型、排拍點表、
//! 開聲音與光暈）、結束（解凍）、中止。
//!
//! ```text
//! 待命 ──歸零前 measure_before──▶ 量測 ──歸零前 lock_before──▶ 鎖定 ──節拍跑完──▶ 結束
//! ```

use std::time::Duration;

/// 歸零時刻＝目標時刻－反應時間－瀏覽器處理時間－單程延遲＋安全餘量。
/// 安全餘量為正：請求早於目標時刻抵達可能被拒，寧可晚一點點。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LeadParams {
    pub reaction_ms: f64,
    pub browser_ms: f64,
    pub one_way_ms: f64,
    pub safety_ms: f64,
}

impl Default for LeadParams {
    fn default() -> Self {
        LeadParams {
            reaction_ms: 200.0,
            browser_ms: 50.0,
            one_way_ms: 15.0,
            safety_ms: 50.0,
        }
    }
}

impl LeadParams {
    /// 歸零比目標早多少毫秒。安全餘量大於其他三項時為負，歸零在目標之後。
    pub fn total_ms(&self) -> f64 {
        self.reaction_ms + self.browser_ms + self.one_way_ms - self.safety_ms
    }

    pub fn zero_for(&self, target_unix_ns: i128) -> i128 {
        target_unix_ns - (self.total_ms() * 1e6).round() as i128
    }
}

pub const DEFAULT_MEASURE_BEFORE: Duration = Duration::from_secs(900);
pub const DEFAULT_LOCK_BEFORE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TargetConfig {
    /// 目標時刻，標準時間的 Unix 奈秒。
    pub target_unix_ns: i128,
    pub lead: LeadParams,
    /// 歸零前多久進入量測。
    pub measure_before: Duration,
    /// 歸零前多久鎖定。
    pub lock_before: Duration,
}

impl TargetConfig {
    pub fn zero_unix_ns(&self) -> i128 {
        self.lead.zero_for(self.target_unix_ns)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortReason {
    /// 歸零時刻已過。
    TargetPassed,
    /// 還沒鎖定就已經離歸零太近，拍點表排不進去。
    TooLateToLock,
    /// 使用者手動停止倒數。
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// 有目標，等時間到。
    Armed,
    /// 已要求重新校時，等鎖定。
    Measuring,
    /// 模型已凍結、拍點表已排，節拍程序在跑。
    Locked,
    /// 節拍程序跑完。
    Done,
    Aborted(AbortReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Measure,
    Lock,
    Finish,
    Abort(AbortReason),
}

/// 目標時刻的狀態機。值都以標準時間表示；主機時間的換算在 app 鎖定時做一次。
#[derive(Clone, Debug)]
pub struct TargetMachine {
    config: TargetConfig,
    stage: Stage,
    /// 鎖定至少要在歸零前這麼久：畫面起跑所需的時間，加上音訊暖機。
    min_lock_margin: Duration,
}

impl TargetMachine {
    /// `min_lock_margin` 由 app 依拍點表算：畫面起跑比歸零早多久，加 0.5 秒。
    /// `lock_before` 至少會比它多一秒；`measure_before` 不會早於 `lock_before`。
    pub fn new(mut config: TargetConfig, min_lock_margin: Duration) -> TargetMachine {
        config.lock_before = config
            .lock_before
            .max(min_lock_margin + Duration::from_secs(1));
        config.measure_before = config.measure_before.max(config.lock_before);
        TargetMachine {
            config,
            stage: Stage::Armed,
            min_lock_margin,
        }
    }

    pub fn config(&self) -> &TargetConfig {
        &self.config
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    pub fn is_locked(&self) -> bool {
        self.stage == Stage::Locked
    }

    /// 已經結束或中止，不會再有動作。
    pub fn is_over(&self) -> bool {
        matches!(self.stage, Stage::Done | Stage::Aborted(_))
    }

    /// 走一步。`now_remote` 是模型可用時「現在」的標準時間估計（Unix 奈秒），不可用時
    /// 給 `None`，狀態機就原地等；`session_over` 是鎖定後 app 回報節拍程序已跑完。
    pub fn step(&mut self, now_remote: Option<i128>, session_over: bool) -> Option<Action> {
        match self.stage {
            Stage::Locked => {
                if session_over {
                    self.stage = Stage::Done;
                    Some(Action::Finish)
                } else {
                    None
                }
            }
            Stage::Done | Stage::Aborted(_) => None,
            Stage::Armed | Stage::Measuring => {
                let now = now_remote?;
                let remaining = self.config.zero_unix_ns() - now;
                if remaining < self.min_lock_margin.as_nanos() as i128 {
                    let reason = if remaining <= 0 {
                        AbortReason::TargetPassed
                    } else {
                        AbortReason::TooLateToLock
                    };
                    self.stage = Stage::Aborted(reason);
                    return Some(Action::Abort(reason));
                }
                if remaining <= self.config.lock_before.as_nanos() as i128 {
                    self.stage = Stage::Locked;
                    return Some(Action::Lock);
                }
                if self.stage == Stage::Armed
                    && remaining <= self.config.measure_before.as_nanos() as i128
                {
                    self.stage = Stage::Measuring;
                    return Some(Action::Measure);
                }
                None
            }
        }
    }

    /// app 鎖定時排不進拍點表（例如剛從睡眠醒來）就呼叫這個。
    pub fn abort(&mut self, reason: AbortReason) {
        self.stage = Stage::Aborted(reason);
    }
}

/// 反應時間校正：每輪只取離歸零最近、落在 `[zero − before, zero + after]` 內的那一次按鍵。
pub const CALIBRATION_BEFORE: Duration = Duration::from_millis(300);
pub const CALIBRATION_AFTER: Duration = Duration::from_millis(700);
pub const CALIBRATION_ROUNDS: u32 = 5;

/// 一輪的反應：事件時刻減歸零時刻（秒），可為負（人會預測）。窗內沒有事件回 `None`。
pub fn pick_reaction(
    events_s: &[f64],
    zero_s: f64,
    before: Duration,
    after: Duration,
) -> Option<f64> {
    events_s
        .iter()
        .map(|e| e - zero_s)
        .filter(|d| *d >= -before.as_secs_f64() && *d <= after.as_secs_f64())
        .min_by(|a, b| a.abs().total_cmp(&b.abs()))
}

pub fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i128 = 1_000_000_000;
    const TARGET: i128 = 1_800_000_000 * S;

    fn config() -> TargetConfig {
        TargetConfig {
            target_unix_ns: TARGET,
            lead: LeadParams::default(),
            measure_before: DEFAULT_MEASURE_BEFORE,
            lock_before: DEFAULT_LOCK_BEFORE,
        }
    }

    fn machine() -> TargetMachine {
        TargetMachine::new(config(), Duration::from_millis(4_500))
    }

    #[test]
    fn default_lead_is_215_ms_before_target() {
        let lead = LeadParams::default();
        assert_eq!(lead.total_ms(), 215.0);
        assert_eq!(lead.zero_for(TARGET), TARGET - 215_000_000);
        let negative = LeadParams {
            safety_ms: 500.0,
            ..LeadParams::default()
        };
        assert!(negative.zero_for(TARGET) > TARGET);
    }

    #[test]
    fn happy_path_measure_lock_finish() {
        let mut m = machine();
        let zero = m.config().zero_unix_ns();
        assert_eq!(m.step(Some(zero - 20 * 60 * S), false), None);
        assert_eq!(m.stage(), Stage::Armed);
        assert_eq!(
            m.step(Some(zero - 15 * 60 * S), false),
            Some(Action::Measure)
        );
        assert_eq!(m.stage(), Stage::Measuring);
        assert_eq!(m.step(Some(zero - 10 * 60 * S), false), None);
        assert_eq!(m.step(Some(zero - 60 * S), false), Some(Action::Lock));
        assert!(m.is_locked());
        assert_eq!(m.step(Some(zero - 30 * S), false), None);
        assert_eq!(m.step(None, false), None);
        assert_eq!(m.step(Some(zero + 2 * S), true), Some(Action::Finish));
        assert_eq!(m.stage(), Stage::Done);
        assert!(m.is_over());
        assert_eq!(m.step(Some(zero + 3 * S), true), None);
    }

    #[test]
    fn late_start_locks_directly_or_aborts() {
        let mut m = machine();
        let zero = m.config().zero_unix_ns();
        assert_eq!(m.step(Some(zero - 30 * S), false), Some(Action::Lock));

        let mut m = machine();
        assert_eq!(
            m.step(Some(zero - 2 * S), false),
            Some(Action::Abort(AbortReason::TooLateToLock))
        );
        assert_eq!(m.stage(), Stage::Aborted(AbortReason::TooLateToLock));

        let mut m = machine();
        assert_eq!(
            m.step(Some(zero + 1), false),
            Some(Action::Abort(AbortReason::TargetPassed))
        );
        assert_eq!(m.step(Some(zero + 2), false), None);
    }

    #[test]
    fn unusable_model_waits_in_place() {
        let mut m = machine();
        assert_eq!(m.step(None, false), None);
        assert_eq!(m.stage(), Stage::Armed);
        let zero = m.config().zero_unix_ns();
        m.step(Some(zero - 15 * 60 * S), false);
        assert_eq!(m.step(None, false), None);
        assert_eq!(m.stage(), Stage::Measuring);
    }

    #[test]
    fn new_keeps_lock_before_above_the_margin() {
        let cfg = TargetConfig {
            lock_before: Duration::from_secs(2),
            measure_before: Duration::from_secs(1),
            ..config()
        };
        let m = TargetMachine::new(cfg, Duration::from_millis(4_500));
        assert_eq!(m.config().lock_before, Duration::from_millis(5_500));
        assert_eq!(m.config().measure_before, Duration::from_millis(5_500));
    }

    #[test]
    fn pick_reaction_takes_the_nearest_event_in_the_window() {
        let z = 1000.0;
        let events = [z - 1.0, z + 0.18, z + 0.25, z + 2.0];
        let r = pick_reaction(&events, z, CALIBRATION_BEFORE, CALIBRATION_AFTER).unwrap();
        assert!((r - 0.18).abs() < 1e-9);
        assert_eq!(
            pick_reaction(
                &[z - 0.4, z + 0.8],
                z,
                CALIBRATION_BEFORE,
                CALIBRATION_AFTER
            ),
            None
        );
        let early = pick_reaction(&[z - 0.05], z, CALIBRATION_BEFORE, CALIBRATION_AFTER).unwrap();
        assert!((early + 0.05).abs() < 1e-9);
        assert_eq!(
            pick_reaction(&[], z, CALIBRATION_BEFORE, CALIBRATION_AFTER),
            None
        );
    }

    #[test]
    fn median_handles_odd_even_and_empty() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }
}
