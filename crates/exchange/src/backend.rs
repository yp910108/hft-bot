//! 执行后端 trait：策略层面向的统一交易所抽象。
//!
//! 策略只依赖本 trait，切换回测 / 模拟 / 真实后端无需改动策略逻辑
//! （见架构决策：回测优先、先模拟）。
//!
//! 职责划分：trait 方法负责**接收指令**（下单 / 撤单），同步返回；
//! 成交、拒单、撤单确认、行情更新等**回报**通过 [`crate::event::ExchangeEvent`]
//! 异步交付给事件循环，不在 trait 方法里同步返回。

use domain::order::{Order, OrderId};
use domain::types::Side;

/// 交易所执行后端。
///
/// 实现者负责接收下单 / 撤单指令并最终通过事件流回报结果。
/// 订单标识由调用方（客户端）在 [`Order`] 中预先分配，故下单方法无需返回标识。
pub trait ExchangeBackend {
    /// 提交一笔挂单。订单标识已包含在 `order.order_id` 中。
    fn submit_order(&mut self, order: Order);

    /// 按订单标识撤销单笔挂单。
    fn cancel_order(&mut self, order_id: OrderId);

    /// 撤销指定一侧的全部活跃挂单。
    ///
    /// 对应策略第四节「撤销对面当前所有活跃挂单」的高频跨侧重算场景。
    fn cancel_side(&mut self, side: Side);

    /// 撤销两侧的全部活跃挂单。
    fn cancel_all(&mut self);
}
