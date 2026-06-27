//! EV 对冲态：IOC Taker 顺势追优势方（方向翻转），终极止血。
//!
//! 动态对冲双闸被冲破后认输：不再接亏损侧飞刀，改用 EV 池 Taker 追买优势方
//! （概率 p>0.5 那侧），靠交割胜方刚性收益覆盖劣势方沉没成本。
//!
//! 战略进入 ≠ 战术开火：进来后只有优势方概率落在分时段甜区才出手，
//! 否则装死（Skip）。退出：EV>0 / 概率跌破 0.55 / 资金耗尽（<1min 由 router 兜底）。

use crate::config::StrategyConfig;
use crate::context::{CommandIntent, Decision, DecisionContext, OrderIntent, PhaseStrategy};
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

    /// 当前剩余时间对应的出手甜区 [low, high]。
    fn sweet_band(&self, ctx: &DecisionContext) -> (Decimal, Decimal) {
        if ctx.time_to_expiry > self.cfg.last_phase_window {
            self.cfg.ev_sweet_far
        } else {
            self.cfg.ev_sweet_near
        }
    }

    /// 是否在冷却中。
    fn in_cooldown(&self, ctx: &DecisionContext) -> bool {
        match ctx.last_hedge_at {
            Some(last) => ctx.now < last + self.cfg.ev_cooldown,
            None => false,
        }
    }
}

impl PhaseStrategy for EvHedgeStrategy {
    fn decide(&self, ctx: &DecisionContext) -> Decision {
        let Some((adv_side, prob)) = self.advantaged(ctx) else {
            return Decision::skip();
        };

        // 退出 ①：EV > 0 → 目的达成，撤单扛结算。
        // EV 用结算口径，以 Up 胜出概率（Up 的 mark price）计算。
        let up_prob = ctx.market.mark_price(Side::Up).unwrap_or(Decimal::ZERO);
        if ctx.position.expected_value(up_prob) > Decimal::ZERO {
            return Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::SettlementWait);
        }

