//! 交易所后端 trait。
//!
//! 策略只依赖这个 trait，换后端（回测/模拟/真实）不用改策略代码。
//! trait 方法只管收指令（下单/撤单），结果通过 [`crate::event::ExchangeEvent`] 异步回报。

use domain::order::{Order, OrderId};
use domain::types::Side;

/// 交易所执行后端。
///
/// 收下单/撤单指令，结果通过事件流异步回报。
/// 订单 ID 由调用方预先分配在 [`Order`] 里，下单方法不需要再返回 ID。
pub trait ExchangeBackend {
    /// 提交一笔挂单。订单 ID 在 `order.order_id` 里。
    fn submit_order(&mut self, order: Order);

    /// 按订单 ID 撤销单笔挂单。
    fn cancel_order(&mut self, order_id: OrderId);

    /// 撤销指定方向的全部活跃挂单。
    fn cancel_side(&mut self, side: Side);

    /// 撤销所有活跃挂单。
    fn cancel_all(&mut self);
}
