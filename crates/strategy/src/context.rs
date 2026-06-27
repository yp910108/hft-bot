//! engine 与 strategy 之间的契约类型。
//!
//! engine 每个 tick 把当前世界打包成只读的 [`DecisionContext`] 喂给 strategy，
//! strategy 算出 [`Decision`]（要发的指令意图 + 可选的阶段跳转）还给 engine。
//! strategy 不知道指令怎么发、账本怎么记、ID 怎么分配——保持纯函数。
//!
//! 订单意图 [`OrderIntent`] 不带 order_id 与 generation：那是 engine 的职责
//! （下发时分配），strategy 只表达「想在某侧某价挂多少、什么角色、什么有效期」。

use domain::market::MarketSnapshot;
use domain::order::{OrderConstraints, OrderDirection, OrderId, TimeInForce};
use domain::pnl::PositionSnapshot;
use domain::state::RobotState;
use domain::types::{Money, OrderRole, Price, Qty, Side};
use exchange::clock::Millis;

/// 一笔活跃挂单的只读视图（engine 维护，喂给 strategy 做精细订单管理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveOrder {
    pub order_id: OrderId,
    pub side: Side,
    pub direction: OrderDirection,
    pub price: Price,
    pub qty: Qty,
    pub role: OrderRole,
}

impl ActiveOrder {
    /// 该挂单占用的名义金额 = 价 × 量。
    pub fn notional(&self) -> Money {
        self.price * self.qty
    }
}

/// 各资金池的额度（engine 跨阶段追踪，花完即停）。
///
/// 两类池基数口径不同（见策略文档）：
/// - 核心做市池：档位比例相对**池总额**（如 2%×15%V 固定），耗尽判定看剩余。
/// - 动态 / EV 池：单步比例相对**当前剩余**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolBudgets {
    /// 核心做市池总额（建仓/续挂档位比例的基数）。
    pub grid_maker_total: Money,
    /// 核心做市池剩余（耗尽判定）。
    pub grid_maker_remaining: Money,
    /// 动态对冲池剩余（单步比例基数）。
    pub dynamic_remaining: Money,
    /// EV 对冲池剩余（单步比例基数）。
    pub ev_remaining: Money,
    /// 最大单边敞口上限（绝对金额，= 动态对冲池的 50%）。
    pub max_exposure: Money,
}

/// 本次 tick 由什么事件触发。小策略据此决定反应（尤其配对态要区分成交来自哪侧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// 盘口更新。
    BookUpdate,
    /// 某侧某笔成交。`side` 是成交侧。
    Fill { side: Side },
    /// 撤单确认 / 撤单失败（订单簿镜像已由 engine 更新）。
    OrderUpdate,
    /// 单步生命周期定时器到期（对冲超时兜底）。
    TimerFired,
}

/// 只读世界切片：strategy 做决策需要的全部信息。
#[derive(Debug, Clone)]
pub struct DecisionContext<'a> {
    /// 总资金 V，阈值（%V）换算用。
    pub total_capital: Money,
    /// 本次 tick 的触发事件。
    pub trigger: Trigger,
    /// 当前时刻（自场开始起毫秒）。
    pub now: Millis,
    /// 剩余时间（到交割还有多少毫秒）。
    pub time_to_expiry: Millis,
    /// 当前状态。
    pub state: RobotState,
    /// 主战场侧（建仓时锁定，一轮不换）。建仓前为 None。
    pub main_field: Option<Side>,
    /// 主战场侧是否已永久停铺（敞口曾超限，做市态本场不再铺）。
    pub main_field_frozen: bool,
    /// 持仓快照（按侧股数与成本）。
    pub position: PositionSnapshot,
    /// 当前盘口。
    pub market: MarketSnapshot,
    /// 各池预算。
    pub pools: PoolBudgets,
    /// 当前活跃挂单（engine 维护的本地镜像）。
    pub active_orders: &'a [ActiveOrder],
    /// 上次对冲动作的时间戳（冷却判定用）。从未对冲为 None。
    pub last_hedge_at: Option<Millis>,
    /// 熔断态下，spread 持续低于恢复阈值的起始时刻；一旦 spread 再次爆宽就清空。
    /// engine 维护：用于判定「稳定 5 秒」。非熔断态或尚未平静为 None。
    pub calm_since: Option<Millis>,
    /// 下单量/价精度与最小量约束。
    pub constraints: OrderConstraints,
}

