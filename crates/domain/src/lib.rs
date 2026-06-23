//! 全系统共享的基础数据类型和纯函数。
//!
//! 不做任何 IO，只定义"词汇"。所有业务 crate 都依赖它，它不依赖任何业务 crate。

pub mod fee;
pub mod market;
pub mod order;
pub mod pnl;
pub mod state;
pub mod types;
