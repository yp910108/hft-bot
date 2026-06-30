//! 决策：组装只读上下文，过全局路由，派发给对应阶段小策略。纯查询，不改状态。

use crate::config::Pool;
use crate::core::Engine;
use strategy::PhaseStrategy;
use strategy::context::{ActiveOrder, Decision, DecisionContext, PoolBudgets, Trigger};
use strategy::router::{Phase, Route};

impl Engine {
    /// 组装上下文 → 全局路由 → 小策略决策。
    pub(crate) fn decide(&self, trigger: Trigger) -> Decision {
        let active = self.active_orders();
        let ctx = DecisionContext {
            total_capital: self.cfg.pools.total_capital(),
            trigger,
            now: self.now,
            time_to_expiry: self.time_to_expiry,
            position: self.ledger.snapshot(),
            market: self.market,
            pools: PoolBudgets {
                grid_maker_total: self.cfg.pools.grid_maker(),
                grid_maker_remaining: self.pool_remaining(Pool::GridMaker),
                dynamic_remaining: self.pool_remaining(Pool::Dynamic),
                ev_remaining: self.pool_remaining(Pool::Ev),
                max_exposure: self.cfg.pools.max_exposure(),
            },
            active_orders: &active,
            constraints: self.cfg.constraints,
            round: &self.round,
        };

        match strategy::route(&ctx, &self.cfg.strategy) {
            Route::Direct(decision) => decision,
            Route::Phase(phase) => self.dispatch(phase, &ctx),
        }
    }

    /// 把上下文交给对应阶段小策略。
    fn dispatch(&self, phase: Phase, ctx: &DecisionContext) -> Decision {
        match phase {
            Phase::Building => self.building.decide(ctx),
            Phase::Pairing => self.pairing.decide(ctx),
            Phase::DynamicHedge => self.dynamic.decide(ctx),
            Phase::EvHedge => self.ev.decide(ctx),
            Phase::CircuitBreaker => self.circuit.decide(ctx),
        }
    }

    /// 当前活跃挂单的只读视图（喂给小策略）。
    pub(crate) fn active_orders(&self) -> Vec<ActiveOrder> {
        self.book
            .iter()
            .map(|o| ActiveOrder {
                order_id: o.order_id,
                side: o.side,
                direction: o.direction,
                price: o.price,
                qty: o.qty,
                role: o.role,
            })
            .collect()
    }
}
