//! 一次性诊断：跑第 0 场，打印每次状态跳转和每次 decide 返回非 Skip 的情况。

use backtest::real_data;
use domain::command::Command;
use domain::fee::FeeModel;
use engine::{Engine, EngineConfig};
use exchange::backend::ExchangeBackend;
use exchange::event::ExchangeEvent;
use exchange::simulator::Simulator;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;

fn main() {
    let idx: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let markets = real_data::load_dir("logs").expect("load");
    let m = &markets[idx];
    let n = m.snapshots.len() as u64;

    let mut engine = Engine::new(EngineConfig::with_capital(dec!(1000)));
    let (mut sim, mut rx) = Simulator::new(FeeModel::default());

    let mut last_state = engine.state();
    let mut last_up_qty = dec!(0);
    let mut last_dn_qty = dec!(0);

    for (i, snap) in m.snapshots.iter().enumerate() {
        let now = i as u64 * 1000;
        let tte = (n.saturating_sub(i as u64 + 1)) * 1000;
        sim.on_market(snap);
        let mut queue = vec![ExchangeEvent::BookUpdate(*snap)];
        while let Ok(e) = rx.try_recv() {
            queue.push(e);
        }
        let mut guard = 0;
        while let Some(ev) = queue.pop() {
            match &ev {
                ExchangeEvent::Filled(f) => {
                    format!("Fill {:?}@{} qty{}", f.side, f.price, f.filled_qty)
                }
                ExchangeEvent::BookUpdate(_) => "Book".into(),
                ExchangeEvent::Canceled(id) => format!("Canceled({})", id.0),
                ExchangeEvent::CancelFailed(id) => format!("CancelFail({})", id.0),
                ExchangeEvent::Rejected { order_id, reason } => {
                    format!("Rejected({},{:?})", order_id.0, reason)
                }
            };
            let cmds = engine.handle_event(ev, now, tte);
            let state = engine.state();
            let snap = engine.ledger().snapshot();

            // 打印状态跳转
            if state != last_state {
                println!(
                    "{:>4}s STATE {:?} -> {:?}  UP={:.1} DN={:.1}",
                    now / 1000,
                    last_state,
                    state,
                    snap.up_qty.to_f64().unwrap(),
                    snap.down_qty.to_f64().unwrap(),
                );
                last_state = state;
            }

            // 打印有 Submit 的指令
            for cmd in &cmds {
                if let Command::SubmitOrder(o) = cmd {
                    println!(
                        "{:>4}s  SUBMIT {:?} {:?} {}@{} {:?}",
                        now / 1000,
                        o.side,
                        o.direction,
                        o.qty,
                        o.price,
                        o.role,
                    );
                }
                if let Command::CancelAll = cmd {
                    println!("{:>4}s  CANCEL_ALL", now / 1000);
                }
            }

            // 打印持仓变化
            if snap.up_qty != last_up_qty || snap.down_qty != last_dn_qty {
                last_up_qty = snap.up_qty;
                last_dn_qty = snap.down_qty;
            }

            dispatch(&mut sim, &cmds);
            while let Ok(e) = rx.try_recv() {
                queue.push(e);
            }
            guard += 1;
            if guard > 10000 {
                break;
            }
        }
    }
    let snap = engine.ledger().snapshot();
    println!(
        "\n终态 {:?} | UP {:.1}@cost{:.2} DN {:.1}@cost{:.2} | total_cost {:.2}",
        engine.state(),
        snap.up_qty.to_f64().unwrap(),
        snap.up_cost.to_f64().unwrap(),
        snap.down_qty.to_f64().unwrap(),
        snap.down_cost.to_f64().unwrap(),
        snap.total_cost().to_f64().unwrap(),
    );
}

fn dispatch(sim: &mut Simulator, cmds: &[Command]) {
    for c in cmds {
        match c {
            Command::SubmitOrder(o) => sim.submit_order(*o),
            Command::CancelOrder(id) => sim.cancel_order(*id),
            Command::CancelSide(s) => sim.cancel_side(*s),
            Command::CancelAll => sim.cancel_all(),
        }
    }
}
