//! 策略模块：梯度接低 + 对冲决策。
//!
//! 三大块：
//! 1. **做市**：在便宜侧分层挂买单，成交后自动在对面挂配对单锁利润。
//! 2. **对冲决策**：判断该追买哪侧、量的上限，供 engine 执行。
//! 3. **对冲梯度执行**：1 档 Taker 确保响应 + N 档 Maker 等好价省费。
//!
//! 纯逻辑模块：吃行情和持仓，吐 [`Command`] 列表或 [`HedgeDecision`]，不碰交易所。

use domain::market::MarketSnapshot;
use domain::order::{
    Command, Generation, Order, OrderConstraints, OrderDirection, OrderIdGenerator,
};
use domain::pnl::PositionSnapshot;
use domain::state::RobotState;
use domain::types::{Money, OrderRole, Price, Qty, Side};
use fsm::Thresholds;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// 一档梯度的参数：挂多低、用多少钱。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderRung {
    /// 挂单价 = best_ask - 这个偏移量。
    pub price_offset: Decimal,
    /// 用核心做市池的多少比例（0.02 = 2%）。
    pub pool_fraction: Decimal,
}

/// 对冲梯度中一档 Maker 的参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HedgeRung {
    /// 挂单价 = best_ask - 这个偏移量。
    pub price_offset: Decimal,
    /// 占单步预算的比例（0.30 = 30%）。
    pub budget_fraction: Decimal,
}

/// 对冲梯度配置：1 档 Taker（立即成交）+ N 档 Maker（等更好价格省费）。
///
/// Taker 保证对冲的即时响应能力，Maker 在价格回落时降低整体均价。
/// 各档预算比例之和应为 1.0（Taker + 所有 Maker）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HedgeGradientConfig {
    /// Taker 档占单步预算的比例（0.40 = 40%）。
    pub taker_fraction: Decimal,
    /// Maker 各档，从近到远排列。
    pub maker_rungs: Vec<HedgeRung>,
}

impl Default for HedgeGradientConfig {
    /// 默认值：Taker 40% + 两档 Maker（偏移 0.01 占 30%、偏移 0.02 占 30%）。
    fn default() -> Self {
        Self {
            taker_fraction: dec!(0.40),
            maker_rungs: vec![
                HedgeRung {
                    price_offset: dec!(0.01),
                    budget_fraction: dec!(0.30),
                },
                HedgeRung {
                    price_offset: dec!(0.02),
                    budget_fraction: dec!(0.30),
                },
            ],
        }
    }
}

/// 布阵配置：三档梯度 + 主战场判定 + 配对重算参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderConfig {
    /// 三档梯度，从浅到深。
    pub rungs: [LadderRung; 3],
    /// 只有 best_ask 低于这个值的一侧才能当主战场。
    pub main_field_max_ask: Price,
    /// 对面配对价 = 1 - 本边均价 - 这个值。留出利润空间。
    pub min_profit_margin: Decimal,
    /// 成交后在更低价续挂的价格步长。
    pub follow_offset: Decimal,
    /// 续挂用核心做市池的比例。
    pub follow_fraction: Decimal,
}

impl Default for LadderConfig {
    /// 默认值：三档偏移 0.01/0.02/0.03，池占比 2%/3%/5%，主战场阈值 0.5，
    /// 利润空间 0.02，续挂步长 0.01、占比 2%。
    fn default() -> Self {
        Self {
            rungs: [
                LadderRung {
                    price_offset: dec!(0.01),
                    pool_fraction: dec!(0.02),
                },
                LadderRung {
                    price_offset: dec!(0.02),
                    pool_fraction: dec!(0.03),
                },
                LadderRung {
                    price_offset: dec!(0.03),
                    pool_fraction: dec!(0.05),
                },
            ],
            main_field_max_ask: dec!(0.5),
            min_profit_margin: dec!(0.02),
            follow_offset: dec!(0.01),
            follow_fraction: dec!(0.02),
        }
    }
}

/// 本边成交后的上下文，给配对重算用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillContext {
    /// 成交发生在哪一侧。
    pub filled_side: Side,
    /// 成交价。
    pub filled_price: Price,
    /// 本边成交后的总持仓量。
    pub own_qty: Qty,
    /// 对面当前持仓量。
    pub opposite_qty: Qty,
    /// 本边成交后的加权均价。
    pub own_average_price: Price,
    /// 对面当前卖一价；没有时配对单只能挂起。
    pub opposite_best_ask: Option<Price>,
}

/// 挂起的配对买单：配对价不低于对面 Ask，暂时挂不出去，等价格跌下来再补。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingOrder {
    /// 要挂到哪一侧。
    pub side: Side,
    /// 目标配对价。
    pub price: Price,
    /// 目标买入量。
    pub qty: Qty,
}

/// 配对重算的输出：立即下发的指令 + 可能挂起的配对单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecomputeResult {
    /// 立即执行的指令（续挂、撤对面、能直接挂的配对单）。
    pub commands: Vec<Command>,
    /// 配对价 >= 对面 Ask 时暂存在这里，等价格到位再补挂。
    pub pending: Option<PendingOrder>,
}

/// 梯度接低策略的执行器。
#[derive(Debug, Clone)]
pub struct GradientLadder {
    config: LadderConfig,
    hedge_config: HedgeGradientConfig,
}

impl GradientLadder {
    /// 用指定配置创建。
    pub fn new(config: LadderConfig, hedge_config: HedgeGradientConfig) -> Self {
        Self {
            config,
            hedge_config,
        }
    }

    /// 用默认配置创建。
    pub fn with_default_config() -> Self {
        Self::new(LadderConfig::default(), HedgeGradientConfig::default())
    }

