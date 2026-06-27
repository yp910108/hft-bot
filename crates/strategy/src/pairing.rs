//! 配对态：首笔成交后常驻。主战场成交触发重算，配对侧成交只查阈值。
//!
//! 做三件事：
//! 1. 退出检查（每个 tick 都先过）：利润锁定、对冲穿线、最后 5 分钟、池耗尽。
//! 2. 主战场成交 → 续挂追低（受敞口刹车）+ 重算 DN 配对单（含精细订单管理）。
//! 3. 配对侧成交 → 不触发重算（断正反馈），仅靠退出检查兜底。
//!
//! 纯函数。续挂/配对的「是否已挂、挂在哪」靠读 active_orders 推断。

use crate::config::StrategyConfig;
use crate::context::{
    ActiveOrder, CommandIntent, Decision, DecisionContext, OrderIntent, PhaseStrategy, Trigger,
};
use domain::state::RobotState;
use domain::types::{Money, Price, Qty, Side};
use rust_decimal::Decimal;

/// 配对态小策略。
#[derive(Debug, Clone)]
pub struct PairingStrategy {
    cfg: StrategyConfig,
}

impl PairingStrategy {
    pub fn new(cfg: StrategyConfig) -> Self {
        Self { cfg }
    }

    /// 退出检查：返回 Some(决策) 表示要退出/转移，None 表示继续做市。
    ///
    /// 优先级：利润锁定 > 对冲穿线 > 最后 5 分钟 > 池耗尽。
    fn check_exits(&self, ctx: &DecisionContext, main: Side) -> Option<Decision> {
        let v = ctx.total_capital;
        let lock = self.cfg.making_profit_lock * v;
        let loss = self.cfg.loss_trigger * v;
        let threshold = self.cfg.single_side_threshold * v;

        let up_settle = ctx.position.settle_pnl(Side::Up);
        let down_settle = ctx.position.settle_pnl(Side::Down);

        // 1. 利润锁定：两侧结算 pnl 都 ≥ +0.5%V → 收手扛结算。
        if up_settle >= lock && down_settle >= lock {
            return Some(
                Decision::skip()
                    .with(CommandIntent::CancelAll)
                    .moving_to(RobotState::SettlementWait),
            );
        }

        // 2. 对冲穿线（任一成立即进动态对冲）：
        //    a) 任一侧结算 pnl ≤ −2%V 且引发亏损的对侧成本 > 3%V
        //    b) 任一侧浮亏 pnl ≤ −2%V
        let settle_breach = (up_settle <= loss && ctx.position.cost(Side::Down) > threshold)
            || (down_settle <= loss && ctx.position.cost(Side::Up) > threshold);
        let float_breach = [Side::Up, Side::Down].iter().any(|&s| {
            ctx.market
                .book(s)
                .best_bid
                .and_then(|bid| ctx.position.float_pnl(s, bid))
                .is_some_and(|fp| fp <= loss)
        });
        if settle_breach || float_breach {
            return Some(Decision::transition(RobotState::DynamicHedge {
                double_negative_count: 0,
            }));
        }

        // 3. 最后 5 分钟：亏损大侧「结算 pnl × 该侧概率 ≤ −2%V」→ 强制 EV，否则收手扛结算。
        if ctx.time_to_expiry < self.cfg.last_phase_window {
            let weaker = ctx.position.weaker_side().unwrap_or(main);
            let prob = ctx.market.mark_price(weaker).unwrap_or(Decimal::ONE);
            let weighted = ctx.position.settle_pnl(weaker) * prob;
            if weighted <= loss {
                return Some(Decision::transition(RobotState::EvHedge));
            }
            return Some(
                Decision::skip()
                    .with(CommandIntent::CancelAll)
                    .moving_to(RobotState::SettlementWait),
            );
        }

        None
    }

    /// 续挂追低股数 = 池总额 × follow_fraction ÷ 价。
    fn follow_qty(&self, ctx: &DecisionContext, price: Price) -> Qty {
        ctx.pools.grid_maker_total * self.cfg.follow_fraction / price
    }

    /// 主战场侧未配对保护成本 + 该侧活跃挂单金额 + 拟发新单金额，是否超最大敞口。
    fn exposure_would_exceed(&self, ctx: &DecisionContext, main: Side, new_notional: Money) -> bool {
        let unpaired = ctx.position.unpaired_cost(main);
        let active = ctx.active_notional(main);
        unpaired + active + new_notional > ctx.pools.max_exposure
    }

