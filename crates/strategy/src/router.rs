//! 全局条件路由：策略文档「Global Evaluation Router」的代码化身。
//!
//! 每个 tick 先过优先级链，从高到低裁决当前归谁管：
//! ```text
//! 时间红线(<1min) > 熔断求生(Spread>30%) > 尾盘规则(TTE<5min) > EV对冲 > 动态对冲 > 核心做市
//! ```
//! 高优先级的全局条件（时间红线、熔断、尾盘）无条件压制当前阶段；都不触发时，
//! 才把控制权交给当前状态对应的阶段小策略。
//!
//! 路由产出的是「该调用哪个小策略」的判定 + 全局态的直接决策。
//! 纯函数：只读 [`DecisionContext`]，不改任何东西。

use crate::config::StrategyConfig;
use crate::context::{CommandIntent, Decision, DecisionContext};
use domain::market::MarketSnapshot;
use domain::state::RobotState;
use domain::types::{Price, Side};
use rust_decimal::Decimal;

/// 路由裁决：要么是某个全局态的直接决策，要么指派给某个阶段小策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// 全局条件命中，直接给出决策（时间红线 / 进熔断 / 尾盘规则）。
    Direct(Decision),
    /// 交给当前阶段对应的小策略处理。
    Phase(Phase),
}

/// 阶段小策略的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Building,
    Pairing,
    DynamicHedge,
    EvHedge,
    CircuitBreaker,
}

/// 计算某侧的买卖价差率 = (best_ask − best_bid) / best_bid。缺价或 bid≤0 返回 None。
pub fn spread_ratio(market: &MarketSnapshot, side: Side) -> Option<Price> {
    let book = market.book(side);
    match (book.best_bid, book.best_ask) {
        (Some(bid), Some(ask)) if bid > Price::ZERO => Some((ask - bid) / bid),
        _ => None,
    }
}

/// 任一侧 Spread_Ratio 是否触发熔断。
pub fn circuit_should_trip(market: &MarketSnapshot, cfg: &StrategyConfig) -> bool {
    [Side::Up, Side::Down].iter().any(|&side| {
        spread_ratio(market, side).is_some_and(|ratio| ratio > cfg.circuit_trigger_ratio)
    })
}

/// 尾盘规则判定：亏损大侧「结算 pnl × 该侧概率」是否 ≤ 亏损触发线。
///
/// 满足时应进 EV（由 router 直接产出 Decision），不满足则收手扛结算。
fn tail_end_breach(ctx: &DecisionContext, cfg: &StrategyConfig) -> bool {
    let loss = cfg.loss_trigger * ctx.total_capital;
    // 找亏损大侧（结算 pnl 更小的那侧）。
    let weaker = ctx.position.weaker_side().unwrap_or(Side::Up);
    let prob = ctx.market.mark_price(weaker).unwrap_or(Decimal::ONE);
    let weighted = ctx.position.settle_pnl(weaker) * prob;
    weighted <= loss
}

