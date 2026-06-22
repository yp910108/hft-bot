//! 临时 edge 重测程序（阶段 10 对冲补全后）：跑合成标准场景 + 933 真实历史市场，
//! 输出 PnL 统计与 final_state 分布。用完即删，不属于交易系统运行时。
//!
//! 运行：`cargo run -p backtest --example edge_replay --release`

use backtest::batch::{self, BatchConfig};
use backtest::driver::{self, BacktestConfig};
use backtest::real_data;
use domain::fee::FeeModel;
use domain::order::OrderConstraints;
use domain::state::RobotState;
use engine::EngineConfig;
use fsm::Thresholds;
use risk::auditor::RiskAuditor;
use risk::pool::CapitalPools;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::BTreeMap;
use strategy::GradientLadder;

fn make_config(total_capital: Decimal) -> BacktestConfig {
    let pools = CapitalPools::with_default_ratios(total_capital);
    BacktestConfig {
        total_capital,
        thresholds: Thresholds {
            hedge_loss_trigger: dec!(30),
            hedge_safety_price: dec!(0.5),
            profit_target: dec!(15),
        },
        engine_config: EngineConfig {
            grid_maker_pool: pools.grid_maker(),
            hedge_attack_pool: pools.hedge_attack(),
            hedge_step_fraction: dec!(0.2),
            max_taker_steps: 5,
            constraints: OrderConstraints::default(),
        },
    }
}

fn state_label(state: RobotState) -> &'static str {
    match state {
        RobotState::Initialization => "Init",
        RobotState::RangeBoundMaking => "RangeBound",
        RobotState::DynamicHedging { .. } => "DynamicHedging",
        RobotState::EvHedging => "EvHedging",
        RobotState::FinalSettlement => "FinalSettlement",
        RobotState::ChopMarketShutdown => "ChopShutdown",
    }
}

fn main() {
    let total_capital = dec!(1000);
    let config = make_config(total_capital);

    // ===== 一、合成标准场景（每场景 50 场）=====
    println!("===== 合成标准场景（50 场/场景，零费）=====");
    let batch = BatchConfig {
        sessions_per_scenario: 50,
        steps: 300,
        backtest: config,
    };
    let make_ladder = GradientLadder::with_default_config;
    let make_auditor =
        || RiskAuditor::with_default_guard(CapitalPools::with_default_ratios(total_capital));
    for scenario in batch::standard_scenarios() {
        let stats = batch::run_scenario(
            &scenario,
            &batch,
            FeeModel::zero(),
            &make_ladder,
            &make_auditor,
        );
        println!(
            "[{}] 胜率 {:.0}% 均PnL {:.2} 最好 {:.2} 最差 {:.2}",
            stats.label,
            (stats.win_rate() * dec!(100)).round(),
            stats.average_pnl,
            stats.max_pnl,
            stats.min_pnl,
        );
    }

    // ===== 二、933 真实历史市场 =====
    println!("\n===== 933 真实历史市场 =====");
    run_real(&config, FeeModel::zero(), "零费");
    run_real(&config, FeeModel::default(), "4% Taker费");
}

fn run_real(config: &BacktestConfig, fee: FeeModel, tag: &str) {
    let total_capital = config.total_capital;
    let mut markets = Vec::new();
    let logs_dir = std::path::Path::new("logs");
    if let Ok(dates) = std::fs::read_dir(logs_dir) {
        let mut date_dirs: Vec<_> = dates
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        date_dirs.sort();
        for dir in date_dirs {
            if let Ok(mut ms) = real_data::load_dir(&dir) {
                markets.append(&mut ms);
            }
        }
    }

    let mut total_pnl = Decimal::ZERO;
    let mut wins = 0usize;
    let mut min_pnl = Decimal::MAX;
    let mut max_pnl = Decimal::MIN;
    let mut states: BTreeMap<&str, usize> = BTreeMap::new();
    let mut win_pnl_sum = Decimal::ZERO;
    let mut loss_pnl_sum = Decimal::ZERO;

    for market in &markets {
        let report = driver::run(
            market,
            config,
            GradientLadder::with_default_config(),
            RiskAuditor::with_default_guard(CapitalPools::with_default_ratios(total_capital)),
            fee,
        );
        total_pnl += report.final_pnl;
        if report.final_pnl > Decimal::ZERO {
            wins += 1;
            win_pnl_sum += report.final_pnl;
        } else {
            loss_pnl_sum += report.final_pnl;
        }
        min_pnl = min_pnl.min(report.final_pnl);
        max_pnl = max_pnl.max(report.final_pnl);
        *states.entry(state_label(report.final_state)).or_insert(0) += 1;
    }

    let n = markets.len();
    let avg = if n > 0 {
        total_pnl / Decimal::from(n)
    } else {
        Decimal::ZERO
    };
    println!(
        "[{tag}] 场数 {n} 累计PnL {:.2} 均PnL {:.2} 胜率 {:.1}% 最好 {:.2} 最差 {:.2}",
        total_pnl,
        avg,
        if n > 0 {
            Decimal::from(wins) / Decimal::from(n) * dec!(100)
        } else {
            Decimal::ZERO
        },
        max_pnl,
        min_pnl,
    );
    println!(
        "      赢场均盈 {:.2}（{}场）亏场均亏 {:.2}（{}场）",
        if wins > 0 {
            win_pnl_sum / Decimal::from(wins)
        } else {
            Decimal::ZERO
        },
        wins,
        if n - wins > 0 {
            loss_pnl_sum / Decimal::from(n - wins)
        } else {
            Decimal::ZERO
        },
        n - wins,
    );
    println!("      final_state 分布: {states:?}");
}
