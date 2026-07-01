//! 模拟撮合后端：内存里的"假交易所"，实现 [`ExchangeBackend`]，用于测试和回测。
//!
//! 四种成交方式：
//! - Maker 买单：进挂单簿，行情驱动。卖一价严格低于限价 → 以限价成交。
//! - Maker 卖单：进挂单簿，行情驱动。买一价严格高于限价 → 以限价成交。
//! - Taker 买单：提交即以卖一价成交（IOC），无行情或不满足则拒单。
//! - Taker 卖单：提交即以买一价成交（IOC），无行情或不满足则拒单。
//!
//! 手续费体现为净入仓股数的扣减（买入）或回收现金的扣减（卖出），见 `domain::fee::FeeModel`。

use crate::backend::ExchangeBackend;
use crate::event::{ExchangeEvent, RejectReason};
use domain::fee::FeeModel;
use domain::market::MarketSnapshot;
use domain::order::{Fill, Order, OrderDirection, OrderId};
use domain::types::{OrderRole, Price, Side};
use rust_decimal::Decimal;
use std::collections::HashMap;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// 模拟撮合后端。
///
/// 构造时返回事件接收端，调用方从中消费 [`ExchangeEvent`]。
pub struct Simulator {
    /// 活跃挂单簿，按订单 ID 索引。
    resting_orders: HashMap<OrderId, Order>,
    /// 手续费模型。
    fee_model: FeeModel,
    /// 最近行情快照。初始无行情为 `None`。
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

    /// 喂入最新行情快照，驱动挂单撮合。
    ///
    /// 先更新行情，再遍历挂单簿，满足条件的成交并移出。
    pub fn on_market(&mut self, snapshot: &MarketSnapshot) {
        self.last_snapshot = Some(*snapshot);
        let filled_ids: Vec<OrderId> = self
            .resting_orders
            .values()
            .filter_map(|order| {
                if self.is_fillable(order, snapshot) {
                    Some(order.order_id)
                } else {
                    None
                }
            })
            .collect();

        for order_id in filled_ids {
            let order = self.resting_orders.remove(&order_id).expect("挂单必存在");
            // Maker 以限价成交（穿越时限价对我方有利）。
            let fill = self.build_fill(&order, order.price);
            let _ = self.event_sender.send(ExchangeEvent::Filled(fill));
        }
    }

    /// 判断一笔挂单在当前行情下是否成交（严格穿越，保守口径）。
    ///
    /// - 买单：卖一价 < 挂单价 → 成交（对手愿以更低价卖给我）。
    /// - 卖单：买一价 > 挂单价 → 成交（对手愿以更高价买走我的）。
    fn is_fillable(&self, order: &Order, snapshot: &MarketSnapshot) -> bool {
        match order.direction {
            OrderDirection::Buy => match snapshot.book(order.side).best_ask {
                Some(best_ask) => best_ask < order.price,
                None => false,
            },
            OrderDirection::Sell => match snapshot.book(order.side).best_bid {
                Some(best_bid) => best_bid > order.price,
                None => false,
            },
        }
    }

    /// 处理 Taker 即时单（买或卖），不进挂单簿。
    ///
    /// - 买单：以卖一价成交（ask ≤ 限价上限）。
    /// - 卖单：以买一价成交（bid ≥ 限价下限）。
    fn execute_taker(&mut self, order: &Order) {
        let exec_price = match order.direction {
            OrderDirection::Buy => self
                .last_snapshot
                .as_ref()
                .and_then(|s| s.book(order.side).best_ask)
                .filter(|&ask| ask <= order.price),
            OrderDirection::Sell => self
                .last_snapshot
                .as_ref()
                .and_then(|s| s.book(order.side).best_bid)
                .filter(|&bid| bid >= order.price),
        };

        match exec_price {
            Some(price) => {
                let fill = self.build_fill(order, price);
                let _ = self.event_sender.send(ExchangeEvent::Filled(fill));
            }
            None => {
                let _ = self.event_sender.send(ExchangeEvent::Rejected {
                    order_id: order.order_id,
                    reason: RejectReason::InvalidPrice,
                });
            }
        }
    }

