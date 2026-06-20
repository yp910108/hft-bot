//! 多场批量回测与分组统计：验证策略 edge 的结构——震荡盘赚的能否覆盖趋势盘亏的。
//!
//! 单场回测受随机性影响大，必须跑多场并按场景分组才能看出 edge 来源：
//! 若策略成立，应观察到「纯震荡组期望为正、单边趋势组期望为负或更差」的结构，
//! 且综合期望需为正才说明策略整体可盈利。

use crate::driver::{self, BacktestConfig};
use crate::market::{self, SyntheticMarketConfig};
use domain::fee::FeeModel;
use domain::types::Money;
use risk::auditor::RiskAuditor;
use rust_decimal::Decimal;
use strategy::GradientLadder;

/// 一个回测场景：一组共享的行情特征 + 一个可读标签。
#[derive(Debug, Clone, Copy)]
pub struct Scenario {
    /// 场景名（如「纯震荡」「上趋势」）。
    pub label: &'static str,
    /// 趋势漂移：0 为纯震荡，正为上趋势，负为下趋势。
    pub drift: f64,
    /// 每步波动幅度。
    pub volatility: f64,
}

/// 一个场景跑 N 场后的聚合统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchStats {
    /// 场景名。
    pub label: &'static str,
    /// 总场数。
    pub sessions: usize,
    /// 盈利场数（final_pnl > 0）。
    pub winning_sessions: usize,
    /// 平均最终盈亏。
    pub average_pnl: Money,
    /// 最差单场盈亏。
    pub min_pnl: Money,
    /// 最好单场盈亏。
    pub max_pnl: Money,
}

impl BatchStats {
    /// 胜率（盈利场数 / 总场数），无场次时为 0。
    pub fn win_rate(&self) -> Decimal {
        if self.sessions == 0 {
            Decimal::ZERO
        } else {
            Decimal::from(self.winning_sessions) / Decimal::from(self.sessions)
        }
    }
}

/// 批量回测的运行参数。
#[derive(Debug, Clone, Copy)]
pub struct BatchConfig {
    /// 每个场景跑多少场（每场用不同种子）。
    pub sessions_per_scenario: usize,
    /// 每场行情步数。
    pub steps: usize,
    /// 单场回测配置（资金、阈值、引擎参数）。
    pub backtest: BacktestConfig,
}

/// 对单个场景跑 `sessions` 场（种子 0..sessions），聚合统计。
///
/// `make_ladder` / `make_auditor` 为每场提供全新的策略与风控实例（二者含可变或值语义状态，
/// 每场独立构造避免跨场污染）。
pub fn run_scenario(
    scenario: &Scenario,
    batch: &BatchConfig,
    fee_model: FeeModel,
    make_ladder: &dyn Fn() -> GradientLadder,
    make_auditor: &dyn Fn() -> RiskAuditor,
) -> BatchStats {
    let mut winning_sessions = 0usize;
    let mut total_pnl = Decimal::ZERO;
    let mut min_pnl = Decimal::MAX;
    let mut max_pnl = Decimal::MIN;

    for seed in 0..batch.sessions_per_scenario {
        let market_config = SyntheticMarketConfig {
            steps: batch.steps,
            volatility: scenario.volatility,
            drift: scenario.drift,
            seed: seed as u64,
            ..SyntheticMarketConfig::default()
        };
        let market = market::generate(&market_config);
        let report = driver::run(
            &market,
            &batch.backtest,
            make_ladder(),
            make_auditor(),
            fee_model,
        );

        if report.final_pnl > Decimal::ZERO {
            winning_sessions += 1;
        }
        total_pnl += report.final_pnl;
        min_pnl = min_pnl.min(report.final_pnl);
        max_pnl = max_pnl.max(report.final_pnl);
    }

    let sessions = batch.sessions_per_scenario;
    let average_pnl = if sessions == 0 {
        Decimal::ZERO
    } else {
        total_pnl / Decimal::from(sessions)
    };

    BatchStats {
        label: scenario.label,
        sessions,
        winning_sessions,
        average_pnl,
        min_pnl: if sessions == 0 {
            Decimal::ZERO
        } else {
            min_pnl
        },
        max_pnl: if sessions == 0 {
            Decimal::ZERO
        } else {
            max_pnl
        },
    }
}