    /// 计算配对价 = min(1 − 主战场均价 − margin, 对面 ask − 0.01)。无主战场均价/对面 ask 时返回 None。
    fn pair_price(&self, ctx: &DecisionContext, main: Side) -> Option<Price> {
        let main_avg = ctx.position.average_price(main)?;
        let by_profit = Decimal::ONE - main_avg - self.cfg.profit_margin;
        let opp_ask = ctx.market.book(main.opposite()).best_ask;
        let raw = match opp_ask {
            Some(ask) => by_profit.min(ask - self.cfg.follow_offset),
            None => by_profit,
        };
        let price = ctx.constraints.quantize_price(raw);
        if price > Price::ZERO {
            Some(price)
        } else {
            None
        }
    }

    /// 对面已有的配对单（取该侧第一笔活跃买单）。
    fn existing_pair<'a>(&self, ctx: &'a DecisionContext, opp: Side) -> Option<&'a ActiveOrder> {
        ctx.active_orders.iter().find(|o| o.side == opp)
    }
}

impl PhaseStrategy for PairingStrategy {
    fn decide(&self, ctx: &DecisionContext) -> Decision {
        let main = match ctx.main_field {
            Some(s) => s,
            None => return Decision::skip(),
        };

        // 退出检查永远先跑（不管什么触发）。
        if let Some(exit) = self.check_exits(ctx, main) {
            return exit;
        }

        // 只有主战场侧成交才触发重算；其他触发（配对侧成交、盘口更新）只靠上面的退出检查。
        if ctx.trigger != (Trigger::Fill { side: main }) {
            return Decision::skip();
        }

        let opp = main.opposite();
        let mut decision = Decision::skip();

        // ① 续挂追低：在主战场均价下方再挂一档（受敞口刹车 + 永久停铺）。
        //    用成交均价近似最近成交价下方。
        if !ctx.main_field_frozen
            && let Some(main_avg) = ctx.position.average_price(main)
        {
            let follow_price = ctx
                .constraints
                .quantize_price(main_avg - self.cfg.follow_offset);
            if follow_price > Price::ZERO {
                let qty = ctx.constraints.quantize_qty(self.follow_qty(ctx, follow_price));
                let notional = follow_price * qty;
                if ctx.constraints.is_satisfied(qty, follow_price) {
                    if self.exposure_would_exceed(ctx, main, notional) {
                        // 撞敞口红线：撤主战场全部挂单，本场永久停铺（engine 据此置 frozen）。
                        decision = decision.with(CommandIntent::CancelSide(main));
                    } else {
                        decision = decision.with(CommandIntent::Submit(OrderIntent::maker_buy(
                            main,
                            follow_price,
                            qty,
                        )));
                    }
                }
            }
        }

        // ② 重算 DN 配对单（精细订单管理）。
        if let Some(new_price) = self.pair_price(ctx, main) {
            // 配对量 = 主战场总持仓 − 对面已有持仓（目标摽齐，差额 ≤ 0 不下单）。
            let gap = ctx.position.qty(main) - ctx.position.qty(opp);
            let target_qty = ctx.constraints.quantize_qty(gap.max(Qty::ZERO));

            match self.existing_pair(ctx, opp) {
                None => {
                    // 无旧配对单：直接挂。
                    if ctx.constraints.is_satisfied(target_qty, new_price) {
                        decision = decision.with(CommandIntent::Submit(OrderIntent::maker_buy(
                            opp, new_price, target_qty,
                        )));
                    }
                }
                Some(old) => {
                    let price_diff = (new_price - old.price).abs();
                    if price_diff > self.cfg.repair_tolerance {
                        // 偏差 > 容差：撤旧重挂。
                        decision = decision.with(CommandIntent::Cancel(old.order_id));
                        if ctx.constraints.is_satisfied(target_qty, new_price) {
                            decision = decision.with(CommandIntent::Submit(
                                OrderIntent::maker_buy(opp, new_price, target_qty),
                            ));
                        }
                    } else {
                        // 偏差 ≤ 容差：不撤旧单保排队。缺口变大则在同价追加增量。
                        let extra = ctx.constraints.quantize_qty(target_qty - old.qty);
                        if extra > Qty::ZERO && ctx.constraints.is_satisfied(extra, old.price) {
                            decision = decision.with(CommandIntent::Submit(
                                OrderIntent::maker_buy(opp, old.price, extra),
                            ));
                        }
                    }
                }
            }
        }

        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PoolBudgets;
    use domain::market::{BookTop, MarketSnapshot};
    use domain::order::{OrderConstraints, OrderDirection, OrderId};
    use domain::pnl::PositionSnapshot;
    use domain::types::OrderRole;
    use rust_decimal_macros::dec;

    fn book(bid: Option<Price>, ask: Option<Price>) -> BookTop {
        BookTop {
            best_bid: bid,
            best_ask: ask,
            last_trade: None,
        }
    }

    struct Builder {
        trigger: Trigger,
        position: PositionSnapshot,
        market: MarketSnapshot,
        active: Vec<ActiveOrder>,
        tte: u64,
        frozen: bool,
    }

    impl Builder {
        fn new() -> Self {
            Self {
                trigger: Trigger::Fill { side: Side::Up },
                // 双边已建仓、未穿任何线的正常配对基线：
                // Up 100@均0.40、Down 50@均0.58，总成本 69。
                // up_settle=31、down_settle=−19（未到 −20）、浮亏安全。
                position: PositionSnapshot {
                    up_qty: dec!(100),
                    down_qty: dec!(50),
                    up_cost: dec!(40),
                    down_cost: dec!(29),
                },
                market: MarketSnapshot {
                    up: book(Some(dec!(0.39)), Some(dec!(0.41))),
                    down: book(Some(dec!(0.57)), Some(dec!(0.60))),
                },
                active: Vec::new(),
                tte: 600_000,
                frozen: false,
            }
        }

        fn build(&self) -> DecisionContext<'_> {
            DecisionContext {
                total_capital: dec!(1000),
                trigger: self.trigger,
                now: 0,
                time_to_expiry: self.tte,
                state: RobotState::Pairing,
                main_field: Some(Side::Up),
                main_field_frozen: self.frozen,
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
                last_hedge_at: None,
                calm_since: None,
                constraints: OrderConstraints::default(),
            }
        }
    }

