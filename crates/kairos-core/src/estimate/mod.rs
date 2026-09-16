//! 估計器層。吃 [`Sample`]、產出 [`ClockModel`]。
//!
//! 階段一會在這裡實作：每筆樣本在 (θ₀, ρ) 平面上是一條帶狀區域，滑動視窗內
//! 所有帶狀區域的交集是凸多邊形；解兩個小型線性規劃求出現在時刻 θ 的上下界，
//! 交集為空時逐一剔除衝突最大的樣本（Marzullo 式）。
//!
//! 估計器是純函數式的狀態機：沒有執行緒、沒有時鐘、沒有網路，
//! 所以可以用合成樣本做性質測試，確認真值永遠落在回報的區間內。

pub mod interval;
pub mod lp2;

use crate::model::ClockModel;
use crate::source::Sample;
use crate::time::HostTime;

pub trait Estimator {
    /// 餵入一筆新樣本。
    fn push(&mut self, sample: Sample);

    /// 以目前所有樣本，產出在 `now` 這一刻最緊的模型。
    fn model(&self, now: HostTime) -> ClockModel;

    /// 睡眠喚醒等事件後呼叫：丟棄所有樣本，模型退回未校正。
    fn invalidate(&mut self);
}
