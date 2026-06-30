//! 命令式外壳：单写者事件循环，串起账本、挂单簿、FSM、策略路由、风控、资金池。
//!
//! 纯函数核心（strategy）只算决策；engine 负责一切副作用：更新事实、组装只读上下文、
//! 按优先级链路由到小策略、给订单意图分配 ID/世代、过风控、产出指令、更新本地镜像、
//! 应用状态跳转与全局量更新。
//!
//! 时间不自持：调用方（回测虚拟时钟 / 实盘系统时钟）每次把 now 与剩余时间喂进来，
//! handle_event 是同步纯逻辑。

pub mod apply;
pub mod book;
pub mod config;
pub mod core;
pub mod decide;
pub mod event;

pub use config::EngineConfig;
pub use core::Engine;
