//! 诊断：分析止盈/止损的频率和幅度，找到调参杠杆点。
//!
//! cargo run -p backtest --example diagnose --release

use backtest::real_data::load_dir;
use backtest::run::{MatchResult, run_match};
use domain::fee::FeeModel;
use engine::config::EngineConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn main() {
    let markets = load_dir("logs").expect("加载 logs/ 失败");
    println!("加载 {} 场", markets.len());

    let fee = FeeModel::default();
    let cfg = EngineConfig::default();

    let mut results: Vec<MatchResult> = Vec::new();
    for market in &markets {
        let r = run_match(market, cfg.clone(), fee);
        results.push(r);
    }

    let n = results.len();

    // 分析成交笔数分布
    let mut fill_counts: Vec<u32> = results.iter().map(|r| r.fills).collect();
    fill_counts.sort();
    println!("\n─── 成交笔数分布 ───");
    println!("  P10: {}", fill_counts[n / 10]);
    println!("  P25: {}", fill_counts[n / 4]);
    println!("  P50: {}", fill_counts[n / 2]);
    println!("  P75: {}", fill_counts[n * 3 / 4]);
    println!("  P90: {}", fill_counts[n * 9 / 10]);

    // 分析已实现盈亏分布
    let mut realized: Vec<Decimal> = results.iter().map(|r| r.realized_pnl).collect();
    realized.sort();
    println!("\n─── 已实现盈亏分布 ───");
    println!("  P10: {:.2}", realized[n / 10]);
    println!("  P25: {:.2}", realized[n / 4]);
    println!("  P50: {:.2}", realized[n / 2]);
    println!("  P75: {:.2}", realized[n * 3 / 4]);
    println!("  P90: {:.2}", realized[n * 9 / 10]);

    // 按阶段终态分组
    let settled_count = results
        .iter()
        .filter(|r| r.final_phase == domain::phase::Phase::Settled)
        .count();
    let harvesting_count = results
        .iter()
        .filter(|r| r.final_phase == domain::phase::Phase::Harvesting)
        .count();
    let cycling_count = results
        .iter()
        .filter(|r| r.final_phase == domain::phase::Phase::Cycling)
        .count();
    println!("\n─── 终态分布 ───");
    println!("  Settled: {settled_count}");
    println!("  Harvesting: {harvesting_count}");
    println!("  Cycling: {cycling_count}");

    // 净投入分布
    let mut invested: Vec<Decimal> = results.iter().map(|r| r.net_invested).collect();
    invested.sort();
    println!("\n─── 净投入分布 ───");
    println!("  P10: {:.2}", invested[n / 10]);
    println!("  P25: {:.2}", invested[n / 4]);
    println!("  P50: {:.2}", invested[n / 2]);
    println!("  P75: {:.2}", invested[n * 3 / 4]);
    println!("  P90: {:.2}", invested[n * 9 / 10]);

    // 分析有正已实现的场
    let positive_realized = results
        .iter()
        .filter(|r| r.realized_pnl > Decimal::ZERO)
        .count();
    let negative_realized = results
        .iter()
        .filter(|r| r.realized_pnl < Decimal::ZERO)
        .count();
    let zero_realized = results
        .iter()
        .filter(|r| r.realized_pnl == Decimal::ZERO)
        .count();
    println!("\n─── 已实现盈亏方向 ───");
    println!(
        "  正: {} ({:.1}%)",
        positive_realized,
        positive_realized as f64 / n as f64 * 100.0
    );
    println!(
        "  负: {} ({:.1}%)",
        negative_realized,
        negative_realized as f64 / n as f64 * 100.0
    );
    println!(
        "  零: {} ({:.1}%)",
        zero_realized,
        zero_realized as f64 / n as f64 * 100.0
    );

    // 有正已实现的场，其结算盈亏如何？
    let pos_realized_settle: Vec<Decimal> = results
        .iter()
        .filter(|r| r.realized_pnl > Decimal::ZERO)
        .map(|r| r.settle_pnl)
        .collect();
    if !pos_realized_settle.is_empty() {
        let avg: Decimal =
            pos_realized_settle.iter().sum::<Decimal>() / Decimal::from(pos_realized_settle.len());
        println!("  正已实现的场，结算盈亏均值: {avg:.2}");
    }

    // 分析: 盈利场 vs 亏损场的成交笔数差异
    let win_fills: Vec<u32> = results
        .iter()
        .filter(|r| r.pnl > Decimal::ZERO)
        .map(|r| r.fills)
        .collect();
    let lose_fills: Vec<u32> = results
        .iter()
        .filter(|r| r.pnl <= Decimal::ZERO)
        .map(|r| r.fills)
        .collect();
    if !win_fills.is_empty() {
        println!("\n─── 盈利场 vs 亏损场 ───");
        println!(
            "  盈利场成交均笔: {:.0}",
            win_fills.iter().sum::<u32>() as f64 / win_fills.len() as f64
        );
        println!(
            "  亏损场成交均笔: {:.0}",
            lose_fills.iter().sum::<u32>() as f64 / lose_fills.len() as f64
        );
    }

    // 核心问题诊断：止损占已实现亏损的比例
    // 由于当前无法区分止盈/止损的已实现细分，用 proxy 判断：
    // 如果已实现<0，说明止损超过止盈
    let all_negative_realized: Decimal = results
        .iter()
        .filter(|r| r.realized_pnl < Decimal::ZERO)
        .map(|r| r.realized_pnl)
        .sum();
    let all_positive_realized: Decimal = results
        .iter()
        .filter(|r| r.realized_pnl > Decimal::ZERO)
        .map(|r| r.realized_pnl)
        .sum();
    println!("\n─── 止盈/止损 proxy ───");
    println!("  正已实现总和: {all_positive_realized:.2}");
    println!("  负已实现总和: {all_negative_realized:.2}");
    println!("  净: {:.2}", all_positive_realized + all_negative_realized);
}
