//! 活跃挂单簿 + 订单生命周期。engine 的本地镜像，真实状态以交易所回调为准。
//!
//! 「发出撤单」只是意图，≠「撤单成功」。撤单与成交撞车时交易所串行处理、命运唯一：

// OrderBook 的基础查询接口（len/is_empty/side_notional/qty_at/lifecycle）是数据结构完整
// 表面的一部分，将来 timer（撤超时单）和 journal（日志快照）会用到。当前非 test 代码
// 暂未调用，但保留作为预留接口。
#![allow(dead_code)]
//! 要么先成交（撤单失败 + Fill），要么先撤单（Canceled、永不成交）。
//! 成交是不可撤销的事实，收到 Fill 一律入账。
//!
//! 生命周期：Active ─(发撤单)→ CancelPending ─┬─(Canceled)→ 移除
//!                                            └─(Fill)────→ 减 qty（部分成交留簿）/ 全成交移除

use domain::order::{Order, OrderId};
use domain::types::{Money, Price, Qty, Side};
use std::collections::HashMap;

/// 一笔挂单的本地生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrderLifecycle {
    /// 活跃挂单。
    Active,
    /// 已发撤单请求、等交易所拍板。
    CancelPending,
}

/// 活跃挂单簿：order_id → (订单, 生命周期态)。
#[derive(Debug, Clone, Default)]
pub(crate) struct OrderBook {
    orders: HashMap<OrderId, (Order, OrderLifecycle)>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// 挂单数（含 CancelPending）。
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    /// 登记一笔新挂单为 Active。
    pub fn insert(&mut self, order: Order) {
        self.orders
            .insert(order.order_id, (order, OrderLifecycle::Active));
    }

    /// 把某单标记为 CancelPending（发出撤单请求时）。单不存在则忽略。
    pub fn mark_cancel_pending(&mut self, order_id: OrderId) {
        if let Some(entry) = self.orders.get_mut(&order_id) {
            entry.1 = OrderLifecycle::CancelPending;
        }
    }

    /// 把某侧所有单标记为 CancelPending。
    pub fn mark_side_cancel_pending(&mut self, side: Side) {
        for (order, lifecycle) in self.orders.values_mut() {
            if order.side == side {
                *lifecycle = OrderLifecycle::CancelPending;
            }
        }
    }

    /// 把所有单标记为 CancelPending。
    pub fn mark_all_cancel_pending(&mut self) {
        for (_, lifecycle) in self.orders.values_mut() {
            *lifecycle = OrderLifecycle::CancelPending;
        }
    }

    /// 移除一笔单（收到 Canceled 或全部成交后）。返回被移除的订单。
    pub fn remove(&mut self, order_id: OrderId) -> Option<Order> {
        self.orders.remove(&order_id).map(|(order, _)| order)
    }

    /// 应用部分成交：减少该单 qty，剩余为 0 则移除。
    ///
    /// 返回 true 表示该单仍留在簿中（部分成交），false 表示已移除（全部成交）。
    /// Polymarket 的 Maker 单部分成交后剩余继续排队，所以不能直接 remove。
    pub fn apply_fill(&mut self, order_id: OrderId, filled_qty: Qty) -> bool {
        let Some((order, _)) = self.orders.get_mut(&order_id) else {
            return false;
        };
        order.qty -= filled_qty;
        if order.qty <= Qty::ZERO {
            self.orders.remove(&order_id);
            false
        } else {
            true // 仍在簿中
        }
    }

    /// 遍历所有挂单（含 CancelPending）。
    pub fn iter(&self) -> impl Iterator<Item = &Order> {
        self.orders.values().map(|(order, _)| order)
    }

    /// 某侧活跃挂单的名义金额合计（含 CancelPending，保守计入敞口）。
    pub fn side_notional(&self, side: Side) -> Money {
        self.orders
            .values()
            .filter(|(o, _)| o.side == side)
            .map(|(o, _)| o.price * o.qty)
            .sum()
    }