    /// 选出本轮做市主战场。
    ///
    /// 取双边中 best_ask 低于阈值且更小的一侧；都不满足返回 None。
    pub fn select_main_field(&self, market: &MarketSnapshot) -> Option<Side> {
        let up_ask = market.book(Side::Up).best_ask;
        let down_ask = market.book(Side::Down).best_ask;
        let threshold = self.config.main_field_max_ask;

        match (up_ask, down_ask) {
            (Some(up), Some(down)) => {
                let up_ok = up < threshold;
                let down_ok = down < threshold;
                match (up_ok, down_ok) {
                    (true, true) => Some(if up <= down { Side::Up } else { Side::Down }),
                    (true, false) => Some(Side::Up),
                    (false, true) => Some(Side::Down),
                    (false, false) => None,
                }
            }
            (Some(up), None) if up < threshold => Some(Side::Up),
            (None, Some(down)) if down < threshold => Some(Side::Down),
            _ => None,
        }
    }

    /// 生成初始布阵的下单指令。
    ///
    /// 无主战场时返回空列表；某档不够最小量约束就跳过（不上调，守资金纪律）。
    pub fn deploy(
        &self,
        market: &MarketSnapshot,
        grid_maker_pool: Money,
        constraints: &OrderConstraints,
        id_generator: &mut OrderIdGenerator,
        generation: Generation,
    ) -> Vec<Command> {
        let Some(side) = self.select_main_field(market) else {
            return Vec::new();
        };
        let best_ask = market.book(side).best_ask.expect("主战场必有 best_ask");

        let mut commands = Vec::with_capacity(self.config.rungs.len());
        for rung in &self.config.rungs {
            // 买单价格向下取整，不抬高买价。
            let price = constraints.quantize_price(best_ask - rung.price_offset);
            // 价格非正说明盘口极薄，跳过。
            if price <= Decimal::ZERO {
                continue;
            }
            // 股数向下量化后校验最小量，不够就跳过（不上调，避免超支）。
            let qty =
                constraints.quantize_qty(self.rung_qty(grid_maker_pool, rung.pool_fraction, price));
            if !constraints.is_satisfied(qty, price) {
                continue;
            }
            let order = Order {
                order_id: id_generator.next(),
                side,
                direction: OrderDirection::Buy,
                price,
                qty,
                role: OrderRole::Maker,
                generation,
            };
            commands.push(Command::SubmitOrder(order));
        }
        commands
    }

    /// 单层股数 = 池额度 × 占比 ÷ 挂单价（把分配金额换算为股数）。
    fn rung_qty(&self, grid_maker_pool: Money, pool_fraction: Decimal, price: Price) -> Qty {
        let budget = grid_maker_pool * pool_fraction;
        budget / price
    }

    /// 跨侧配对重算：本边 Maker 买单成交后触发。
    ///
    /// 做三件事：
    /// 1. 本边续挂追低：在成交价下方再挂一档，继续拉低均价。
    /// 2. 撤对面所有旧挂单。
    /// 3. 重算对面配对价（= 1 - 本边均价 - 利润空间），只补到跟本边持仓一样多（防滚雪球）。
    ///    配对价 < 对面 Ask 就直接挂，否则存到 PendingOrder 等价格到位再补。
    pub fn recompute_after_fill(
        &self,
        fill: &FillContext,
        grid_maker_pool: Money,
        constraints: &OrderConstraints,
        id_generator: &mut OrderIdGenerator,
        generation: Generation,
    ) -> RecomputeResult {
        let mut commands = Vec::new();
        let mut pending = None;

        // ① 本边续挂追低：成交价下方 follow_offset 再挂一档。
        let follow_price =
            constraints.quantize_price(fill.filled_price - self.config.follow_offset);
        if follow_price > Decimal::ZERO {
            let follow_qty = constraints.quantize_qty(self.rung_qty(
                grid_maker_pool,
                self.config.follow_fraction,
                follow_price,
            ));
            if constraints.is_satisfied(follow_qty, follow_price) {
                commands.push(Command::SubmitOrder(Order {
                    order_id: id_generator.next(),
                    side: fill.filled_side,
                    direction: OrderDirection::Buy,
                    price: follow_price,
                    qty: follow_qty,
                    role: OrderRole::Maker,
                    generation,
                }));
            }
        }

        // ② 撤销对面全部活跃挂单。
        let opposite = fill.filled_side.opposite();
        commands.push(Command::CancelSide(opposite));

        // ③ 重算对面配对价 = 1 - 本边均价 - 最小利润空间。
        let pair_price = constraints
            .quantize_price(Decimal::ONE - fill.own_average_price - self.config.min_profit_margin);
        if pair_price > Decimal::ZERO {
            // 配对量采用「目标摽齐」：对面目标持仓 = 本边持仓，只补齐差额。
            // 差额 ≤ 0 表示对面已摽齐（或超过），不再下配对单——这是根除仓位滚雪球的关键
            // （配对买入成交不会再无限放大持仓）。
            let target_gap = fill.own_qty - fill.opposite_qty;
            let pair_qty = constraints.quantize_qty(target_gap.max(Decimal::ZERO));
            if constraints.is_satisfied(pair_qty, pair_price) {
                // Ask 审计：配对价低于对面当前卖一价才能作为 Maker 直接挂出，否则挂起。
                let can_post_now = matches!(fill.opposite_best_ask, Some(ask) if pair_price < ask);
                if can_post_now {
                    commands.push(Command::SubmitOrder(Order {
                        order_id: id_generator.next(),
                        side: opposite,
                        direction: OrderDirection::Buy,
                        price: pair_price,
                        qty: pair_qty,
                        role: OrderRole::Maker,
                        generation,
                    }));
                } else {
                    pending = Some(PendingOrder {
                        side: opposite,
                        price: pair_price,
                        qty: pair_qty,
                    });
                }
            }
        }

        RecomputeResult { commands, pending }
    }