impl DecisionContext<'_> {
    /// 把「占总资金的比例」换算成绝对金额。如 0.005 → 0.5%V。
    pub fn pct_of_capital(&self, ratio: Money) -> Money {
        self.total_capital * ratio
    }

    /// 当前活跃挂单里属于某侧的名义金额合计。
    pub fn active_notional(&self, side: Side) -> Money {
        self.active_orders
            .iter()
            .filter(|o| o.side == side)
            .map(|o| o.notional())
            .sum()
    }
}

/// strategy 想让 engine 执行的一条指令意图。
///
/// 与 `domain::Command` 的区别：Submit 携带的是 [`OrderIntent`]（无 ID/世代），
/// engine 落地时再补全。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandIntent {
    /// 提交一笔新挂单。
    Submit(OrderIntent),
    /// 撤销指定订单。
    Cancel(OrderId),
    /// 撤销某侧全部活跃挂单。
    CancelSide(Side),
    /// 撤销所有活跃挂单。
    CancelAll,
}

/// 一笔订单意图：strategy 表达「想挂什么」，不含 engine 才知道的 ID 与世代。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderIntent {
    pub side: Side,
    pub direction: OrderDirection,
    pub price: Price,
    pub qty: Qty,
    pub role: OrderRole,
    pub time_in_force: TimeInForce,
}

impl OrderIntent {
    /// 便捷构造：Gtc Maker 买单。
    pub fn maker_buy(side: Side, price: Price, qty: Qty) -> Self {
        Self {
            side,
            direction: OrderDirection::Buy,
            price,
            qty,
            role: OrderRole::Maker,
            time_in_force: TimeInForce::Gtc,
        }
    }

    /// 便捷构造：Ioc Taker 买单（EV 扫盘，price 为保护上限价）。
    pub fn ioc_taker_buy(side: Side, price: Price, qty: Qty) -> Self {
        Self {
            side,
            direction: OrderDirection::Buy,
            price,
            qty,
            role: OrderRole::Taker,
            time_in_force: TimeInForce::Ioc,
        }
    }
}

/// strategy 的决策结果：要发的指令 + 可选的阶段跳转。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decision {
    /// 本次要执行的指令意图，按顺序下发。
    pub commands: Vec<CommandIntent>,
    /// 请求跳转到的新状态。None 表示留在当前状态。
    pub transition: Option<RobotState>,
}

impl Decision {
    /// 空决策：什么都不做，不跳转（装死 / Skip）。
    pub fn skip() -> Self {
        Self::default()
    }

    /// 只跳转、不发指令。
    pub fn transition(to: RobotState) -> Self {
        Self {
            commands: Vec::new(),
            transition: Some(to),
        }
    }

    /// 追加一条指令意图，链式调用。
    pub fn with(mut self, command: CommandIntent) -> Self {
        self.commands.push(command);
        self
    }

    /// 设置跳转目标，链式调用。
    pub fn moving_to(mut self, to: RobotState) -> Self {
        self.transition = Some(to);
        self
    }

    /// 是否什么都不做（无指令无跳转）。
    pub fn is_skip(&self) -> bool {
        self.commands.is_empty() && self.transition.is_none()
    }
}

/// 一个阶段小策略：给定只读世界快照，算出此刻的决策。纯函数，无副作用。
pub trait PhaseStrategy {
    fn decide(&self, ctx: &DecisionContext) -> Decision;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn order_intent_maker_buy_is_gtc_maker() {
        let intent = OrderIntent::maker_buy(Side::Up, dec!(0.4), dec!(100));
        assert_eq!(intent.role, OrderRole::Maker);
        assert_eq!(intent.time_in_force, TimeInForce::Gtc);
        assert_eq!(intent.direction, OrderDirection::Buy);
    }

    #[test]
    fn order_intent_ioc_taker_is_ioc_taker() {
        let intent = OrderIntent::ioc_taker_buy(Side::Down, dec!(0.85), dec!(50));
        assert_eq!(intent.role, OrderRole::Taker);
        assert_eq!(intent.time_in_force, TimeInForce::Ioc);
    }

    #[test]
    fn active_order_notional_is_price_times_qty() {
        let o = ActiveOrder {
            order_id: OrderId(1),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.4),
            qty: dec!(100),
            role: OrderRole::Maker,
        };
        assert_eq!(o.notional(), dec!(40.0));
    }

    #[test]
    fn decision_builders_compose() {
        let d = Decision::skip()
            .with(CommandIntent::CancelAll)
            .moving_to(RobotState::SettlementWait);
        assert_eq!(d.commands.len(), 1);
        assert_eq!(d.transition, Some(RobotState::SettlementWait));
        assert!(!d.is_skip());
    }

    #[test]
    fn skip_is_empty() {
        assert!(Decision::skip().is_skip());
    }
}
