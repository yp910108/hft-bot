//! 动态对冲态：Maker 织网补少仓侧摽齐（Delta Neutral）。
//!
//! 目的：把双边股数摽平，让 sum_avg 压回 1 以下。受敞口红线约束，撞线即原地挂机，
//! 绝不无限加仓、绝不因撞红线升级 EV。
//!
//! 决策顺序（每 tick）：
//! 1. 微利逃生：两侧结算 pnl 都 ≥ +0.25%V → 撤单扛结算。
//! 2. 资金耗尽黏住：funds_exhausted 已置 → Skip（不再对冲）。
//! 3. 双边负 2 次 → 升级 EV（不限 TTE，立即进 EV）。
//! 4. pnl ≥ −1%V 且缺口平齐 → 静默挂机。
//! 5. 冷却中 → Skip。
//! 6. 织网（带摽齐 Cap）：
//!    a. 缺口 ≤ 0 或粉尘缺口 → 视同平齐：查利润达标则撤单扛结算，否则 Skip。
//!    b. 缺口大但预算买不起最低单 → 资金耗尽：CancelAll + engine 置标志位。
//!    c. 正常 → 选 Target Side，撤深海死单，Min(预算,缺口) 织网；敞口红线 → Skip 挂机。
//!
//! 双边负计数（double_negative_count / was_double_negative）是跨阶段全局量，
//! 由 engine 统一维护。strategy 从 ctx 读取，通过 Decision::with_dn_update 表达更新意图。

use crate::PhaseStrategy;
use crate::config::StrategyConfig;
use crate::context::{CommandIntent, Decision, DecisionContext, OrderIntent};
use domain::order::OrderId;
use domain::state::RobotState;
use domain::types::{Money, Price, Qty, Side};

/// 动态对冲小策略。
#[derive(Debug, Clone)]
pub struct DynamicHedgeStrategy {
    cfg: StrategyConfig,
}

impl DynamicHedgeStrategy {
    pub fn new(cfg: StrategyConfig) -> Self {
        Self { cfg }
    }

    /// 边沿触发计数：只有从「非双边负」跳变为「双边负」时才 +1，持续双边负不重复计数。
    ///
    /// 从 ctx 全局上下文读 count/was，算出本 tick 最新值。
    fn edge_triggered_count(&self, ctx: &DecisionContext) -> (u8, bool) {
        let base = ctx.round.double_negative_count;
        let was = ctx.round.was_double_negative;
        let current = ctx.position.both_sides_settle_negative();
        let count = if current && !was { base + 1 } else { base };
        (count, current)
    }

    /// 选 Target Side = 持仓少的那侧。两侧相等返回 None（已平齐）。
    fn target_side(ctx: &DecisionContext) -> Option<Side> {
        let up = ctx.position.up_qty;
        let down = ctx.position.down_qty;
        if up < down {
            Some(Side::Up)
        } else if down < up {
            Some(Side::Down)
        } else {
            None
        }
    }