    /// 对冲单步：产出一条 Taker 买单。
    ///
    /// 动态对冲传 `cap = Some(摽齐缺口)`，EV 对冲传 `cap = None`。
    /// 以该侧 best_ask 为成交价，缺卖一就不打。
    /// 至多产出一条指令，计步和限频由 engine 管。
    #[allow(clippy::too_many_arguments)]
    pub fn hedge_taker_step(
        &self,
        side: Side,
        cap: Option<Qty>,
        market: &MarketSnapshot,
        step_budget: Money,
        constraints: &OrderConstraints,
        id_generator: &mut OrderIdGenerator,
        generation: Generation,
    ) -> Vec<Command> {
        // 摽齐缺口为 Some 且 ≤ 0：已无需补，直接收手本步。
        if matches!(cap, Some(gap) if gap <= Decimal::ZERO) {
            return Vec::new();
        }
        // Taker 以卖一价吃单；卖一缺失则无从成交。
        let Some(best_ask) = market.book(side).best_ask else {
            return Vec::new();
        };
        if best_ask <= Decimal::ZERO {
            return Vec::new();
        }

        // 单步量 = 预算可买量，再受摽齐缺口封顶（若有）。
        let budget_qty = step_budget / best_ask;
        let target_qty = match cap {
            Some(gap) => budget_qty.min(gap),
            None => budget_qty,
        };
        let qty = constraints.quantize_qty(target_qty);
        if !constraints.is_satisfied(qty, best_ask) {
            return Vec::new();
        }

        vec![Command::SubmitOrder(Order {
            order_id: id_generator.next(),
            side,
            direction: OrderDirection::Buy,
            price: best_ask,
            qty,
            role: OrderRole::Taker,
            generation,
        })]
    }

    /// 对冲梯度单步：产出 1 条 Taker + N 条 Maker 买单。
    ///
    /// 第 1 档 Taker 以 best_ask 立即成交（确保对冲响应），
    /// 后续各档 Maker 以更低价被动挂单等成交（省 4% 手续费）。
    /// cap（摽齐缺口）按顺序扣减：Taker 先用，剩下的分给 Maker。
    #[allow(clippy::too_many_arguments)]
    pub fn hedge_gradient_step(
        &self,
        side: Side,
        cap: Option<Qty>,
        market: &MarketSnapshot,
        step_budget: Money,
        constraints: &OrderConstraints,
        id_generator: &mut OrderIdGenerator,
        generation: Generation,
    ) -> Vec<Command> {
        // 摽齐缺口已补完，不再追买。
        if matches!(cap, Some(gap) if gap <= Decimal::ZERO) {
            return Vec::new();
        }
        // 没有卖一价就无法确定基准价格。
        let Some(best_ask) = market.book(side).best_ask else {
            return Vec::new();
        };
        if best_ask <= Decimal::ZERO {
            return Vec::new();
        }

        let mut commands = Vec::new();
        let mut remaining_cap = cap; // None 表示无上限

        // — 第 1 档：Taker，以 best_ask 吃单 —
        let taker_budget = step_budget * self.hedge_config.taker_fraction;
        let taker_qty = Self::calc_rung_qty(taker_budget, best_ask, remaining_cap, constraints);
        if constraints.is_satisfied(taker_qty, best_ask) {
            commands.push(Command::SubmitOrder(Order {
                order_id: id_generator.next(),
                side,
                direction: OrderDirection::Buy,
                price: best_ask,
                qty: taker_qty,
                role: OrderRole::Taker,
                generation,
            }));
            remaining_cap = remaining_cap.map(|c| c - taker_qty);
        }

        // — 后续各档：Maker，以 best_ask - offset 被动挂单 —
        for rung in &self.hedge_config.maker_rungs {
            // cap 已耗尽，不再铺。
            if matches!(remaining_cap, Some(c) if c <= Decimal::ZERO) {
                break;
            }
            let maker_price = constraints.quantize_price(best_ask - rung.price_offset);
            if maker_price <= Decimal::ZERO {
                continue;
            }
            let maker_budget = step_budget * rung.budget_fraction;
            let maker_qty =
                Self::calc_rung_qty(maker_budget, maker_price, remaining_cap, constraints);
            if !constraints.is_satisfied(maker_qty, maker_price) {
                continue;
            }
            commands.push(Command::SubmitOrder(Order {
                order_id: id_generator.next(),
                side,
                direction: OrderDirection::Buy,
                price: maker_price,
                qty: maker_qty,
                role: OrderRole::Maker,
                generation,
            }));
            remaining_cap = remaining_cap.map(|c| c - maker_qty);
        }

        commands
    }

    /// 计算某档对冲的下单量：取预算可买量和剩余 cap 的较小值，再量化。
    fn calc_rung_qty(
        budget: Money,
        price: Price,
        remaining_cap: Option<Qty>,
        constraints: &OrderConstraints,
    ) -> Qty {
        let budget_qty = budget / price;
        let target_qty = match remaining_cap {
            Some(cap) => budget_qty.min(cap),
            None => budget_qty,
        };
        constraints.quantize_qty(target_qty)
    }
}

/// 对冲阶段的粗分类。
///
/// 为什么不直接用 RobotState？因为 DynamicHedging 内部的 double_negative_count 自增
/// 也会让状态变体变化，但那不算"切换了对冲阶段"。这里只关心三种情况：
/// 没在对冲 / 在动态对冲 / 在 EV 对冲。只有这三种之间切换时才重置 Taker 步数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HedgePhase {
    /// 不在对冲（常规做市、初始化、已收手等）。
    None,
    /// 动态对冲：追买亏损侧，补到跟对面持仓一样多。
    Dynamic,
    /// EV 对冲：追买胜率高的一侧，把数学期望逼到非负。
    Ev,
}

impl HedgePhase {
    /// 把状态机状态映射到对冲阶段分类。
    pub fn of(state: RobotState) -> Self {
        match state {
            RobotState::DynamicHedging { .. } => HedgePhase::Dynamic,
            RobotState::EvHedging => HedgePhase::Ev,
            _ => HedgePhase::None,
        }
    }

