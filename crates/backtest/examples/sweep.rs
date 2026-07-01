//! 参数扫描：跑多组 tp/sl + 库存约束组合，对比场均 PnL 和胜率。
//!
//! cargo run -p backtest --example sweep --release

use backtest::market::Market;
use backtest::real_data::load_dir;
use backtest::run::run_match;
use domain::fee::FeeModel;
use domain::types::{Price, Qty};
use engine::config::EngineConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

struct Params {
    tp: Price,
    sl: Price,
    inv_cap: Option<Qty>,
    imb_cap: Option<Qty>,
}

fn main() {
    let markets = load_dir("logs").expect("加载 logs/ 失败");
    println!("加载 {} 场\n", markets.len());

    let fee = FeeModel::default();

    // 无止损方向：强对称 + 压 sum_avg 扛结算。
    let combos = vec![
        Params { tp: dec!(0.03), sl: dec!(0.99), inv_cap: Some(dec!(20)), imb_cap: Some(dec!(10)) },
        Params { tp: dec!(0.03), sl: dec!(0.99), inv_cap: Some(dec!(30)), imb_cap: Some(dec!(5)) },
        Params { tp: dec!(0.03), sl: dec!(0.99), inv_cap: Some(dec!(50)), imb_cap: Some(dec!(5)) },
        Params { tp: dec!(0.02), sl: dec!(0.99), inv_cap: Some(dec!(50)), imb_cap: Some(dec!(5)) },
        Params { tp: dec!(0.01), sl: dec!(0.99), inv_cap: Some(dec!(50)), imb_cap: Some(dec!(5)) },
        Params { tp: dec!(0.05), sl: dec!(0.99), inv_cap: Some(dec!(50)), imb_cap: Some(dec!(5)) },
        // 更大囤货扛结算
        Params { tp: dec!(0.03), sl: dec!(0.99), inv_cap: Some(dec!(100)), imb_cap: Some(dec!(5)) },
        Params { tp: dec!(0.03), sl: dec!(0.99), inv_cap: Some(dec!(150)), imb_cap: Some(dec!(10)) },
    ];

    println!(
        "{:<6} {:<6} {:<8} {:<8} {:<12} {:<8} {:<12}",
        "tp", "sl", "inv_cap", "imb_cap", "场均PnL", "胜率%", "已实现PnL"
    );
    println!("{}", "─".repeat(64));

    for p in &combos {
        let (avg_pnl, win_rate, realized) = run_sweep(&markets, fee, p);
        println!(
            "{:<6} {:<6} {:<8} {:<8} {:<12.2} {:<8.1} {:<12.0}",
            p.tp,
            p.sl,
            p.inv_cap.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
            p.imb_cap.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
            avg_pnl,
            win_rate,
            realized
        );
    }
}

fn run_sweep(markets: &[Market], fee: FeeModel, p: &Params) -> (Decimal, f64, Decimal) {
    let mut cfg = EngineConfig::default();
    cfg.strategy.tp_by_quartile = [p.tp, p.tp, p.tp, p.tp];
    cfg.strategy.sl_by_quartile = [p.sl, p.sl, p.sl, p.sl];
    cfg.strategy.inventory_cap = p.inv_cap;
    cfg.strategy.imbalance_cap = p.imb_cap;

    let n = markets.len();
    let mut total_pnl = Decimal::ZERO;
    let mut total_realized = Decimal::ZERO;
    let mut wins = 0usize;

    for market in markets {
        let r = run_match(market, cfg.clone(), fee);
        total_pnl += r.pnl;
        total_realized += r.realized_pnl;
        if r.pnl > Decimal::ZERO {
            wins += 1;
        }
    }

    (
        total_pnl / Decimal::from(n),
        wins as f64 / n as f64 * 100.0,
        total_realized,
    )
}
