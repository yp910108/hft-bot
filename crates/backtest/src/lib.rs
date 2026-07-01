//! 回测工具库：用真实历史行情驱动模拟撮合与事件循环，验证策略盈亏。
//!
//! 回测入口为 examples，产出批量结果供报告渲染。

pub mod market;
pub mod real_data;
pub mod run;
