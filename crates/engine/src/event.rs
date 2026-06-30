//! 事件事实更新：把交易所事件落到本地状态（账本 / 挂单簿 / 行情 / calm_since），
//! 返回本次 tick 的触发类型。

use crate::config::Pool;
use crate::core::Engine;
use domain::order::Fill;
use domain::state::RobotState;
use domain::types::{Money, Side};
use exchange::event::ExchangeEvent;
use strategy::context::Trigger;
use strategy::router::spread_ratio;

impl Engine {
    /// 更新事件带来的事实，返回触发类型。
    pub(crate) fn apply_event_facts(&mut self, event: &ExchangeEvent) -> Trigger {
        match event {
            ExchangeEvent::BookUpdate(snapshot) => {
                self.market = *snapshot;
                self.update_calm_tracking();
                Trigger::BookUpdate
            }
            ExchangeEvent::Filled(fill) => {
                self.book_fill(fill);
                Trigger::Fill { side: fill.side }
            }
            ExchangeEvent::Canceled(order_id) | ExchangeEvent::CancelFailed(order_id) => {
                self.book.remove(*order_id);
                self.order_pool.remove(order_id);
                Trigger::OrderUpdate
            }
            ExchangeEvent::Rejected { order_id, .. } => {
                self.book.remove(*order_id);
                self.order_pool.remove(order_id);
                Trigger::OrderUpdate
            }
        }
    }

    /// 记一笔成交：入账、更新挂单簿、累加所属池成本、锁定主战场。
    fn book_fill(&mut self, fill: &Fill) {
        self.ledger.apply_fill(fill);
        // 首笔成交锁定主战场。
        self.round.lock_main_field(fill.side);
        // 累加所属池的已成交成本。
        if let Some(pool) = self.order_pool.get(&fill.order_id).copied() {
            *self.filled_cost.entry(pool).or_insert(Money::ZERO) += fill.cash;
        }
        // 部分成交：减少该单 qty，全部成交则移除。
        // Polymarket Maker 单部分成交后剩余继续排队。
        let still_resting = self.book.apply_fill(fill.order_id, fill.filled_qty);
        if !still_resting {
            // 全部成交或单不在簿中：清理 order_pool 映射。
            self.order_pool.remove(&fill.order_id);
        }
    }

    /// 熔断态下追踪 spread 平静起点。非熔断态清空。
    fn update_calm_tracking(&mut self) {
        if self.round.state != RobotState::CircuitBreaker {
            self.round.calm_since = None;
            return;
        }
        // 两侧 spread 都 < 恢复阈值才算平静。
        let recover = self.cfg.strategy.circuit_recover_ratio;
        let all_calm = [Side::Up, Side::Down]
            .iter()
            .all(|&s| spread_ratio(&self.market, s).is_some_and(|r| r < recover));
        self.round.update_calm(all_calm, self.now);
    }

    /// 可用现金 = 总资金 − 已成交总成本 − 活跃挂单总名义。
    pub(crate) fn free_cash(&self) -> Money {
        let filled: Money = self.filled_cost.values().copied().sum();
        let active: Money = self.book.iter().map(|o| o.price * o.qty).sum();
        self.cfg.pools.total_capital() - filled - active
    }

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
}
