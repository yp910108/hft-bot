//! 单场回测驱动器：用合成行情序列驱动 [`Simulator`] 撮合与 [`Engine`] 决策跑完一场，
//! 交割时按胜出方兑现，产出最终盈亏报告。
//!
//! 流程：每个行情步先喂 simulator 撮合产出成交事件，再把事件交给 engine 更新账本与决策，
//! engine 产出的新指令翻译为对 simulator 的下单/撤单调用，如此循环。

use crate::market::SyntheticMarket;
use domain::market::MarketSnapshot;
use domain::order::Command;
use domain::types::{Money, Side};
use engine::{Engine, EngineConfig};
use exchange::backend::ExchangeBackend;
use exchange::event::ExchangeEvent;
use exchange::simulator::Simulator;
use fsm::Thresholds;
use risk::auditor::RiskAuditor;
use strategy::GradientLadder;
use tokio::sync::mpsc::UnboundedReceiver;

/// 一场回测的配置（资金与各模块参数）。
#[derive(Debug, Clone, Copy)]
pub struct BacktestConfig {
    /// 账户总资金 V。
    pub total_capital: Money,
    /// 状态机阈值。
    pub thresholds: Thresholds,
    /// 引擎运行参数（核心做市池额度、最小量约束）。
    pub engine_config: EngineConfig,
}

/// 一场回测的结果报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BacktestReport {
    /// 交割胜出方。
    pub winner: Side,
    /// 胜出方持仓股数。
    pub winner_qty: Money,
    /// 双边累计总成本。
    pub total_cost: Money,
    /// 最终盈亏 = 胜出方持仓 × 1 - 总成本（每股兑付 1 美元）。
    pub final_pnl: Money,
    /// 全程累计成交笔数。
    pub fill_count: usize,
}

/// 用一场合成行情驱动一次完整回测。
pub fn run(
    market: &SyntheticMarket,
    config: &BacktestConfig,
    ladder: GradientLadder,
    auditor: RiskAuditor,
    fee_model: domain::fee::FeeModel,
) -> BacktestReport {
    let mut engine = Engine::new(
        config.total_capital,
        config.thresholds,
        ladder,
        auditor,
        config.engine_config,
    );
    let (mut simulator, mut events) = Simulator::new(fee_model);
    let mut fill_count = 0usize;

    let mut snapshots = market.snapshots.iter();

    // 用首个行情快照初始化引擎，再做初始布阵。
    if let Some(first) = snapshots.next() {
        dispatch(
            &mut simulator,
            engine.handle_event(ExchangeEvent::BookUpdate(*first)),
        );
        dispatch(&mut simulator, engine.start());
        drain_events(
            &mut simulator,
            &mut engine,
            &mut events,
            &mut fill_count,
            first,
        );
    }

    // 逐个行情步推进。
    for snapshot in snapshots {
        // 行情先入引擎（刷新状态机视角），再驱动撮合。
        dispatch(
            &mut simulator,
            engine.handle_event(ExchangeEvent::BookUpdate(*snapshot)),
        );
        simulator.on_market(snapshot);
        drain_events(
            &mut simulator,
            &mut engine,
            &mut events,
            &mut fill_count,
            snapshot,
        );
    }

    settle(&engine, market.winner, fill_count)
}

/// 把 simulator 当前产出的事件全部排空，喂给 engine，并下发 engine 的新指令。
fn drain_events(
    simulator: &mut Simulator,
    engine: &mut Engine,
    events: &mut UnboundedReceiver<ExchangeEvent>,
    fill_count: &mut usize,
    snapshot: &MarketSnapshot,
) {
    let _ = snapshot;
    while let Ok(event) = events.try_recv() {
        if matches!(event, ExchangeEvent::Filled(_)) {
            *fill_count += 1;
        }
        let commands = engine.handle_event(event);
        dispatch(simulator, commands);
    }
}

