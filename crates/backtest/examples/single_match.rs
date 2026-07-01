//! 单场回测快速验证：跑一场真实数据，打印结果。
//!
//! 用法：cargo run -p backtest --example single_match -- logs/2026-05-22/07_30_508877.csv

use backtest::real_data::load_market;
use backtest::run::run_match;
use domain::fee::FeeModel;
use engine::config::EngineConfig;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "logs/2026-05-22/07_30_508877.csv".to_string());

    let market = match load_market(&path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("加载失败: {e:?}");
            std::process::exit(1);
        }
    };

    println!("场: {}", market.title);
    println!(
        "快照数: {}, 胜方: {:?}",
        market.snapshots.len(),
        market.winner
    );

    let result = run_match(&market, EngineConfig::default(), FeeModel::default());

    println!("─────────────────────────");
    println!("总 PnL:     {}", result.pnl);
    println!("已实现盈亏:  {}", result.realized_pnl);
    println!("结算盈亏:    {}", result.settle_pnl);
    println!("净投入:      {}", result.net_invested);
    println!("成交笔数:    {}", result.fills);
    println!("终态:        {:?}", result.final_phase);
    println!("UP 净持仓:   {}", result.up_net_qty);
    println!("DN 净持仓:   {}", result.dn_net_qty);
    println!("sum_avg:     {}", result.sum_avg);
}
