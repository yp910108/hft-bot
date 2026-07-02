//! Engine：单写者事件循环的核心编排者。
//!
//! 持有全部可变状态，串行处理事件。纯函数策略只算决策，副作用全在这里。
//! handle_event 是同步纯逻辑（回测和实盘共用）。

mod book;
pub mod config;

use book::OrderBook;
use config::EngineConfig;
use domain::clock::Millis;
use domain::command::Command;
use domain::market::MarketSnapshot;
use domain::order::{Generation, Order, OrderDirection, OrderIdGenerator, TimeInForce};
use domain::phase::Phase;
use domain::types::{Money, OrderRole, Side};
use exchange::event::ExchangeEvent;
use inventory::Inventory;
use rust_decimal::Decimal;
use strategy::{CommandIntent, Decision, DecisionContext, Trigger};

/// 单写者引擎。
pub struct Engine {
    cfg: EngineConfig,

    // 可变状态
    phase: Phase,
    inventory: Inventory,
    book: OrderBook,
    id_gen: OrderIdGenerator,
    generation: Generation,
    market: MarketSnapshot,

    now: Millis,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        Self {
            cfg,
            phase: Phase::Building,
            inventory: Inventory::new(),
            book: OrderBook::new(),
            id_gen: OrderIdGenerator::new(),
            generation: Generation::new(),
            market: MarketSnapshot::default(),
            now: 0,
        }
    }

    /// 主入口：处理一个事件，返回要下发的指令。
    pub fn handle_event(&mut self, event: &ExchangeEvent, now: Millis) -> Vec<Command> {
        self.now = now;

        // 终态不再处理。
        if self.phase.is_terminal() {
            return vec![];
        }

        let (trigger, mut extra_cmds) = self.apply_event_facts(event);
        let decision = self.decide(trigger);
        let mut cmds = self.apply_decision(decision);
        cmds.append(&mut extra_cmds);
        cmds
    }

    /// 当前阶段。
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// 逐笔账本只读引用（回测结算用）。
    pub fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    // ─── ① 更新事件事实 ───

    fn apply_event_facts(&mut self, event: &ExchangeEvent) -> (Trigger, Vec<Command>) {
        match event {
            ExchangeEvent::BookUpdate(snapshot) => {
                self.market = *snapshot;
                (Trigger::BookUpdate, vec![])
            }
            ExchangeEvent::Filled(fill) => {
                match fill.direction {
                    OrderDirection::Buy => {
                        let buy_price = fill.cash / fill.filled_qty;
                        let lot_id = self.inventory.open_lot(
                            fill.side,
                            buy_price,
                            fill.filled_qty,
                            fill.cash,
                            self.now,
                        );
                        self.book.apply_fill(fill.order_id, fill.filled_qty);
                        (Trigger::Filled {
                            side: fill.side,
                            direction: OrderDirection::Buy,
                            lot_id: Some(lot_id),
                        }, vec![])
                    }
                    OrderDirection::Sell => {
                        let lot_id = self.book.lot_id_for(fill.order_id);
                        let mut cancels = vec![];
                        if let Some(lid) = lot_id
                            && self.inventory.close_lot(fill.side, lid, fill.filled_qty, fill.cash).is_some()
                        {
                            // 只有 Lot 全平（qty 归零）时才清理冗余卖单。
                            // 部分成交时 Lot 还在，不撤其他单。
                            if !self.inventory.lot_exists(fill.side, lid) {
                                let stale_ids = self.book.remove_sells_for_lot(lid, fill.order_id);
                                for id in stale_ids {
                                    cancels.push(Command::CancelOrder(id));
                                }
                            }
                        }
                        self.book.apply_fill(fill.order_id, fill.filled_qty);
                        (Trigger::Filled {
                            side: fill.side,
                            direction: OrderDirection::Sell,
                            lot_id,
                        }, cancels)
                    }
                }
            }
            ExchangeEvent::Canceled(order_id) | ExchangeEvent::CancelFailed(order_id) => {
                self.book.remove(*order_id);
                (Trigger::OrderUpdate, vec![])
            }
            ExchangeEvent::Rejected { order_id, .. } => {
                self.book.remove(*order_id);
                (Trigger::OrderUpdate, vec![])
            }
        }
    }

    // ─── ② 决策 ───

    fn decide(&self, trigger: Trigger) -> Decision {
        let progress = self.progress();
        let active_orders = self.book.active_order_views();
        let free_cash = self.free_cash();

        let ctx = DecisionContext {
            trigger,
            progress,
            market: self.market,
            inventory: &self.inventory,
            active_orders: &active_orders,
            free_cash,
            constraints: self.cfg.constraints,
            config: &self.cfg.strategy,
        };

        strategy::route(self.phase, &ctx)
    }

    // ─── ③ 落地决策 ───

    fn apply_decision(&mut self, decision: Decision) -> Vec<Command> {
        let mut commands = Vec::new();

        for intent in &decision.commands {
            match intent {
                CommandIntent::SubmitBuy {
                    side,
                    price,
                    qty,
                    role,
                    tif,
                } => {
                    // 现金哨兵：买单名义不能超过可用现金。
                    let notional = *price * *qty;
                    if notional > self.free_cash() {
                        continue;
                    }
                    let order =
                        self.make_order(*side, OrderDirection::Buy, *price, *qty, *role, *tif);
                    self.book.insert(order, None);
                    commands.push(Command::SubmitOrder(order));
                }
                CommandIntent::SubmitSell {
                    lot_id,
                    side,
                    price,
                    qty,
                    role,
                    tif,
                } => {
                    let order =
                        self.make_order(*side, OrderDirection::Sell, *price, *qty, *role, *tif);
                    self.book.insert(order, Some(*lot_id));
                    commands.push(Command::SubmitOrder(order));
                }
                CommandIntent::Cancel(order_id) => {
                    self.book.mark_cancel_pending(*order_id);
                    commands.push(Command::CancelOrder(*order_id));
                }
                CommandIntent::CancelSide(side) => {
                    self.book.mark_side_cancel_pending(*side);
                    commands.push(Command::CancelSide(*side));
                }
                CommandIntent::CancelAll => {
                    self.book.mark_all_cancel_pending();
                    commands.push(Command::CancelAll);
                }
            }
        }

        // 阶段跳转。
        if let Some(new_phase) = decision
            .transition
            .filter(|&p| self.phase.can_transition_to(p))
        {
            self.phase = new_phase;
            self.generation = self.generation.next();
        }

        commands
    }

    // ─── 辅助 ───

    fn make_order(
        &mut self,
        side: Side,
        direction: OrderDirection,
        price: Money,
        qty: Money,
        role: OrderRole,
        tif: TimeInForce,
    ) -> Order {
        Order {
            order_id: self.id_gen.next(),
            side,
            direction,
            price,
            qty,
            role,
            time_in_force: tif,
            generation: self.generation,
        }
    }

    /// 场内进度 = 已过时间 / 900s。
    fn progress(&self) -> Decimal {
        let elapsed_ms = self.now;
        let total_ms: u64 = 900_000;
        if total_ms == 0 {
            return Decimal::ZERO;
        }
        Decimal::from(elapsed_ms) / Decimal::from(total_ms)
    }

    /// 可用现金 = 总资金 − 净投入 − 活跃买单名义。
    fn free_cash(&self) -> Money {
        self.cfg.total_capital
            - self.inventory.net_invested()
            - self.book.total_active_buy_notional()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::fee::FeeModel;
    use domain::market::{BookTop, MarketSnapshot};
    use exchange::backend::ExchangeBackend;
    use exchange::simulator::Simulator;
    use rust_decimal_macros::dec;

    fn snapshot(
        up_bid: Decimal,
        up_ask: Decimal,
        dn_bid: Decimal,
        dn_ask: Decimal,
    ) -> MarketSnapshot {
        MarketSnapshot {
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
        }
    }

    /// 跑一个 tick：喂行情给 sim 撮合，事件喂 engine，指令下发 sim。
    fn tick(
        engine: &mut Engine,
        sim: &mut Simulator,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<ExchangeEvent>,
        snap: &MarketSnapshot,
        now: Millis,
    ) {
        // 先喂行情撮合。
        sim.on_market(snap);
        // 发送 BookUpdate 事件给 engine。
        let book_cmds = engine.handle_event(&ExchangeEvent::BookUpdate(*snap), now);
        // 处理 sim 撮合产出的 Fill 事件。
        while let Ok(event) = rx.try_recv() {
            let fill_cmds = engine.handle_event(&event, now);
            for cmd in fill_cmds {
                dispatch(sim, &cmd);
            }
        }
        // 下发 book_update 产出的指令。
        for cmd in book_cmds {
            dispatch(sim, &cmd);
        }
    }

    fn dispatch(sim: &mut Simulator, cmd: &Command) {
        match cmd {
            Command::SubmitOrder(order) => sim.submit_order(*order),
            Command::CancelOrder(id) => sim.cancel_order(*id),
            Command::CancelSide(side) => sim.cancel_side(*side),
            Command::CancelAll => sim.cancel_all(),
        }
    }

    #[test]
    fn engine_submits_buy_orders_on_first_tick() {
        let mut engine = Engine::new(EngineConfig::default());
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        let snap = snapshot(dec!(0.55), dec!(0.56), dec!(0.44), dec!(0.45));

        tick(&mut engine, &mut sim, &mut rx, &snap, 6000);

        // Engine 应在两侧各挂一笔 Maker 买单。
        assert!(sim.resting_order_count() >= 2);
        assert_eq!(engine.phase(), Phase::Building);
    }

    #[test]
    fn engine_transitions_to_cycling() {
        let mut engine = Engine::new(EngineConfig::default());
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());
        let snap = snapshot(dec!(0.55), dec!(0.56), dec!(0.44), dec!(0.45));

        // progress = 72000/900000 = 0.08 → 应跳转 Cycling。
        tick(&mut engine, &mut sim, &mut rx, &snap, 72_000);

        assert_eq!(engine.phase(), Phase::Cycling);
    }

    #[test]
    fn buy_fill_opens_lot() {
        let mut engine = Engine::new(EngineConfig::default());
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());

        // 第一个 tick：挂买单。
        let snap = snapshot(dec!(0.50), dec!(0.51), dec!(0.49), dec!(0.50));
        tick(&mut engine, &mut sim, &mut rx, &snap, 6000);

        // 第二个 tick：ask 下穿 → 买单成交。
        let snap2 = snapshot(dec!(0.48), dec!(0.49), dec!(0.50), dec!(0.51));
        tick(&mut engine, &mut sim, &mut rx, &snap2, 7000);

        // 应有 Lot 开立。
        assert!(
            engine.inventory().lot_count(Side::Up) > 0
                || engine.inventory().lot_count(Side::Down) > 0
        );
    }

    #[test]
    fn sell_fill_closes_lot_and_records_pnl() {
        let mut engine = Engine::new(EngineConfig::default());
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());

        // tick 1: 建仓期挂买单。
        let snap1 = snapshot(dec!(0.45), dec!(0.46), dec!(0.54), dec!(0.55));
        tick(&mut engine, &mut sim, &mut rx, &snap1, 6000);

        // tick 2: ask 穿越→UP买单成交(0.45 挂单,ask=0.44<0.45 成交)。
        let snap2 = snapshot(dec!(0.44), dec!(0.44), dec!(0.55), dec!(0.56));
        tick(&mut engine, &mut sim, &mut rx, &snap2, 7000);

        let up_lots_before = engine.inventory().lot_count(Side::Up);

        // 进入 Cycling（手动推 progress > 8%）。
        let snap3 = snapshot(dec!(0.51), dec!(0.52), dec!(0.48), dec!(0.49));
        tick(&mut engine, &mut sim, &mut rx, &snap3, 73_000);
        assert_eq!(engine.phase(), Phase::Cycling);

        // tick: bid 涨到 0.51 ≥ 0.45 + 0.05(tp) = 0.50 → 应挂止盈卖单。
        // 再 tick 一次让卖单成交（bid > 卖单价 0.50）。
        let snap4 = snapshot(dec!(0.52), dec!(0.53), dec!(0.47), dec!(0.48));
        tick(&mut engine, &mut sim, &mut rx, &snap4, 74_000);

        // 如果止盈卖单挂上且 bid 穿越，下一个 tick 会成交。
        let snap5 = snapshot(dec!(0.52), dec!(0.53), dec!(0.47), dec!(0.48));
        tick(&mut engine, &mut sim, &mut rx, &snap5, 75_000);

        // 检查：已实现盈亏应 > 0（如果止盈成交了）或 Lot 数减少。
        let pnl = engine.inventory().realized_pnl();
        let up_lots_after = engine.inventory().lot_count(Side::Up);
        // 至少一个条件成立：要么赚了钱，要么 Lot 被平了。
        assert!(
            pnl > Decimal::ZERO || up_lots_after < up_lots_before,
            "止盈应平仓或记录盈亏: pnl={pnl}, lots before={up_lots_before}, after={up_lots_after}"
        );
    }

    #[test]
    fn cash_sentinel_prevents_over_buying() {
        let cfg = EngineConfig::with_capital(dec!(5)); // 只有 $5
        let mut engine = Engine::new(cfg);
        let (mut sim, mut rx) = Simulator::new(FeeModel::zero());

        // bid 0.55 × 10 股 = $5.5 > $5 可用 → 不应挂。
        let snap = snapshot(dec!(0.55), dec!(0.56), dec!(0.54), dec!(0.55));
        tick(&mut engine, &mut sim, &mut rx, &snap, 6000);

        // 两侧都太贵，不应有买单。
        assert_eq!(sim.resting_order_count(), 0);
    }
}
