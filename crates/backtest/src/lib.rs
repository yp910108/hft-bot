//! 回测引擎：用合成行情驱动模拟撮合与事件循环，验证策略 edge（震荡盘 Maker 价差
//! 能否覆盖趋势盘瘸腿亏损 + 摩擦成本）。
//!
//! 本 crate 是验证工具，不属于交易系统运行时核心，依赖 engine/exchange/domain。

pub mod batch;
pub mod driver;
pub mod market;
pub mod real_data;