    /// 当前是否在对冲中（Dynamic 或 Ev）。
    pub fn is_hedging(self) -> bool {
        matches!(self, HedgePhase::Dynamic | HedgePhase::Ev)
    }
}

/// 对冲单步的决策结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HedgeDecision {
    /// 不执行（没找到追买侧 / 缺口已补齐 / 盘口缺失）。
    Skip,
    /// 执行：追买指定侧，带量上限。
    Execute { side: Side, cap: Option<Qty> },
}

impl GradientLadder {
    /// 决定对冲该追买哪侧、量的上限。纯决策，不产出订单。
    ///
    /// - Dynamic 阶段：追买 PnL 更小的一侧，纯预算封顶（不看穿线、不用摽齐缺口）。
    ///   利润已锁定或两侧 PnL 相等时返回 Skip。
    /// - Ev 阶段：找优势方（mark_price > 0.5 的那侧），cap = None（纯靠预算封顶）。
    /// - None 阶段：返回 Skip。
    pub fn decide_hedge_step(
        &self,
        phase: HedgePhase,
        position: &PositionSnapshot,
        market: &MarketSnapshot,
        _thresholds: &Thresholds,
    ) -> HedgeDecision {
        match phase {
            HedgePhase::Dynamic => {
                // 双边都为正 → 已锁定利润，不需要再追。
                if position.is_profit_locked() {
                    return HedgeDecision::Skip;
                }
                // 追买 PnL 更小的那一侧；两侧相等则不追。
                let Some(side) = position.weaker_side() else {
                    return HedgeDecision::Skip;
                };
                // 摽齐缺口：补到两边持仓对齐就停，防止把对面 PnL 打成负的。
                let gap = position.qty(side.opposite()) - position.qty(side);
                HedgeDecision::Execute {
                    side,
                    cap: Some(gap),
                }
            }
            HedgePhase::Ev => {
                let Some(side) = Self::advantaged_side(market) else {
                    return HedgeDecision::Skip;
                };
                HedgeDecision::Execute { side, cap: None }
            }
            HedgePhase::None => HedgeDecision::Skip,
        }
    }

