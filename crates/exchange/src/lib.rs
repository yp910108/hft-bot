//! 执行后端层：定义统一的交易所后端 trait，并提供回测 / 模拟 / 真实三种实现。
//!
//! 策略代码只面向 trait，切换后端无需改动策略逻辑（见架构决策：回测优先、先模拟）。
//! 当前已定义后端 trait 与事件类型；模拟撮合、回测、真实接入留待后续阶段。

pub mod backend;
pub mod event;
pub mod simulator;
