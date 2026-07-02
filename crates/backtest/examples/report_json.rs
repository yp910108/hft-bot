//! 逐场回测报告：跑 logs/ 全部真实市场，记录逐笔成交，产出 full_analysis.json，
//! 供 `analysis_data/render_rust_report.py` 渲染成 HTML 报告。
//!
//! 运行：`cargo run -p backtest --example report_json --release`
//! 默认输出 `analysis_data/output/rust_strategy/full_analysis.json`。
//!
//! 展示层转换：真实 SELL X@p 在报告里记为 BUY 对面@(1-p)，标 converted，
//! 与分析真实玩家（0xe00740bc）的报告口径一致。转换后 `qty - total_cost` 恒等于真实 PnL。

use backtest::real_data;
use backtest::run::{self, FillRecord};
use domain::fee::FeeModel;
use domain::order::OrderDirection;
use domain::types::Side;
use engine::config::EngineConfig;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};

fn f64v(x: Decimal) -> f64 {
    x.to_f64().unwrap_or(0.0)
}

fn side_label(side: Side) -> &'static str {
    match side {
        Side::Up => "UP",
        Side::Down => "DN",
    }
}

/// 展示层累计持仓：卖出 = 买对面，两侧只增不减，与 0xe00740bc 报告口径一致。
#[derive(Default)]
struct DisplayPosition {
    up_qty: f64,
    up_cost: f64,
    dn_qty: f64,
    dn_cost: f64,
}

/// 把一笔真实成交转成报告 operation JSON。
///
/// 把一笔真实成交转成报告 operation JSON。
///
/// 分两层，各取所需：
/// - 持仓/成本/均价列：展示层口径（卖 X@p = 买对面@(1-p)，对面增长、本侧不动），可读性好。
/// - 盈亏列 pnl_if_*：引擎真值（已实现盈亏 + 该侧真实净持仓结算），真金白银不含糊。
fn op_json(r: &FillRecord, start_hms: &str, dp: &mut DisplayPosition) -> Value {
    let (disp_side, disp_price, converted, orig_dir, orig_price) = match r.direction {
        OrderDirection::Buy => (r.side, r.price, false, Value::Null, Value::Null),
        OrderDirection::Sell => (
            r.side.opposite(),
            Decimal::ONE - r.price,
            true,
            json!(side_label(r.side)),
            json!(f64v(r.price)),
        ),
    };

    // 展示层口径：所有操作都是"买入 disp_side"，累加该侧的持仓和成本，本侧不动。
    let size = f64v(r.size);
    let price = f64v(disp_price);
    match disp_side {
        Side::Up => {
            dp.up_qty += size;
            dp.up_cost += price * size;
        }
        Side::Down => {
            dp.dn_qty += size;
            dp.dn_cost += price * size;
        }
    }

    let total_cost = dp.up_cost + dp.dn_cost;

    // 盈亏用引擎真值：该时点若结算 = 已实现 + 真实净持仓×1 − 真实净持仓成本。
    let real_total_cost = f64v(r.up_cost + r.dn_cost);
    let realized = f64v(r.realized);
    let pnl_if_up = realized + f64v(r.up_qty) - real_total_cost;
    let pnl_if_dn = realized + f64v(r.dn_qty) - real_total_cost;

    json!({
        "time": format!("{} +{:>3}s", start_hms, r.second),
        "direction": side_label(disp_side),
        "side": "BUY",
        "role": match r.role { domain::types::OrderRole::Maker => "Maker", domain::types::OrderRole::Taker => "Taker" },
        "price": price,
        "size": size,
        "converted": converted,
        "orig_dir": orig_dir,
        "orig_price": orig_price,
        "up_qty": dp.up_qty,
        "up_cost": dp.up_cost,
        "dn_qty": dp.dn_qty,
        "dn_cost": dp.dn_cost,
        "total_cost": total_cost,
        "pnl_if_up_wins": pnl_if_up,
        "pnl_if_dn_wins": pnl_if_dn,
    })
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "logs".to_string());
    let out = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "analysis_data/output/rust_strategy/full_analysis.json".to_string());

    let markets = real_data::load_dir(&dir).expect("加载行情失败");
    println!("加载 {} 场，开始回测…", markets.len());

    let fee = FeeModel::default();
    let cfg = EngineConfig::default();
    let mut report: Vec<Value> = Vec::with_capacity(markets.len());

    for market in &markets {
        let (result, records) = run::run_match_recorded(market, cfg.clone(), fee);

        // 从标题尾部取 "HH:MM" 作时间前缀。
        let start_hms = market.title.rsplit(' ').next().unwrap_or("00:00").to_string();

        // 展示层累计：卖出=买对面，两侧只增不减。
        let mut dp = DisplayPosition::default();
        let operations: Vec<Value> = records.iter().map(|r| op_json(r, &start_hms, &mut dp)).collect();

        // final：持仓/成本用展示层累计值（与末条 operation 一致）；
        // 盈亏用引擎真值（result.pnl 是权威真实盈亏）。
        let total_cost = dp.up_cost + dp.dn_cost;
        let real_total_cost = f64v(result.up_net_cost + result.dn_net_cost);
        let realized = f64v(result.realized_pnl);

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
                "actual_pnl": f64v(result.pnl),
                "up_qty": dp.up_qty,
                "up_cost": dp.up_cost,
                "dn_qty": dp.dn_qty,
                "dn_cost": dp.dn_cost,
                "total_cost": total_cost,
                "pnl_if_up": realized + f64v(result.up_net_qty) - real_total_cost,
                "pnl_if_dn": realized + f64v(result.dn_net_qty) - real_total_cost,
                "deepest_phase": format!("{:?}", result.final_phase),
            },
        }));
    }

    let out_path = std::path::Path::new(&out);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).expect("创建输出目录失败");
    }
    let json_str = serde_json::to_string(&report).expect("序列化失败");
    std::fs::write(out_path, json_str).expect("写文件失败");

    let total: f64 = report
        .iter()
        .map(|m| m["final"]["actual_pnl"].as_f64().unwrap_or(0.0))
        .sum();
    println!("已写出 {} → {} 场，总 PnL {:.2}", out, report.len(), total);
    println!("渲染 HTML：python3 analysis_data/render_rust_report.py");
}
