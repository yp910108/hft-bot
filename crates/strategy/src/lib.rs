//! 策略决策核心：全局条件路由 + 各阶段小策略，全部纯函数。
//!
//! engine 每个 tick 组装只读的 [`context::DecisionContext`] 喂进来，
//! [`router::route`] 按优先级链裁决归哪个阶段管，对应小策略产出 [`context::Decision`]。
//! strategy 不碰 IO、不分配 ID、不改账本——副作用全在 engine。

pub mod building;
pub mod circuit_breaker;
pub mod config;
pub mod context;
pub mod dynamic_hedge;
pub mod ev_hedge;
pub mod pairing;
pub mod router;

/// 一个阶段小策略：给定只读世界快照，算出此刻的决策。纯函数，无副作用。
pub trait PhaseStrategy {
    fn decide(&self, ctx: &context::DecisionContext) -> context::Decision;
}

pub use building::BuildingStrategy;
pub use circuit_breaker::CircuitBreakerStrategy;
pub use config::StrategyConfig;
pub use context::{Decision, DecisionContext};
pub use dynamic_hedge::DynamicHedgeStrategy;
pub use ev_hedge::EvHedgeStrategy;
pub use pairing::PairingStrategy;
pub use router::{route, Phase, Route};