/// 把 engine 产出的指令翻译为对 simulator 的后端调用。
fn dispatch(simulator: &mut Simulator, commands: Vec<Command>) {
    for command in commands {
        match command {
            Command::SubmitOrder(order) => simulator.submit_order(order),
            Command::CancelOrder(order_id) => simulator.cancel_order(order_id),
            Command::CancelSide(side) => simulator.cancel_side(side),
            Command::CancelAll => simulator.cancel_all(),
        }
    }
}

/// 交割结算：按胜出方持仓兑现，计算最终盈亏。
fn settle(engine: &Engine, winner: Side, fill_count: usize) -> BacktestReport {
    let ledger = engine.ledger();
    let winner_qty = ledger.qty(winner);
    let total_cost = ledger.total_cost();
    // 胜出方每股兑付 1 美元，败方持仓归零。
    let final_pnl = winner_qty - total_cost;
    BacktestReport {
        winner,
        winner_qty,
        total_cost,
        final_pnl,
        fill_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::{self, SyntheticMarketConfig};
    use domain::fee::FeeModel;
    use domain::order::OrderConstraints;
    use risk::pool::CapitalPools;
    use rust_decimal_macros::dec;

    /// 构造一份标准回测配置：总资金 1000、默认三池与阈值。
    fn backtest_setup() -> (BacktestConfig, GradientLadder, RiskAuditor, FeeModel) {
        let total_capital = dec!(1000);
        let pools = CapitalPools::with_default_ratios(total_capital);
        let config = BacktestConfig {
            total_capital,
            thresholds: Thresholds {
                hedge_loss_trigger: dec!(30),
                hedge_safety_price: dec!(0.5),
                profit_target: dec!(15),
            },
            engine_config: EngineConfig {
                grid_maker_pool: pools.grid_maker(),
                constraints: OrderConstraints::default(),
            },
        };
        (
            config,
            GradientLadder::with_default_config(),
            RiskAuditor::with_default_guard(pools),
            FeeModel::zero(),
        )
    }

    #[test]
    fn runs_a_full_session_without_panic() {
        let market = market::generate(&SyntheticMarketConfig::default());
        let (config, ladder, auditor, fee) = backtest_setup();
        let report = run(&market, &config, ladder, auditor, fee);
        // 报告字段自洽：最终盈亏 = 胜出方持仓 - 总成本。
        assert_eq!(report.final_pnl, report.winner_qty - report.total_cost);
    }

    #[test]
    fn winner_matches_market_outcome() {
        // 强上行行情 → 胜出方应为 Up。
        let market = market::generate(&SyntheticMarketConfig {
            drift: 100.0,
            volatility: 1.0,
            steps: 100,
            ..SyntheticMarketConfig::default()
        });
        let (config, ladder, auditor, fee) = backtest_setup();
        let report = run(&market, &config, ladder, auditor, fee);
        assert_eq!(report.winner, Side::Up);
    }

    #[test]
    fn ranging_market_executes_some_fills() {
        // 纯震荡、较大波动 → 价格在区间内来回，应能产生若干成交。
        let market = market::generate(&SyntheticMarketConfig {
            drift: 0.0,
            volatility: 200.0,
            steps: 300,
            seed: 12345,
            ..SyntheticMarketConfig::default()
        });
        let (config, ladder, auditor, fee) = backtest_setup();
        let report = run(&market, &config, ladder, auditor, fee);
        // 这一步只验证回测闭环能跑出成交，不对盈亏方向下断言（edge 验证留待多场统计）。
        assert!(report.fill_count > 0);
    }

    #[test]
    fn deterministic_same_seed_same_report() {
        let market_config = SyntheticMarketConfig {
            seed: 999,
            ..SyntheticMarketConfig::default()
        };
        let market = market::generate(&market_config);
        let (config, ladder, auditor, fee) = backtest_setup();
        let report_a = run(&market, &config, ladder.clone(), auditor, fee);
        let report_b = run(&market, &config, ladder, auditor, fee);
        // 同种子同配置 → 回测完全可复现。
        assert_eq!(report_a, report_b);
    }
}
