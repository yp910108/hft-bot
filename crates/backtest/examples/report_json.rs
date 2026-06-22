//! 逐场回测报告生成器：跑 logs/ 全部真实市场,记录逐笔成交,产出 JSON 文件,
//! 格式对齐 `analysis_data/run_analysis.py` 的 `full_analysis.json`,供 Python 渲染 HTML。
//!
//! 运行：`cargo run -p backtest --example report_json --release`

use backtest::real_data;
use domain::fee::FeeModel;
use domain::order::{Command, OrderConstraints};
use domain::types::Side;
use engine::{Engine, EngineConfig};
use exchange::backend::ExchangeBackend;
use exchange::event::ExchangeEvent;
use exchange::simulator::Simulator;
use fsm::Thresholds;
use risk::auditor::RiskAuditor;
use risk::pool::CapitalPools;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use strategy::GradientLadder;
use tokio::sync::mpsc::UnboundedReceiver;

use std::fs;
use std::path::Path;

/// 一笔成交的快照记录。
struct OpRecord {
    time: String,
    direction: &'static str,
    price: f64,
    size: f64,
    up_qty: f64,
    up_cost: f64,
    dn_qty: f64,
    dn_cost: f64,
    total_cost: f64,
    pnl_if_up_wins: f64,
    pnl_if_dn_wins: f64,
}

impl OpRecord {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "time": self.time,
            "direction": self.direction,
            "side": "BUY",
            "price": self.price,
            "size": self.size,
            "converted": false,
            "orig_dir": null,
            "orig_price": null,
            "up_qty": self.up_qty,
            "up_cost": self.up_cost,
            "dn_qty": self.dn_qty,
            "dn_cost": self.dn_cost,
            "total_cost": self.total_cost,
            "pnl_if_up_wins": self.pnl_if_up_wins,
            "pnl_if_dn_wins": self.pnl_if_dn_wins,
        })
    }
}

fn decimal_to_f64(d: Decimal) -> f64 {
    d.to_string().parse::<f64>().unwrap_or(0.0)
}

fn make_config(total_capital: Decimal) -> (EngineConfig, Thresholds) {
    let pools = CapitalPools::with_default_ratios(total_capital);
    let thresholds = Thresholds {
        hedge_loss_trigger: dec!(30),
        hedge_safety_price: dec!(0.5),
        profit_target: dec!(15),
    };
    let config = EngineConfig {
        grid_maker_pool: pools.grid_maker(),
        hedge_attack_pool: pools.hedge_attack(),
        hedge_step_fraction: dec!(0.2),
        max_taker_steps: 5,
        constraints: OrderConstraints::default(),
    };
    (config, thresholds)
}

/// 从 ledger 读取单边成本(qty × average_price)。
fn side_cost(engine: &Engine, side: Side) -> f64 {
    let ledger = engine.ledger();
    let qty = ledger.qty(side);
    let avg = ledger.average_price(side);
    decimal_to_f64(qty * avg)
}

/// 跑一场回测,记录逐笔成交,返回 JSON Value。
fn run_with_log(
    market: &backtest::market::SyntheticMarket,
    title: String,
    condition_id: String,
    total_capital: Decimal,
    fee_model: FeeModel,
) -> serde_json::Value {
    let pools = CapitalPools::with_default_ratios(total_capital);
    let (config, thresholds) = make_config(total_capital);
    let mut engine = Engine::new(
        total_capital,
        thresholds,
        GradientLadder::with_default_config(),
        RiskAuditor::with_default_guard(pools),
        config,
    );
    let (mut simulator, mut events) = Simulator::new(fee_model);
    let mut ops: Vec<OpRecord> = Vec::new();
    let mut tick: usize = 0;

    let mut snapshots = market.snapshots.iter();

    // 首帧初始化。
    if let Some(first) = snapshots.next() {
        dispatch(
            &mut simulator,
            engine.handle_event(ExchangeEvent::BookUpdate(*first)),
        );
        dispatch(&mut simulator, engine.start());
        drain_and_record(&mut simulator, &mut engine, &mut events, &mut ops, tick);
        tick += 1;
    }

    // 逐帧推进。
    for snapshot in snapshots {
        dispatch(
            &mut simulator,
            engine.handle_event(ExchangeEvent::BookUpdate(*snapshot)),
        );
        simulator.on_market(snapshot);
        drain_and_record(&mut simulator, &mut engine, &mut events, &mut ops, tick);
        tick += 1;
    }

    // 交割。
    let up_qty = decimal_to_f64(engine.ledger().qty(Side::Up));
    let up_cost = side_cost(&engine, Side::Up);
    let dn_qty = decimal_to_f64(engine.ledger().qty(Side::Down));
    let dn_cost = side_cost(&engine, Side::Down);
    let total_cost = up_cost + dn_cost;
    let pnl_if_up = up_qty - total_cost;
    let pnl_if_dn = dn_qty - total_cost;
    let (winner_str, winner_idx) = match market.winner {
        Side::Up => ("UP", 0u8),
        Side::Down => ("DN", 1u8),
    };
    let actual_pnl = if market.winner == Side::Up {
        pnl_if_up
    } else {
        pnl_if_dn
    };

    let ops_json: Vec<serde_json::Value> = ops.iter().map(|op| op.to_json()).collect();

    json!({
        "condition_id": condition_id,
        "title": title,
        "cycle_type": "15m",
        "winner": winner_str,
        "winner_idx": winner_idx,
        "settled": true,
        "num_trades": ops_json.len(),
        "operations": ops_json,
        "final": {
            "up_qty": up_qty,
            "up_cost": up_cost,
            "dn_qty": dn_qty,
            "dn_cost": dn_cost,
            "total_cost": total_cost,
            "pnl_if_up": pnl_if_up,
            "pnl_if_dn": pnl_if_dn,
            "actual_pnl": actual_pnl,
        }
    })
}

