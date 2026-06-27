//! 动态对冲态 + 观察态：Maker 织网补亏损侧摊薄均价，戴双闸的有限赌反弹。
//!
//! 动态对冲每步在亏损大侧低位铺三档 Maker，单步资金 = 对冲池剩余 × 7.5%。
//! 两道死闸任一触发即强制升级 EV：① 敞口撞 11.25%V ② 双边负 2 次。
//! pnl 落入 (−2%V, +0.25%V) 进观察态静默等行情；≥+0.25%V 微利逃生。
//!
//! 观察态与动态对冲共用一个小策略：观察态只是「停发新单」，升级检查照跑。

use crate::config::StrategyConfig;
use crate::context::{
    ActiveOrder, CommandIntent, Decision, DecisionContext, OrderIntent, PhaseStrategy,
};
use domain::state::RobotState;
use domain::types::{Money, Price, Side};

/// 动态对冲 + 观察态小策略。
#[derive(Debug, Clone)]
pub struct DynamicHedgeStrategy {
    cfg: StrategyConfig,
}

impl DynamicHedgeStrategy {
    pub fn new(cfg: StrategyConfig) -> Self {
        Self { cfg }
    }

    /// 当前双边负计数（从状态里取）。
    fn double_negative_count(state: RobotState) -> u8 {
        state.double_negative_count()
    }

    /// 升级 EV 前先把可能的计数自增算进去：本 tick 若双边都负，count+1。
    fn next_double_negative(&self, ctx: &DecisionContext) -> u8 {
        let base = Self::double_negative_count(ctx.state);
        if ctx.position.both_sides_settle_negative() {
            base + 1
        } else {
            base
        }
    }

    /// 是否在冷却中（距上次对冲不足冷却时长）。
    fn in_cooldown(&self, ctx: &DecisionContext) -> bool {
        match ctx.last_hedge_at {
            Some(last) => ctx.now < last + self.cfg.dynamic_cooldown,
            None => false,
        }
    }

    /// 找出该侧偏离 best_ask 超过深海阈值的活跃挂单（要撤的死单）。
    fn deep_sea_orders<'a>(&self, ctx: &'a DecisionContext, side: Side) -> Vec<&'a ActiveOrder> {
        let best_ask = ctx.market.book(side).best_ask;
        match best_ask {
            Some(ask) => ctx
                .active_orders
                .iter()
                .filter(|o| o.side == side && (o.price - ask).abs() > self.cfg.deep_sea_deviation)
                .collect(),
            None => Vec::new(),
        }
    }

    /// 单步织网：在 side 侧铺三档 Maker。预算 = 对冲池剩余 × step_fraction。
    fn weave_step(&self, ctx: &DecisionContext, side: Side) -> Vec<CommandIntent> {
        let best_ask = match ctx.market.book(side).best_ask {
            Some(a) => a,
            None => return Vec::new(),
        };
        let step_budget = ctx.pools.dynamic_remaining * self.cfg.dynamic_step_fraction;
        let mut cmds = Vec::new();
        for rung in &self.cfg.dynamic_rungs {
            let price = ctx.constraints.quantize_price(best_ask - rung.price_offset);
            if price <= Price::ZERO {
                continue;
            }
            let rung_budget = step_budget * rung.step_fraction;
            let qty = ctx.constraints.quantize_qty(rung_budget / price);
            if ctx.constraints.is_satisfied(qty, price) {
                cmds.push(CommandIntent::Submit(OrderIntent::maker_buy(side, price, qty)));
            }
        }
        cmds
    }

    /// 选要补的那一侧。
    ///
    /// 动态对冲的本意是把 sum_avg 拉回 1 以下，而 sum_avg 高的根因通常是双边没摽齐
    /// （一侧重仓、对面太少）。所以补哪边的第一原则是**补持仓少的那侧去摽齐**：
    /// 哪侧股数少就补哪侧。只有双边股数基本相等（缺口可忽略）时，才退化为
    /// 「补浮亏最差侧摊薄站岗成本」。
    ///
    /// m000 那种 DN 264 / UP 0 的极端单边，按此逻辑会去补空仓的 UP，符合常识。
    fn lame_side(&self, ctx: &DecisionContext) -> Side {
        let up_qty = ctx.position.up_qty;
        let down_qty = ctx.position.down_qty;

        // 双边股数差距明显 → 补少的那侧摽齐。
        if up_qty != down_qty {
            return if up_qty < down_qty {
                Side::Up
            } else {
                Side::Down
            };
        }

        // 双边股数相等（含都为 0）：退化为补浮亏最差侧；都无浮亏则默认 Up。
        [Side::Up, Side::Down]
            .into_iter()
            .filter_map(|s| {
                ctx.market
                    .book(s)
                    .best_bid
                    .and_then(|bid| ctx.position.float_pnl(s, bid))
                    .map(|fp| (s, fp))
            })
            .min_by(|a, b| a.1.cmp(&b.1))
            .map(|(s, _)| s)
            .unwrap_or(Side::Up)
    }

    /// 补单后该侧投影敞口是否超红线。
    fn exposure_would_exceed(&self, ctx: &DecisionContext, side: Side, new_notional: Money) -> bool {
        let unpaired = ctx.position.unpaired_cost(side);
        let active = ctx.active_notional(side);
        unpaired + active + new_notional > ctx.pools.max_exposure
    }
}