/// 对一组场景分别批量回测，返回每个场景的统计。
pub fn run_scenarios(
    scenarios: &[Scenario],
    batch: &BatchConfig,
    fee_model: FeeModel,
    make_ladder: &dyn Fn() -> GradientLadder,
    make_auditor: &dyn Fn() -> RiskAuditor,
) -> Vec<BatchStats> {
    scenarios
        .iter()
        .map(|scenario| run_scenario(scenario, batch, fee_model, make_ladder, make_auditor))
        .collect()
}

/// 验证 edge 常用的标准场景集合：纯震荡、上趋势、下趋势。
pub fn standard_scenarios() -> [Scenario; 3] {
    [
        Scenario {
            label: "纯震荡",
            drift: 0.0,
            volatility: 200.0,
        },
        Scenario {
            label: "上趋势",
            drift: 80.0,
            volatility: 100.0,
        },
        Scenario {
            label: "下趋势",
            drift: -80.0,
            volatility: 100.0,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::order::OrderConstraints;
    use engine::EngineConfig;
    use fsm::Thresholds;
    use risk::pool::CapitalPools;
    use rust_decimal_macros::dec;

    /// 构造标准批量配置 + 策略/风控工厂。
    fn batch_setup() -> (
        BatchConfig,
        FeeModel,
        impl Fn() -> GradientLadder,
        impl Fn() -> RiskAuditor,
    ) {
        let total_capital = dec!(1000);
        let backtest = BacktestConfig {
            total_capital,
            thresholds: Thresholds {
                hedge_loss_trigger: dec!(30),
                hedge_safety_price: dec!(0.5),
                profit_target: dec!(15),
            },
            engine_config: EngineConfig {
                grid_maker_pool: CapitalPools::with_default_ratios(total_capital).grid_maker(),
                constraints: OrderConstraints::default(),
            },
        };
        let batch = BatchConfig {
            sessions_per_scenario: 20,
            steps: 300,
            backtest,
        };
        let make_ladder = || GradientLadder::with_default_config();
        let make_auditor =
            || RiskAuditor::with_default_guard(CapitalPools::with_default_ratios(dec!(1000)));
        (batch, FeeModel::zero(), make_ladder, make_auditor)
    }

    #[test]
    fn batch_runs_requested_session_count() {
        let (batch, fee, ladder, auditor) = batch_setup();
        let stats = run_scenario(&standard_scenarios()[0], &batch, fee, &ladder, &auditor);
        assert_eq!(stats.sessions, 20);
        // 盈利场数不超过总场数。
        assert!(stats.winning_sessions <= stats.sessions);
    }

    #[test]
    fn win_rate_is_ratio_of_winning_sessions() {
        let stats = BatchStats {
            label: "测试",
            sessions: 20,
            winning_sessions: 5,
            average_pnl: dec!(0),
            min_pnl: dec!(0),
            max_pnl: dec!(0),
        };
        assert_eq!(stats.win_rate(), dec!(0.25));
    }

    #[test]
    fn all_scenarios_produce_stats() {
        let (batch, fee, ladder, auditor) = batch_setup();
        let scenarios = standard_scenarios();
        let results = run_scenarios(&scenarios, &batch, fee, &ladder, &auditor);
        assert_eq!(results.len(), 3);
        // 每个场景标签与输入一致。
        assert_eq!(results[0].label, "纯震荡");
        assert_eq!(results[1].label, "上趋势");
        assert_eq!(results[2].label, "下趋势");
    }

    #[test]
    fn min_pnl_not_greater_than_max() {
        let (batch, fee, ladder, auditor) = batch_setup();
        let stats = run_scenario(&standard_scenarios()[0], &batch, fee, &ladder, &auditor);
        assert!(stats.min_pnl <= stats.max_pnl);
    }
}
