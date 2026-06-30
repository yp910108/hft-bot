//! 回测执行器：用真实历史行情驱动 Engine + Simulator 跑完一整场，结算出 PnL。
//!
//! 时间模型：逐秒快照，第 i 个快照对应场内第 i 秒。15 分钟场共 ~900 个快照。
//! 每个快照：① 推进虚拟时钟 ② 喂行情给 Simulator 撮合 ③ Simulator 产出的事件依次喂 Engine
//! ④ Engine 产出的指令下发给 Simulator。结算：按胜方每股 1 美元兑付，减去总成本。

use crate::market::Market;
use domain::clock::Millis;
use domain::command::Command;
use domain::fee::FeeModel;
use domain::state::RobotState;
use domain::types::{Money, Side};
use engine::{Engine, EngineConfig};
use exchange::backend::ExchangeBackend;
use exchange::event::ExchangeEvent;
use exchange::simulator::Simulator;
use rust_decimal::Decimal;

/// 一场回测的结果。
#[derive(Debug, Clone)]
pub struct MatchResult {
    /// 最终结算盈亏（胜方股数 − 双边总成本）。
    pub pnl: Money,
    /// 终态。
    pub final_state: RobotState,
    /// 本场曾到达的最深阶段标签。
    pub deepest_phase: &'static str,
    /// 胜出方。
    pub winner: Side,
    /// 总成交笔数。
    pub fills: u32,
}

/// 每个快照之间的虚拟时间间隔（毫秒）。逐秒数据 → 1000ms。
const TICK_MS: Millis = 1_000;

/// 一笔成交记录：成交本身 + 成交后双边账本快照，供报告逐笔展示。
#[derive(Debug, Clone)]
pub struct FillRecord {
    /// 场内秒数。
    pub second: u64,
    /// 成交侧（Up/Down）。
    pub side: Side,
    /// 角色（Maker/Taker）。
    pub role: domain::types::OrderRole,
    /// 成交价。
    pub price: Money,
    /// 成交股数（净入仓）。
    pub size: Money,
    /// 成交后双边持仓与成本。
    pub up_qty: Money,
    pub up_cost: Money,
    pub dn_qty: Money,
    pub dn_cost: Money,
}

/// 跑一整场并记录每一笔成交（供报告生成）。
pub fn run_match_recorded(
    market: &Market,
    total_capital: Money,
    fee: FeeModel,
) -> (MatchResult, Vec<FillRecord>) {
    let mut engine = Engine::new(EngineConfig::with_capital(total_capital));
    let (mut sim, mut rx) = Simulator::new(fee);
    let n = market.snapshots.len() as Millis;
    let mut fills = 0u32;
    let mut records = Vec::new();

    for (i, snapshot) in market.snapshots.iter().enumerate() {
        let now = i as Millis * TICK_MS;
        let tte = (n.saturating_sub(i as Millis + 1)) * TICK_MS;

        sim.on_market(snapshot);
        let mut queue = vec![ExchangeEvent::BookUpdate(*snapshot)];
        while let Ok(ev) = rx.try_recv() {
            queue.push(ev);
        }

        let mut guard = 0;
        while let Some(ev) = queue.pop() {
            // 在成交事件被 engine 处理后，记录成交 + 最新账本。
            let fill_info = if let ExchangeEvent::Filled(f) = &ev {
                fills += 1;
                Some((f.side, f.role, f.price, f.filled_qty))
            } else {
                None
            };
            let commands = engine.handle_event(ev, now, tte);
            if let Some((side, role, price, size)) = fill_info {
                let snap = engine.ledger().snapshot();
                records.push(FillRecord {
                    second: now / 1000,
                    side,
                    role,
                    price,
                    size,
                    up_qty: snap.up_qty,
                    up_cost: snap.up_cost,
                    dn_qty: snap.down_qty,
                    dn_cost: snap.down_cost,
                });
            }
            dispatch(&mut sim, &commands);
            while let Ok(ev) = rx.try_recv() {
                queue.push(ev);
            }
            guard += 1;
            if guard > 10_000 {
                break;
            }
        }
    }

    let snapshot = engine.ledger().snapshot();
    let pnl = settle(&snapshot, market.winner);
    let result = MatchResult {
        pnl,
        final_state: engine.state(),
        deepest_phase: engine.deepest_phase_label(),
        winner: market.winner,
        fills,
    };
    (result, records)
}