fn drain_and_record(
    simulator: &mut Simulator,
    engine: &mut Engine,
    events: &mut UnboundedReceiver<ExchangeEvent>,
    ops: &mut Vec<OpRecord>,
    tick: usize,
) {
    while let Ok(event) = events.try_recv() {
        if let ExchangeEvent::Filled(ref fill) = event {
            let direction = match fill.side {
                Side::Up => "UP",
                Side::Down => "DN",
            };
            let price = decimal_to_f64(fill.price);
            let size = decimal_to_f64(fill.filled_qty);
            let time = format!("T+{:04}s", tick);

            // 喂给 engine(更新 ledger)。
            let commands = engine.handle_event(event);

            // 从更新后的 ledger 读持仓。
            let up_qty = decimal_to_f64(engine.ledger().qty(Side::Up));
            let up_cost = side_cost(engine, Side::Up);
            let dn_qty = decimal_to_f64(engine.ledger().qty(Side::Down));
            let dn_cost = side_cost(engine, Side::Down);
            let total_cost = up_cost + dn_cost;

            ops.push(OpRecord {
                time,
                direction,
                price,
                size,
                up_qty,
                up_cost,
                dn_qty,
                dn_cost,
                total_cost,
                pnl_if_up_wins: up_qty - total_cost,
                pnl_if_dn_wins: dn_qty - total_cost,
            });

            dispatch(simulator, commands);
        } else {
            let commands = engine.handle_event(event);
            dispatch(simulator, commands);
        }
    }
}

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

fn main() {
    let total_capital = dec!(1000);
    let fee_model = FeeModel::default(); // Taker 4%, Maker 0%

    // 遍历 logs/ 全部日期目录。
    let logs_dir = Path::new("logs");
    let mut all_files: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(dates) = fs::read_dir(logs_dir) {
        let mut date_dirs: Vec<_> = dates
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        date_dirs.sort();
        for dir in date_dirs {
            if let Ok(entries) = fs::read_dir(&dir) {
                let mut files: Vec<_> = entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|ext| ext == "csv"))
                    .collect();
                files.sort();
                all_files.extend(files);
            }
        }
    }

    println!("找到 {} 个 CSV 文件", all_files.len());

    let mut records: Vec<serde_json::Value> = Vec::new();
    let mut errors = 0usize;

    for (i, path) in all_files.iter().enumerate() {
        let market = match real_data::load_market(path) {
            Ok(m) => m,
            Err(_) => {
                errors += 1;
                continue;
            }
        };

        // 从文件名构造 title 和 condition_id。
        let filename = path.file_stem().unwrap_or_default().to_string_lossy();
        let date_dir = path
            .parent()
            .and_then(|p| p.file_name())
            .unwrap_or_default()
            .to_string_lossy();
        let title = format!("BTC 15m - {} {}", date_dir, filename.replace('_', ":"));
        let condition_id = format!("rust_{}", filename);

        let record = run_with_log(&market, title, condition_id, total_capital, fee_model);
        records.push(record);

        if (i + 1) % 100 == 0 {
            println!("  已完成 {}/{}", i + 1, all_files.len());
        }
    }

    // 输出 JSON。
    let output_dir = Path::new("analysis_data/output/rust_strategy");
    fs::create_dir_all(output_dir).expect("创建输出目录失败");
    let json_path = output_dir.join("full_analysis.json");
    let json = serde_json::to_string_pretty(&records).expect("JSON 序列化失败");
    fs::write(&json_path, &json).expect("写入 JSON 失败");

    // 统计摘要。
    let n = records.len();
    let total_pnl: f64 = records
        .iter()
        .map(|r| r["final"]["actual_pnl"].as_f64().unwrap_or(0.0))
        .sum();
    let wins = records
        .iter()
        .filter(|r| r["final"]["actual_pnl"].as_f64().unwrap_or(0.0) > 0.0)
        .count();
    println!("\n完成! {} 场回测 (跳过 {} 个错误文件)", n, errors);
    println!(
        "总 PnL: {:+.1}, 胜率: {}/{} ({:.1}%), 场均: {:+.2}",
        total_pnl,
        wins,
        n,
        wins as f64 / n as f64 * 100.0,
        total_pnl / n as f64
    );
    println!("JSON 已写入: {}", json_path.display());
}