        // 退出 ②：优势方概率跌破 0.55 → 期望反转，立即收手。
        if prob < self.cfg.ev_reversal {
            return Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::SettlementWait);
        }

        // 冷却中：装死等下一步。
        if self.in_cooldown(ctx) {
            return Decision::skip();
        }

        // 出手甜区判定。
        let (low, high) = self.sweet_band(ctx);
        if prob < low || prob > high {
            // 迟滞区 [0.55,0.60) 或高位 (>上限)：装死，等市场审判，绝不做冤大头。
            return Decision::skip();
        }

        // 在甜区 → 开火：IOC@0.85 Taker 追买优势方。
        let best_ask = match ctx.market.book(adv_side).best_ask {
            Some(a) => a,
            None => return Decision::skip(),
        };
        let step_budget = ctx.pools.ev_remaining * self.cfg.ev_step_fraction;
        // 用保护上限价估算可买股数（保守：按 cap 算，实际成交价更低）。
        let qty = ctx.constraints.quantize_qty(step_budget / self.cfg.ev_price_cap);
        // 资金耗尽：剩余不够最小单 → 撤单扛结算。
        if !ctx.constraints.is_satisfied(qty, best_ask) {
            return Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::SettlementWait);
        }
        Decision::skip().with(CommandIntent::Submit(OrderIntent::ioc_taker_buy(
            adv_side,
            self.cfg.ev_price_cap,
            qty,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ActiveOrder, PoolBudgets, Trigger};
    use domain::market::{BookTop, MarketSnapshot};
    use domain::order::OrderConstraints;
    use domain::pnl::PositionSnapshot;
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
        tte: u64,
        now: u64,
        last_hedge_at: Option<u64>,
        ev_remaining: Decimal,
    }

    impl Builder {
        fn new() -> Self {
            Self {
                // 双边都负的灾难态（进 EV 的前提）：Up 40/Down 50，成本 100。
                position: PositionSnapshot {
                    up_qty: dec!(40),
                    down_qty: dec!(50),
                    up_cost: dec!(50),
                    down_cost: dec!(50),
                },
                // Down 优势方，概率 0.68（在 >5min 甜区 [0.60,0.75] 内）。
                market: MarketSnapshot {
                    up: book(dec!(0.31), dec!(0.33)),
                    down: book(dec!(0.67), dec!(0.69)),
                },
                tte: 600_000,
                now: 10_000,
                last_hedge_at: None,
                ev_remaining: dec!(375),
            }
        }

        fn build(&self) -> DecisionContext<'_> {
            const NO_ORDERS: &[ActiveOrder] = &[];
            DecisionContext {
                total_capital: dec!(1000),
                trigger: Trigger::BookUpdate,
                now: self.now,
                time_to_expiry: self.tte,
                state: RobotState::EvHedge,
                main_field: Some(Side::Up),
                main_field_frozen: false,
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
                last_hedge_at: self.last_hedge_at,
                calm_since: None,
                constraints: OrderConstraints::default(),
            }
        }
    }

    fn strat() -> EvHedgeStrategy {
        EvHedgeStrategy::new(StrategyConfig::default())
    }

    #[test]
    fn fires_ioc_taker_on_advantaged_side_in_sweet_band() {
        let d = strat().decide(&Builder::new().build());
        let order = d.commands.iter().find_map(|c| match c {
            CommandIntent::Submit(o) => Some(o),
            _ => None,
        });
        let o = order.expect("甜区内应开火");
        assert_eq!(o.side, Side::Down); // 优势方
        assert_eq!(o.price, dec!(0.85)); // IOC 保护上限
        assert_eq!(o.role, domain::types::OrderRole::Taker);
        assert_eq!(o.time_in_force, domain::order::TimeInForce::Ioc);
    }

    #[test]
    fn skips_in_hysteresis_band() {
        let mut b = Builder::new();
        // 优势方概率 0.58 ∈ [0.55,0.60) 迟滞区 → 装死。
        b.market = MarketSnapshot {
            up: book(dec!(0.41), dec!(0.43)),
            down: book(dec!(0.57), dec!(0.59)),
        };
        let d = strat().decide(&b.build());
        assert!(d.is_skip());
    }

    #[test]
    fn skips_when_probability_above_band() {
        let mut b = Builder::new();
        // 优势方概率 0.80 > 甜区上限 0.75 → 装死不做冤大头。
        b.market = MarketSnapshot {
            up: book(dec!(0.19), dec!(0.21)),
            down: book(dec!(0.79), dec!(0.81)),
        };
        let d = strat().decide(&b.build());
        assert!(d.is_skip());
    }

    #[test]
    fn reversal_exit_when_prob_below_055() {
        let mut b = Builder::new();
        // 优势方概率 0.52 < 0.55 → 反转退出收手。
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
        // 让 EV > 0：Up 200/Down 200，成本 100 → 两侧 settle 都 +100，EV 必 >0。
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
        b.last_hedge_at = Some(9_000);
        b.now = 10_000; // 距上次 1000 < 2000 冷却 → 装死。
        let d = strat().decide(&b.build());
        assert!(d.is_skip());
    }

    #[test]
    fn near_window_uses_higher_band() {
        let mut b = Builder::new();
        // 剩余 4min（<5min 窗口）→ 甜区 [0.75,0.85]。概率 0.68 不在内 → 装死。
        b.tte = 240_000;
        let d = strat().decide(&b.build());
        assert!(d.is_skip());
        // 概率 0.80 在 [0.75,0.85] 内 → 开火。
        b.market = MarketSnapshot {
            up: book(dec!(0.19), dec!(0.21)),
            down: book(dec!(0.79), dec!(0.81)),
        };
        let d = strat().decide(&b.build());
        assert!(d.commands.iter().any(|c| matches!(c, CommandIntent::Submit(_))));
    }

    #[test]
    fn ev_step_is_25_percent_of_remaining() {
        // EV 池 375 × 25% = 93.75，/ 0.85 = 110.29 股。
        let d = strat().decide(&Builder::new().build());
        let qty = d.commands.iter().find_map(|c| match c {
            CommandIntent::Submit(o) => Some(o.qty),
            _ => None,
        });
        let expected = OrderConstraints::default().quantize_qty(dec!(93.75) / dec!(0.85));
        assert_eq!(qty, Some(expected));
    }
}
