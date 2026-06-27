//! 回测工具库：用真实历史行情驱动模拟撮合与事件循环，验证策略盈亏。
//!
//! 本 crate 是验证工具，不属于交易系统运行时核心，依赖 engine/exchange/domain。
//! 回测入口为 `examples/report_json.rs`，产出 JSON 供 `analysis_data/` 渲染 HTML 报告。

pub mod market;
pub mod real_data;
pub mod run;
