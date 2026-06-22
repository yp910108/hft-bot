//! 执行后端层：定义统一的交易所后端 trait，并提供模拟实现。
//!
//! 策略代码只面向 trait，切换后端无需改动策略逻辑。

pub mod backend;
pub mod event;
pub mod simulator;
