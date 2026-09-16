//! kairos 的純 Rust 核心。不碰任何 macOS 框架，測試可以直接跑。
//!
//! 分三層，層與層之間只傳遞不可變的值：
//!
//! ```text
//! source（時間來源）──Sample──▶ estimate（估計器）──ClockModel──▶ 呈現層（kairos-app）
//! ```
//!
//! - [`time`]：全程式唯一的時間基準 `mach_absolute_time`，以 [`time::HostTime`] 表示。
//! - [`source`]：每筆量測是偏移 θ 的硬性上下界，型別為 [`source::Sample`]。
//! - [`estimate`]：吃樣本、產出 [`model::ClockModel`]；標準時間與指定網站各跑一個實例。
//! - [`model`]：不可變的時鐘模型，呈現層每一格讀一次、用 `estimate_at` 換出當下的遠端時間與不確定度。
//! - [`sync`]：背景執行緒定期取樣、算模型、發布到 [`sync::ModelSlot`]；睡眠偵測也在這裡。
//! - [`display`]：呈現層用的純邏輯——顯示偏移平滑器、本地時分秒拆解。
//! - [`beat`]：以主機時間表示的拍點表，以及滴答聲與節拍元件的純函數；畫面、聲音、光暈共用同一份表。

pub mod beat;
pub mod display;
pub mod estimate;
pub mod model;
pub mod source;
pub mod sync;
pub mod time;
