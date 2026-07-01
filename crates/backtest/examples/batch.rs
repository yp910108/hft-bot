//! 批量回测：跑 logs/ 下全部 933 场，汇总胜率/场均/盈亏归因/波动分组。
//!
//! 用法：cargo run -p backtest --example batch --release

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
    for (idx, market) in markets.iter().enumerate() {
        use std::io::Write;
        eprintln!("[{idx}] {} ({} 快照)", market.title, market.snapshots.len());
        std::io::stderr().flush().ok();
        let r = run_match(market, cfg.clone(), fee);
        results.push(r);
    }

    let n = results.len();
    let wins = results.iter().filter(|r| r.pnl > Decimal::ZERO).count();
    let total_pnl: Decimal = results.iter().map(|r| r.pnl).sum();
    let avg_pnl = total_pnl / Decimal::from(n);

    let win_pnls: Vec<Decimal> = results
        .iter()
        .filter(|r| r.pnl > Decimal::ZERO)
        .map(|r| r.pnl)
        .collect();
    let lose_pnls: Vec<Decimal> = results
        .iter()
        .filter(|r| r.pnl <= Decimal::ZERO)
        .map(|r| r.pnl)
        .collect();
    let avg_win = if !win_pnls.is_empty() {
        win_pnls.iter().sum::<Decimal>() / Decimal::from(win_pnls.len())
    } else {
        Decimal::ZERO
    };
    let avg_loss = if !lose_pnls.is_empty() {
        lose_pnls.iter().sum::<Decimal>() / Decimal::from(lose_pnls.len())
    } else {
        Decimal::ZERO
    };

    println!("\n═══════════════════════════════════════");
    println!("  批量回测结果 ({n} 场)");
    println!("═══════════════════════════════════════");
    println!("总 PnL:      {total_pnl:.2}");
    println!("场均 PnL:    {avg_pnl:.2}");
    println!(
        "胜率:        {wins}/{n} ({:.1}%)",
        wins as f64 / n as f64 * 100.0
    );
    println!("赢场均盈:    {avg_win:.2}");
    println!("亏场均亏:    {avg_loss:.2}");

    // 盈亏归因五类
    let mut cat_a = 0usize; // sum_avg >= 1.05
    let mut cat_b = 0usize; // 1.0 <= sum_avg < 1.05
    let mut cat_c = 0usize; // sum_avg < 1 但亏（持仓不对称）
    let mut cat_profit = 0usize;
    let mut cat_low_vol = 0usize; // 净持仓为 0（全平了，低波动或趋势）

    for r in &results {
        if r.pnl > Decimal::ZERO {
            cat_profit += 1;
        } else if r.up_net_qty == Decimal::ZERO && r.dn_net_qty == Decimal::ZERO {
            cat_low_vol += 1;
        } else if r.sum_avg >= dec!(1.05) {
            cat_a += 1;
        } else if r.sum_avg >= dec!(1.0) {
            cat_b += 1;
        } else {
            cat_c += 1;
        }
    }

    println!("\n─── 盈亏归因 ───");
    println!("盈利:              {cat_profit}");
    println!("亏损A(sum_avg≥1.05): {cat_a}");
    println!("亏损B(1.0~1.05):    {cat_b}");
    println!("亏损C(不对称):      {cat_c}");
    println!("全平仓(趋势/低波动): {cat_low_vol}");

    // 已实现 vs 结算 拆解
    let total_realized: Decimal = results.iter().map(|r| r.realized_pnl).sum();
    let total_settle: Decimal = results.iter().map(|r| r.settle_pnl).sum();
    let avg_fills: f64 = results.iter().map(|r| r.fills as f64).sum::<f64>() / n as f64;

    println!("\n─── 收益拆解 ───");
    println!("总已实现盈亏:  {total_realized:.2}");
    println!("总结算盈亏:    {total_settle:.2}");
    println!("场均成交笔数:  {avg_fills:.1}");

    // 按成交数分组
    let high_fill: Vec<&MatchResult> = results.iter().filter(|r| r.fills >= 100).collect();
    let low_fill: Vec<&MatchResult> = results.iter().filter(|r| r.fills < 50).collect();
    if !high_fill.is_empty() {
        let hf_pnl: Decimal =
            high_fill.iter().map(|r| r.pnl).sum::<Decimal>() / Decimal::from(high_fill.len());
        println!(
            "\n成交≥100 场: {} 场, 场均 PnL: {hf_pnl:.2}",
            high_fill.len()
        );
    }
    if !low_fill.is_empty() {
        let lf_pnl: Decimal =
            low_fill.iter().map(|r| r.pnl).sum::<Decimal>() / Decimal::from(low_fill.len());
        println!("成交<50 场:  {} 场, 场均 PnL: {lf_pnl:.2}", low_fill.len());
    }
}
