//! 回测市场数据结构。
//!
//! 定义 [`Market`] 结构体：一场回测所需的盘口快照序列 + 交割胜出方。
//! 真实数据由 [`real_data`](super::real_data) 模块从 CSV 加载并填充此结构。

use domain::market::MarketSnapshot;
use domain::types::Side;

/// 一场回测的市场数据：按时间顺序的盘口快照序列 + 交割胜出方。
pub struct Market {
    /// 按时间顺序的逐秒盘口快照。
    pub snapshots: Vec<MarketSnapshot>,
    /// 交割时的胜出方。
    pub winner: Side,
    /// 场标题（报告展示用），如 "BTC 15m - 2026-05-22 07:30"。
    pub title: String,
}
