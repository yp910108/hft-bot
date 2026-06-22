//! 风控审计层：现金安全哨兵（Cash Guard）、三资金池治理与红线约束。
//!
//! 所有下单指令下发前必须无条件通过本层校验。
//! 已实现三资金池划拨与 Cash Guard 现金红线校验。

pub mod auditor;
pub mod pool;