    /// 找优势方：Up 侧 Mark Price > 0.5 说明市场看好 Up，就追买 Up；否则追 Down。
    /// 盘口缺失时返回 None。
    fn advantaged_side(market: &MarketSnapshot) -> Option<Side> {
        market.mark_price(Side::Up).map(|up_probability| {
            if up_probability > dec!(0.5) {
                Side::Up
            } else {
                Side::Down
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::market::BookTop;

    /// 构造指定双边 best_ask 的市场快照。
    fn market(up_ask: Option<Price>, down_ask: Option<Price>) -> MarketSnapshot {
        MarketSnapshot {
            up: BookTop {
                best_bid: None,
                best_ask: up_ask,
                last_trade: None,
            },
            down: BookTop {
                best_bid: None,
                best_ask: down_ask,
                last_trade: None,
            },
        }
    }

    #[test]
    fn selects_only_side_below_threshold() {
        let ladder = GradientLadder::with_default_config();
        // 仅 Up 侧 0.4 < 0.5，Down 侧 0.7 不满足 → 选 Up。
        let side = ladder.select_main_field(&market(Some(dec!(0.4)), Some(dec!(0.7))));
        assert_eq!(side, Some(Side::Up));
    }

    #[test]
    fn selects_lower_ask_when_both_below_threshold() {
        let ladder = GradientLadder::with_default_config();
        // 两侧皆 < 0.5，取更低者 Down(0.3)。
        let side = ladder.select_main_field(&market(Some(dec!(0.45)), Some(dec!(0.3))));
        assert_eq!(side, Some(Side::Down));
    }

    #[test]
    fn no_main_field_when_neither_below_threshold() {
        let ladder = GradientLadder::with_default_config();
        let side = ladder.select_main_field(&market(Some(dec!(0.6)), Some(dec!(0.7))));
        assert_eq!(side, None);
    }

    #[test]
    fn deploy_produces_three_descending_rungs() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 主战场 Up，best_ask = 0.40，核心做市池 1000。
        let commands = ladder.deploy(
            &market(Some(dec!(0.40)), Some(dec!(0.80))),
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert_eq!(commands.len(), 3);

        // 逐层校验价格与股数：股数 = 池额度 × 占比 ÷ 挂单价，再按精度向下量化。
        // 保留完整算式以体现推导链（如 1 层 = 1000 × 2% / 0.39 = 51.282… → 量化 51.28）。
        let constraints = OrderConstraints::default();
        let expected = [
            (
                dec!(0.39),
                constraints.quantize_qty(dec!(1000) * dec!(0.02) / dec!(0.39)),
            ), // 1 层：ask-0.01，2%
            (
                dec!(0.38),
                constraints.quantize_qty(dec!(1000) * dec!(0.03) / dec!(0.38)),
            ), // 2 层：ask-0.02，3%
            (
                dec!(0.37),
                constraints.quantize_qty(dec!(1000) * dec!(0.05) / dec!(0.37)),
            ), // 3 层：ask-0.03，5%
        ];
        for (command, (exp_price, exp_qty)) in commands.iter().zip(expected) {
            match command {
                Command::SubmitOrder(order) => {
                    assert_eq!(order.side, Side::Up);
                    assert_eq!(order.direction, OrderDirection::Buy);
                    assert_eq!(order.role, OrderRole::Maker);
                    assert_eq!(order.price, exp_price);
                    assert_eq!(order.qty, exp_qty);
                }
                _ => panic!("应为 SubmitOrder 指令"),
            }
        }
    }

    #[test]
    fn deploy_assigns_unique_increasing_order_ids() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        let commands = ladder.deploy(
            &market(Some(dec!(0.40)), None),
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        let ids: Vec<_> = commands
            .iter()
            .map(|c| match c {
                Command::SubmitOrder(o) => o.order_id,
                _ => panic!("应为 SubmitOrder"),
            })
            .collect();
        // 三个标识互不相同且递增。
        assert!(ids[0] < ids[1] && ids[1] < ids[2]);
    }

    #[test]
    fn deploy_skips_rung_below_min_order_size() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 小核心做市池 105、主战场 best_ask = 0.45：
        // 1 层 价0.44 预算105×2%=2.1 股数≈4.77 < 5 份 → 跳过；
        // 2 层 价0.43 预算105×3%=3.15 股数≈7.33 ≥5 且金额≥1 → 保留；
        // 3 层 价0.42 预算105×5%=5.25 股数≈12.5 → 保留。
        let commands = ladder.deploy(
            &market(Some(dec!(0.45)), None),
            dec!(105),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        // 首档因不足 5 份被跳过，仅保留 2 档。
        assert_eq!(commands.len(), 2);
        // 保留档的价格应为 2 层与 3 层（0.43、0.42），首档 0.44 缺席。
        let prices: Vec<_> = commands
            .iter()
            .map(|c| match c {
                Command::SubmitOrder(o) => o.price,
                _ => panic!("应为 SubmitOrder"),
            })
            .collect();
        assert_eq!(prices, vec![dec!(0.43), dec!(0.42)]);
    }

    #[test]
    fn deploy_empty_when_no_main_field() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        let commands = ladder.deploy(
            &market(Some(dec!(0.6)), Some(dec!(0.7))),
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert!(commands.is_empty());
    }

    #[test]
    fn recompute_follows_down_on_own_side() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 本边 Up 以 0.40 成交 100 股，本边均价 0.40，对面 Ask 0.65。
        let result = ladder.recompute_after_fill(
            &FillContext {
                filled_side: Side::Up,
                filled_price: dec!(0.40),
                own_qty: dec!(100),
                opposite_qty: dec!(0),
                own_average_price: dec!(0.40),
                opposite_best_ask: Some(dec!(0.65)),
            },
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        // 应含一笔本边 Up 的续挂追低单，价格 = 0.40 - 0.01 = 0.39。
        let follow = result.commands.iter().find_map(|c| match c {
            Command::SubmitOrder(o) if o.side == Side::Up => Some(o),
            _ => None,
        });
        let follow = follow.expect("应有本边续挂追低单");
        assert_eq!(follow.price, dec!(0.39));
    }

    #[test]
    fn recompute_cancels_opposite_side() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        let result = ladder.recompute_after_fill(
            &FillContext {
                filled_side: Side::Up,
                filled_price: dec!(0.40),
                own_qty: dec!(100),
                opposite_qty: dec!(0),
                own_average_price: dec!(0.40),
                opposite_best_ask: Some(dec!(0.65)),
            },
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        // 应撤销对面 Down 侧全部活跃挂单。
        assert!(result.commands.contains(&Command::CancelSide(Side::Down)));
    }

    #[test]
    fn recompute_posts_pair_when_below_ask() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 本边均价 0.40，配对价 = 1 - 0.40 - 0.02 = 0.58；对面 Ask 0.65 > 0.58 → 直接挂。
        let result = ladder.recompute_after_fill(
            &FillContext {
                filled_side: Side::Up,
                filled_price: dec!(0.40),
                own_qty: dec!(100),
                opposite_qty: dec!(0),
                own_average_price: dec!(0.40),
                opposite_best_ask: Some(dec!(0.65)),
            },
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert!(result.pending.is_none());
        // 应含一笔对面 Down 的配对买单，价 0.58。
        let pair = result.commands.iter().find_map(|c| match c {
            Command::SubmitOrder(o) if o.side == Side::Down => Some(o),
            _ => None,
        });
        let pair = pair.expect("应有对面配对买单");
        assert_eq!(pair.price, dec!(0.58));
    }

    #[test]
    fn recompute_pends_pair_when_at_or_above_ask() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 配对价 0.58，但对面 Ask 仅 0.55 ≤ 0.58 → 不能直接挂，挂起为 Pending。
        let result = ladder.recompute_after_fill(
            &FillContext {
                filled_side: Side::Up,
                filled_price: dec!(0.40),
                own_qty: dec!(100),
                opposite_qty: dec!(0),
                own_average_price: dec!(0.40),
                opposite_best_ask: Some(dec!(0.55)),
            },
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        let pending = result.pending.expect("配对价≥Ask 应挂起");
        assert_eq!(pending.side, Side::Down);
        assert_eq!(pending.price, dec!(0.58));
        assert_eq!(pending.qty, dec!(100));
        // 挂起时不应有对面 Down 的直接下单指令。
        assert!(!result.commands.iter().any(|c| matches!(
            c,
            Command::SubmitOrder(o) if o.side == Side::Down
        )));
    }

    #[test]
    fn recompute_pair_price_leaves_profit_margin() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 本边均价 0.45，配对价 = 1 - 0.45 - 0.02 = 0.53。
        // 本边均价 + 配对价 = 0.98 ≤ 1 - 0.02，留出了最小利润空间。
        let result = ladder.recompute_after_fill(
            &FillContext {
                filled_side: Side::Up,
                filled_price: dec!(0.45),
                own_qty: dec!(100),
                opposite_qty: dec!(0),
                own_average_price: dec!(0.45),
                opposite_best_ask: Some(dec!(0.70)),
            },
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        let pair = result.commands.iter().find_map(|c| match c {
            Command::SubmitOrder(o) if o.side == Side::Down => Some(o),
            _ => None,
        });
        let pair = pair.expect("应有对面配对买单");
        assert_eq!(pair.price, dec!(0.53));
        assert!(dec!(0.45) + pair.price <= Decimal::ONE - dec!(0.02));
    }

    #[test]
    fn recompute_no_pair_when_opposite_already_aligned() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 对面持仓 120 ≥ 本边 100 → 已摽齐，差额 ≤ 0，不应再下配对单（根除滚雪球）。
        let result = ladder.recompute_after_fill(
            &FillContext {
                filled_side: Side::Up,
                filled_price: dec!(0.40),
                own_qty: dec!(100),
                opposite_qty: dec!(120),
                own_average_price: dec!(0.40),
                opposite_best_ask: Some(dec!(0.65)),
            },
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert!(result.pending.is_none());
        assert!(!result.commands.iter().any(|c| matches!(
            c,
            Command::SubmitOrder(o) if o.side == Side::Down
        )));
    }

    #[test]
    fn recompute_pair_qty_is_alignment_gap() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 本边持仓 100、对面已有 30 → 只补差额 70（目标摽齐），而非固定值。
        let result = ladder.recompute_after_fill(
            &FillContext {
                filled_side: Side::Up,
                filled_price: dec!(0.40),
                own_qty: dec!(100),
                opposite_qty: dec!(30),
                own_average_price: dec!(0.40),
                opposite_best_ask: Some(dec!(0.65)),
            },
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        let pair = result.commands.iter().find_map(|c| match c {
            Command::SubmitOrder(o) if o.side == Side::Down => Some(o),
            _ => None,
        });
        let pair = pair.expect("应有对面配对买单");
        assert_eq!(pair.qty, dec!(70));
    }

    /// 构造仅设某侧 best_ask 的市场快照（对冲单步用）。
    fn market_one_side_ask(side: Side, ask: Price) -> MarketSnapshot {
        let book = BookTop {
            best_bid: None,
            best_ask: Some(ask),
            last_trade: None,
        };
        match side {
            Side::Up => MarketSnapshot {
                up: book,
                down: BookTop::default(),
            },
            Side::Down => MarketSnapshot {
                up: BookTop::default(),
                down: book,
            },
        }
    }

    #[test]
    fn hedge_step_capped_by_alignment_gap() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 预算 1000、卖一 0.50 → 可买 2000 股；但缺口仅 100 → 受缺口封顶为 100。
        let commands = ladder.hedge_taker_step(
            Side::Up,
            Some(dec!(100)),
            &market_one_side_ask(Side::Up, dec!(0.50)),
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.side, Side::Up);
                assert_eq!(o.role, OrderRole::Taker);
                assert_eq!(o.price, dec!(0.50));
                assert_eq!(o.qty, dec!(100));
            }
            _ => panic!("应为 SubmitOrder"),
        }
    }

    #[test]
    fn hedge_step_capped_by_budget() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 预算 50、卖一 0.50 → 可买 100 股；缺口 1000 远大于预算 → 受预算封顶为 100。
        let commands = ladder.hedge_taker_step(
            Side::Up,
            Some(dec!(1000)),
            &market_one_side_ask(Side::Up, dec!(0.50)),
            dec!(50),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        match &commands[0] {
            Command::SubmitOrder(o) => assert_eq!(o.qty, dec!(100)),
            _ => panic!("应为 SubmitOrder"),
        }
    }

    #[test]
    fn hedge_step_ev_uses_budget_when_no_cap() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // EV 对冲：cap=None，纯预算封顶。预算 60、卖一 0.40 → 150 股。
        let commands = ladder.hedge_taker_step(
            Side::Down,
            None,
            &market_one_side_ask(Side::Down, dec!(0.40)),
            dec!(60),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        match &commands[0] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.side, Side::Down);
                assert_eq!(o.qty, dec!(150));
            }
            _ => panic!("应为 SubmitOrder"),
        }
    }

    #[test]
    fn hedge_step_empty_when_no_ask() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 该侧无卖一价 → 无从吃单，返回空。
        let commands = ladder.hedge_taker_step(
            Side::Up,
            Some(dec!(100)),
            &MarketSnapshot::default(),
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert!(commands.is_empty());
    }

    #[test]
    fn hedge_step_empty_when_gap_non_positive() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 缺口 ≤ 0（已摽齐）→ 不再补，返回空。
        let commands = ladder.hedge_taker_step(
            Side::Up,
            Some(dec!(0)),
            &market_one_side_ask(Side::Up, dec!(0.50)),
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert!(commands.is_empty());
    }

    #[test]
    fn hedge_step_skips_below_min_order_size() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 缺口仅 3 股 < 最小 5 份 → 跳过（不上调，保持纪律）。
        let commands = ladder.hedge_taker_step(
            Side::Up,
            Some(dec!(3)),
            &market_one_side_ask(Side::Up, dec!(0.50)),
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert!(commands.is_empty());
    }

    fn thresholds() -> Thresholds {
        Thresholds {
            hedge_loss_trigger: dec!(30),
            hedge_min_qty: dec!(100),
            profit_target: dec!(15),
        }
    }

    fn position(up_qty: Qty, down_qty: Qty, total_cost: Money) -> PositionSnapshot {
        PositionSnapshot {
            up_qty,
            down_qty,
            total_cost,
        }
    }

    fn market_with_mid(up_bid: Price, up_ask: Price) -> MarketSnapshot {
        MarketSnapshot {
            up: BookTop {
                best_bid: Some(up_bid),
                best_ask: Some(up_ask),
                last_trade: None,
            },
            down: BookTop::default(),
        }
    }

    #[test]
    fn decide_hedge_dynamic_chases_weaker_side() {
        let ladder = GradientLadder::with_default_config();
        // up_pnl = 20-80 = -60, down_pnl = 100-80 = 20。Up 更弱，追买 Up。
        // cap = opposite_qty - own_qty = 100 - 20 = 80（摽齐缺口）。
        let pos = position(dec!(20), dec!(100), dec!(80));
        let decision = ladder.decide_hedge_step(
            HedgePhase::Dynamic,
            &pos,
            &market(Some(dec!(0.40)), Some(dec!(0.60))),
            &thresholds(),
        );
        assert!(matches!(
            decision,
            HedgeDecision::Execute { side: Side::Up, cap: Some(gap) } if gap == dec!(80)
        ));
    }

    #[test]
    fn decide_hedge_dynamic_skips_when_profit_locked() {
        let ladder = GradientLadder::with_default_config();
        // up_pnl = 100-80 = 20 > 0, down_pnl = 100-80 = 20 > 0 → 利润已锁定。
        let pos = position(dec!(100), dec!(100), dec!(80));
        let decision = ladder.decide_hedge_step(
            HedgePhase::Dynamic,
            &pos,
            &market(Some(dec!(0.40)), Some(dec!(0.60))),
            &thresholds(),
        );
        assert_eq!(decision, HedgeDecision::Skip);
    }

    #[test]
    fn decide_hedge_dynamic_skips_when_pnl_equal() {
        let ladder = GradientLadder::with_default_config();
        // up_pnl = 50-80 = -30, down_pnl = 50-80 = -30。两侧相等，不追。
        let pos = position(dec!(50), dec!(50), dec!(80));
        let decision = ladder.decide_hedge_step(
            HedgePhase::Dynamic,
            &pos,
            &market(Some(dec!(0.40)), Some(dec!(0.60))),
            &thresholds(),
        );
        assert_eq!(decision, HedgeDecision::Skip);
    }

    #[test]
    fn decide_hedge_dynamic_chases_even_when_not_breached() {
        let ladder = GradientLadder::with_default_config();
        // up_pnl = 90-100 = -10（未穿 -30 线），down_pnl = 120-100 = 20。
        // 旧逻辑会 Skip（因为不穿线），新逻辑追买 Up（PnL 更小）。
        // cap = 120 - 90 = 30（摽齐缺口）。
        let pos = position(dec!(90), dec!(120), dec!(100));
        let decision = ladder.decide_hedge_step(
            HedgePhase::Dynamic,
            &pos,
            &market(Some(dec!(0.40)), Some(dec!(0.60))),
            &thresholds(),
        );
        assert!(matches!(
            decision,
            HedgeDecision::Execute { side: Side::Up, cap: Some(gap) } if gap == dec!(30)
        ));
    }

    #[test]
    fn decide_hedge_ev_finds_advantaged_side() {
        let ladder = GradientLadder::with_default_config();
        let pos = position(dec!(100), dec!(100), dec!(200));
        // Up mark = (0.58+0.62)/2 = 0.60 > 0.5 → 优势 Up。
        let decision = ladder.decide_hedge_step(
            HedgePhase::Ev,
            &pos,
            &market_with_mid(dec!(0.58), dec!(0.62)),
            &thresholds(),
        );
        assert!(matches!(
            decision,
            HedgeDecision::Execute {
                side: Side::Up,
                cap: None
            }
        ));
        // Up mark = (0.28+0.32)/2 = 0.30 < 0.5 → 优势 Down。
        let decision = ladder.decide_hedge_step(
            HedgePhase::Ev,
            &pos,
            &market_with_mid(dec!(0.28), dec!(0.32)),
            &thresholds(),
        );
        assert!(matches!(
            decision,
            HedgeDecision::Execute {
                side: Side::Down,
                cap: None
            }
        ));
    }

    #[test]
    fn decide_hedge_ev_skips_when_no_market() {
        let ladder = GradientLadder::with_default_config();
        let pos = position(dec!(100), dec!(100), dec!(200));
        // 盘口全空 → 算不出 mark price → Skip。
        let empty_market = MarketSnapshot::default();
        let decision = ladder.decide_hedge_step(HedgePhase::Ev, &pos, &empty_market, &thresholds());
        assert_eq!(decision, HedgeDecision::Skip);
    }

    #[test]
    fn decide_hedge_none_always_skips() {
        let ladder = GradientLadder::with_default_config();
        let pos = position(dec!(100), dec!(100), dec!(200));
        let decision = ladder.decide_hedge_step(
            HedgePhase::None,
            &pos,
            &market(Some(dec!(0.40)), Some(dec!(0.60))),
            &thresholds(),
        );
        assert_eq!(decision, HedgeDecision::Skip);
    }

    // ==================== hedge_gradient_step 测试 ====================

    #[test]
    fn hedge_gradient_produces_taker_and_makers() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // EV 模式（cap=None），预算 100，best_ask 0.50。
        // Taker: 100×0.40/0.50 = 80 股
        // Maker1: 100×0.30/0.49 ≈ 61.22 股
        // Maker2: 100×0.30/0.48 = 62.50 股
        let commands = ladder.hedge_gradient_step(
            Side::Up,
            None,
            &market_one_side_ask(Side::Up, dec!(0.50)),
            dec!(100),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert_eq!(commands.len(), 3);
        // 第 1 条：Taker，价 0.50。
        match &commands[0] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.role, OrderRole::Taker);
                assert_eq!(o.price, dec!(0.50));
                assert_eq!(o.qty, dec!(80));
            }
            _ => panic!("应为 SubmitOrder"),
        }
        // 第 2 条：Maker，价 0.49。
        match &commands[1] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.role, OrderRole::Maker);
                assert_eq!(o.price, dec!(0.49));
                assert_eq!(o.qty, dec!(61.22));
            }
            _ => panic!("应为 SubmitOrder"),
        }
        // 第 3 条：Maker，价 0.48。
        match &commands[2] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.role, OrderRole::Maker);
                assert_eq!(o.price, dec!(0.48));
                assert_eq!(o.qty, dec!(62.50));
            }
            _ => panic!("应为 SubmitOrder"),
        }
    }

    #[test]
    fn hedge_gradient_cap_consumed_by_taker() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // cap 仅 30 股，Taker 预算可买 80 股但受 cap 封顶为 30 → Maker 无余额。
        let commands = ladder.hedge_gradient_step(
            Side::Up,
            Some(dec!(30)),
            &market_one_side_ask(Side::Up, dec!(0.50)),
            dec!(100),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        // 只有 Taker 一条（cap 被 Taker 耗尽）。
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.role, OrderRole::Taker);
                assert_eq!(o.qty, dec!(30));
            }
            _ => panic!("应为 SubmitOrder"),
        }
    }

    #[test]
    fn hedge_gradient_partial_cap_for_makers() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // cap = 100，Taker 预算可买 80 → 剩余 cap 20 给 Maker。
        // Maker1 预算可买 61.22 但受 cap 封顶为 20 → 剩余 cap 0，Maker2 不铺。
        let commands = ladder.hedge_gradient_step(
            Side::Up,
            Some(dec!(100)),
            &market_one_side_ask(Side::Up, dec!(0.50)),
            dec!(100),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert_eq!(commands.len(), 2); // Taker + Maker1
        match &commands[0] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.role, OrderRole::Taker);
                assert_eq!(o.qty, dec!(80));
            }
            _ => panic!("应为 SubmitOrder"),
        }
        match &commands[1] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.role, OrderRole::Maker);
                assert_eq!(o.price, dec!(0.49));
                assert_eq!(o.qty, dec!(20)); // 受 remaining_cap 封顶
            }
            _ => panic!("应为 SubmitOrder"),
        }
    }

    #[test]
    fn hedge_gradient_skips_maker_below_min_size() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // cap=10，best_ask=0.50。
        // Taker: min(100×0.40/0.50=80, cap=10) = 10 股，剩余 cap=0 → Maker 全跳过。
        let commands = ladder.hedge_gradient_step(
            Side::Up,
            Some(dec!(10)),
            &market_one_side_ask(Side::Up, dec!(0.50)),
            dec!(100),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        // Taker 10 股，剩余 cap 0 → 所有 Maker 都跳过。
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.role, OrderRole::Taker);
                assert_eq!(o.qty, dec!(10));
            }
            _ => panic!("应为 SubmitOrder"),
        }
    }

    #[test]
    fn hedge_gradient_skips_maker_when_price_non_positive() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // best_ask = 0.02，Maker1 价 = 0.02-0.01 = 0.01 > 0 → OK。
        // Maker2 价 = 0.02-0.02 = 0.00 ≤ 0 → 跳过。
        // 但 Taker 预算可买量 = 100×0.40/0.02 = 2000 股（cap=None）。
        let commands = ladder.hedge_gradient_step(
            Side::Up,
            None,
            &market_one_side_ask(Side::Up, dec!(0.02)),
            dec!(100),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        // Taker + Maker1（Maker2 价格为 0 被跳过）。
        assert_eq!(commands.len(), 2);
        match &commands[1] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.role, OrderRole::Maker);
                assert_eq!(o.price, dec!(0.01));
            }
            _ => panic!("应为 SubmitOrder"),
        }
    }

    #[test]
    fn hedge_gradient_ev_no_cap_all_rungs_by_budget() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // EV 模式无 cap 约束，各档纯预算封顶。预算 200，best_ask 0.40。
        let commands = ladder.hedge_gradient_step(
            Side::Down,
            None,
            &market_one_side_ask(Side::Down, dec!(0.40)),
            dec!(200),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert_eq!(commands.len(), 3);
        // Taker: 200×0.40/0.40 = 200 股
        match &commands[0] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.side, Side::Down);
                assert_eq!(o.role, OrderRole::Taker);
                assert_eq!(o.qty, dec!(200));
            }
            _ => panic!("应为 SubmitOrder"),
        }
        // Maker1: 200×0.30/0.39 ≈ 153.84 股
        match &commands[1] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.role, OrderRole::Maker);
                assert_eq!(o.price, dec!(0.39));
                assert_eq!(o.qty, dec!(153.84));
            }
            _ => panic!("应为 SubmitOrder"),
        }
        // Maker2: 200×0.30/0.38 ≈ 157.89 股
        match &commands[2] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.role, OrderRole::Maker);
                assert_eq!(o.price, dec!(0.38));
                assert_eq!(o.qty, dec!(157.89));
            }
            _ => panic!("应为 SubmitOrder"),
        }
    }

    #[test]
    fn hedge_gradient_empty_when_no_ask() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 该侧无卖一价 → 无基准价，返回空。
        let commands = ladder.hedge_gradient_step(
            Side::Up,
            Some(dec!(100)),
            &MarketSnapshot::default(),
            dec!(100),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert!(commands.is_empty());
    }

    #[test]
    fn hedge_gradient_empty_when_gap_non_positive() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // cap = 0（已摽齐）→ 不再补，返回空。
        let commands = ladder.hedge_gradient_step(
            Side::Up,
            Some(dec!(0)),
            &market_one_side_ask(Side::Up, dec!(0.50)),
            dec!(100),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert!(commands.is_empty());
    }

    #[test]
    fn hedge_gradient_taker_skip_but_maker_still_tries() {
        // Taker 预算太小不够最小量，但 Maker 价更低能买更多股。
        let config = HedgeGradientConfig {
            taker_fraction: dec!(0.05), // 极小的 Taker 比例
            maker_rungs: vec![HedgeRung {
                price_offset: dec!(0.01),
                budget_fraction: dec!(0.95),
            }],
        };
        let ladder = GradientLadder::new(LadderConfig::default(), config);
        let mut generator = OrderIdGenerator::new();
        // 预算 10，best_ask 0.50。
        // Taker: 10×0.05/0.50 = 1 股 < min_size 5 → 跳过。
        // Maker: 10×0.95/0.49 ≈ 19.38 股 ≥ 5 → 产出。
        let commands = ladder.hedge_gradient_step(
            Side::Up,
            None,
            &market_one_side_ask(Side::Up, dec!(0.50)),
            dec!(10),
            &OrderConstraints::default(),
            &mut generator,
            Generation::new(),
        );
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.role, OrderRole::Maker);
                assert_eq!(o.price, dec!(0.49));
            }
            _ => panic!("应为 SubmitOrder"),
        }
    }
}
