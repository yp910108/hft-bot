//! 资金池剩余、可用现金、活跃挂单视图的计算。纯查询，不改状态。

use crate::config::Pool;
use crate::Engine;
use domain::state::RobotState;
use domain::types::Money;
use strategy::context::ActiveOrder;

impl Engine {
    /// 当前各池剩余 = 池总额 − 已成交成本 − 活跃挂单名义。
    pub(crate) fn pool_remaining(&self, pool: Pool) -> Money {
        let total = match pool {
            Pool::GridMaker => self.cfg.pools.grid_maker(),
            Pool::Dynamic => self.cfg.pools.dynamic(),
            Pool::Ev => self.cfg.pools.ev(),
        };
        let filled = self.filled_cost.get(&pool).copied().unwrap_or(Money::ZERO);
        let active: Money = self
            .book
            .iter()
            .filter(|o| self.order_pool.get(&o.order_id).copied() == Some(pool))
            .map(|o| o.price * o.qty)
            .sum();
        total - filled - active
    }

    /// 可用现金 = 总资金 − 已成交总成本 − 活跃挂单总名义。
    pub(crate) fn free_cash(&self) -> Money {
        let filled: Money = self.filled_cost.values().copied().sum();
        let active: Money = self.book.iter().map(|o| o.price * o.qty).sum();
        self.cfg.pools.total_capital() - filled - active
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

    /// 某状态下新挂单出自哪个池。
    pub(crate) fn pool_for_state(state: RobotState) -> Pool {
        match state {
            RobotState::Building | RobotState::Pairing => Pool::GridMaker,
            RobotState::DynamicHedge { .. } | RobotState::Observing { .. } => Pool::Dynamic,
            RobotState::EvHedge => Pool::Ev,
            // 熔断/结算态不发新单，给个默认。
            RobotState::CircuitBreaker | RobotState::SettlementWait => Pool::GridMaker,
        }
    }
}