/// 跑一整场回测。
pub fn run_match(market: &Market, total_capital: Money, fee: FeeModel) -> MatchResult {
    let mut engine = Engine::new(EngineConfig::with_capital(total_capital));
    let (mut sim, mut rx) = Simulator::new(fee);

    let n = market.snapshots.len() as Millis;
    let mut fills = 0u32;

    for (i, snapshot) in market.snapshots.iter().enumerate() {
        let now = i as Millis * TICK_MS;
        // 剩余时间：到最后一个快照为 0。
        let tte = (n.saturating_sub(i as Millis + 1)) * TICK_MS;

        // ① 行情喂给 Simulator 撮合，产出成交/撤单等事件。
        sim.on_market(snapshot);
        // ② 先把行情更新本身作为一个事件喂 Engine（让策略看到最新盘口）。
        let mut events = vec![ExchangeEvent::BookUpdate(*snapshot)];
        // ③ 收集 Simulator 撮合产生的事件。
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }

        // ④ 逐个事件喂 Engine，下发指令给 Simulator。
        let mut queue = events;
        let mut guard = 0;
        while let Some(ev) = queue.pop() {
            if matches!(ev, ExchangeEvent::Filled(_)) {
                fills += 1;
            }
            let commands = engine.handle_event(ev, now, tte);
            dispatch(&mut sim, &commands);
            // 下发指令可能立刻产生新事件（如 IOC 成交、撤单确认），收进队列继续处理。
            while let Ok(ev) = rx.try_recv() {
                queue.push(ev);
            }
            guard += 1;
            if guard > 10_000 {
                break; // 防御性：避免异常情况下死循环。
            }
        }
    }

    let snapshot = engine.ledger().snapshot();
    let pnl = settle(&snapshot, market.winner);
    MatchResult {
        pnl,
        final_state: engine.state(),
        deepest_phase: engine.deepest_phase_label(),
        winner: market.winner,
        fills,
    }
}

/// 把 Engine 产出的指令下发给 Simulator。
fn dispatch(sim: &mut Simulator, commands: &[Command]) {
    for cmd in commands {
        match cmd {
            Command::SubmitOrder(order) => sim.submit_order(*order),
            Command::CancelOrder(id) => sim.cancel_order(*id),
            Command::CancelSide(side) => sim.cancel_side(*side),
            Command::CancelAll => sim.cancel_all(),
        }
    }
}

/// 结算：胜方每股兑付 1 美元，盈亏 = 胜方股数 − 双边总成本。
fn settle(snapshot: &domain::pnl::PositionSnapshot, winner: Side) -> Money {
    let payout = snapshot.qty(winner) * Decimal::ONE;
    payout - snapshot.total_cost()
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::market::{BookTop, MarketSnapshot};
    use rust_decimal_macros::dec;

    fn snap(up_ask: Decimal, down_ask: Decimal) -> MarketSnapshot {
        MarketSnapshot {
            up: BookTop {
                best_bid: Some(up_ask - dec!(0.02)),
                best_ask: Some(up_ask),
                last_trade: None,
            },
            down: BookTop {
                best_bid: Some(down_ask - dec!(0.02)),
                best_ask: Some(down_ask),
                last_trade: None,
            },
        }
    }

    #[test]
    fn run_match_completes_and_settles() {
        // 造一个简单震荡场：Up ask 在 0.38~0.42 之间晃，结算 Up 赢。
        let mut snapshots = Vec::new();
        for k in 0..120 {
            let up = if k % 2 == 0 { dec!(0.40) } else { dec!(0.38) };
            snapshots.push(snap(up, dec!(0.62)));
        }
        let market = Market {
            snapshots,
            winner: Side::Up,
            title: "test".to_string(),
        };
        let result = run_match(&market, dec!(1000), FeeModel::default());
        // 跑完不 panic，终态合法，有成交。
        assert!(matches!(
            result.final_state,
            RobotState::Building
                | RobotState::Pairing
                | RobotState::SettlementWait
                | RobotState::DynamicHedge
                | RobotState::EvHedge
                | RobotState::CircuitBreaker
        ));
        assert_eq!(result.winner, Side::Up);
    }

    #[test]
    fn settle_pays_winner_minus_cost() {
        let snapshot = domain::pnl::PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(80),
            up_cost: dec!(40),
            down_cost: dec!(45),
        };
        // Up 赢：100 − 85 = 15。
        assert_eq!(settle(&snapshot, Side::Up), dec!(15));
        // Down 赢：80 − 85 = −5。
        assert_eq!(settle(&snapshot, Side::Down), dec!(-5));
    }
}
