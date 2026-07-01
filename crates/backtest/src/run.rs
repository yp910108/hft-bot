//! 回测执行器：用真实历史行情驱动 Engine + Simulator 跑完一整场，结算出 PnL。
//!
//! 时间模型：逐秒快照，第 i 个快照对应场内第 i 秒。15 分钟场共 ~900 个快照。
//! 每个快照：① 喂行情给 Simulator 撮合 ② Simulator 产出的事件依次喂 Engine
//! ③ Engine 产出的指令下发给 Simulator ④ 推进虚拟时钟。
//! 结算：已实现盈亏 + 赢家侧净持仓 − 净投入。

use crate::market::Market;
use domain::clock::Millis;
use domain::command::Command;
use domain::fee::FeeModel;
use domain::phase::Phase;
use domain::types::{Money, Side};
use engine::Engine;
use engine::config::EngineConfig;
use exchange::backend::ExchangeBackend;
use exchange::event::ExchangeEvent;
use exchange::simulator::Simulator;
use rust_decimal::Decimal;

/// 一场回测的结果。
#[derive(Debug, Clone)]
pub struct MatchResult {
    /// 最终结算盈亏 = 已实现盈亏 + 结算盈亏。
    pub pnl: Money,
    /// 已实现盈亏（场内循环卖出利润）。
    pub realized_pnl: Money,
    /// 结算盈亏（净持仓部分）。
    pub settle_pnl: Money,
    /// 净投入。
    pub net_invested: Money,
    /// 终态阶段。
    pub final_phase: Phase,
    /// 胜出方。
    pub winner: Side,
    /// 总成交笔数。
    pub fills: u32,
    /// UP 侧净持仓。
    pub up_net_qty: Money,
    /// DN 侧净持仓。
    pub dn_net_qty: Money,
    /// 最终 sum_avg。
    pub sum_avg: Money,
}

/// 每个快照之间的虚拟时间间隔（毫秒）。逐秒数据 → 1000ms。
const TICK_MS: Millis = 1_000;

/// 跑一整场回测。
pub fn run_match(market: &Market, cfg: EngineConfig, fee: FeeModel) -> MatchResult {
    let mut engine = Engine::new(cfg);
    let (mut sim, mut rx) = Simulator::new(fee);
    let n = market.snapshots.len() as u64;
    let mut fills = 0u32;

    for (i, snapshot) in market.snapshots.iter().enumerate() {
        let now: Millis = (i as u64 + 1) * TICK_MS;
        let tte: Millis = n.saturating_sub(i as u64 + 1) * TICK_MS;

        // ① 喂行情给 Simulator，驱动挂单撮合。
        sim.on_market(snapshot);

        // ② drain 事件 → engine 入账 → 收集指令（不立刻 dispatch，防止自我喂食循环）。
        let mut pending_cmds: Vec<Command> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if matches!(&event, ExchangeEvent::Filled(_)) {
                fills += 1;
            }
            let cmds = engine.handle_event(&event, now, tte);
            pending_cmds.extend(cmds);
        }
        // 统一 dispatch（可能产出新事件，但留给下一步处理）。
        for cmd in pending_cmds.drain(..) {
            dispatch(&mut sim, &cmd);
        }

        // ③ BookUpdate 事件喂 Engine，产出决策指令。
        let cmds = engine.handle_event(&ExchangeEvent::BookUpdate(*snapshot), now, tte);
        for cmd in &cmds {
            dispatch(&mut sim, cmd);
        }

        // ④ 处理 ②③ dispatch 产出的新事件（只入账收集指令，再统一 dispatch 一轮）。
        while let Ok(event) = rx.try_recv() {
            if matches!(&event, ExchangeEvent::Filled(_)) {
                fills += 1;
            }
            let cmds = engine.handle_event(&event, now, tte);
            pending_cmds.extend(cmds);
        }
        for cmd in pending_cmds {
            dispatch(&mut sim, &cmd);
        }
    }

    // 结算。
    let inv = engine.inventory();
    let snapshot = inv.snapshot();
    let realized_pnl = inv.realized_pnl();
    let settle = snapshot.settle_pnl(market.winner);
    let pnl = realized_pnl + settle;

    MatchResult {
        pnl,
        realized_pnl,
        settle_pnl: settle,
        net_invested: inv.net_invested(),
        final_phase: engine.phase(),
        winner: market.winner,
        fills,
        up_net_qty: snapshot.up_qty,
        dn_net_qty: snapshot.down_qty,
        sum_avg: if snapshot.up_qty > Decimal::ZERO && snapshot.down_qty > Decimal::ZERO {
            inv.sum_avg()
        } else {
            Decimal::ZERO
        },
    }
}

/// 下发指令给 Simulator。
fn dispatch(sim: &mut Simulator, cmd: &Command) {
    match cmd {
        Command::SubmitOrder(order) => sim.submit_order(*order),
        Command::CancelOrder(id) => sim.cancel_order(*id),
        Command::CancelSide(side) => sim.cancel_side(*side),
        Command::CancelAll => sim.cancel_all(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::market::{BookTop, MarketSnapshot};
    use rust_decimal_macros::dec;

    /// 构造一场简单的震荡市：价格在 0.45~0.55 间来回。
    fn oscillating_market() -> Market {
        let mut snapshots = Vec::new();
        for i in 0..900 {
            // 简单正弦波模拟震荡。
            let phase = (i % 60) as f64 / 60.0 * std::f64::consts::TAU;
            let up_mid = 0.50 + 0.05 * phase.sin();
            let up_bid = Decimal::from_f64_retain(up_mid - 0.005)
                .unwrap()
                .round_dp(2);
            let up_ask = Decimal::from_f64_retain(up_mid + 0.005)
                .unwrap()
                .round_dp(2);
            let dn_bid = (Decimal::ONE - up_ask).max(dec!(0.01));
            let dn_ask = (Decimal::ONE - up_bid).max(dec!(0.02));

            snapshots.push(MarketSnapshot {
                up: BookTop {
                    best_bid: Some(up_bid),
                    best_ask: Some(up_ask),
                    last_trade: None,
                },
                down: BookTop {
                    best_bid: Some(dn_bid),
                    best_ask: Some(dn_ask),
                    last_trade: None,
                },
            });
        }
        Market {
            snapshots,
            winner: Side::Up,
            title: "test oscillating".to_string(),
            source_file: "test".to_string(),
        }
    }

    #[test]
    fn run_match_completes_without_panic() {
        let market = oscillating_market();
        let result = run_match(&market, EngineConfig::default(), FeeModel::zero());
        // 应有成交。
        assert!(
            result.fills > 0,
            "震荡市应有成交，实际 fills={}",
            result.fills
        );
    }

    #[test]
    fn pnl_equals_realized_plus_settle() {
        let market = oscillating_market();
        let result = run_match(&market, EngineConfig::default(), FeeModel::zero());
        let expected = result.realized_pnl + result.settle_pnl;
        assert_eq!(result.pnl, expected);
    }

    #[test]
    fn oscillating_market_has_positive_realized_pnl() {
        let market = oscillating_market();
        let result = run_match(&market, EngineConfig::default(), FeeModel::zero());
        // 震荡市循环做市应有正的已实现盈亏。
        assert!(
            result.realized_pnl > Decimal::ZERO,
            "震荡市 realized_pnl 应>0，实际={}",
            result.realized_pnl
        );
    }
}
