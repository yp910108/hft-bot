//! 熔断求生态：流动性崩溃时清盘求生，等恢复后重走全局路由。
//!
//! 进入由 router 处理（CancelAll + 切到本态）。本小策略只管「在熔断里等什么、何时恢复」：
//! - spread 仍宽 / 平静不足 5 秒 → 继续装死。
//! - spread < 恢复阈值且持续稳定 ≥ 5 秒 → 恢复，跳回阶段态由 router 重新评估。
//!
//! 恢复是「带记忆的重新评估」：计数器、成本、预算都保留（engine 维护），
//! 这里只负责把状态拨回一个合理的阶段态，下一 tick router 会按最新盘面重判。

use crate::PhaseStrategy;
use crate::config::StrategyConfig;
use crate::context::{Decision, DecisionContext};
use crate::router::circuit_should_trip;
use domain::state::RobotState;
use domain::types::Qty;

/// 熔断求生态小策略。
#[derive(Debug, Clone)]
pub struct CircuitBreakerStrategy {
    cfg: StrategyConfig,
}

impl CircuitBreakerStrategy {
    pub fn new(cfg: StrategyConfig) -> Self {
        Self { cfg }
    }

    /// 恢复后该回到哪个阶段态。
    ///
    /// 带记忆重评：有持仓就回配对态（router 下一 tick 会按 pnl 决定是否再进对冲），
    /// 无持仓回建仓态重新铺。
    fn recovery_target(ctx: &DecisionContext) -> RobotState {
        let has_position = ctx.position.up_qty > Qty::ZERO || ctx.position.down_qty > Qty::ZERO;
        if has_position {
            RobotState::Pairing
        } else {
            RobotState::Building
        }
    }
}

impl PhaseStrategy for CircuitBreakerStrategy {
    fn decide(&self, ctx: &DecisionContext) -> Decision {
        // spread 仍然爆宽 → 继续熔断装死。
        if circuit_should_trip(&ctx.market, &self.cfg) {
            return Decision::skip();
        }

        // spread 已平静，但要持续稳定 ≥ 5 秒才恢复。
        match ctx.round.calm_since {
            Some(since) if ctx.now >= since + self.cfg.circuit_recover_stable => {
                // 稳定够久 → 恢复，跳回阶段态。
                Decision::transition(Self::recovery_target(ctx))
            }
            // 刚平静或还没数够 5 秒 → 继续等。
            _ => Decision::skip(),
        }
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
    use domain::types::{Price, Side};
    use rust_decimal_macros::dec;

    fn book(bid: Price, ask: Price) -> BookTop {
        BookTop {
            best_bid: Some(bid),
            best_ask: Some(ask),
            last_trade: None,
        }
    }

    struct Builder {
        market: MarketSnapshot,
        now: u64,
        has_position: bool,
        round_state: RoundState,
    }

    impl Builder {
        fn new() -> Self {
            let mut round = RoundState::new();
            round.state = RobotState::CircuitBreaker;
            round.main_field = Some(Side::Up);
            Self {
                market: MarketSnapshot {
                    up: book(dec!(0.40), dec!(0.41)),
                    down: book(dec!(0.58), dec!(0.60)),
                },
                now: 10_000,
                has_position: true,
                round_state: round,
            }
        }

        fn build(&self) -> DecisionContext<'_> {
            const NO_ORDERS: &[ActiveOrder] = &[];
            let position = if self.has_position {
                PositionSnapshot {
                    up_qty: dec!(100),
                    down_qty: dec!(50),
                    up_cost: dec!(40),
                    down_cost: dec!(29),
                }
            } else {
                PositionSnapshot {
                    up_qty: dec!(0),
                    down_qty: dec!(0),
                    up_cost: dec!(0),
                    down_cost: dec!(0),
                }
            };
            DecisionContext {
                total_capital: dec!(1000),
                trigger: Trigger::BookUpdate,
                now: self.now,
                time_to_expiry: 600_000,
                position,
                market: self.market,
                pools: PoolBudgets {
                    grid_maker_total: dec!(150),
                    grid_maker_remaining: dec!(150),
                    dynamic_remaining: dec!(225),
                    ev_remaining: dec!(375),
                    max_exposure: dec!(112.5),
                },
                active_orders: NO_ORDERS,
                constraints: OrderConstraints::default(),
                round: &self.round_state,
            }
        }
    }

    fn strat() -> CircuitBreakerStrategy {
        CircuitBreakerStrategy::new(StrategyConfig::default())
    }

    #[test]
    fn stays_when_spread_still_wide() {
        let mut b = Builder::new();
        b.market = MarketSnapshot {
            up: book(dec!(0.40), dec!(0.41)),
            down: book(dec!(0.20), dec!(0.55)), // Down spread 爆宽。
        };
        b.round_state.calm_since = None;
        assert!(strat().decide(&b.build()).is_skip());
    }

    #[test]
    fn stays_when_calm_not_long_enough() {
        let mut b = Builder::new();
        // 平静起始 8000，现在 10000，只过了 2 秒 < 5 秒。
        b.round_state.calm_since = Some(8_000);
        b.now = 10_000;
        assert!(strat().decide(&b.build()).is_skip());
    }

    #[test]
    fn recovers_after_five_seconds_calm() {
        let mut b = Builder::new();
        // 平静起始 4000，现在 10000，过了 6 秒 ≥ 5 → 恢复到配对态（有持仓）。
        b.round_state.calm_since = Some(4_000);
        b.now = 10_000;
        b.has_position = true;
        let d = strat().decide(&b.build());
        assert_eq!(d.transition, Some(RobotState::Pairing));
    }

    #[test]
    fn recovers_to_building_when_no_position() {
        let mut b = Builder::new();
        b.round_state.calm_since = Some(4_000);
        b.now = 10_000;
        b.has_position = false;
        let d = strat().decide(&b.build());
        assert_eq!(d.transition, Some(RobotState::Building));
    }
}
