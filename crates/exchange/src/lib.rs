//! 交易所后端层：统一的后端 trait + 模拟撮合实现。
//!
//! 策略只面向 trait 编程，换后端不改策略。

pub mod backend;
pub mod clock;
pub mod event;
pub mod simulator;
