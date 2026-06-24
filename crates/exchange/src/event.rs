//! 交易所事件：后端异步回报给事件循环的消息。
//!
//! 行情、成交、拒单、撤单确认，全部收敛为一个枚举 [`ExchangeEvent`]，由事件循环串行消费。

use domain::market::MarketSnapshot;
use domain::order::{Fill, OrderId};

/// 订单被拒的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// 现金不够下这笔单。
    InsufficientCash,
    /// 下单量低于交易所最小量。
    BelowMinOrderSize,
    /// 价格不符合最小 tick。
    InvalidPrice,
    /// 被 Cash Guard 红线拦截。
    CashGuardBlocked,
}

/// 交易所异步回报事件。
#[derive(Debug, Clone, PartialEq)]
pub enum ExchangeEvent {
    /// 盘口变化，带最新双边快照。
    BookUpdate(MarketSnapshot),
    /// 成交回报。
    Filled(Fill),
    /// 下单被拒。
    Rejected {
        /// 被拒订单 ID。
        order_id: OrderId,
        /// 拒绝原因。
        reason: RejectReason,
    },
    /// 撤单确认。
    Canceled(OrderId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::order::{Generation, OrderDirection};
    use domain::types::{OrderRole, Side};
    use rust_decimal_macros::dec;

    #[test]
    fn filled_event_carries_fill() {
        let fill = Fill {
            order_id: OrderId(7),
            side: Side::Up,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.4),
            filled_qty: dec!(100),
            cash: dec!(40),
            generation: Generation::new(),
        };
        let event = ExchangeEvent::Filled(fill);
        // 成交事件应原样携带 Fill 数据，账本靠它更新。
        match event {
            ExchangeEvent::Filled(f) => assert_eq!(f, fill),
            _ => panic!("应为 Filled 事件"),
        }
    }

    #[test]
    fn rejected_event_carries_order_id_and_reason() {
        let event = ExchangeEvent::Rejected {
            order_id: OrderId(3),
            reason: RejectReason::CashGuardBlocked,
        };
        match event {
            ExchangeEvent::Rejected { order_id, reason } => {
                assert_eq!(order_id, OrderId(3));
                assert_eq!(reason, RejectReason::CashGuardBlocked);
            }
            _ => panic!("应为 Rejected 事件"),
        }
    }
}
