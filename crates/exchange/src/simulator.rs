//! 模拟撮合后端：实现 [`ExchangeBackend`] 的内存"假交易所"，供策略与事件循环测试。
//!
//! 本阶段为**最小可用版**，仅支持 Maker 限价买单：
//! - 维护内存挂单簿，按行情驱动撮合；
//! - 成交判定采用**保守口径**——卖一价严格穿越挂单限价（`best_ask < limit`）才成交，
//!   不高估成交以缓解 Maker 逆向选择（见策略风险修复项 #2）；
//! - 手续费体现为净入仓股数的扣减（见 `domain::fee::FeeModel`）。
//!
//! Taker 即时成交、卖出撮合、部分成交等留待后续阶段补充。

use crate::backend::ExchangeBackend;
use crate::event::ExchangeEvent;
use domain::fee::FeeModel;
use domain::market::MarketSnapshot;
use domain::order::{Fill, Order, OrderDirection, OrderId};
use domain::types::Side;
use std::collections::HashMap;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// 模拟撮合后端。
///
/// 通过 [`Simulator::new`] 构造时返回事件接收端，调用方据此消费 [`ExchangeEvent`]。
pub struct Simulator {
    /// 当前活跃挂单簿，按订单标识索引。
    resting_orders: HashMap<OrderId, Order>,
    /// 手续费模型，用于将名义成交股数换算为净入仓股数。
    fee_model: FeeModel,
    /// 事件回报发送端。
    event_sender: UnboundedSender<ExchangeEvent>,
}

impl Simulator {
    /// 创建模拟后端，返回后端实例与事件接收端。
    pub fn new(fee_model: FeeModel) -> (Self, UnboundedReceiver<ExchangeEvent>) {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let simulator = Self {
            resting_orders: HashMap::new(),
            fee_model,
            event_sender,
        };
        (simulator, event_receiver)
    }

    /// 当前活跃挂单数量。
    pub fn resting_order_count(&self) -> usize {
        self.resting_orders.len()
    }

    /// 喂入一笔最新市场快照，驱动撮合。
    ///
    /// 遍历活跃挂单，对满足保守成交条件的买单产出 [`ExchangeEvent::Filled`] 并将其移出挂单簿。
    pub fn on_market(&mut self, snapshot: &MarketSnapshot) {
        let filled_ids: Vec<OrderId> = self
            .resting_orders
            .values()
            .filter(|order| Self::is_fillable(order, snapshot))
            .map(|order| order.order_id)
            .collect();

        for order_id in filled_ids {
            let order = self.resting_orders.remove(&order_id).expect("挂单必存在");
            let fill = self.build_fill(&order);
            // 接收端已关闭时忽略发送错误：模拟后端不因下游停止消费而 panic。
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

    /// 依据挂单与手续费模型构造成交回报。
    ///
    /// 成交价取挂单限价，名义股数按费率扣减为净入仓股数，现金为名义股数 × 限价。
    fn build_fill(&self, order: &Order) -> Fill {
        let filled_qty = self.fee_model.net_qty(order.role, order.qty);
        let cash = order.price * order.qty;
        Fill {
            order_id: order.order_id,
            side: order.side,
            direction: order.direction,
            price: order.price,
            filled_qty,
            cash,
            generation: order.generation,
        }
    }
}

impl ExchangeBackend for Simulator {
    fn submit_order(&mut self, order: Order) {
        self.resting_orders.insert(order.order_id, order);
    }

    fn cancel_order(&mut self, order_id: OrderId) {
        if self.resting_orders.remove(&order_id).is_some() {
            let _ = self.event_sender.send(ExchangeEvent::Canceled(order_id));
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
            self.resting_orders.remove(&order_id);
            let _ = self.event_sender.send(ExchangeEvent::Canceled(order_id));
        }
    }

    fn cancel_all(&mut self) {
        let ids: Vec<OrderId> = self.resting_orders.keys().copied().collect();
        for order_id in ids {
            self.resting_orders.remove(&order_id);
            let _ = self.event_sender.send(ExchangeEvent::Canceled(order_id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::market::BookTop;
    use domain::order::Generation;
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
        // Taker 角色买单，默认 4% 费率。
        let mut order = maker_buy(1, Side::Up, dec!(0.50), dec!(100));
        order.role = OrderRole::Taker;
        simulator.submit_order(order);
        simulator.on_market(&snapshot_with_ask(Side::Up, dec!(0.49)));
        match rx.try_recv() {
            Ok(ExchangeEvent::Filled(fill)) => {
                // 净入仓 100 × 0.96 = 96 股，现金仍为 100 × 0.50 = 50。
                assert_eq!(fill.filled_qty, dec!(96.00));
                assert_eq!(fill.cash, dec!(50.00));
            }
            other => panic!("应成交，实际为 {other:?}"),
        }
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