    /// 构造成交回报。
    ///
    /// 买入：cash = 名义股数 × 成交价（付出现金），filled_qty = 扣费后净入仓。
    /// 卖出：cash = 扣费后回收现金，filled_qty = 减仓股数（卖出不扣股数，扣现金）。
    fn build_fill(&self, order: &Order, exec_price: Price) -> Fill {
        let (filled_qty, cash) = match order.direction {
            OrderDirection::Buy => {
                // 买入：扣费后入仓股数，花的钱不变。
                let net_qty = self.fee_model.net_qty(order.role, order.qty);
                let paid = exec_price * order.qty;
                (net_qty, paid)
            }
            OrderDirection::Sell => {
                // 卖出：减仓股数不扣，回收现金扣费。
                let gross_cash = exec_price * order.qty;
                let fee_rate = self.fee_model.rate_for(order.role);
                let net_cash = gross_cash * (Decimal::ONE - fee_rate);
                (order.qty, net_cash)
            }
        };

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
            OrderRole::Taker => self.execute_taker(&order),
            OrderRole::Maker => {
                self.resting_orders.insert(order.order_id, order);
            }
        }
    }

    fn cancel_order(&mut self, order_id: OrderId) {
        if self.resting_orders.remove(&order_id).is_some() {
            let _ = self.event_sender.send(ExchangeEvent::Canceled(order_id));
        } else {
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
    use domain::order::{Generation, TimeInForce};
    use domain::types::Qty;
    use rust_decimal_macros::dec;

    fn make_order(
        id: u64,
        side: Side,
        direction: OrderDirection,
        price: Price,
        qty: Qty,
        role: OrderRole,
    ) -> Order {
        Order {
            order_id: OrderId(id),
            side,
            direction,
            price,
            qty,
            role,
            time_in_force: TimeInForce::Gtc,
            generation: Generation::new(),
        }
    }

    fn snapshot_with(
        up_bid: Option<Price>,
        up_ask: Option<Price>,
        dn_bid: Option<Price>,
        dn_ask: Option<Price>,
    ) -> MarketSnapshot {
        MarketSnapshot {
            up: BookTop {
                best_bid: up_bid,
                best_ask: up_ask,
                last_trade: None,
            },
            down: BookTop {
                best_bid: dn_bid,
                best_ask: dn_ask,
                last_trade: None,
            },
        }
    }

    // ─── Maker 买单（旧功能，验证不破坏） ───

    #[test]
    fn maker_buy_fills_when_ask_crosses_below_limit() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        let order = make_order(
            1,
            Side::Up,
            OrderDirection::Buy,
            dec!(0.50),
            dec!(10),
            OrderRole::Maker,
        );
        sim.submit_order(order);
        assert_eq!(sim.resting_order_count(), 1);

        // 卖一价 0.49 < 限价 0.50 → 成交。
        let snap = snapshot_with(Some(dec!(0.48)), Some(dec!(0.49)), None, None);
        sim.on_market(&snap);
        assert_eq!(sim.resting_order_count(), 0);

        let event = rx.try_recv().unwrap();
        match event {
            ExchangeEvent::Filled(fill) => {
                assert_eq!(fill.price, dec!(0.50)); // 以限价成交
                assert_eq!(fill.filled_qty, dec!(10)); // 零费率全额
                assert_eq!(fill.cash, dec!(5.00)); // 0.50 × 10
                assert_eq!(fill.direction, OrderDirection::Buy);
            }
            _ => panic!("应为 Filled"),
        }
    }

    #[test]
    fn maker_buy_does_not_fill_when_ask_equals_limit() {
        let (mut sim, _rx) = Simulator::new(FeeModel::zero());
        let order = make_order(
            1,
            Side::Up,
            OrderDirection::Buy,
            dec!(0.50),
            dec!(10),
            OrderRole::Maker,
        );
        sim.submit_order(order);

        // 卖一价 0.50 = 限价 0.50 → 不成交（严格穿越）。
        let snap = snapshot_with(Some(dec!(0.48)), Some(dec!(0.50)), None, None);
        sim.on_market(&snap);
        assert_eq!(sim.resting_order_count(), 1);
    }

    // ─── Maker 卖单（新功能） ───

    #[test]
    fn maker_sell_fills_when_bid_crosses_above_limit() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        let order = make_order(
            2,
            Side::Up,
            OrderDirection::Sell,
            dec!(0.55),
            dec!(10),
            OrderRole::Maker,
        );
        sim.submit_order(order);
        assert_eq!(sim.resting_order_count(), 1);

        // 买一价 0.56 > 限价 0.55 → 成交。
        let snap = snapshot_with(Some(dec!(0.56)), Some(dec!(0.60)), None, None);
        sim.on_market(&snap);
        assert_eq!(sim.resting_order_count(), 0);

        let event = rx.try_recv().unwrap();
        match event {
            ExchangeEvent::Filled(fill) => {
                assert_eq!(fill.price, dec!(0.55)); // 以限价成交
                assert_eq!(fill.filled_qty, dec!(10)); // 卖出不扣股数
                assert_eq!(fill.cash, dec!(5.50)); // 零费率：0.55 × 10
                assert_eq!(fill.direction, OrderDirection::Sell);
            }
            _ => panic!("应为 Filled"),
        }
    }

    #[test]
    fn maker_sell_does_not_fill_when_bid_equals_limit() {
        let (mut sim, _rx) = Simulator::new(FeeModel::zero());
        let order = make_order(
            2,
            Side::Up,
            OrderDirection::Sell,
            dec!(0.55),
            dec!(10),
            OrderRole::Maker,
        );
        sim.submit_order(order);

        // 买一价 0.55 = 限价 0.55 → 不成交（严格穿越）。
        let snap = snapshot_with(Some(dec!(0.55)), Some(dec!(0.60)), None, None);
        sim.on_market(&snap);
        assert_eq!(sim.resting_order_count(), 1);
    }

    #[test]
    fn maker_sell_does_not_fill_when_bid_below_limit() {
        let (mut sim, _rx) = Simulator::new(FeeModel::zero());
        let order = make_order(
            2,
            Side::Up,
            OrderDirection::Sell,
            dec!(0.55),
            dec!(10),
            OrderRole::Maker,
        );
        sim.submit_order(order);

        // 买一价 0.50 < 限价 0.55 → 不成交。
        let snap = snapshot_with(Some(dec!(0.50)), Some(dec!(0.60)), None, None);
        sim.on_market(&snap);
        assert_eq!(sim.resting_order_count(), 1);
    }

    // ─── Taker 买单（旧功能，验证不破坏） ───

    #[test]
    fn taker_buy_fills_at_ask_when_within_limit() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        let snap = snapshot_with(Some(dec!(0.48)), Some(dec!(0.50)), None, None);
        sim.on_market(&snap);

        let order = make_order(
            3,
            Side::Up,
            OrderDirection::Buy,
            dec!(0.52),
            dec!(10),
            OrderRole::Taker,
        );
        sim.submit_order(order);

        let event = rx.try_recv().unwrap();
        match event {
            ExchangeEvent::Filled(fill) => {
                assert_eq!(fill.price, dec!(0.50)); // 以卖一价成交
                assert_eq!(fill.filled_qty, dec!(10));
                assert_eq!(fill.cash, dec!(5.00));
            }
            _ => panic!("应为 Filled"),
        }
    }

    #[test]
    fn taker_buy_rejected_when_ask_above_limit() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        let snap = snapshot_with(Some(dec!(0.48)), Some(dec!(0.55)), None, None);
        sim.on_market(&snap);

        let order = make_order(
            3,
            Side::Up,
            OrderDirection::Buy,
            dec!(0.52),
            dec!(10),
            OrderRole::Taker,
        );
        sim.submit_order(order);

        let event = rx.try_recv().unwrap();
        assert!(matches!(event, ExchangeEvent::Rejected { .. }));
    }

    // ─── Taker 卖单（新功能） ───

    #[test]
    fn taker_sell_fills_at_bid_when_within_limit() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        let snap = snapshot_with(Some(dec!(0.52)), Some(dec!(0.55)), None, None);
        sim.on_market(&snap);

        // 限价下限 0.50 ≤ 买一价 0.52 → 以 0.52 成交。
        let order = make_order(
            4,
            Side::Up,
            OrderDirection::Sell,
            dec!(0.50),
            dec!(10),
            OrderRole::Taker,
        );
        sim.submit_order(order);

        let event = rx.try_recv().unwrap();
        match event {
            ExchangeEvent::Filled(fill) => {
                assert_eq!(fill.price, dec!(0.52)); // 以买一价成交
                assert_eq!(fill.filled_qty, dec!(10));
                assert_eq!(fill.cash, dec!(5.20)); // 零费率：0.52 × 10
                assert_eq!(fill.direction, OrderDirection::Sell);
            }
            _ => panic!("应为 Filled"),
        }
    }

    #[test]
    fn taker_sell_rejected_when_bid_below_limit() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        let snap = snapshot_with(Some(dec!(0.48)), Some(dec!(0.55)), None, None);
        sim.on_market(&snap);

        // 限价下限 0.50 > 买一价 0.48 → 拒单。
        let order = make_order(
            4,
            Side::Up,
            OrderDirection::Sell,
            dec!(0.50),
            dec!(10),
            OrderRole::Taker,
        );
        sim.submit_order(order);

        let event = rx.try_recv().unwrap();
        assert!(matches!(event, ExchangeEvent::Rejected { .. }));
    }

    #[test]
    fn taker_sell_rejected_when_no_bid() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        let snap = snapshot_with(None, Some(dec!(0.55)), None, None);
        sim.on_market(&snap);

        let order = make_order(
            4,
            Side::Up,
            OrderDirection::Sell,
            dec!(0.40),
            dec!(10),
            OrderRole::Taker,
        );
        sim.submit_order(order);

        let event = rx.try_recv().unwrap();
        assert!(matches!(event, ExchangeEvent::Rejected { .. }));
    }

    // ─── 手续费 ───

    #[test]
    fn buy_fee_deducts_from_qty() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::default()); // Taker 4%
        let snap = snapshot_with(Some(dec!(0.48)), Some(dec!(0.50)), None, None);
        sim.on_market(&snap);

        let order = make_order(
            5,
            Side::Up,
            OrderDirection::Buy,
            dec!(0.52),
            dec!(100),
            OrderRole::Taker,
        );
        sim.submit_order(order);

        let event = rx.try_recv().unwrap();
        match event {
            ExchangeEvent::Filled(fill) => {
                // 100 × (1 − 0.04) = 96 股入仓。
                assert_eq!(fill.filled_qty, dec!(96));
                // 付出现金 = 0.50 × 100 = 50。
                assert_eq!(fill.cash, dec!(50));
            }
            _ => panic!("应为 Filled"),
        }
    }

    #[test]
    fn sell_fee_deducts_from_cash() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::default()); // Taker 4%
        let snap = snapshot_with(Some(dec!(0.55)), Some(dec!(0.60)), None, None);
        sim.on_market(&snap);

        let order = make_order(
            6,
            Side::Up,
            OrderDirection::Sell,
            dec!(0.50),
            dec!(100),
            OrderRole::Taker,
        );
        sim.submit_order(order);

        let event = rx.try_recv().unwrap();
        match event {
            ExchangeEvent::Filled(fill) => {
                // 卖出不扣股数。
                assert_eq!(fill.filled_qty, dec!(100));
                // 回收现金 = 0.55 × 100 × (1 − 0.04) = 52.80。
                assert_eq!(fill.cash, dec!(52.80));
            }
            _ => panic!("应为 Filled"),
        }
    }

    #[test]
    fn maker_sell_zero_fee() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::default()); // Maker 0%
        let order = make_order(
            7,
            Side::Down,
            OrderDirection::Sell,
            dec!(0.50),
            dec!(20),
            OrderRole::Maker,
        );
        sim.submit_order(order);

        // DN 侧买一 0.55 > 限价 0.50 → 成交。
        let snap = snapshot_with(None, None, Some(dec!(0.55)), Some(dec!(0.60)));
        sim.on_market(&snap);

        let event = rx.try_recv().unwrap();
        match event {
            ExchangeEvent::Filled(fill) => {
                assert_eq!(fill.filled_qty, dec!(20)); // 不扣
                // Maker 零费：回收 = 0.50 × 20 × (1 − 0) = 10.00。
                assert_eq!(fill.cash, dec!(10.00));
                assert_eq!(fill.price, dec!(0.50));
            }
            _ => panic!("应为 Filled"),
        }
    }

    // ─── 撤单 ───

    #[test]
    fn cancel_removes_resting_order() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        let order = make_order(
            8,
            Side::Up,
            OrderDirection::Buy,
            dec!(0.50),
            dec!(10),
            OrderRole::Maker,
        );
        sim.submit_order(order);
        sim.cancel_order(OrderId(8));
        assert_eq!(sim.resting_order_count(), 0);
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, ExchangeEvent::Canceled(OrderId(8))));
    }

    #[test]
    fn cancel_unknown_order_sends_cancel_failed() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        sim.cancel_order(OrderId(99));
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, ExchangeEvent::CancelFailed(OrderId(99))));
    }

    #[test]
    fn cancel_side_removes_all_orders_on_that_side() {
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        sim.submit_order(make_order(
            1,
            Side::Up,
            OrderDirection::Buy,
            dec!(0.50),
            dec!(10),
            OrderRole::Maker,
        ));
        sim.submit_order(make_order(
            2,
            Side::Up,
            OrderDirection::Sell,
            dec!(0.60),
            dec!(10),
            OrderRole::Maker,
        ));
        sim.submit_order(make_order(
            3,
            Side::Down,
            OrderDirection::Buy,
            dec!(0.50),
            dec!(10),
            OrderRole::Maker,
        ));
        assert_eq!(sim.resting_order_count(), 3);

        sim.cancel_side(Side::Up);
        assert_eq!(sim.resting_order_count(), 1); // 只剩 DN 侧

        // 两个 Canceled 事件。
        let e1 = rx.try_recv().unwrap();
        let e2 = rx.try_recv().unwrap();
        assert!(matches!(e1, ExchangeEvent::Canceled(_)));
        assert!(matches!(e2, ExchangeEvent::Canceled(_)));
    }

    #[test]
    fn cancel_all_removes_everything() {
        let (mut sim, _rx) = Simulator::new(FeeModel::zero());
        sim.submit_order(make_order(
            1,
            Side::Up,
            OrderDirection::Buy,
            dec!(0.50),
            dec!(10),
            OrderRole::Maker,
        ));
        sim.submit_order(make_order(
            2,
            Side::Down,
            OrderDirection::Sell,
            dec!(0.55),
            dec!(10),
            OrderRole::Maker,
        ));
        sim.cancel_all();
        assert_eq!(sim.resting_order_count(), 0);
    }
}
