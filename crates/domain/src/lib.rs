//! 核心领域模型层：定义整个系统共享的「词汇表」。
//!
//! 本 crate 不触碰任何 IO，只包含纯数据类型与纯函数，是依赖图的地基
//! （所有其他 crate 均依赖 domain，domain 不依赖任何业务 crate）。

pub mod fee;
pub mod market;
pub mod order;
pub mod pnl;
pub mod state;
pub mod types;