/// 优先级链路由。返回当前 tick 的裁决。
pub fn route(ctx: &DecisionContext, cfg: &StrategyConfig) -> Route {
    // 终态：已在等待结算，什么都不做。
    if ctx.round.state == RobotState::SettlementWait {
        return Route::Direct(Decision::skip());
    }

    // 第 1 优先级：时间红线。TTE < 1min，无条件 CancelAll 锁结算。
    if ctx.time_to_expiry < cfg.time_red_line {
        return Route::Direct(
            Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::SettlementWait),
        );
    }

    // 第 2 优先级：熔断。任一侧 spread 崩溃。
    // 已在熔断态则交给熔断小策略处理恢复；否则触发进入熔断。
    if ctx.round.state == RobotState::CircuitBreaker {
        return Route::Phase(Phase::CircuitBreaker);
    }
    if circuit_should_trip(&ctx.market, cfg) {
        return Route::Direct(
            Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::CircuitBreaker),
        );
    }

    // 第 3 优先级：全局尾盘规则（TTE < 5min）。
    // 做市态和动态对冲态共用：亏损破线进 EV，否则收手扛结算。
    // 已在 EV 态则交给 EV 小策略继续处理，不重复裁决。
    if ctx.time_to_expiry < cfg.last_phase_window && ctx.round.state != RobotState::EvHedge {
        if tail_end_breach(ctx, cfg) {
            return Route::Direct(
                Decision::skip()
                    .with(CommandIntent::CancelAll)
                    .moving_to(RobotState::EvHedge),
            );
        }
        return Route::Direct(
            Decision::skip()
                .with(CommandIntent::CancelAll)
                .moving_to(RobotState::SettlementWait),
        );
    }

    // 第 4~5 优先级：交给当前状态对应的阶段小策略。
    match ctx.round.state {
        RobotState::Building => Route::Phase(Phase::Building),
        RobotState::Pairing => Route::Phase(Phase::Pairing),
        RobotState::DynamicHedge => Route::Phase(Phase::DynamicHedge),
        RobotState::EvHedge => Route::Phase(Phase::EvHedge),
        // CircuitBreaker 已在上面处理，SettlementWait 已在最前返回。
        RobotState::CircuitBreaker | RobotState::SettlementWait => Route::Direct(Decision::skip()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{PoolBudgets, Trigger};
    use domain::market::BookTop;
    use domain::order::OrderConstraints;
    use domain::pnl::PositionSnapshot;
    use domain::round_state::RoundState;
    use rust_decimal_macros::dec;

    fn ctx_with(state: RobotState, tte: u64, market: MarketSnapshot) -> DecisionContext<'static> {
        let mut round = RoundState::new();
        round.state = state;
        round.main_field = Some(Side::Up);
        // Leak for 'static lifetime in tests (tiny, never freed, acceptable in test code).
        let round_ref: &'static RoundState = Box::leak(Box::new(round));
        DecisionContext {
            total_capital: dec!(1000),
            trigger: Trigger::BookUpdate,
            now: 0,
            time_to_expiry: tte,
            position: PositionSnapshot {
                up_qty: dec!(0),
                down_qty: dec!(0),
                up_cost: dec!(0),
                down_cost: dec!(0),
            },
            market,
            pools: PoolBudgets {
                grid_maker_total: dec!(150),
                grid_maker_remaining: dec!(150),
                dynamic_remaining: dec!(225),
                ev_remaining: dec!(375),
                max_exposure: dec!(112.5),
            },
            active_orders: &[],
            constraints: OrderConstraints::default(),
            round: round_ref,
        }
    }

    fn book(bid: Price, ask: Price) -> BookTop {
        BookTop {
            best_bid: Some(bid),
            best_ask: Some(ask),
            last_trade: None,
        }
    }

    fn healthy_market() -> MarketSnapshot {
        MarketSnapshot {
            up: book(dec!(0.40), dec!(0.41)),
            down: book(dec!(0.59), dec!(0.60)),
        }
    }

    #[test]
    fn settlement_wait_does_nothing() {
        let r = route(
            &ctx_with(RobotState::SettlementWait, 600_000, healthy_market()),
            &StrategyConfig::default(),
        );
        assert_eq!(r, Route::Direct(Decision::skip()));
    }

    #[test]
    fn time_red_line_overrides_everything() {
        // TTE 30s < 1min，即便在 EV 对冲也强制收手。
        let r = route(
            &ctx_with(RobotState::EvHedge, 30_000, healthy_market()),
            &StrategyConfig::default(),
        );
        match r {
            Route::Direct(d) => {
                assert!(d.commands.contains(&CommandIntent::CancelAll));
                assert_eq!(d.transition, Some(RobotState::SettlementWait));
            }
            _ => panic!("时间红线应直接收手"),
        }
    }

    #[test]
    fn circuit_trips_on_wide_spread() {
        // Down 侧 spread 爆宽：bid 0.20 ask 0.55 → ratio = 1.75 > 0.30。
        let market = MarketSnapshot {
            up: book(dec!(0.40), dec!(0.41)),
            down: book(dec!(0.20), dec!(0.55)),
        };
        let r = route(
            &ctx_with(RobotState::Pairing, 600_000, market),
            &StrategyConfig::default(),
        );
        match r {
            Route::Direct(d) => {
                assert!(d.commands.contains(&CommandIntent::CancelAll));
                assert_eq!(d.transition, Some(RobotState::CircuitBreaker));
            }
            _ => panic!("宽 spread 应触发熔断"),
        }
    }

    #[test]
    fn time_red_line_beats_circuit() {
        // 同时满足时间红线和熔断 → 时间红线优先（进结算而非熔断）。
        let market = MarketSnapshot {
            up: book(dec!(0.40), dec!(0.41)),
            down: book(dec!(0.20), dec!(0.55)),
        };
        let r = route(
            &ctx_with(RobotState::Pairing, 30_000, market),
            &StrategyConfig::default(),
        );
        match r {
            Route::Direct(d) => assert_eq!(d.transition, Some(RobotState::SettlementWait)),
            _ => panic!("时间红线应压制熔断"),
        }
    }

    #[test]
    fn tail_end_rule_enters_ev_when_breach() {
        // TTE 200s（<5min），且亏损大侧加权穿线。
        let mut ctx = ctx_with(RobotState::Pairing, 200_000, healthy_market());
        // Up 持仓 100，总成本 150 → up_settle = 100-150 = -50。mark 0.405 → -50*0.405=-20.25 ≤ -20。
        ctx.position = PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(0),
            up_cost: dec!(150),
            down_cost: dec!(0),
        };
        let r = route(&ctx, &StrategyConfig::default());
        match r {
            Route::Direct(d) => {
                assert!(d.commands.contains(&CommandIntent::CancelAll));
                assert_eq!(d.transition, Some(RobotState::EvHedge));
            }
            _ => panic!("尾盘亏损破线应进 EV"),
        }
    }

    #[test]
    fn tail_end_rule_settles_when_no_breach() {
        // TTE 200s（<5min），但没穿线 → 收手扛结算。
        let ctx = ctx_with(RobotState::Pairing, 200_000, healthy_market());
        // 默认持仓全 0，settle pnl = 0，不穿线。
        let r = route(&ctx, &StrategyConfig::default());
        match r {
            Route::Direct(d) => {
                assert!(d.commands.contains(&CommandIntent::CancelAll));
                assert_eq!(d.transition, Some(RobotState::SettlementWait));
            }
            _ => panic!("尾盘未破线应收手扛结算"),
        }
    }

    #[test]
    fn tail_end_does_not_affect_ev_state() {
        // 已在 EV 态，TTE<5min → 尾盘规则不重复裁决，交给 EV 小策略。
        let r = route(
            &ctx_with(RobotState::EvHedge, 200_000, healthy_market()),
            &StrategyConfig::default(),
        );
        assert_eq!(r, Route::Phase(Phase::EvHedge));
    }

    #[test]
    fn healthy_market_routes_to_current_phase() {
        let cfg = StrategyConfig::default();
        assert_eq!(
            route(
                &ctx_with(RobotState::Building, 600_000, healthy_market()),
                &cfg
            ),
            Route::Phase(Phase::Building)
        );
        assert_eq!(
            route(
                &ctx_with(RobotState::Pairing, 600_000, healthy_market()),
                &cfg
            ),
            Route::Phase(Phase::Pairing)
        );
        assert_eq!(
            route(
                &ctx_with(RobotState::DynamicHedge, 600_000, healthy_market()),
                &cfg
            ),
            Route::Phase(Phase::DynamicHedge)
        );
        assert_eq!(
            route(
                &ctx_with(RobotState::EvHedge, 600_000, healthy_market()),
                &cfg
            ),
            Route::Phase(Phase::EvHedge)
        );
    }

    #[test]
    fn circuit_state_routes_to_circuit_strategy() {
        // 已在熔断态，即便 spread 仍宽也交给熔断小策略管恢复。
        let market = MarketSnapshot {
            up: book(dec!(0.40), dec!(0.41)),
            down: book(dec!(0.20), dec!(0.55)),
        };
        let r = route(
            &ctx_with(RobotState::CircuitBreaker, 600_000, market),
            &StrategyConfig::default(),
        );
        assert_eq!(r, Route::Phase(Phase::CircuitBreaker));
    }

    #[test]
    fn spread_ratio_computed_correctly() {
        let market = MarketSnapshot {
            up: book(dec!(0.40), dec!(0.50)),
            down: BookTop::default(),
        };
        // (0.50 − 0.40) / 0.40 = 0.25。
        assert_eq!(spread_ratio(&market, Side::Up), Some(dec!(0.25)));
        // Down 侧缺价 → None。
        assert_eq!(spread_ratio(&market, Side::Down), None);
    }
}