    /// 找出 Target Side 偏离 best_ask 超过深海阈值的活跃挂单 ID。
    fn deep_sea_orders<'a>(
        &self,
        ctx: &'a DecisionContext,
        side: Side,
    ) -> impl Iterator<Item = OrderId> + 'a {
        let dev = self.cfg.deep_sea_deviation;
        let best_ask = ctx.market.book(side).best_ask;
        ctx.active_orders
            .iter()
            .filter(move |o| {
                o.side == side && best_ask.is_some_and(|ask| (o.price - ask).abs() > dev)
            })
            .map(|o| o.order_id)
    }

    /// 单步织网（带摽齐 Cap + 宏观总量封顶 + 差额追加）。
    ///
    /// 宏观封顶：先算 Target 侧**所有**活跃挂单总量，`可用增量 = gap − 总活跃`。
    /// 可用增量 ≤ 0 说明已挂的单已经覆盖了整个缺口（哪怕散在不同价位），不再发新单。
    /// 可用增量 > 0 才在各档分配、差额追加。避免行情移动时旧单在别的价位占坑导致过度对冲。
    fn weave(&self, ctx: &DecisionContext, side: Side, gap: Qty) -> Vec<CommandIntent> {
        let best_ask = match ctx.market.book(side).best_ask {
            Some(a) => a,
            None => return Vec::new(),
        };

        // 宏观封顶：Target 侧所有活跃挂单总量（不区分价位）。
        let total_active: Qty = ctx
            .active_orders
            .iter()
            .filter(|o| o.side == side)
            .map(|o| o.qty)
            .sum();
        let macro_available = if gap > total_active {
            gap - total_active
        } else {
            return Vec::new(); // 已挂的单已覆盖缺口，不发新单。
        };

        let step_budget = ctx.pools.dynamic_remaining * self.cfg.dynamic_step_fraction;
        let first_price = ctx
            .constraints
            .quantize_price(best_ask - self.cfg.dynamic_rungs[0].price_offset);
        if first_price <= Price::ZERO {
            return Vec::new();
        }
        let budget_qty = step_budget / first_price;

        // 本步实际可发总量 = min(预算可买, 宏观可用增量)。
        let step_cap = if macro_available < budget_qty {
            macro_available
        } else {
            budget_qty
        };

        if gap <= budget_qty {
            // 摽齐 Cap：全挂第一档。
            // 先算绝对缺口（gap − 该价位活跃），再用 step_cap 截断。避免双重扣减。
            let active_at_price = self.active_qty_at(ctx, side, first_price);
            let price_gap = if gap > active_at_price {
                ctx.constraints.quantize_qty(gap - active_at_price)
            } else {
                Qty::ZERO
            };
            // 用宏观 step_cap 截断，防过度对冲。
            let increment = if price_gap < step_cap {
                price_gap
            } else {
                ctx.constraints.quantize_qty(step_cap)
            };
            if increment > Qty::ZERO && ctx.constraints.is_satisfied(increment, first_price) {
                return vec![CommandIntent::Submit(OrderIntent::maker_buy(
                    side,
                    first_price,
                    increment,
                ))];
            }
            return Vec::new();
        }

        // 三档蚕食。每档差额追加，但总量不超 step_cap。
        let mut cmds = Vec::new();
        let mut remaining_cap = step_cap;
        for rung in &self.cfg.dynamic_rungs {
            if remaining_cap <= Qty::ZERO {
                break;
            }
            let price = ctx.constraints.quantize_price(best_ask - rung.price_offset);
            if price <= Price::ZERO {
                continue;
            }
            let rung_budget = step_budget * rung.step_fraction;
            let rung_target = ctx.constraints.quantize_qty(rung_budget / price);
            let active_at_price = self.active_qty_at(ctx, side, price);
            let rung_increment = if rung_target > active_at_price {
                ctx.constraints.quantize_qty(rung_target - active_at_price)
            } else {
                Qty::ZERO
            };
            // 不超过剩余 cap。
            let actual = if rung_increment < remaining_cap {
                rung_increment
            } else {
                ctx.constraints.quantize_qty(remaining_cap)
            };
            if actual > Qty::ZERO && ctx.constraints.is_satisfied(actual, price) {
                cmds.push(CommandIntent::Submit(OrderIntent::maker_buy(
                    side, price, actual,
                )));
                remaining_cap -= actual;
            }
        }
        cmds
    }

    /// 某侧某价位的活跃挂单总量。
    fn active_qty_at(&self, ctx: &DecisionContext, side: Side, price: Price) -> Qty {
        ctx.active_orders
            .iter()
            .filter(|o| o.side == side && o.price == price)
            .map(|o| o.qty)
            .sum()
    }

    /// Target Side 自身总敞口是否超红线。
    fn exposure_would_exceed(
        &self,
        ctx: &DecisionContext,
        side: Side,
        new_notional: Money,
    ) -> bool {
        let held = ctx.position.cost(side);
        let active = ctx.active_notional(side);
        held + active + new_notional > ctx.pools.max_exposure
    }

    /// 平齐后处置：利润达标撤单扛结算，否则 Skip。
    fn settle_or_idle(&self, ctx: &DecisionContext) -> Decision {
        let escape = self.cfg.dynamic_escape * ctx.total_capital;
        let up = ctx.position.settle_pnl(Side::Up);
        let down = ctx.position.settle_pnl(Side::Down);
        if up >= escape && down >= escape {
            Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::SettlementWait)
        } else {
            Decision::skip()
        }
    }
}