impl PhaseStrategy for DynamicHedgeStrategy {
    fn decide(&self, ctx: &DecisionContext) -> Decision {
        let v = ctx.total_capital;
        let escape = self.cfg.dynamic_escape * v;
        let loss = self.cfg.loss_trigger * v;

        let up_settle = ctx.position.settle_pnl(Side::Up);
        let down_settle = ctx.position.settle_pnl(Side::Down);
        let worst = up_settle.min(down_settle);

        // ① 双边负 2 次 → 升级 EV（最高内部优先级，先于一切动作）。
        let next_count = self.next_double_negative(ctx);
        if next_count >= 2 {
            return Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::EvHedge);
        }

        // ② 微利逃生：两侧结算 pnl 都 ≥ +0.25%V → 撤单扛结算。
        if up_settle >= escape && down_settle >= escape {
            return Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::SettlementWait);
        }

        // ③ 观察区间 (−2%V, +0.25%V)：停发新单等行情。
        //    但若计数本 tick 自增了（双边负但还没到 2），要把计数写回状态。
        if worst > loss {
            let target = RobotState::Observing {
                double_negative_count: next_count,
            };
            // 已在观察态且计数没变 → 啥也不做；否则跳到（更新计数的）观察态。
            if ctx.state == target {
                return Decision::skip();
            }
            return Decision::transition(target);
        }

        // ④ pnl ≤ −2%V：继续对冲。先确认不在冷却中。
        if self.in_cooldown(ctx) {
            // 冷却中：保持对冲态（若当前是观察态，跌破线了要切回对冲态并更新计数）。
            let target = RobotState::DynamicHedge {
                double_negative_count: next_count,
            };
            if ctx.state == target {
                return Decision::skip();
            }
            return Decision::transition(target);
        }

        // 选要补的那一侧（持仓少的侧优先摽齐）。
        let lame = self.lame_side(ctx);

        // 先撤深海死单（占预算额度）。
        let mut decision = Decision::skip();
        for dead in self.deep_sea_orders(ctx, lame) {
            decision = decision.with(CommandIntent::Cancel(dead.order_id));
        }

        // 算本步织网指令，校验敞口红线。
        let step = self.weave_step(ctx, lame);
        let new_notional: Money = step
            .iter()
            .filter_map(|c| match c {
                CommandIntent::Submit(o) => Some(o.price * o.qty),
                _ => None,
            })
            .sum();

        if self.exposure_would_exceed(ctx, lame, new_notional) {
            // ⑤ 敞口撞红线 → 拒绝下单 + 强制升级 EV。
            return Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::EvHedge);
        }

        // 正常织网。确保停在动态对冲态（更新计数）。
        for cmd in step {
            decision = decision.with(cmd);
        }
        let target = RobotState::DynamicHedge {
            double_negative_count: next_count,
        };
        if ctx.state != target {
            decision = decision.moving_to(target);
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{PoolBudgets, Trigger};
    use domain::market::{BookTop, MarketSnapshot};
    use domain::order::{OrderConstraints, OrderDirection, OrderId};
    use domain::pnl::PositionSnapshot;
    use domain::types::{OrderRole, Qty};
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
        state: RobotState,
        position: PositionSnapshot,
        market: MarketSnapshot,
        active: Vec<ActiveOrder>,
        now: u64,
        last_hedge_at: Option<u64>,
    }

    impl Builder {
        fn new() -> Self {
            Self {
                state: RobotState::DynamicHedge {
                    double_negative_count: 0,
                },
                // Up 亏损大侧（高位站岗）：Up 100@0.60=60, Down 50@0.30=15，总成本 75。
                // up_settle = 100−75 = 25？不行，要让 Up 穿线。
                // 设 Up 30 股 @ 成本 60（高位），Down 100 @ 成本 30，总成本 90。
                // up_settle = 30−90 = −60 ≤ −20；down_settle = 100−90=10。worst=−60。
                position: PositionSnapshot {
                    up_qty: dec!(30),
                    down_qty: dec!(100),
                    up_cost: dec!(60),
                    down_cost: dec!(30),
                },
                market: MarketSnapshot {
                    up: book(Some(dec!(0.28)), Some(dec!(0.30))),
                    down: book(Some(dec!(0.68)), Some(dec!(0.70))),
                },
                active: Vec::new(),
                now: 10_000,
                last_hedge_at: None,
            }
        }

        fn build(&self) -> DecisionContext<'_> {
            DecisionContext {
                total_capital: dec!(1000),
                trigger: Trigger::Fill { side: Side::Up },
                now: self.now,
                time_to_expiry: 600_000,
                state: self.state,
                main_field: Some(Side::Up),
                main_field_frozen: false,
                position: self.position,
                market: self.market,
                pools: PoolBudgets {
                    grid_maker_total: dec!(150),
                    grid_maker_remaining: dec!(150),
                    dynamic_remaining: dec!(225),
                    ev_remaining: dec!(375),
                    max_exposure: dec!(112.5),
                },
                active_orders: &self.active,
                last_hedge_at: self.last_hedge_at,
                calm_since: None,
                constraints: OrderConstraints::default(),
            }
        }
    }

    fn strat() -> DynamicHedgeStrategy {
        DynamicHedgeStrategy::new(StrategyConfig::default())
    }

    #[test]
    fn weaves_three_rungs_on_lame_side() {
        // 基线 Up 30 / Down 100，Up 是持仓少侧 → 补 Up，在 Up best_ask 0.30 下方铺三档。
        let d = strat().decide(&Builder::new().build());
        let submits: Vec<_> = d
            .commands
            .iter()
            .filter_map(|c| match c {
                CommandIntent::Submit(o) => Some(o),
                _ => None,
            })
            .collect();
        assert_eq!(submits.len(), 3);
        assert!(submits.iter().all(|o| o.side == Side::Up));
        // 价格 0.29/0.28/0.27。
        assert_eq!(submits[0].price, dec!(0.29));
        assert_eq!(submits[1].price, dec!(0.28));
        assert_eq!(submits[2].price, dec!(0.27));
    }

    #[test]
    fn weaves_on_empty_side_when_one_sided() {
        // m000 场景：Down 重仓裸露、Up 空仓 → 应补空仓的 Up（摽齐），不是补已重仓的 Down。
        let mut b = Builder::new();
        b.position = PositionSnapshot {
            up_qty: dec!(0),
            down_qty: dec!(264),
            up_cost: dec!(0),
            down_cost: dec!(108),
        };
        // 两侧都给盘口，确认它选的是空仓的 Up 而非重仓的 Down。
        b.market = MarketSnapshot {
            up: book(Some(dec!(0.58)), Some(dec!(0.60))),
            down: book(Some(dec!(0.38)), Some(dec!(0.40))),
        };
        let d = strat().decide(&b.build());
        let submits: Vec<_> = d
            .commands
            .iter()
            .filter_map(|c| match c {
                CommandIntent::Submit(o) => Some(o),
                _ => None,
            })
            .collect();
        assert!(!submits.is_empty(), "应在 Up 侧织网");
        assert!(
            submits.iter().all(|o| o.side == Side::Up),
            "单边裸露时必须补空仓的 Up，而不是已重仓的 Down"
        );
    }

    #[test]
    fn step_budget_is_7_5_percent_of_remaining() {
        // 对冲池剩余 225 × 7.5% = 16.875。第一档 40% = 6.75，价 0.29 → 23.27 股。
        let d = strat().decide(&Builder::new().build());
        let first = d.commands.iter().find_map(|c| match c {
            CommandIntent::Submit(o) => Some(o),
            _ => None,
        });
        let qty = first.unwrap().qty;
        // 6.75 / 0.29 = 23.27... 量化到 2 位 = 23.27。
        assert_eq!(qty, ctx_qty(dec!(16.875) * dec!(0.40), dec!(0.29)));
    }

    fn ctx_qty(budget: Money, price: Price) -> Qty {
        OrderConstraints::default().quantize_qty(budget / price)
    }

    #[test]
    fn cooldown_blocks_new_step() {
        let mut b = Builder::new();
        // 上次对冲在 9500，冷却 1000 → 现在 10000 < 10500，冷却中。
        b.last_hedge_at = Some(dec_to_u64(dec!(9500)));
        b.now = 10_000;
        let d = strat().decide(&b.build());
        // 冷却中不发新单（无 Submit）。
        assert!(!d.commands.iter().any(|c| matches!(c, CommandIntent::Submit(_))));
    }

    fn dec_to_u64(d: Decimal) -> u64 {
        use rust_decimal::prelude::ToPrimitive;
        d.to_u64().unwrap()
    }

    #[test]
    fn micro_escape_settles_when_both_above_quarter_percent() {
        let mut b = Builder::new();
        // 两侧结算 pnl 都 ≥ +0.25%V(=2.5)：Up 100/Down 100，成本 90 → 都 +10。
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
    fn enters_observing_in_band() {
        let mut b = Builder::new();
        // pnl 落在 (−20, +2.5)：Up 88/Down 100，成本 90 → up −2、down +10，worst −2 > −20。
        b.position = PositionSnapshot {
            up_qty: dec!(88),
            down_qty: dec!(100),
            up_cost: dec!(45),
            down_cost: dec!(45),
        };
        let d = strat().decide(&b.build());
        assert_eq!(
            d.transition,
            Some(RobotState::Observing {
                double_negative_count: 0
            })
        );
        assert!(d.commands.is_empty());
    }

    #[test]
    fn double_negative_twice_upgrades_to_ev() {
        let mut b = Builder::new();
        // 已经 count=1，本 tick 双边都负 → next=2 → 升级 EV。
        b.state = RobotState::DynamicHedge {
            double_negative_count: 1,
        };
        // 双边都负：Up 40/Down 50，成本 100 → 都负。
        b.position = PositionSnapshot {
            up_qty: dec!(40),
            down_qty: dec!(50),
            up_cost: dec!(50),
            down_cost: dec!(50),
        };
        let d = strat().decide(&b.build());
        assert!(d.commands.contains(&CommandIntent::CancelAll));
        assert_eq!(d.transition, Some(RobotState::EvHedge));
    }

    #[test]
    fn one_sided_weaves_empty_side_not_upgrade_ev() {
        // Up 重仓裸露、Down 空仓 → 补空仓的 Down 摽齐（不是撞红线弹 EV）。
        // 这修正了旧 bug：旧逻辑补已重仓侧会立刻撞敞口红线弹 EV。
        let mut b = Builder::new();
        b.position = PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(0),
            up_cost: dec!(112),
            down_cost: dec!(20),
        };
        // 让 Up 穿线（worst ≤ −20）：total=132，up_settle=100−132=−32。
        b.market = MarketSnapshot {
            up: book(Some(dec!(0.43)), Some(dec!(0.45))),
            down: book(Some(dec!(0.53)), Some(dec!(0.55))),
        };
        let d = strat().decide(&b.build());
        // 补空仓的 Down，不升级 EV。
        let submits: Vec<_> = d
            .commands
            .iter()
            .filter_map(|c| match c {
                CommandIntent::Submit(o) => Some(o),
                _ => None,
            })
            .collect();
        assert!(submits.iter().all(|o| o.side == Side::Down), "应补空仓的 Down");
        assert_ne!(d.transition, Some(RobotState::EvHedge), "补空仓侧不应撞红线弹 EV");
    }

    #[test]
    fn cancels_deep_sea_orders_before_weaving() {
        let mut b = Builder::new();
        // Up 侧 best_ask 0.30，有个挂在 0.10 的死单（偏离 0.20 > 0.03）。
        b.active = vec![ActiveOrder {
            order_id: OrderId(9),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.10),
            qty: dec!(50),
            role: OrderRole::Maker,
        }];
        let d = strat().decide(&b.build());
        assert!(d.commands.contains(&CommandIntent::Cancel(OrderId(9))));
    }
}
