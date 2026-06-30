//! EV 对冲态：终极止血，动量顺势单边押注。
//!
//! 战略意图翻转：不再追求 Delta Neutral，转为追买优势方（概率 ∈ [0.75, 0.85]），
//! 靠交割胜方刚性收益覆盖全局沉没成本。
//!
//! 决策逻辑（每 tick，此时已在 EV 态内）：
//! 1. EV > 0 → 期望转正，撤单扛结算。
//! 2. 优势方概率 < 0.55 → 反转退出，撤单扛结算。
//! 3. 资金耗尽（EV 池买不起最低单）→ 撤单扛结算。
//! 4. 冷却中 → Skip。
//! 5. 概率 ∈ [0.75, 0.85] → 开火：IOC Taker 追买优势方。
//! 6. 其他（概率在 [0.55, 0.75) 或 > 0.85）→ 装死等行情进甜区。

use crate::PhaseStrategy;
use crate::config::StrategyConfig;
use crate::context::{CommandIntent, Decision, DecisionContext, OrderIntent};
use domain::state::RobotState;
use domain::types::{Price, Side};
use rust_decimal::Decimal;

/// EV 对冲态小策略。
#[derive(Debug, Clone)]
pub struct EvHedgeStrategy {
    cfg: StrategyConfig,
}

impl EvHedgeStrategy {
    pub fn new(cfg: StrategyConfig) -> Self {
        Self { cfg }
    }

    /// 优势方 = Mark Price 更高（胜出概率更大）那侧，及其概率。无盘口时 None。
    fn advantaged(&self, ctx: &DecisionContext) -> Option<(Side, Price)> {
        let up_p = ctx.market.mark_price(Side::Up)?;
        let down_p = ctx.market.mark_price(Side::Down)?;
        if up_p >= down_p {
            Some((Side::Up, up_p))
        } else {
            Some((Side::Down, down_p))
        }
    }
}

impl PhaseStrategy for EvHedgeStrategy {
    fn decide(&self, ctx: &DecisionContext) -> Decision {
        let Some((adv_side, prob)) = self.advantaged(ctx) else {
            return Decision::skip();
        };

        // 1. EV > 0 → 期望转正，撤单扛结算。
        let up_prob = ctx.market.mark_price(Side::Up).unwrap_or(Decimal::ZERO);
        if ctx.position.expected_value(up_prob) > Decimal::ZERO {
            return Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::SettlementWait);
        }

