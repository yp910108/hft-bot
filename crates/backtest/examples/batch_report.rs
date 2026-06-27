//! 批量回测：跑 logs/ 全部真实市场，统计场均 PnL、胜率、终态分布。
//!
//! 运行：`cargo run -p backtest --example batch_report --release`

use backtest::real_data;
use backtest::run::{self, MatchResult};
use domain::fee::FeeModel;
use domain::types::Money;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::collections::BTreeMap;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "logs".to_string());
    let markets = match real_data::load_dir(&dir) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("加载 {dir} 失败: {e:?}");
            std::process::exit(1);
        }
    };

    let total_capital = dec!(1000);
    let fee = FeeModel::default();

    let mut results: Vec<MatchResult> = Vec::with_capacity(markets.len());
    for market in &markets {
        results.push(run::run_match(market, total_capital, fee));
    }

    let n = results.len() as i64;
    if n == 0 {
        eprintln!("没有可回测的场次");
        std::process::exit(1);
    }

    let total_pnl: Money = results.iter().map(|r| r.pnl).sum();
    let wins = results.iter().filter(|r| r.pnl > Money::ZERO).count();
    let avg = total_pnl / Money::from(n);

    let mut state_dist: BTreeMap<&str, u32> = BTreeMap::new();
    for r in &results {
        *state_dist.entry(r.deepest_phase).or_insert(0) += 1;
    }
    let avg_fills: f64 =
        results.iter().map(|r| r.fills as f64).sum::<f64>() / n as f64;

    println!("==== 批量回测结果 ====");
    println!("场数:        {n}");
    println!("总 PnL:      {:.2}", total_pnl.to_f64().unwrap_or(0.0));
    println!("场均 PnL:    {:.4}", avg.to_f64().unwrap_or(0.0));
    println!("胜率:        {:.1}%", wins as f64 / n as f64 * 100.0);
    println!("场均成交笔数: {avg_fills:.1}");
    println!("---- 曾达最深阶段分布 ----");
    for (state, count) in &state_dist {
        println!("  {state:16} {count}");
    }
}
