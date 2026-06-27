//! 模拟撮合后端：内存里的"假交易所"，实现 [`ExchangeBackend`]，用于测试和回测。
//!
//! 两种成交方式：
//! - Maker 限价买单：进挂单簿，行情驱动撮合。卖一价严格低于限价才成交（保守口径，减少逆向选择）。
//! - Taker 即时买单：提交即以最新卖一价成交，不进挂单簿；无行情或价格超限则拒单。
//!
//! 手续费体现为净入仓股数的扣减（见 `domain::fee::FeeModel`）。

use crate::backend::ExchangeBackend;
use crate::event::{ExchangeEvent, RejectReason};
use domain::fee::FeeModel;
use domain::market::MarketSnapshot;
use domain::order::{Fill, Order, OrderDirection, OrderId};
use domain::types::{OrderRole, Price, Side};
use std::collections::HashMap;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// 模拟撮合后端。
///
/// 构造时返回事件接收端，调用方从中消费 [`ExchangeEvent`]。
pub struct Simulator {
    /// 活跃挂单簿，按订单 ID 索引。
    resting_orders: HashMap<OrderId, Order>,
    /// 手续费模型，把名义股数算成净入仓股数。
    fee_model: FeeModel,
    /// 最近行情快照，Taker 成交时读卖一价用；初始无行情为 `None`。
    last_snapshot: Option<MarketSnapshot>,
    /// 事件回报发送端。
    event_sender: UnboundedSender<ExchangeEvent>,
}

impl Simulator {
    /// 创建模拟后端，返回实例和事件接收端。
    pub fn new(fee_model: FeeModel) -> (Self, UnboundedReceiver<ExchangeEvent>) {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let simulator = Self {
            resting_orders: HashMap::new(),
            fee_model,
            last_snapshot: None,
            event_sender,
        };
        (simulator, event_receiver)
    }

    /// 活跃挂单数量。
    pub fn resting_order_count(&self) -> usize {
        self.resting_orders.len()
    }

    /// 喂入最新行情快照，驱动撮合。
    ///
    /// 先更新行情（后续 Taker 要用），再遍历挂单，满足条件的成交并移出挂单簿。
    pub fn on_market(&mut self, snapshot: &MarketSnapshot) {
        self.last_snapshot = Some(*snapshot);
        let filled_ids: Vec<OrderId> = self
            .resting_orders
            .values()
            .filter(|order| Self::is_fillable(order, snapshot))
            .map(|order| order.order_id)
            .collect();

        for order_id in filled_ids {
            let order = self.resting_orders.remove(&order_id).expect("挂单必存在");
            // Maker 挂单以限价成交（保守口径下卖一已穿越限价）。
            let fill = self.build_fill(&order, order.price);
            // 接收端关了就忽略，模拟后端不因下游停止而 panic。
            let _ = self.event_sender.send(ExchangeEvent::Filled(fill));
        }
    }

    /// 判断一笔挂单在当前行情下是否成交（保守口径：卖一价严格穿越买单限价）。
    fn is_fillable(order: &Order, snapshot: &MarketSnapshot) -> bool {
        if order.direction != OrderDirection::Buy {
            return false;
        }
        match snapshot.book(order.side).best_ask {
            Some(best_ask) => best_ask < order.price,
            None => false,
        }
    }

    /// 处理一笔 Taker 即时买单：以最近行情的卖一价立即成交，不进挂单簿。
    ///
    /// 卖一价存在且不高于限价上限（`best_ask <= order.price`）则以**卖一价**成交；
    /// 否则（无行情 / 无卖一 / 价太高）拒单。
    fn execute_taker(&mut self, order: &Order) {
        let best_ask = self
            .last_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.book(order.side).best_ask);
        match best_ask {
            Some(ask) if ask <= order.price => {
                // 以真实卖一价成交（而非限价上限），避免 Taker 成交价被错记为限价。
                let fill = self.build_fill(order, ask);
                let _ = self.event_sender.send(ExchangeEvent::Filled(fill));
            }
            _ => {
                let _ = self.event_sender.send(ExchangeEvent::Rejected {
                    order_id: order.order_id,
                    reason: RejectReason::InvalidPrice,
                });
            }
        }
    }

    /// 依据挂单与成交价构造成交回报。
    ///
    /// `exec_price` 为实际成交价（Maker 取限价、Taker 取卖一价）；名义股数按角色费率扣减为
    /// 净入仓股数，现金为名义股数 × 成交价。
    fn build_fill(&self, order: &Order, exec_price: Price) -> Fill {
        let filled_qty = self.fee_model.net_qty(order.role, order.qty);
        let cash = exec_price * order.qty;
        Fill {
            order_id: order.order_id,
            side: order.side,
            direction: order.direction,
            role: order.role,
            price: exec_price,
            filled_qty,
            cash,
            generation: order.generation,
        }
    }
}

