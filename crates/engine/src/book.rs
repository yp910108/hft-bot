//! 活跃挂单簿：engine 的本地镜像，真实状态以交易所回调为准。
//!
//! 生命周期：Active ─(发撤单)→ CancelPending ─┬─(Canceled)→ 移除
//!                                            └─(Fill)────→ 减 qty / 全成交移除

use domain::order::{Order, OrderDirection, OrderId};
use domain::types::{Money, Qty, Side};
use inventory::lot::LotId;
use std::collections::HashMap;

/// 一笔挂单的本地生命周期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrderLifecycle {
    Active,
    CancelPending,
}

/// 挂单簿中的条目：订单 + 生命周期 + 可选的 Lot 关联。
#[derive(Debug, Clone)]
pub(crate) struct BookEntry {
    pub order: Order,
    pub lifecycle: OrderLifecycle,
    /// 卖单关联的 Lot（买单为 None）。
    pub lot_id: Option<LotId>,
}

/// 活跃挂单簿。
#[derive(Debug, Clone, Default)]
pub(crate) struct OrderBook {
    entries: HashMap<OrderId, BookEntry>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一笔新挂单。
    pub fn insert(&mut self, order: Order, lot_id: Option<LotId>) {
        self.entries.insert(
            order.order_id,
            BookEntry {
                order,
                lifecycle: OrderLifecycle::Active,
                lot_id,
            },
        );
    }

    /// 移除某单。
    pub fn remove(&mut self, order_id: OrderId) -> Option<BookEntry> {
        self.entries.remove(&order_id)
    }

    /// 标记某单为 CancelPending。
    pub fn mark_cancel_pending(&mut self, order_id: OrderId) {
        if let Some(entry) = self.entries.get_mut(&order_id) {
            entry.lifecycle = OrderLifecycle::CancelPending;
        }
    }

    /// 标记某侧全部单为 CancelPending。
    pub fn mark_side_cancel_pending(&mut self, side: Side) {
        for entry in self.entries.values_mut() {
            if entry.order.side == side {
                entry.lifecycle = OrderLifecycle::CancelPending;
            }
        }
    }

    /// 标记所有单为 CancelPending。
    pub fn mark_all_cancel_pending(&mut self) {
        for entry in self.entries.values_mut() {
            entry.lifecycle = OrderLifecycle::CancelPending;
        }
    }

    /// 成交减 qty：部分成交减少、全成交移除。返回是否仍在簿上。
    pub fn apply_fill(&mut self, order_id: OrderId, filled_qty: Qty) -> bool {
        let Some(entry) = self.entries.get_mut(&order_id) else {
            return false;
        };
        entry.order.qty -= filled_qty;
        if entry.order.qty <= Qty::ZERO {
            self.entries.remove(&order_id);
            false
        } else {
            true
        }
    }

    /// 查某单关联的 LotId。
    pub fn lot_id_for(&self, order_id: OrderId) -> Option<LotId> {
        self.entries.get(&order_id).and_then(|e| e.lot_id)
    }

    /// 某侧活跃买单的总名义（现金哨兵用：占用的潜在现金）。
    pub fn active_buy_notional(&self, side: Side) -> Money {
        self.entries
            .values()
            .filter(|e| {
                e.order.side == side
                    && e.order.direction == OrderDirection::Buy
                    && e.lifecycle == OrderLifecycle::Active
            })
            .map(|e| e.order.price * e.order.qty)
            .sum()
    }

    /// 总活跃买单名义（两侧合计）。
    pub fn total_active_buy_notional(&self) -> Money {
        self.active_buy_notional(Side::Up) + self.active_buy_notional(Side::Down)
    }

    /// 构建 ActiveOrder 视图数组（喂给 strategy）。
    pub fn active_order_views(&self) -> Vec<strategy::ActiveOrder> {
        self.entries
            .values()
            .map(|e| strategy::ActiveOrder {
                order_id: e.order.order_id,
                side: e.order.side,
                direction: e.order.direction,
                price: e.order.price,
                qty: e.order.qty,
                role: e.order.role,
                lot_id: e.lot_id,
            })
            .collect()
    }
}
