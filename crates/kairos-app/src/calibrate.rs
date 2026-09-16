//! 反應時間校正：用本地事件監聽收按鍵與滑鼠的時間戳，跟節拍程序的歸零時刻相減。
//!
//! `NSEvent.addLocalMonitorForEventsMatchingMask:handler:` 只看本程式的事件、不需要任何權限；
//! `timestamp` 與 `mach_absolute_time` 同基準，跟拍點表可以直接相減。量到的是
//! 「提示輸出＋人＋輸入裝置」整條鏈，正是要補償的量。

use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask};

use kairos_core::target::{CALIBRATION_AFTER, CALIBRATION_BEFORE, median, pick_reaction};
use kairos_core::time::HostTime;

/// 事件時間戳的收集器。被 drop 就取消監聽。
struct EventTap {
    monitor: Retained<AnyObject>,
    events_s: Rc<RefCell<Vec<f64>>>,
}

impl EventTap {
    fn install() -> Option<EventTap> {
        let events_s = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&events_s);
        let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit 交來的事件指標在 block 執行期間有效。
            let e = unsafe { event.as_ref() };
            if !e.isARepeat() {
                sink.borrow_mut().push(e.timestamp());
            }
            event.as_ptr()
        });
        let mask = NSEventMask::KeyDown
            | NSEventMask::LeftMouseDown
            | NSEventMask::RightMouseDown
            | NSEventMask::OtherMouseDown;
        // SAFETY: block 型別與簽章相符；只在主執行緒安裝與移除。
        let monitor =
            unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) }?;
        Some(EventTap { monitor, events_s })
    }

    fn drain(&self) -> Vec<f64> {
        std::mem::take(&mut *self.events_s.borrow_mut())
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        // SAFETY: 這個物件就是 addLocalMonitor 回傳的監聽器。
        unsafe { NSEvent::removeMonitor(&self.monitor) };
    }
}

/// 一次校正：連跑幾輪節拍，每輪取離歸零最近的一次按鍵，最後取中位數。
pub struct CalibrationRun {
    tap: EventTap,
    total: u32,
    /// 每輪的反應（毫秒），窗內沒按到就是 `None`。
    pub rounds: Vec<Option<f64>>,
}

impl CalibrationRun {
    /// 安裝監聽失敗（理論上不會）回 `None`。
    pub fn new(total: u32) -> Option<CalibrationRun> {
        Some(CalibrationRun {
            tap: EventTap::install()?,
            total: total.max(1),
            rounds: Vec::new(),
        })
    }

    pub fn total(&self) -> u32 {
        self.total
    }

    /// 下一輪是第幾輪（1 起算）。
    pub fn next_round(&self) -> u32 {
        self.rounds.len() as u32 + 1
    }

    pub fn is_complete(&self) -> bool {
        self.rounds.len() as u32 >= self.total
    }

    /// 一輪跑完：從這輪收到的事件裡挑出反應。回傳這輪的結果（毫秒）。
    pub fn finish_round(&mut self, zero: HostTime) -> Option<f64> {
        let events = self.tap.drain();
        let zero_s = zero.as_nanos() as f64 / 1e9;
        let reaction =
            pick_reaction(&events, zero_s, CALIBRATION_BEFORE, CALIBRATION_AFTER).map(|s| s * 1e3);
        self.rounds.push(reaction);
        reaction
    }

    /// 開新一輪前把上一輪殘留的事件清掉。
    pub fn clear_events(&self) {
        self.tap.drain();
    }

    pub fn samples_ms(&self) -> Vec<f64> {
        self.rounds.iter().flatten().copied().collect()
    }

    pub fn median_ms(&self) -> Option<f64> {
        median(&self.samples_ms())
    }

    /// 給面板看的一行。
    pub fn summary(&self) -> String {
        let rounds: Vec<String> = self
            .rounds
            .iter()
            .enumerate()
            .map(|(i, r)| match r {
                Some(ms) => format!("第 {} 輪 {ms:+.0} ms", i + 1),
                None => format!("第 {} 輪 沒按到", i + 1),
            })
            .collect();
        match self.median_ms() {
            Some(m) => format!("{}；中位 {m:+.0} ms", rounds.join("、")),
            None => format!("{}；沒有有效的輪次", rounds.join("、")),
        }
    }
}