        // 2. 优势方概率 < 反转线 → 期望反转，撤单扛结算。
        if prob < self.cfg.ev_reversal {
            return Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::SettlementWait);
        }

        // 3. 资金耗尽：单步预算在保护价 0.85 下连最低单都买不起 → 撤单扛结算。
        //    用 ev_price_cap（而非 best_ask）校验，因为 IOC Limit 单交易所按 Limit Price 校验金额，
        //    不受盘口闪空或低价粉尘影响。
        let step_budget = ctx.pools.ev_remaining * self.cfg.ev_step_fraction;
        let qty = ctx
            .constraints
            .quantize_qty(step_budget / self.cfg.ev_price_cap);
        if !ctx.constraints.is_satisfied(qty, self.cfg.ev_price_cap) {
            return Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::SettlementWait);
        }

        // 4. 冷却中 → 装死等下一步。
        if ctx.in_cooldown(self.cfg.ev_cooldown) {
            return Decision::skip();
        }

        // 5. 出手甜区判定（分时段）：TTE>5min [0.60,0.75] / 5~1min [0.75,0.85]。
        let (low, high) = if ctx.time_to_expiry > self.cfg.ev_entry_window {
            self.cfg.ev_sweet_far
        } else {
            self.cfg.ev_sweet_near
        };
        if prob >= low && prob <= high {
            // 盘口检查：best_ask 存在且 ≤ 保护价才开火，否则装死等盘口回来（不触发冷却）。
            let can_fire = ctx
                .market
                .book(adv_side)
                .best_ask
                .is_some_and(|ask| ask <= self.cfg.ev_price_cap);
            if can_fire {
                return Decision::skip().with(CommandIntent::Submit(OrderIntent::ioc_taker_buy(
                    adv_side,
                    self.cfg.ev_price_cap,
                    qty,
                )));
            }
        }

        // 6. 其他（概率不在甜区，或在甜区但盘口太贵/没货）→ 装死等行情。
        Decision::skip()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ActiveOrder, PoolBudgets, Trigger};
    use domain::market::{BookTop, MarketSnapshot};
    use domain::order::OrderConstraints;
    use domain::pnl::PositionSnapshot;
    use domain::round_state::RoundState;
    use domain::state::RobotState;
    use domain::types::OrderRole;
    use rust_decimal_macros::dec;

    fn book(bid: Price, ask: Price) -> BookTop {
        BookTop {
            best_bid: Some(bid),
            best_ask: Some(ask),
            last_trade: None,
        }
    }

    struct Builder {
        position: PositionSnapshot,
        market: MarketSnapshot,
        now: u64,
        ev_remaining: Decimal,
        round_state: RoundState,
    }

    impl Builder {
        fn new() -> Self {
            let mut round = RoundState::new();
            round.state = RobotState::EvHedge;
            round.main_field = Some(Side::Up);
            Self {
                position: PositionSnapshot {
                    up_qty: dec!(40),
                    down_qty: dec!(50),
                    up_cost: dec!(50),
                    down_cost: dec!(50),
                },
                market: MarketSnapshot {
                    up: book(dec!(0.19), dec!(0.21)),
                    down: book(dec!(0.79), dec!(0.81)),
                },
                now: 10_000,
                ev_remaining: dec!(375),
                round_state: round,
            }
        }

        fn build(&self) -> DecisionContext<'_> {
            const NO_ORDERS: &[ActiveOrder] = &[];
            DecisionContext {
                total_capital: dec!(1000),
                trigger: Trigger::BookUpdate,
                now: self.now,
                time_to_expiry: 200_000,
                position: self.position,
                market: self.market,
                pools: PoolBudgets {
                    grid_maker_total: dec!(150),
                    grid_maker_remaining: dec!(150),
                    dynamic_remaining: dec!(225),
                    ev_remaining: self.ev_remaining,
                    max_exposure: dec!(112.5),
                },
                active_orders: NO_ORDERS,
                constraints: OrderConstraints::default(),
                round: &self.round_state,
            }
        }
    }

    fn strat() -> EvHedgeStrategy {
        EvHedgeStrategy::new(StrategyConfig::default())
    }

    #[test]
    fn fires_ioc_taker_in_sweet_band() {
        // Down 优势方概率 0.80 ∈ [0.75, 0.85] → 开火。
        let d = strat().decide(&Builder::new().build());
        let order = d.commands.iter().find_map(|c| match c {
            CommandIntent::Submit(o) => Some(o),
            _ => None,
        });
        let o = order.expect("甜区内应开火");
        assert_eq!(o.side, Side::Down);
        assert_eq!(o.price, dec!(0.85));
        assert_eq!(o.role, OrderRole::Taker);
        assert_eq!(o.time_in_force, domain::order::TimeInForce::Ioc);
    }

    #[test]
    fn skips_below_sweet_band() {
        let mut b = Builder::new();
        // 优势方概率 0.68 ∈ [0.55, 0.75) → 装死。
        b.market = MarketSnapshot {
            up: book(dec!(0.31), dec!(0.33)),
            down: book(dec!(0.67), dec!(0.69)),
        };
        let d = strat().decide(&b.build());
        assert!(d.is_skip());
    }

    #[test]
    fn skips_above_sweet_band() {
        let mut b = Builder::new();
        // 优势方概率 0.90 > 0.85 → 装死不做冤大头。
        b.market = MarketSnapshot {
            up: book(dec!(0.09), dec!(0.11)),
            down: book(dec!(0.89), dec!(0.91)),
        };
        let d = strat().decide(&b.build());
        assert!(d.is_skip());
    }

    #[test]
    fn reversal_exit_when_prob_below_055() {
        let mut b = Builder::new();
        // 优势方概率 0.52 < 0.55 → 反转退出。
        b.market = MarketSnapshot {
            up: book(dec!(0.47), dec!(0.49)),
            down: book(dec!(0.51), dec!(0.53)),
        };
        let d = strat().decide(&b.build());
        assert!(d.commands.contains(&CommandIntent::CancelAll));
        assert_eq!(d.transition, Some(RobotState::SettlementWait));
    }

    #[test]
    fn ev_positive_exits_to_settlement() {
        let mut b = Builder::new();
        // EV > 0：Up 200/Down 200，成本 100 → 两侧 settle 都 +100，EV 必 >0。
        b.position = PositionSnapshot {
            up_qty: dec!(200),
            down_qty: dec!(200),
            up_cost: dec!(50),
            down_cost: dec!(50),
        };
        let d = strat().decide(&b.build());
        assert!(d.commands.contains(&CommandIntent::CancelAll));
        assert_eq!(d.transition, Some(RobotState::SettlementWait));
    }

    #[test]
    fn cooldown_blocks_fire() {
        let mut b = Builder::new();
        b.round_state.last_hedge_at = Some(9_000);
        b.now = 10_000; // 距上次 1000 < 2000 冷却 → 装死。
        let d = strat().decide(&b.build());
        assert!(d.is_skip());
    }

    #[test]
    fn funds_exhausted_exits() {
        let mut b = Builder::new();
        b.ev_remaining = dec!(0.01); // 几乎没钱了。
        let d = strat().decide(&b.build());
        assert!(d.commands.contains(&CommandIntent::CancelAll));
        assert_eq!(d.transition, Some(RobotState::SettlementWait));
    }

    #[test]
    fn step_is_25_percent_of_remaining() {
        // EV 池 375 × 25% = 93.75，/ 0.85 = 110.29 股。
        let d = strat().decide(&Builder::new().build());
        let qty = d.commands.iter().find_map(|c| match c {
            CommandIntent::Submit(o) => Some(o.qty),
            _ => None,
        });
        let expected = OrderConstraints::default().quantize_qty(dec!(93.75) / dec!(0.85));
        assert_eq!(qty, Some(expected));
    }

    #[test]
    fn funds_exhausted_uses_price_cap_not_best_ask() {
        // 回归 bug1：资金耗尽判定用保护价 0.85 校验，不受盘口闪空影响。
        // 场景：EV 池有钱（375），但盘口 best_ask = None（闪空）→ 不应误判资金耗尽。
        let mut b = Builder::new();
        b.market = MarketSnapshot {
            up: BookTop::default(), // 盘口闪空
            down: BookTop::default(),
        };
        let d = strat().decide(&b.build());
        // 盘口闪空 → advantaged() 返回 None → Skip（不是 CancelAll 退出）。
        assert!(!d.commands.contains(&CommandIntent::CancelAll));
        assert_ne!(d.transition, Some(RobotState::SettlementWait));
    }

    #[test]
    fn no_fire_when_ask_above_cap() {
        // 回归 bug2：盘口最便宜价格已超保护上限 → 不发无意义的一枪，装死等行情。
        let mut b = Builder::new();
        // Down 优势方：mark=(0.72+0.90)/2=0.81 ∈ 甜区 [0.75,0.85]，但 best_ask 0.90 > 0.85 保护价。
        b.market = MarketSnapshot {
            up: book(dec!(0.10), dec!(0.28)),
            down: book(dec!(0.72), dec!(0.90)), // ask 0.90 > cap 0.85，但 mark 0.81 在甜区
        };
        let d = strat().decide(&b.build());
        // 不开火（装死），不触发冷却。
        assert!(
            !d.commands
                .iter()
                .any(|c| matches!(c, CommandIntent::Submit(_)))
        );
        assert!(d.is_skip());
    }
}