    fn strat() -> PairingStrategy {
        PairingStrategy::new(StrategyConfig::default())
    }

    fn pair_order(id: u64, price: Price, qty: Qty) -> ActiveOrder {
        ActiveOrder {
            order_id: OrderId(id),
            side: Side::Down,
            direction: OrderDirection::Buy,
            price,
            qty,
            role: OrderRole::Maker,
        }
    }

    #[test]
    fn main_fill_posts_follow_and_pair() {
        // 主战场 Up 成交，均价 0.40。续挂 0.39 一档 + DN 配对单。
        let b = Builder::new();
        let d = strat().decide(&b.build());
        // 至少有续挂(Up) + 配对(Down) 两条 Submit。
        let submits: Vec<_> = d
            .commands
            .iter()
            .filter_map(|c| match c {
                CommandIntent::Submit(o) => Some(o),
                _ => None,
            })
            .collect();
        assert!(submits.iter().any(|o| o.side == Side::Up)); // 续挂
        assert!(submits.iter().any(|o| o.side == Side::Down)); // 配对
    }

    #[test]
    fn pair_price_takes_min_of_profit_and_ask_minus_one() {
        // 主战场均价 0.40 → 利润价 = 1 − 0.40 − 0.02 = 0.58；对面 ask 0.60 − 0.01 = 0.59。
        // min = 0.58。
        let b = Builder::new();
        let ctx = b.build();
        assert_eq!(strat().pair_price(&ctx, Side::Up), Some(dec!(0.58)));
    }

    #[test]
    fn pair_qty_is_alignment_gap() {
        // Up 100、Down 50 → 配对量 = 50。
        let b = Builder::new();
        let d = strat().decide(&b.build());
        let pair = d.commands.iter().find_map(|c| match c {
            CommandIntent::Submit(o) if o.side == Side::Down => Some(o),
            _ => None,
        });
        assert_eq!(pair.unwrap().qty, dec!(50));
    }

