//! 订单与成交的领域类型，以及用于消除撤单 / 改挂竞态的订单世代号。
//!
//! 用单调递增的 [`Generation`] 给每一批挂单打标；成交一律入账（成交不可撤销），
//! 世代号仅用于区分回报新旧、决定是否触发后续重算决策。

use crate::types::{Money, OrderRole, Price, Qty, Side};
use rust_decimal::RoundingStrategy;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// 订单世代号：单调递增的批次标记。
///
/// 每当策略发起一轮「撤旧单 → 重算 → 挂新单」，世代号自增一次。
/// 交易所回报携带其所属世代，据此区分回报新旧（成交仍一律入账，旧世代成交入账后
/// 不再触发新的重算决策）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Generation(pub u64);

impl Generation {
    /// 初始世代。
    pub fn new() -> Self {
        Self(0)
    }

    /// 返回下一个世代号。
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl Default for Generation {
    fn default() -> Self {
        Self::new()
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
    /// 下一个待分配的标识值。
    next_value: u64,
}

impl OrderIdGenerator {
    /// 创建一个从 0 开始的生成器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 产出下一个订单标识，内部计数随即自增（命名与 [`Generation::next`] 对齐）。
    ///
    /// 此处刻意保留 `next` 之名以与 [`Generation::next`] 保持一致；它并非迭代器语义
    /// （不返回 `Option`、序列无尽），故抑制 clippy 对 `Iterator::next` 的混淆告警。
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> OrderId {
        let id = OrderId(self.next_value);
        self.next_value += 1;
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

/// 策略产出的执行指令。
///
/// 策略层不直接调用交易所，而是产出一组 `Command` 交由事件循环下发给执行后端
/// （见架构决策：策略产出指令列表、不持有 exchange 引用）。各变体与
/// `ExchangeBackend` 的方法一一对应。
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

/// 交易所对单笔订单的最小量约束。
///
/// 真实交易所对单笔订单有最小下单门槛与精度限制：
/// - 最小份数：单笔至少买卖若干份代币；
/// - 最小金额：单笔名义金额（份数 × 价格）至少若干美元；
/// - 价格 / 数量精度：价格与份数各自限制到固定小数位（超出会被拒）。
///
/// 最小份数与最小金额两个约束在不同价位下松紧不同，故须同时满足。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderConstraints {
    /// 单笔最小份数。
    pub min_order_size: Qty,
    /// 单笔最小名义金额（美元）。
    pub min_notional: Money,
    /// 价格精度：允许的小数位数。
    pub price_scale: u32,
    /// 数量精度：允许的小数位数。
    pub size_scale: u32,
}

impl OrderConstraints {
    /// 判断给定份数与价格是否同时满足最小份数与最小金额约束。
    pub fn is_satisfied(&self, qty: Qty, price: Price) -> bool {
        qty >= self.min_order_size && qty * price >= self.min_notional
    }

    /// 将价格向下量化到允许的精度（买单向下取整不会抬高买价）。
    pub fn quantize_price(&self, price: Price) -> Price {
        price.round_dp_with_strategy(self.price_scale, RoundingStrategy::ToZero)
    }

    /// 将份数向下量化到允许的精度（向下取整不会超出预算）。
    pub fn quantize_qty(&self, qty: Qty) -> Qty {
        qty.round_dp_with_strategy(self.size_scale, RoundingStrategy::ToZero)
    }
}

impl Default for OrderConstraints {
    /// 默认门槛：最少 5 份、最少 1 美元，价格与数量各 2 位小数（Polymarket 实测约束）。
    ///
    /// 挂单（Maker）精度上限即 2 位；吃单（Taker）虽支持更高精度，但 2 位是其合法子集，
    /// 故全局统一用 2 位，既合法又免去按订单类型分支。
    fn default() -> Self {
        Self {
            min_order_size: dec!(5),
            min_notional: dec!(1),
            price_scale: 2,
            size_scale: 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_increments_monotonically() {
        let g0 = Generation::new();
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
        let id0 = generator.next();
        let id1 = generator.next();
        let id2 = generator.next();
        assert_eq!(id0, OrderId(0));
        assert_eq!(id1, OrderId(1));
        assert_eq!(id2, OrderId(2));
        // 后产出的标识严格大于先产出的，保证唯一且单调递增。
        assert!(id2 > id0);
    }

    #[test]
    fn command_submit_order_carries_order() {
        let order = Order {
            order_id: OrderId(5),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: Price::ZERO,
            qty: Qty::ZERO,
            role: OrderRole::Maker,
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

    #[test]
    fn default_constraints_are_five_shares_and_one_dollar() {
        let constraints = OrderConstraints::default();
        assert_eq!(constraints.min_order_size, dec!(5));
        assert_eq!(constraints.min_notional, dec!(1));
    }

    #[test]
    fn constraint_rejects_below_min_size() {
        let constraints = OrderConstraints::default();
        // 价 0.5、4 份：金额 2 美元达标，但份数 4 < 5 → 不满足。
        assert!(!constraints.is_satisfied(dec!(4), dec!(0.5)));
    }

    #[test]
    fn constraint_size_binds_when_price_above_threshold() {
        let constraints = OrderConstraints::default();
        // 价 0.5（高于默认临界价 0.2）：5 份 = 2.5 美元，份数约束更紧。
        // 5 份达标，4.9 份不达标（即便金额仍 > 1）。
        assert!(constraints.is_satisfied(dec!(5), dec!(0.5)));
        assert!(!constraints.is_satisfied(dec!(4.9), dec!(0.5)));
    }

    #[test]
    fn constraint_notional_binds_when_price_below_threshold() {
        let constraints = OrderConstraints::default();
        // 价 0.1（低于默认临界价 0.2）：满足 5 份仅 0.5 美元 < 1，金额约束更紧。
        // 需 10 份才够 1 美元。
        assert!(!constraints.is_satisfied(dec!(5), dec!(0.1)));
        assert!(constraints.is_satisfied(dec!(10), dec!(0.1)));
    }

    #[test]
    fn constraint_satisfied_at_exact_minimums() {
        let constraints = OrderConstraints::default();
        // 价 0.2：5 份恰好 = 1 美元，两约束同时踩线 → 满足（>= 边界）。
        assert!(constraints.is_satisfied(dec!(5), dec!(0.2)));
    }

    #[test]
    fn quantize_price_truncates_down_to_two_decimals() {
        let constraints = OrderConstraints::default();
        // 0.4567 向下量化到 2 位 → 0.45（不四舍五入、不抬高买价）。
        assert_eq!(constraints.quantize_price(dec!(0.4567)), dec!(0.45));
        // 已是 2 位则不变。
        assert_eq!(constraints.quantize_price(dec!(0.45)), dec!(0.45));
    }

    #[test]
    fn quantize_qty_truncates_down_to_two_decimals() {
        let constraints = OrderConstraints::default();
        // 4.7727... 向下量化到 2 位 → 4.77（向下取整不超预算）。
        assert_eq!(constraints.quantize_qty(dec!(4.7727)), dec!(4.77));
        // 整数份不变。
        assert_eq!(constraints.quantize_qty(dec!(100)), dec!(100));
    }

    #[test]
    fn quantize_respects_custom_scale() {
        let constraints = OrderConstraints {
            price_scale: 3,
            size_scale: 0,
            ..OrderConstraints::default()
        };
        // 价格 3 位、数量 0 位（整数份）。
        assert_eq!(constraints.quantize_price(dec!(0.12349)), dec!(0.123));
        assert_eq!(constraints.quantize_qty(dec!(7.9)), dec!(7));
    }
}
