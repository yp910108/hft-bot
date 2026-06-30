//! 配对态：首笔成交后常驻。主战场成交触发重算，配对侧成交只查阈值。
//!
//! 做三件事：
//! 1. 退出检查（每个 tick 都先过）：利润锁定、对冲穿线、池耗尽。
//!    （尾盘规则由 router 在上层统一处理，这里不重复。）
//! 2. 主战场成交 → 续挂追低（受敞口刹车）+ 重算 DN 配对单（含精细订单管理）。
//! 3. 配对侧成交 → 不触发重算（断正反馈），仅靠退出检查兜底。
//!
//! 纯函数。续挂/配对的「是否已挂、挂在哪」靠读 active_orders 推断。

use crate::PhaseStrategy;
use crate::config::StrategyConfig;
use crate::context::{CommandIntent, Decision, DecisionContext, OrderIntent, Trigger};
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
    /// 优先级：利润锁定 > 对冲穿线 > 池耗尽。
    /// （尾盘规则由 router 在上层统一裁决，这里不处理。）
    fn check_exits(&self, ctx: &DecisionContext) -> Option<Decision> {
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
            return Some(Decision::transition(RobotState::DynamicHedge));
        }

        // 3. 核心做市池耗尽：拟挂金额 + 已挂金额 + 已成交金额 ≥ 池总额（即剩余不够最小单）。
        //    留在配对态挂机等已挂单成交，不进对冲。
        if ctx.pools.grid_maker_remaining < ctx.constraints.min_notional {
            return Some(Decision::skip());
        }

        None
    }

    /// 续挂追低股数 = 池总额 × follow_fraction ÷ 价。
    fn follow_qty(&self, ctx: &DecisionContext, price: Price) -> Qty {
        ctx.pools.grid_maker_total * self.cfg.follow_fraction / price
    }

    /// 主战场侧未配对保护成本 + 该侧活跃挂单金额 + 拟发新单金额，是否超最大敞口。
    fn exposure_would_exceed(
        &self,
        ctx: &DecisionContext,
        main: Side,
        new_notional: Money,
    ) -> bool {
        let unpaired = ctx.position.unpaired_cost(main);
        let active = ctx.active_notional(main);
        unpaired + active + new_notional > ctx.pools.max_exposure
    }

    /// 计算配对价 = min(1 − 主战场均价 − margin, 对面 ask − 0.01)。
    /// 无主战场均价/对面 ask 时返回 None。
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

    /// 汇总对侧活跃配对单：返回 (共同价位, 活跃总量)。
    ///
    /// 不变量：对侧所有活跃配对单强制单一价位，故共同价取任意一笔的价即可。
    /// 无活跃单返回 None。
    fn pair_aggregate(&self, ctx: &DecisionContext, opp: Side) -> Option<(Price, Qty)> {
        let mut total = Qty::ZERO;
        let mut price = None;
        for o in ctx.active_orders.iter().filter(|o| o.side == opp) {
            total += o.qty;
            price.get_or_insert(o.price);
        }
        price.map(|p| (p, total))
    }
}

