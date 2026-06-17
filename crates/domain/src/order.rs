//! 订单与成交的领域类型，以及用于消除撤单 / 改挂竞态的订单世代号。
//!
//! 对应策略风险修复项 #6：在异步高频环境下，「撤单尚未确认时又来一笔成交」
//! 会导致基于过期状态误操作。本模块用单调递增的 [`Generation`] 给每一批挂单打标，
//! 重算时旧世代的回报可被安全丢弃。

use crate::types::{Money, OrderRole, Price, Qty, Side};
use serde::{Deserialize, Serialize};

/// 订单世代号：单调递增的批次标记。
///
/// 每当策略发起一轮「撤旧单 → 重算 → 挂新单」，世代号自增一次。
/// 交易所回报携带其所属世代，事件循环据此丢弃过期世代的回报，避免竞态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Generation(pub u64);

impl Generation {
    /// 初始世代。
    pub fn first() -> Self {
        Self(0)
    }

    /// 返回下一个世代号。
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// 订单标识：由客户端分配的单调递增编号，唯一指认一笔挂单。
///
/// 采用客户端生成而非交易所回填，使下单瞬间即可本地引用该单（撤单、对账），
/// 无需等待交易所异步返回；且回测、模拟、实盘三种后端共用同一套标识口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OrderId(pub u64);

/// 订单标识生成器：持续产出单调递增、互不重复的 [`OrderId`]。
#[derive(Debug, Clone, Default)]
pub struct OrderIdGenerator {
    next: u64,
}

impl OrderIdGenerator {
    /// 创建一个从 0 开始的生成器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 产出下一个订单标识，内部计数随即自增。
    pub fn next_id(&mut self) -> OrderId {
        let id = OrderId(self.next);
        self.next += 1;
        id
    }
}

/// 订单方向：在二元市场中买入或卖出某一侧。
///
/// 策略常规阶段只买入（梯度接低），卖出仅在特定清仓场景使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderDirection {
    Buy,
    Sell,
}

/// 一笔挂单的描述。
///
/// `role` 标明这笔单意图作为 Maker 还是 Taker 成交，决定适用费率与所处策略阶段
/// （常规梯度接低阶段禁止 Taker，见策略说明书第八节红线约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    /// 客户端分配的订单标识。
    pub order_id: OrderId,
    /// 下单作用于哪一侧资产。
    pub side: Side,
    /// 买入或卖出。
    pub direction: OrderDirection,
    /// 限价。
    pub price: Price,
    /// 下单股数。
    pub qty: Qty,
    /// 撮合角色（Maker / Taker）。
    pub role: OrderRole,
    /// 所属世代，用于竞态隔离。
    pub generation: Generation,
}

/// 一笔成交回报。
///
/// 手续费体现为到手股数的扣减（见 `domain::fee::FeeModel`），故成交回报直接记录
/// **净入仓股数**与**花费现金**两个事实，账本据此更新持仓与成本，无需再做费率换算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    /// 触发本笔成交的订单标识。
    pub order_id: OrderId,
    /// 成交作用于哪一侧资产。
    pub side: Side,
    /// 买入或卖出。
    pub direction: OrderDirection,
    /// 实际成交价（每股价格），EV 模块据此映射胜出概率。
    pub price: Price,
    /// 扣除手续费后实际入仓的净股数。
    pub filled_qty: Qty,
    /// 本笔成交花费的现金 = 下单名义股数 × 成交价。
    pub cash: Money,
    /// 触发本笔成交的订单所属世代。
    pub generation: Generation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_increments_monotonically() {
        let g0 = Generation::first();
        let g1 = g0.next();
        let g2 = g1.next();
        assert_eq!(g0, Generation(0));
        assert_eq!(g1, Generation(1));
        assert_eq!(g2, Generation(2));
        // 较新世代严格大于较旧世代，可用于丢弃过期回报。
        assert!(g2 > g0);
    }

    #[test]
    fn order_id_generator_yields_monotonic_unique_ids() {
        let mut generator = OrderIdGenerator::new();
        let id0 = generator.next_id();
        let id1 = generator.next_id();
        let id2 = generator.next_id();
        assert_eq!(id0, OrderId(0));
        assert_eq!(id1, OrderId(1));
        assert_eq!(id2, OrderId(2));
        // 后产出的标识严格大于先产出的，保证唯一且单调递增。
        assert!(id2 > id0);
    }
}
