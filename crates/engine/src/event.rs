//! 事件事实更新：把交易所事件落到本地状态（账本 / 挂单簿 / 行情），并得出触发类型。
//!
//! 真实状态以交易所回调为准：撤单确认/失败都从本地镜像移除该单；成交一律入账。

use crate::Engine;
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
                // 撤单确认或失败：本地镜像都移除该单（失败通常因已成交，Fill 已另行入账）。
                self.book.remove(*order_id);
                self.order_pool.remove(order_id);
                Trigger::OrderUpdate
            }
            ExchangeEvent::Rejected { order_id, .. } => {
                self.book.remove(*order_id);
                self.order_pool.remove(order_id);
                Trigger::OrderUpdate
            }
            ExchangeEvent::TimerFired(_) => Trigger::TimerFired,
        }
    }

    /// 记一笔成交：入账、移出挂单簿、累加所属池成本、锁定主战场。
    fn book_fill(&mut self, fill: &Fill) {
        self.ledger.apply_fill(fill);
        // 首笔成交锁定主战场。
        if self.main_field.is_none() {
            self.main_field = Some(fill.side);
        }
        // 累加所属池的已成交成本。
        if let Some(pool) = self.order_pool.get(&fill.order_id).copied() {
            *self.filled_cost.entry(pool).or_insert(Money::ZERO) += fill.cash;
        }
        self.book.remove(fill.order_id);
        self.order_pool.remove(&fill.order_id);
    }

    /// 熔断态下追踪 spread 平静起点。非熔断态清空。
    fn update_calm_tracking(&mut self) {
        if self.machine.state() != RobotState::CircuitBreaker {
            self.calm_since = None;
            return;
        }
        let tripping = [Side::Up, Side::Down].iter().any(|&s| {
            spread_ratio(&self.market, s)
                .is_some_and(|r| r > self.cfg.strategy.circuit_trigger_ratio)
        });
        let calm = [Side::Up, Side::Down].iter().any(|&s| {
            spread_ratio(&self.market, s)
                .is_some_and(|r| r < self.cfg.strategy.circuit_recover_ratio)
        });
        if tripping || !calm {
            self.calm_since = None;
        } else if self.calm_since.is_none() {
            self.calm_since = Some(self.now);
        }
    }
}