impl PhaseStrategy for PairingStrategy {
    fn decide(&self, ctx: &DecisionContext) -> Decision {
        let main = match ctx.round.main_field {
            Some(s) => s,
            None => return Decision::skip(),
        };

        // 退出检查永远先跑（不管什么触发）。
        if let Some(exit) = self.check_exits(ctx) {
            return exit;
        }

        // 只有主战场侧成交才触发重算；其他触发（配对侧成交、盘口更新）只靠上面的退出检查。
        if ctx.trigger != (Trigger::Fill { side: main }) {
            return Decision::skip();
        }

        let opp = main.opposite();
        let mut decision = Decision::skip();

        // ① 续挂追低：在主战场当前最深挂单价下方再挂一档（受敞口刹车 + 永久停铺）。
        if !ctx.round.main_field_frozen {
            // 找主战场当前最低活跃挂单价（最深买价）。
            let lowest_active_price = ctx
                .active_orders
                .iter()
                .filter(|o| o.side == main)
                .map(|o| o.price)
                .min();

            // 追低基准价：有活跃挂单取最低价；全成交无挂单则退化取均价。
            let base_price = match lowest_active_price {
                Some(p) => p,
                None => ctx.position.average_price(main).unwrap_or(Price::ZERO),
            };

            if base_price > Price::ZERO {
                let follow_price = ctx
                    .constraints
                    .quantize_price(base_price - self.cfg.follow_offset);

                if follow_price > Price::ZERO {
                    // 价位防重检测：该价位已有我方挂单 → 不发新单（防部分成交风暴重复铺单）。
                    if !ctx.has_active_order_at(main, follow_price) {
                        let qty = ctx
                            .constraints
                            .quantize_qty(self.follow_qty(ctx, follow_price));
                        let notional = follow_price * qty;
                        if ctx.constraints.is_satisfied(qty, follow_price) {
                            if self.exposure_would_exceed(ctx, main, notional) {
                                // 撞敞口红线：撤主战场全部挂单，做市阶段永久停铺。
                                decision = decision.with(CommandIntent::CancelSide(main));
                                decision.freeze_main_field = true;
                            } else {
                                decision = decision.with(CommandIntent::Submit(
                                    OrderIntent::maker_buy(main, follow_price, qty),
                                ));
                            }
                        }
                    }
                }
            }
        }

        // ② 重算 DN 配对单（精细订单管理，对侧强制单一价位）。
        if let Some(new_price) = self.pair_price(ctx, main) {
            // 配对量 = 主战场总持仓 − 对面已有持仓（目标摽齐，差额 ≤ 0 不下单）。
            let gap = ctx.position.qty(main) - ctx.position.qty(opp);
            let target_qty = ctx.constraints.quantize_qty(gap.max(Qty::ZERO));

            match self.pair_aggregate(ctx, opp) {
                None => {
                    // 对侧无活跃配对单：直接挂目标量 @ 新配对价。
                    if ctx.constraints.is_satisfied(target_qty, new_price) {
                        decision = decision.with(CommandIntent::Submit(OrderIntent::maker_buy(
                            opp, new_price, target_qty,
                        )));
                    }
                }
                Some((cur_price, active_qty)) => {
                    let price_diff = (new_price - cur_price).abs();
                    if price_diff > self.cfg.repair_tolerance {
                        // 偏差 > 容差：撤干净对侧全部（CancelSide 杜绝僵尸单）+ 重挂。
                        decision = decision.with(CommandIntent::CancelSide(opp));
                        if ctx.constraints.is_satisfied(target_qty, new_price) {
                            decision = decision.with(CommandIntent::Submit(
                                OrderIntent::maker_buy(opp, new_price, target_qty),
                            ));
                        }
                    } else {
                        // 偏差 ≤ 容差：不撤旧单保排队。缺口比活跃总量大则在 cur_price 追加增量
                        //（用 cur_price 而非 new_price，保证对侧单一价位不变量）。
                        let extra = ctx.constraints.quantize_qty(target_qty - active_qty);
                        if extra > Qty::ZERO && ctx.constraints.is_satisfied(extra, cur_price) {
                            decision = decision.with(CommandIntent::Submit(
                                OrderIntent::maker_buy(opp, cur_price, extra),
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
    use crate::context::{ActiveOrder, PoolBudgets};
    use domain::market::{BookTop, MarketSnapshot};
    use domain::order::{OrderConstraints, OrderDirection, OrderId};
    use domain::pnl::PositionSnapshot;
    use domain::round_state::RoundState;
    use domain::state::RobotState;
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
        round_state: RoundState,
    }

    impl Builder {
        fn new() -> Self {
            let mut round = RoundState::new();
            round.state = RobotState::Pairing;
            round.main_field = Some(Side::Up);
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
                round_state: round,
            }
        }

        fn build(&self) -> DecisionContext<'_> {
            DecisionContext {
                total_capital: dec!(1000),
                trigger: self.trigger,
                now: 0,
                time_to_expiry: self.tte,
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
                constraints: OrderConstraints::default(),
                round: &self.round_state,
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
        // 主战场 Up 成交，均价 0.40。续挂一档 + DN 配对单。
        let b = Builder::new();
        let d = strat().decide(&b.build());
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
        // 只建了 Up：总成本 40 → Down settle = −40 ≤ −20，且对侧(Up)成本 40 > 30。
        b.position = PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(0),
            up_cost: dec!(40),
            down_cost: dec!(0),
        };
        b.trigger = Trigger::BookUpdate; // 退出检查与触发无关。
        let d = strat().decide(&b.build());
        assert_eq!(d.transition, Some(RobotState::DynamicHedge));
    }

    #[test]
    fn float_breach_enters_dynamic_hedge() {
        let mut b = Builder::new();
        // Up 持仓 100、成本 50，bid 0.25 → 浮亏 = 100×0.25 − 50 = −25 ≤ −20。
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
        assert_eq!(d.transition, Some(RobotState::DynamicHedge));
    }

    #[test]
    fn pairing_side_fill_does_not_recompute() {
        let mut b = Builder::new();
        // 配对侧(Down)成交触发，且没到任何退出线 → 不重算，skip。
        b.trigger = Trigger::Fill { side: Side::Down };
        let d = strat().decide(&b.build());
        assert!(d.is_skip());
    }

    #[test]
    fn fine_management_keeps_order_when_price_close() {
        let mut b = Builder::new();
        // 已有配对单 0.58、50 股（正好摽齐）。新配对价也是 0.58 → 不动。
        b.active = vec![pair_order(7, dec!(0.58), dec!(50))];
        let d = strat().decide(&b.build());
        // 无 Cancel、无 Down 的 Submit（续挂 Up 可能有，但配对侧不动）。
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Cancel(_)))
        );
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Submit(o) if o.side == Side::Down))
        );
    }

