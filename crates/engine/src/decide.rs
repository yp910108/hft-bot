//! 决策：组装只读上下文，过全局路由，派发给对应阶段小策略。纯查询，不改状态。

use crate::config::Pool;
use crate::Engine;
use strategy::context::{Decision, DecisionContext, PoolBudgets, Trigger};
use strategy::router::{Phase, Route};
use strategy::PhaseStrategy;

impl Engine {
    /// 组装上下文 → 全局路由 → 小策略决策。
    pub(crate) fn decide(&self, trigger: Trigger) -> Decision {
        let active = self.active_orders();
        let ctx = DecisionContext {
            total_capital: self.cfg.pools.total_capital(),
            trigger,
            now: self.now,
            time_to_expiry: self.time_to_expiry,
            state: self.machine.state(),
            main_field: self.main_field,
            main_field_frozen: self.main_field_frozen,
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
            last_hedge_at: self.last_hedge_at,
            calm_since: self.calm_since,
            constraints: self.cfg.constraints,
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
            Phase::DynamicHedge | Phase::Observing => self.dynamic.decide(ctx),
            Phase::EvHedge => self.ev.decide(ctx),
            Phase::CircuitBreaker => self.circuit.decide(ctx),
        }
    }
}
