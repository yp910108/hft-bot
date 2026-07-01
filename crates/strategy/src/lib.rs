//! 策略决策核心：三阶段纯函数 + progress 路由。
//!
//! engine 每个 tick 组装只读的 [`context::DecisionContext`] 喂进来，
//! [`route`] 按 progress 选阶段，对应小策略产出 [`context::Decision`]。
//! strategy 不碰 IO、不分配 ID、不改账本——副作用全在 engine。

pub mod building;
pub mod config;
pub mod context;
pub mod cycling;
pub mod harvesting;

pub use building::BuildingStrategy;
pub use config::StrategyConfig;
pub use context::{ActiveOrder, CommandIntent, Decision, DecisionContext, Trigger};
pub use cycling::CyclingStrategy;
pub use harvesting::HarvestingStrategy;

use domain::phase::Phase;

/// 按 progress 路由到对应阶段，执行决策。
///
/// Engine 每 tick 调用此函数。阶段跳转由各策略的返回值中 `transition` 驱动，
/// 本函数只做路由分发。
pub fn route(phase: Phase, ctx: &DecisionContext) -> Decision {
    match phase {
        Phase::Building => BuildingStrategy.decide(ctx),
        Phase::Cycling => CyclingStrategy.decide(ctx),
        Phase::Harvesting => HarvestingStrategy.decide(ctx),
        Phase::Settled => Decision::skip(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::market::{BookTop, MarketSnapshot};
    use domain::order::OrderConstraints;
    use inventory::Inventory;
    use rust_decimal_macros::dec;

    fn empty_ctx() -> (Inventory, Vec<ActiveOrder>, StrategyConfig) {
        (Inventory::new(), vec![], StrategyConfig::default())
    }

    #[test]
    fn route_building_before_threshold() {
        let (inv, orders, cfg) = empty_ctx();
        let market = MarketSnapshot {
            up: BookTop {
                best_bid: Some(dec!(0.50)),
                best_ask: Some(dec!(0.51)),
                last_trade: None,
            },
            down: BookTop {
                best_bid: Some(dec!(0.49)),
                best_ask: Some(dec!(0.50)),
                last_trade: None,
            },
        };
        let ctx = DecisionContext {
            trigger: Trigger::BookUpdate,
            progress: dec!(0.05),
            market,
            inventory: &inv,
            active_orders: &orders,
            free_cash: dec!(1000),
            constraints: OrderConstraints::default(),
            config: &cfg,
        };
        let decision = route(Phase::Building, &ctx);
        // Building 阶段应产出买单。
        assert!(!decision.commands.is_empty());
        assert_eq!(decision.transition, None);
    }

    #[test]
    fn route_settled_does_nothing() {
        let (inv, orders, cfg) = empty_ctx();
        let market = MarketSnapshot::default();
        let ctx = DecisionContext {
            trigger: Trigger::BookUpdate,
            progress: dec!(0.99),
            market,
            inventory: &inv,
            active_orders: &orders,
            free_cash: dec!(1000),
            constraints: OrderConstraints::default(),
            config: &cfg,
        };
        let decision = route(Phase::Settled, &ctx);
        assert!(decision.commands.is_empty());
        assert_eq!(decision.transition, None);
    }
}
