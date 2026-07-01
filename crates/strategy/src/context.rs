//! Engine 与 strategy 之间的契约类型。
//!
//! Engine 每 tick 把当前世界打包成只读的 [`DecisionContext`] 喂给 strategy，
//! strategy 算出 [`Decision`]（要发的指令意图 + 可选的阶段跳转）还给 engine。
//! strategy 不知道指令怎么发、ID 怎么分配——保持纯函数。

use domain::market::MarketSnapshot;
use domain::order::{OrderConstraints, OrderDirection, OrderId, TimeInForce};
use domain::phase::Phase;
use domain::types::{Money, OrderRole, Price, Qty, Side};
use inventory::lot::LotId;
use inventory::Inventory;
use rust_decimal::Decimal;

use crate::config::StrategyConfig;

// ─────────────────────────── Trigger ───────────────────────────

/// 本次 tick 由什么事件触发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// 盘口更新。
    BookUpdate,
    /// 某笔成交。
    Filled {
        side: Side,
        direction: OrderDirection,
        /// 买入成交时关联的 LotId（卖出成交时为 None，engine 层处理）。
        lot_id: Option<LotId>,
    },
    /// 撤单确认 / 撤单失败。
    OrderUpdate,
}

// ─────────────────────────── ActiveOrder ───────────────────────────

/// 一笔活跃挂单的只读视图（engine 维护，喂给 strategy 做决策）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveOrder {
    pub order_id: OrderId,
    pub side: Side,
    pub direction: OrderDirection,
    pub price: Price,
    pub qty: Qty,
    pub role: OrderRole,
    /// 关联的 LotId（卖单绑定它要平的那笔；买单为 None）。
    pub lot_id: Option<LotId>,
}

// ─────────────────────────── DecisionContext ───────────────────────────

/// Engine 每 tick 组装的只读世界切片。策略的全部输入。
pub struct DecisionContext<'a> {
    /// 本 tick 触发原因。
    pub trigger: Trigger,
    /// 场内进度 (0~1)，= elapsed_secs / 900。
    pub progress: Decimal,
    /// 当前盘口。
    pub market: MarketSnapshot,
    /// 逐笔持仓账本（只读）。
    pub inventory: &'a Inventory,
    /// 当前活跃挂单。
    pub active_orders: &'a [ActiveOrder],
    /// 可用现金（总资金 − 净投入 − 活跃买单名义）。
    pub free_cash: Money,
    /// 精度约束。
    pub constraints: OrderConstraints,
    /// 策略参数。
    pub config: &'a StrategyConfig,
}

// ─────────────────────────── CommandIntent ───────────────────────────

/// 策略产出的指令意图。不带 order_id（由 engine 落地时分配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandIntent {
    /// 提交买单。
    SubmitBuy {
        side: Side,
        price: Price,
        qty: Qty,
        role: OrderRole,
        tif: TimeInForce,
    },
    /// 提交卖单，绑定要平的 Lot。
    SubmitSell {
        lot_id: LotId,
        side: Side,
        price: Price,
        qty: Qty,
        role: OrderRole,
        tif: TimeInForce,
    },
    /// 撤销指定订单。
    Cancel(OrderId),
    /// 撤销指定侧全部挂单。
    CancelSide(Side),
    /// 撤销所有挂单。
    CancelAll,
}

// ─────────────────────────── Decision ───────────────────────────

/// 策略产出的决策。
#[derive(Debug, Clone, Default)]
pub struct Decision {
    /// 要下发的指令列表。
    pub commands: Vec<CommandIntent>,
    /// 可选的阶段跳转（None = 留在当前阶段）。
    pub transition: Option<Phase>,
}

impl Decision {
    /// 空决策（什么也不做）。
    pub fn skip() -> Self {
        Self::default()
    }
}
