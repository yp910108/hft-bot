//! 策略产出的执行指令。
//!
//! 策略层不直接调用交易所，而是产出一组 `Command` 交由事件循环下发给执行后端
//! （见架构决策：策略产出指令列表、不持有 exchange 引用）。各变体与
//! `ExchangeBackend` 的方法一一对应。

use crate::order::{Order, OrderId};
use crate::types::Side;
use serde::{Deserialize, Serialize};

/// 策略产出的执行指令。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    /// 提交一笔挂单。
    SubmitOrder(Order),
    /// 按订单标识撤销单笔挂单。
    CancelOrder(OrderId),
    /// 撤销指定一侧的全部活跃挂单。
    CancelSide(Side),
    /// 撤销两侧的全部活跃挂单。
    CancelAll,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{Generation, OrderDirection, TimeInForce};
    use crate::types::{OrderRole, Price, Qty};

    #[test]
    fn command_submit_order_carries_order() {
        let order = Order {
            order_id: OrderId(5),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: Price::ZERO,
            qty: Qty::ZERO,
            role: OrderRole::Maker,
            time_in_force: TimeInForce::Gtc,
            generation: Generation::new(),
        };
        let command = Command::SubmitOrder(order);
        match command {
            Command::SubmitOrder(carried) => assert_eq!(carried, order),
            _ => panic!("应为 SubmitOrder 指令"),
        }
    }

    #[test]
    fn command_cancel_variants_carry_targets() {
        assert_eq!(
            Command::CancelOrder(OrderId(3)),
            Command::CancelOrder(OrderId(3))
        );
        assert_eq!(
            Command::CancelSide(Side::Down),
            Command::CancelSide(Side::Down)
        );
        assert_eq!(Command::CancelAll, Command::CancelAll);
    }
}