impl PhaseStrategy for DynamicHedgeStrategy {
    fn decide(&self, ctx: &DecisionContext) -> Decision {
        let v = ctx.total_capital;
        let escape = self.cfg.dynamic_escape * v;

        let up_settle = ctx.position.settle_pnl(Side::Up);
        let down_settle = ctx.position.settle_pnl(Side::Down);
        let worst = up_settle.min(down_settle);

        // 1. 微利逃生。
        if up_settle >= escape && down_settle >= escape {
            return Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::SettlementWait);
        }

        // 2. 资金耗尽黏住。
        if ctx.round.funds_exhausted {
            return Decision::skip();
        }

        // 边沿计数（从 ctx 全局量读）。
        let (next_count, current_is_dn) = self.edge_triggered_count(ctx);

        // 3. 双边负 2 次 → 升级 EV（不限 TTE，立即进 EV）。
        if next_count >= 2 {
            return Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::EvHedge)
                .with_dn_update(next_count, current_is_dn);
        }

        // 4. 修复完成判定：pnl ≥ 修复线(−1%V) 且 缺口已平齐 → 停止织网静默。
        //    两条 AND 都满足才停手，避免"刚脱线就停、带着敞口扛结算"。
        let repair = self.cfg.repair_target * v;
        let gap_qty = (ctx.position.up_qty - ctx.position.down_qty).abs();
        let gap_is_flat = !ctx.constraints.is_satisfied(gap_qty, Price::ONE); // 缺口够不到最小单 = 平齐
        if worst >= repair && gap_is_flat {
            return self
                .settle_or_idle(ctx)
                .with_dn_update(next_count, current_is_dn);
        }

        // 5. 冷却中。
        if ctx.in_cooldown(self.cfg.dynamic_cooldown) {
            return Decision::skip().with_dn_update(next_count, current_is_dn);
        }

        // 6. 织网。
        let Some(target) = Self::target_side(ctx) else {
            return self
                .settle_or_idle(ctx)
                .with_dn_update(next_count, current_is_dn);
        };

        let gap = ctx
            .constraints
            .quantize_qty(ctx.position.qty(target.opposite()) - ctx.position.qty(target));
        let best_ask = match ctx.market.book(target).best_ask {
            Some(a) => a,
            None => return Decision::skip().with_dn_update(next_count, current_is_dn),
        };
        let first_price = ctx
            .constraints
            .quantize_price(best_ask - self.cfg.dynamic_rungs[0].price_offset);
        if gap <= Qty::ZERO
            || first_price <= Price::ZERO
            || !ctx.constraints.is_satisfied(gap, first_price)
        {
            return self
                .settle_or_idle(ctx)
                .with_dn_update(next_count, current_is_dn);
        }

        // 资金耗尽。
        let step_budget = ctx.pools.dynamic_remaining * self.cfg.dynamic_step_fraction;
        let budget_qty = ctx.constraints.quantize_qty(step_budget / first_price);
        if !ctx.constraints.is_satisfied(budget_qty, first_price) {
            let mut d = Decision::skip()
                .with(CommandIntent::CancelAll)
                .with_dn_update(next_count, current_is_dn);
            d.mark_funds_exhausted = true;
            return d;
        }

        // 正常织网。
        let mut decision = Decision::skip();
        for dead_id in self.deep_sea_orders(ctx, target) {
            decision = decision.with(CommandIntent::Cancel(dead_id));
        }

        let step = self.weave(ctx, target, gap);
        let new_notional: Money = step
            .iter()
            .filter_map(|c| match c {
                CommandIntent::Submit(o) => Some(o.price * o.qty),
                _ => None,
            })
            .sum();

        if self.exposure_would_exceed(ctx, target, new_notional) {
            return decision.with_dn_update(next_count, current_is_dn);
        }

        for cmd in step {
            decision = decision.with(cmd);
        }

        decision.with_dn_update(next_count, current_is_dn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ActiveOrder, PoolBudgets, Trigger};
    use domain::market::{BookTop, MarketSnapshot};
    use domain::order::{OrderConstraints, OrderDirection, OrderId};
    use domain::pnl::PositionSnapshot;
    use domain::round_state::RoundState;
    use domain::state::RobotState;
    use domain::types::OrderRole;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn book(bid: Option<Price>, ask: Option<Price>) -> BookTop {
        BookTop {
            best_bid: bid,
            best_ask: ask,
            last_trade: None,
        }
    }

    struct Builder {
        position: PositionSnapshot,
        market: MarketSnapshot,
        active: Vec<ActiveOrder>,
        now: u64,
        tte: u64,
        dynamic_remaining: Decimal,
        round_state: RoundState,
    }

    impl Builder {
        fn new() -> Self {
            let mut round = RoundState::new();
            round.state = RobotState::DynamicHedge;
            round.main_field = Some(Side::Up);
            Self {
                position: PositionSnapshot {
                    up_qty: dec!(100),
                    down_qty: dec!(0),
                    up_cost: dec!(30),
                    down_cost: dec!(0),
                },
                market: MarketSnapshot {
                    up: book(Some(dec!(0.28)), Some(dec!(0.30))),
                    down: book(Some(dec!(0.68)), Some(dec!(0.70))),
                },
                active: Vec::new(),
                now: 10_000,
                tte: 600_000,
                dynamic_remaining: dec!(225),
                round_state: round,
            }
        }

        fn build(&self) -> DecisionContext<'_> {
            DecisionContext {
                total_capital: dec!(1000),
                trigger: Trigger::Fill { side: Side::Up },
                now: self.now,
                time_to_expiry: self.tte,
                position: self.position,
                market: self.market,
                pools: PoolBudgets {
                    grid_maker_total: dec!(150),
                    grid_maker_remaining: dec!(150),
                    dynamic_remaining: self.dynamic_remaining,
                    ev_remaining: dec!(375),
                    max_exposure: dec!(112.5),
                },
                active_orders: &self.active,
                constraints: OrderConstraints::default(),
                round: &self.round_state,
            }
        }
    }

    fn strat() -> DynamicHedgeStrategy {
        DynamicHedgeStrategy::new(StrategyConfig::default())
    }

    #[test]
    fn weaves_on_target_side_down() {
        let d = strat().decide(&Builder::new().build());
        let submits: Vec<_> = d
            .commands
            .iter()
            .filter_map(|c| match c {
                CommandIntent::Submit(o) => Some(o),
                _ => None,
            })
            .collect();
        assert!(!submits.is_empty(), "应在 Down 侧织网");
        assert!(submits.iter().all(|o| o.side == Side::Down));
    }

    #[test]
    fn empty_side_weave_not_upgrade_ev() {
        let d = strat().decide(&Builder::new().build());
        assert_ne!(d.transition, Some(RobotState::EvHedge));
    }

    #[test]
    fn micro_escape_settles() {
        let mut b = Builder::new();
        b.position = PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(100),
            up_cost: dec!(45),
            down_cost: dec!(45),
        };
        let d = strat().decide(&b.build());
        assert!(d.commands.contains(&CommandIntent::CancelAll));
        assert_eq!(d.transition, Some(RobotState::SettlementWait));
    }

    #[test]
    fn funds_exhausted_sticks_skip() {
        let mut b = Builder::new();
        b.round_state.funds_exhausted = true;
        let d = strat().decide(&b.build());
        assert!(d.is_skip());
    }

    #[test]
    fn double_negative_twice_upgrades_ev_when_tte_small() {
        let mut b = Builder::new();
        b.round_state.double_negative_count = 1;
        b.round_state.was_double_negative = false;
        b.position = PositionSnapshot {
            up_qty: dec!(40),
            down_qty: dec!(50),
            up_cost: dec!(50),
            down_cost: dec!(50),
        };
        b.tte = 200_000;
        let d = strat().decide(&b.build());
        assert!(d.commands.contains(&CommandIntent::CancelAll));
        assert_eq!(d.transition, Some(RobotState::EvHedge));
    }

    #[test]
    fn double_negative_twice_upgrades_ev_regardless_of_tte() {
        // 新逻辑：双边负 2 次不限 TTE，即使 TTE > 5min 也立即进 EV。
        let mut b = Builder::new();
        b.round_state.double_negative_count = 1;
        b.round_state.was_double_negative = false;
        b.position = PositionSnapshot {
            up_qty: dec!(40),
            down_qty: dec!(50),
            up_cost: dec!(50),
            down_cost: dec!(50),
        };
        b.tte = 600_000; // > 5min，旧逻辑不进 EV，新逻辑照进。
        let d = strat().decide(&b.build());
        assert!(d.commands.contains(&CommandIntent::CancelAll));
        assert_eq!(d.transition, Some(RobotState::EvHedge));
    }

    #[test]
    fn continues_weaving_when_pnl_ok_but_gap_large() {
        // 新逻辑：pnl 已修复（> −1%V）但缺口仍大（12 > 最小量 5）→ 不停手，继续织网。
        let mut b = Builder::new();
        b.position = PositionSnapshot {
            up_qty: dec!(88),
            down_qty: dec!(100),
            up_cost: dec!(45),
            down_cost: dec!(45),
        };
        let d = strat().decide(&b.build());
        // worst = 88−90 = −2 > −10 (repair)，但缺口 12 > 5 → 继续织网。
        assert!(
            d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Submit(_))),
            "pnl 已修复但缺口大，应继续织网"
        );
    }

    #[test]
    fn stops_when_pnl_repaired_and_gap_flat() {
        // 新逻辑：pnl ≥ −1%V 且 缺口 ≤ 最小量 → 修复完成，停手静默。
        let mut b = Builder::new();
        // Up 98 / Down 100，缺口 2 < 最小量 5。总成本 90 → worst = 98−90 = 8 > −10 ✓。
        b.position = PositionSnapshot {
            up_qty: dec!(98),
            down_qty: dec!(100),
            up_cost: dec!(45),
            down_cost: dec!(45),
        };
        let d = strat().decide(&b.build());
        // 两条 AND 都满足 → 停手（无 Submit）。
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Submit(_))),
            "pnl 修复 + 缺口平齐，应停手静默"
        );
    }

    #[test]
    fn cooldown_blocks_new_step() {
        let mut b = Builder::new();
        b.round_state.last_hedge_at = Some(9_500);
        b.now = 10_000;
        let d = strat().decide(&b.build());
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Submit(_)))
        );
    }

    #[test]
    fn exposure_breach_idles_not_upgrade_ev() {
        let mut b = Builder::new();
        b.position = PositionSnapshot {
            up_qty: dec!(500),
            down_qty: dec!(0),
            up_cost: dec!(150),
            down_cost: dec!(0),
        };
        b.active = vec![ActiveOrder {
            order_id: OrderId(1),
            side: Side::Down,
            direction: OrderDirection::Buy,
            price: dec!(0.69),
            qty: dec!(170),
            role: OrderRole::Maker,
        }];
        let d = strat().decide(&b.build());
        assert_ne!(d.transition, Some(RobotState::EvHedge));
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Submit(_)))
        );
    }

    #[test]
    fn cancels_deep_sea_orders() {
        let mut b = Builder::new();
        b.active = vec![ActiveOrder {
            order_id: OrderId(9),
            side: Side::Down,
            direction: OrderDirection::Buy,
            price: dec!(0.50),
            qty: dec!(50),
            role: OrderRole::Maker,
        }];
        let d = strat().decide(&b.build());
        assert!(d.commands.contains(&CommandIntent::Cancel(OrderId(9))));
    }

    #[test]
    fn cap_caps_at_gap_when_budget_rich() {
        let b = Builder::new();
        let ctx = b.build();
        let cmds = strat().weave(&ctx, Side::Down, dec!(10));
        assert_eq!(cmds.len(), 1);
        if let CommandIntent::Submit(o) = &cmds[0] {
            assert_eq!(o.qty, dec!(10));
            assert_eq!(o.price, dec!(0.69));
        } else {
            panic!("应为单笔 Submit");
        }
    }

    #[test]
    fn weave_three_rungs_when_budget_tight() {
        let b = Builder::new();
        let ctx = b.build();
        let cmds = strat().weave(&ctx, Side::Down, dec!(10000));
        assert_eq!(cmds.len(), 3);
    }

    #[test]
    fn weave_appends_increment_at_occupied_price() {
        // 差额追加：0.69/0.68 各有旧单 10 股，本档应挂更多 → 追加差额而非跳过。
        let mut b = Builder::new();
        b.active = vec![
            ActiveOrder {
                order_id: OrderId(1),
                side: Side::Down,
                direction: OrderDirection::Buy,
                price: dec!(0.69),
                qty: dec!(10),
                role: OrderRole::Maker,
            },
            ActiveOrder {
                order_id: OrderId(2),
                side: Side::Down,
                direction: OrderDirection::Buy,
                price: dec!(0.68),
                qty: dec!(10),
                role: OrderRole::Maker,
            },
        ];
        let ctx = b.build();
        let cmds = strat().weave(&ctx, Side::Down, dec!(10000));
        // 前两档旧单已超本档应挂 → 只有第三档 0.67 发增量。
        assert_eq!(cmds.len(), 1);
        if let CommandIntent::Submit(o) = &cmds[0] {
            assert_eq!(o.price, dec!(0.67));
        } else {
            panic!("应在 0.67 发增量单");
        }
    }

    #[test]
    fn weave_appends_partial_increment() {
        // 差额追加：旧单 3 股，本档应挂 ≈9.78 股 → 追加 ≈6.78 股（≥最小量 5）。
        let mut b = Builder::new();
        b.active = vec![ActiveOrder {
            order_id: OrderId(1),
            side: Side::Down,
            direction: OrderDirection::Buy,
            price: dec!(0.69),
            qty: dec!(3),
            role: OrderRole::Maker,
        }];
        let ctx = b.build();
        let cmds = strat().weave(&ctx, Side::Down, dec!(10000));
        // 第 1 档 0.69：应挂 ≈9.78，旧 3，增量 ≈6.78 ≥ 5 → 发。
        // 第 2/3 档无旧单 → 正常发。共 3 笔。
        assert_eq!(cmds.len(), 3);
        if let CommandIntent::Submit(o) = &cmds[0] {
            assert_eq!(o.price, dec!(0.69));
            assert!(o.qty > dec!(0) && o.qty < dec!(10), "应为差额而非全量");
        } else {
            panic!("第一档应有增量单");
        }
    }

    #[test]
    fn exposure_counts_target_held_cost() {
        let mut b = Builder::new();
        b.position = PositionSnapshot {
            up_qty: dec!(200),
            down_qty: dec!(190),
            up_cost: dec!(124),
            down_cost: dec!(110.2),
        };
        let d = strat().decide(&b.build());
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Submit(_))),
            "Target 侧持仓成本已逼近红线，补单应被挡住"
        );
    }

    #[test]
    fn idle_path_writes_back_edge_state() {
        // 安全区间挂机也要通过 with_dn_update 写回边沿状态。
        let mut b = Builder::new();
        b.round_state.double_negative_count = 0;
        b.round_state.was_double_negative = false;
        // 两侧都负但 worst 在安全区间：Up 95/Down 96，成本 100 → up −5、down −4，worst=−5 > −20。
        b.position = PositionSnapshot {
            up_qty: dec!(95),
            down_qty: dec!(96),
            up_cost: dec!(50),
            down_cost: dec!(50),
        };
        let d = strat().decide(&b.build());
        // 边沿触发 count 0→1，was false→true。
        assert_eq!(d.double_negative_update, Some((1, true)));
    }
}
