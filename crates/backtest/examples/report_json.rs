//! 逐场回测报告：跑 logs/ 全部真实市场，记录逐笔成交，产出 full_analysis.json，
//! 供 `analysis_data/render_rust_report.py` 渲染成 HTML 报告。
//!
//! 运行：`cargo run -p backtest --example report_json --release`
//! 默认输出 `analysis_data/output/rust_strategy/full_analysis.json`。

use backtest::real_data;
use backtest::run::{self, FillRecord};
use domain::fee::FeeModel;
use domain::types::{OrderRole, Side};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use serde_json::{json, Value};
use std::path::PathBuf;

fn f64v(x: rust_decimal::Decimal) -> f64 {
    x.to_f64().unwrap_or(0.0)
}

fn side_label(side: Side) -> &'static str {
    match side {
        Side::Up => "UP",
        Side::Down => "DN",
    }
}

/// 把一笔成交记录转成报告期望的 operation JSON。
fn op_json(r: &FillRecord, start_hms: &str) -> Value {
    let up_qty = f64v(r.up_qty);
    let up_cost = f64v(r.up_cost);
    let dn_qty = f64v(r.dn_qty);
    let dn_cost = f64v(r.dn_cost);
    let total_cost = up_cost + dn_cost;
    json!({
        "time": format!("{} +{:>3}s", start_hms, r.second),
        "direction": side_label(r.side),
        "side": "BUY",
        "role": match r.role { OrderRole::Maker => "Maker", OrderRole::Taker => "Taker" },
        "price": f64v(r.price),
        "size": f64v(r.size),
        "converted": false,
        "orig_dir": Value::Null,
        "orig_price": Value::Null,
        "up_qty": up_qty,
        "up_cost": up_cost,
        "dn_qty": dn_qty,
        "dn_cost": dn_cost,
        "total_cost": total_cost,
        "pnl_if_up_wins": up_qty - total_cost,
        "pnl_if_dn_wins": dn_qty - total_cost,
    })
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "logs".to_string());
    let out = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "analysis_data/output/rust_strategy/full_analysis.json".to_string());

    let markets = real_data::load_dir(&dir).expect("加载行情失败");
    println!("加载 {} 场，开始回测…", markets.len());

    let total_capital = dec!(1000);
    let fee = FeeModel::default();
    let mut report: Vec<Value> = Vec::with_capacity(markets.len());

    for market in &markets {
        let (result, records) = run::run_match_recorded(market, total_capital, fee);

        // 从标题尾部取 "HH:MM" 作为时间前缀（操作 time 字段展示用）。
        let start_hms = market
            .title
            .rsplit(' ')
            .next()
            .unwrap_or("00:00")
            .to_string();

        let last = records.last();
        let (up_qty, up_cost, dn_qty, dn_cost) = last
            .map(|r| (f64v(r.up_qty), f64v(r.up_cost), f64v(r.dn_qty), f64v(r.dn_cost)))
            .unwrap_or((0.0, 0.0, 0.0, 0.0));
        let total_cost = up_cost + dn_cost;
        let pnl = f64v(result.pnl);

        let operations: Vec<Value> = records.iter().map(|r| op_json(r, &start_hms)).collect();

        report.push(json!({
            "condition_id": market.title,
            "title": market.title,
            "source_file": market.source_file,
            "cycle_type": "15m",
            "winner": side_label(result.winner),
            "winner_idx": match result.winner { Side::Up => 0, Side::Down => 1 },
            "settled": true,
            "num_trades": result.fills,
            "operations": operations,
            "final": {
                "actual_pnl": pnl,
                "up_qty": up_qty,
                "up_cost": up_cost,
                "dn_qty": dn_qty,
                "dn_cost": dn_cost,
                "total_cost": total_cost,
                "pnl_if_up": up_qty - total_cost,
                "pnl_if_dn": dn_qty - total_cost,
                "deepest_phase": result.deepest_phase,
            },
        }));
    }

    let path = PathBuf::from(&out);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, serde_json::to_string(&report).expect("序列化失败"))
        .expect("写文件失败");

    let total: f64 = report
        .iter()
        .map(|m| m["final"]["actual_pnl"].as_f64().unwrap_or(0.0))
        .sum();
    println!("已写出 {} → {} 场，总 PnL {:.2}", out, report.len(), total);
    println!("渲染 HTML：python3 analysis_data/render_rust_report.py");
}