    #[test]
    fn fine_management_appends_when_gap_grows() {
        let mut b = Builder::new();
        // 旧配对单 0.58、30 股；缺口 50（Up100−Down50）→ 追加 20 股，价不变。
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
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Cancel(_)))
        );
    }

    #[test]
    fn fine_management_recancels_when_price_far() {
        let mut b = Builder::new();
        // 旧配对单挂在 0.50，新配对价 0.58，偏差 0.08 > 容差 0.01 → CancelSide 撤干净 + 重挂。
        b.active = vec![pair_order(7, dec!(0.50), dec!(50))];
        let d = strat().decide(&b.build());
        assert!(d.commands.contains(&CommandIntent::CancelSide(Side::Down)));
        assert!(d.commands.iter().any(
            |c| matches!(c, CommandIntent::Submit(o) if o.side == Side::Down && o.price == dec!(0.58))
        ));
    }

    #[test]
    fn fine_management_aggregates_multiple_pair_orders() {
        // 关键回归：对侧有两笔活跃单（Order A 30 + Order B 20 = 50），缺口正好 50。
        // 必须看到「活跃总量 50」而非「第一笔 30」，从而判定已摽齐、不再追加。
        let mut b = Builder::new();
        b.active = vec![
            pair_order(7, dec!(0.58), dec!(30)),
            pair_order(8, dec!(0.58), dec!(20)),
        ];
        let d = strat().decide(&b.build());
        // 活跃总量 50 == 缺口 50 → extra=0，不追加任何 Down 单。
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Submit(o) if o.side == Side::Down)),
            "活跃总量已达缺口，不应再追加（防无限滚雪球 bug）"
        );
    }

    #[test]
    fn fine_management_cancel_side_clears_all_zombies() {
        // 关键回归：对侧两笔单、价格偏离 → 必须 CancelSide 撤干净两笔，不留僵尸单。
        let mut b = Builder::new();
        b.active = vec![
            pair_order(7, dec!(0.50), dec!(30)),
            pair_order(8, dec!(0.50), dec!(20)),
        ];
        let d = strat().decide(&b.build());
        // 用 CancelSide 一次撤干净，而非只 Cancel 第一笔。
        assert!(d.commands.contains(&CommandIntent::CancelSide(Side::Down)));
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Cancel(_))),
            "不应只撤单笔（会漏掉第二笔成僵尸单）"
        );
    }

    #[test]
    fn frozen_main_field_skips_follow() {
        let mut b = Builder::new();
        b.round_state.main_field_frozen = true;
        let d = strat().decide(&b.build());
        // 永久停铺 → 无 Up 续挂单（但仍有 Down 配对）。
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Submit(o) if o.side == Side::Up))
        );
    }

    #[test]
    fn pool_exhausted_returns_skip() {
        let b = Builder::new();
        let d = strat().decide(&DecisionContext {
            pools: PoolBudgets {
                grid_maker_total: dec!(150),
                grid_maker_remaining: dec!(0.5), // 低于 min_notional=1
                dynamic_remaining: dec!(225),
                ev_remaining: dec!(375),
                max_exposure: dec!(112.5),
            },
            ..b.build()
        });
        assert!(d.is_skip());
    }

    #[test]
    fn follow_uses_lowest_active_not_avg() {
        // 回归：追低基准价应是主战场最低活跃挂单价，不是均价（防网格重叠倒挂）。
        // 场景：均价 0.40，但主战场最低活跃挂单在 0.38 → 追低价 = 0.38 − 0.01 = 0.37。
        let mut b = Builder::new();
        b.active = vec![ActiveOrder {
            order_id: OrderId(1),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.38),
            qty: dec!(10),
            role: OrderRole::Maker,
        }];
        let d = strat().decide(&b.build());
        let follow = d.commands.iter().find_map(|c| match c {
            CommandIntent::Submit(o) if o.side == Side::Up => Some(o),
            _ => None,
        });
        let f = follow.expect("应有续挂追低单");
        // 基准 0.38 − 0.01 = 0.37，不是均价 0.40 − 0.01 = 0.39。
        assert_eq!(f.price, dec!(0.37));
    }

    #[test]
    fn follow_skips_when_price_already_occupied() {
        // 回归：目标追低价位已有我方挂单 → 不重复发单（防部分成交风暴）。
        // 用 follow_offset=0 模拟追低价恰好等于最低活跃挂单价的极端场景。
        let cfg = StrategyConfig {
            follow_offset: dec!(0),
            ..StrategyConfig::default()
        };
        let s = PairingStrategy::new(cfg);

        let mut b = Builder::new();
        b.active = vec![ActiveOrder {
            order_id: OrderId(1),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.39),
            qty: dec!(10),
            role: OrderRole::Maker,
        }];
        let d = s.decide(&b.build());
        // follow_offset=0 → 追低价=基准价=0.39，该价位已有挂单 → 不发 Up 续挂。
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Submit(o) if o.side == Side::Up)),
            "目标价位已有挂单，不应重复发单"
        );
    }
}