impl ExchangeBackend for Simulator {
    fn submit_order(&mut self, order: Order) {
        match order.role {
            // Taker 即时成交（或拒单），不进挂单簿。
            OrderRole::Taker => self.execute_taker(&order),
            // Maker 进入挂单簿，等待行情撮合。
            OrderRole::Maker => {
                self.resting_orders.insert(order.order_id, order);
            }
        }
    }

    fn cancel_order(&mut self, order_id: OrderId) {
        if self.resting_orders.remove(&order_id).is_some() {
            let _ = self.event_sender.send(ExchangeEvent::Canceled(order_id));
        } else {
            // 目标不在簿上（已成交或从未挂上）→ 撤单失败。
            // 真实状态以回调为准：若因已成交，对应 Fill 会另行回报。
            let _ = self
                .event_sender
                .send(ExchangeEvent::CancelFailed(order_id));
        }
    }

    fn cancel_side(&mut self, side: Side) {
        let ids: Vec<OrderId> = self
            .resting_orders
            .values()
            .filter(|order| order.side == side)
            .map(|order| order.order_id)
            .collect();
        for order_id in ids {
            self.cancel_order(order_id);
        }
    }

    fn cancel_all(&mut self) {
        let ids: Vec<OrderId> = self.resting_orders.keys().copied().collect();
        for order_id in ids {
            self.cancel_order(order_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::market::BookTop;
    use domain::order::Generation;
    use domain::order::TimeInForce;
    use domain::types::OrderRole;
    use rust_decimal_macros::dec;

    /// 构造一笔 Maker 限价买单的测试辅助函数。
    fn maker_buy(
        order_id: u64,
        side: Side,
        price: domain::types::Price,
        qty: domain::types::Qty,
    ) -> Order {
        Order {
            order_id: OrderId(order_id),
            side,
            direction: OrderDirection::Buy,
            price,
            qty,
            role: OrderRole::Maker,
            time_in_force: TimeInForce::Gtc,
            generation: Generation::new(),
        }
    }

    /// 构造仅设置某侧卖一价的市场快照。
    fn snapshot_with_ask(side: Side, best_ask: domain::types::Price) -> MarketSnapshot {
        let book = BookTop {
            best_bid: None,
            best_ask: Some(best_ask),
            last_trade: None,
        };
        match side {
            Side::Up => MarketSnapshot {
                up: book,
                down: BookTop::default(),
            },
            Side::Down => MarketSnapshot {
                up: BookTop::default(),
                down: book,
            },
        }
    }

    #[test]
    fn submit_order_rests_in_book() {
        let (mut simulator, _rx) = Simulator::new(FeeModel::zero());
        simulator.submit_order(maker_buy(1, Side::Up, dec!(0.40), dec!(100)));
        assert_eq!(simulator.resting_order_count(), 1);
    }

    #[test]
    fn ask_crossing_limit_fills_order() {
        let (mut simulator, mut rx) = Simulator::new(FeeModel::zero());
        simulator.submit_order(maker_buy(1, Side::Up, dec!(0.40), dec!(100)));
        // 卖一价 0.39 < 限价 0.40，严格穿越 → 成交。
        simulator.on_market(&snapshot_with_ask(Side::Up, dec!(0.39)));
        match rx.try_recv() {
            Ok(ExchangeEvent::Filled(fill)) => {
                assert_eq!(fill.order_id, OrderId(1));
                assert_eq!(fill.filled_qty, dec!(100));
                assert_eq!(fill.cash, dec!(40.00));
            }
            other => panic!("应成交并产出 Filled，实际为 {other:?}"),
        }
        // 成交后挂单移出簿。
        assert_eq!(simulator.resting_order_count(), 0);
    }

    #[test]
    fn ask_touching_limit_does_not_fill() {
        let (mut simulator, mut rx) = Simulator::new(FeeModel::zero());
        simulator.submit_order(maker_buy(1, Side::Up, dec!(0.40), dec!(100)));
        // 卖一价恰等于限价 0.40，未严格穿越 → 保守口径不成交。
        simulator.on_market(&snapshot_with_ask(Side::Up, dec!(0.40)));
        assert!(rx.try_recv().is_err());
        assert_eq!(simulator.resting_order_count(), 1);
    }

    #[test]
    fn taker_fee_reduces_filled_qty() {
        let (mut simulator, mut rx) = Simulator::new(FeeModel::default());
        // 先有行情：Up 侧卖一 0.49。
        simulator.on_market(&snapshot_with_ask(Side::Up, dec!(0.49)));
        // Taker 买单限价 0.50（上限），提交瞬间即时成交。
        let mut order = maker_buy(1, Side::Up, dec!(0.50), dec!(100));
        order.role = OrderRole::Taker;
        simulator.submit_order(order);
        match rx.try_recv() {
            Ok(ExchangeEvent::Filled(fill)) => {
                // 以真实卖一价 0.49 成交（非限价 0.50）：净入仓 100×0.96=96 股，现金 100×0.49=49。
                assert_eq!(fill.price, dec!(0.49));
                assert_eq!(fill.filled_qty, dec!(96.00));
                assert_eq!(fill.cash, dec!(49.00));
            }
            other => panic!("应成交，实际为 {other:?}"),
        }
        // Taker 不进挂单簿。
        assert_eq!(simulator.resting_order_count(), 0);
    }

    #[test]
    fn taker_fills_when_ask_equals_limit() {
        let (mut simulator, mut rx) = Simulator::new(FeeModel::zero());
        // 卖一价恰等于 Taker 限价上限 0.50 → 命中（Taker 用 <=，区别于 Maker 严格穿越）。
        simulator.on_market(&snapshot_with_ask(Side::Up, dec!(0.50)));
        let mut order = maker_buy(1, Side::Up, dec!(0.50), dec!(100));
        order.role = OrderRole::Taker;
        simulator.submit_order(order);
        match rx.try_recv() {
            Ok(ExchangeEvent::Filled(fill)) => assert_eq!(fill.price, dec!(0.50)),
            other => panic!("应成交，实际为 {other:?}"),
        }
    }

    #[test]
    fn taker_rejected_when_ask_above_limit() {
        let (mut simulator, mut rx) = Simulator::new(FeeModel::zero());
        // 卖一价 0.55 高于 Taker 限价上限 0.50 → 拒单，不成交不挂起。
        simulator.on_market(&snapshot_with_ask(Side::Up, dec!(0.55)));
        let mut order = maker_buy(1, Side::Up, dec!(0.50), dec!(100));
        order.role = OrderRole::Taker;
        simulator.submit_order(order);
        assert_eq!(
            rx.try_recv(),
            Ok(ExchangeEvent::Rejected {
                order_id: OrderId(1),
                reason: RejectReason::InvalidPrice,
            })
        );
        assert_eq!(simulator.resting_order_count(), 0);
    }

    #[test]
    fn taker_rejected_when_no_snapshot() {
        let (mut simulator, mut rx) = Simulator::new(FeeModel::zero());
        // 尚无任何行情（last_snapshot=None）→ Taker 无从取卖一价 → 拒单。
        let mut order = maker_buy(1, Side::Up, dec!(0.50), dec!(100));
        order.role = OrderRole::Taker;
        simulator.submit_order(order);
        assert!(matches!(
            rx.try_recv(),
            Ok(ExchangeEvent::Rejected {
                reason: RejectReason::InvalidPrice,
                ..
            })
        ));
    }

    #[test]
    fn taker_rejected_when_side_has_no_ask() {
        let (mut simulator, mut rx) = Simulator::new(FeeModel::zero());
        // 行情只给了 Up 侧卖一，Down 侧无卖一 → Down 的 Taker 拒单。
        simulator.on_market(&snapshot_with_ask(Side::Up, dec!(0.40)));
        let mut order = maker_buy(1, Side::Down, dec!(0.60), dec!(100));
        order.role = OrderRole::Taker;
        simulator.submit_order(order);
        assert!(matches!(
            rx.try_recv(),
            Ok(ExchangeEvent::Rejected {
                reason: RejectReason::InvalidPrice,
                ..
            })
        ));
    }

    #[test]
    fn cancel_order_removes_and_reports() {
        let (mut simulator, mut rx) = Simulator::new(FeeModel::zero());
        simulator.submit_order(maker_buy(1, Side::Up, dec!(0.40), dec!(100)));
        simulator.cancel_order(OrderId(1));
        assert_eq!(simulator.resting_order_count(), 0);
        assert_eq!(rx.try_recv(), Ok(ExchangeEvent::Canceled(OrderId(1))));
    }

    #[test]
    fn cancel_missing_order_reports_failure() {
        let (mut simulator, mut rx) = Simulator::new(FeeModel::zero());
        // 撤一个根本不在簿上的单（已成交或从未挂上）→ CancelFailed。
        simulator.cancel_order(OrderId(99));
        assert_eq!(rx.try_recv(), Ok(ExchangeEvent::CancelFailed(OrderId(99))));
    }

    #[test]
    fn cancel_side_only_removes_matching_side() {
        let (mut simulator, _rx) = Simulator::new(FeeModel::zero());
        simulator.submit_order(maker_buy(1, Side::Up, dec!(0.40), dec!(100)));
        simulator.submit_order(maker_buy(2, Side::Down, dec!(0.55), dec!(100)));
        simulator.cancel_side(Side::Up);
        // 仅 Up 侧被撤，Down 侧保留。
        assert_eq!(simulator.resting_order_count(), 1);
    }

    #[test]
    fn cancel_all_clears_book() {
        let (mut simulator, _rx) = Simulator::new(FeeModel::zero());
        simulator.submit_order(maker_buy(1, Side::Up, dec!(0.40), dec!(100)));
        simulator.submit_order(maker_buy(2, Side::Down, dec!(0.55), dec!(100)));
        simulator.cancel_all();
        assert_eq!(simulator.resting_order_count(), 0);
    }
}
