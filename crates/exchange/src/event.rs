//! 交易所事件：后端异步回报给事件循环的统一消息类型。
//!
//! 后端既是指令的去处（下单 / 撤单），也是事件的来源（行情 / 成交 / 拒单 / 撤单确认）。
//! 所有事件收敛为单一枚举 [`ExchangeEvent`]，由单写者事件循环串行消费。

use domain::market::MarketSnapshot;
use domain::order::{Fill, OrderId};

/// 订单被拒的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// 可用现金不足，无法承接该笔下单。
    InsufficientCash,
    /// 下单量低于交易所最小下单量。
    BelowMinOrderSize,
    /// 价格不符合交易所最小报价单位（tick size）。
    InvalidPrice,
    /// 被风控的现金安全哨兵（Cash Guard）拦截。
    CashGuardBlocked,
}

/// 交易所异步回报的事件。
#[derive(Debug, Clone, PartialEq)]
pub enum ExchangeEvent {
    /// 盘口顶部发生变化，携带最新双边市场快照。
    BookUpdate(MarketSnapshot),
    /// 一笔成交回报。
    Filled(Fill),
    /// 一笔下单被拒。
    Rejected {
        /// 被拒订单的标识。
        order_id: OrderId,
        /// 被拒原因。
        reason: RejectReason,
    },
    /// 一笔撤单已被交易所确认。
    Canceled(OrderId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::order::{Generation, OrderDirection};
    use domain::types::Side;
    use rust_decimal_macros::dec;

    #[test]
    fn filled_event_carries_fill() {
        let fill = Fill {
            order_id: OrderId(7),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.4),
            filled_qty: dec!(100),
            cash: dec!(40),
            generation: Generation::new(),
        };
        let event = ExchangeEvent::Filled(fill);
        // 成交事件应原样携带 Fill 数据，供账本更新使用。
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
