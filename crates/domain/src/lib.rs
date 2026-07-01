//! 全系统共享的基础数据类型和纯函数。
//!
//! 不做任何 IO，只定义"词汇"。所有业务 crate 都依赖它，它不依赖任何业务 crate。

pub mod clock;
pub mod command;
pub mod fee;
pub mod market;
pub mod order;
pub mod phase;
pub mod pnl;
pub mod types;