    #[test]
    fn profit_lock_cancels_all_and_settles() {
        let mut b = Builder::new();
        // 两侧结算 pnl 都 ≥ +0.5%V(=5)：Up 100/Down 100，总成本 90 → 两侧都 +10。
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
    fn settle_breach_enters_dynamic_hedge() {
        let mut b = Builder::new();
        // 只建了 Up：Down 结算 pnl = 0 − 总成本。要 ≤ −20（2%×1000）且对侧(Up)成本 > 30（3%）。
        // Up 成本 40 > 30；总成本 40 → Down settle = −40 ≤ −20。触发。
        b.position = PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(0),
            up_cost: dec!(40),
            down_cost: dec!(0),
        };
        b.trigger = Trigger::BookUpdate; // 退出检查与触发无关。
        let d = strat().decide(&b.build());
        assert_eq!(
            d.transition,
            Some(RobotState::DynamicHedge {
                double_negative_count: 0
            })
        );
    }

    #[test]
    fn float_breach_enters_dynamic_hedge() {
        let mut b = Builder::new();
        // Up 持仓 100、成本 50，bid 0.25 → 浮亏 = 100×0.25 − 50 = −25 ≤ −20。触发。
        // 但对侧成本门槛不满足结算穿线，这里靠浮亏穿线。
        b.position = PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(0),
            up_cost: dec!(50),
            down_cost: dec!(0),
        };
        b.market = MarketSnapshot {
            up: book(Some(dec!(0.25)), Some(dec!(0.27))),
            down: book(Some(dec!(0.71)), Some(dec!(0.73))),
        };
        let d = strat().decide(&b.build());
        assert_eq!(
            d.transition,
            Some(RobotState::DynamicHedge {
                double_negative_count: 0
            })
        );
    }

    #[test]
    fn pairing_side_fill_does_not_recompute() {
        let mut b = Builder::new();
        // 配对侧(Down)成交触发，且没到任何退出线 → 不重算，skip。
        b.trigger = Trigger::Fill { side: Side::Down };
        // 双边各 100、低成本 → 已锁利润？避免：用未锁定但安全的持仓。
        b.position = PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(50),
            up_cost: dec!(40),
            down_cost: dec!(28),
        };
        let d = strat().decide(&b.build());
        assert!(d.is_skip());
    }

    #[test]
    fn fine_management_keeps_order_when_price_close() {
        let mut b = Builder::new();
        // 已有配对单 0.58、100 股。新配对价也是 0.58 → 偏差 0 ≤ 容差，缺口未变 → 不动。
        b.active = vec![pair_order(7, dec!(0.58), dec!(100))];
        let d = strat().decide(&b.build());
        // 无 Cancel、无 Down 的 Submit（续挂 Up 可能有，但配对侧不动）。
        assert!(!d.commands.iter().any(|c| matches!(c, CommandIntent::Cancel(_))));
        assert!(!d.commands.iter().any(
            |c| matches!(c, CommandIntent::Submit(o) if o.side == Side::Down)
        ));
    }

    #[test]
    fn fine_management_appends_when_gap_grows() {
        let mut b = Builder::new();
        // 旧配对单 0.58、30 股；现在缺口 50（Up100−Down50）→ 追加 20 股，价不变。
        b.active = vec![pair_order(7, dec!(0.58), dec!(30))];
        let d = strat().decide(&b.build());
        let appended = d.commands.iter().find_map(|c| match c {
            CommandIntent::Submit(o) if o.side == Side::Down => Some(o),
            _ => None,
        });
        let a = appended.expect("应追加增量配对单");
        assert_eq!(a.price, dec!(0.58));
        assert_eq!(a.qty, dec!(20));
        // 不撤旧单。
        assert!(!d.commands.iter().any(|c| matches!(c, CommandIntent::Cancel(_))));
    }

    #[test]
    fn fine_management_recancels_when_price_far() {
        let mut b = Builder::new();
        // 旧配对单挂在 0.50，新配对价 0.58，偏差 0.08 > 容差 0.01 → 撤旧重挂。
        b.active = vec![pair_order(7, dec!(0.50), dec!(100))];
        let d = strat().decide(&b.build());
        assert!(d.commands.contains(&CommandIntent::Cancel(OrderId(7))));
        assert!(d.commands.iter().any(
            |c| matches!(c, CommandIntent::Submit(o) if o.side == Side::Down && o.price == dec!(0.58))
        ));
    }

    #[test]
    fn frozen_main_field_skips_follow() {
        let mut b = Builder::new();
        b.frozen = true;
        let d = strat().decide(&b.build());
        // 永久停铺 → 无 Up 续挂单（但仍可能有 Down 配对）。
        assert!(!d.commands.iter().any(
            |c| matches!(c, CommandIntent::Submit(o) if o.side == Side::Up)
        ));
    }
}