    /// 某侧某价位的活跃挂单总量（含 CancelPending）。
    pub fn qty_at(&self, side: Side, price: Price) -> Qty {
        self.orders
            .values()
            .filter(|(o, _)| o.side == side && o.price == price)
            .map(|(o, _)| o.qty)
            .sum()
    }

    /// 查某单的生命周期态。
    pub fn lifecycle(&self, order_id: OrderId) -> Option<OrderLifecycle> {
        self.orders.get(&order_id).map(|(_, l)| *l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::order::{Generation, OrderDirection, TimeInForce};
    use domain::types::OrderRole;
    use rust_decimal_macros::dec;

    fn order(id: u64, side: Side, price: Price, qty: Qty) -> Order {
        Order {
            order_id: OrderId(id),
            side,
            direction: OrderDirection::Buy,
            price,
            qty,
            role: OrderRole::Maker,
            time_in_force: TimeInForce::Gtc,
            generation: Generation::new(),
        }
    }

    #[test]
    fn insert_and_remove() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Up, dec!(0.4), dec!(100)));
        assert_eq!(book.len(), 1);
        assert_eq!(book.lifecycle(OrderId(1)), Some(OrderLifecycle::Active));
        let removed = book.remove(OrderId(1));
        assert!(removed.is_some());
        assert!(book.is_empty());
    }

    #[test]
    fn mark_cancel_pending_changes_lifecycle() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Up, dec!(0.4), dec!(100)));
        book.mark_cancel_pending(OrderId(1));
        assert_eq!(
            book.lifecycle(OrderId(1)),
            Some(OrderLifecycle::CancelPending)
        );
    }

    #[test]
    fn side_notional_sums_matching_side() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Up, dec!(0.4), dec!(100))); // 40
        book.insert(order(2, Side::Up, dec!(0.3), dec!(100))); // 30
        book.insert(order(3, Side::Down, dec!(0.5), dec!(100))); // 50
        assert_eq!(book.side_notional(Side::Up), dec!(70.0));
        assert_eq!(book.side_notional(Side::Down), dec!(50.0));
    }

    #[test]
    fn mark_side_cancel_pending_only_matching() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Up, dec!(0.4), dec!(100)));
        book.insert(order(2, Side::Down, dec!(0.5), dec!(100)));
        book.mark_side_cancel_pending(Side::Up);
        assert_eq!(
            book.lifecycle(OrderId(1)),
            Some(OrderLifecycle::CancelPending)
        );
        assert_eq!(book.lifecycle(OrderId(2)), Some(OrderLifecycle::Active));
    }

    #[test]
    fn qty_at_sums_specific_price_level() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Down, dec!(0.58), dec!(30)));
        book.insert(order(2, Side::Down, dec!(0.58), dec!(20)));
        book.insert(order(3, Side::Down, dec!(0.57), dec!(10)));
        // 0.58 价位总量 50。
        assert_eq!(book.qty_at(Side::Down, dec!(0.58)), dec!(50));
        // 0.57 价位只有 10。
        assert_eq!(book.qty_at(Side::Down, dec!(0.57)), dec!(10));
        // Up 侧 0.58 无单。
        assert_eq!(book.qty_at(Side::Up, dec!(0.58)), dec!(0));
    }

    #[test]
    fn apply_fill_partial_reduces_qty() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Up, dec!(0.4), dec!(100)));
        // 部分成交 30 股，剩余 70 仍在簿中。
        let still = book.apply_fill(OrderId(1), dec!(30));
        assert!(still, "部分成交后应仍留在簿中");
        assert_eq!(book.qty_at(Side::Up, dec!(0.4)), dec!(70));
        assert_eq!(book.lifecycle(OrderId(1)), Some(OrderLifecycle::Active));
    }

    #[test]
    fn apply_fill_full_removes_order() {
        let mut book = OrderBook::new();
        book.insert(order(1, Side::Up, dec!(0.4), dec!(100)));
        // 全部成交 100 股 → 移除。
        let still = book.apply_fill(OrderId(1), dec!(100));
        assert!(!still, "全部成交后应移除");
        assert_eq!(book.lifecycle(OrderId(1)), None);
    }
}
