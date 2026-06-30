//! Engine：命令式外壳的核心编排者。
//!
//! 单写者事件循环：所有副作用和状态维护都在这里，纯函数核心（strategy）只算决策。
//! 时间不自持——调用方（回测虚拟时钟 / 实盘系统时钟）每次把 now 与剩余时间喂进来，
//! handle_event 是同步纯逻辑（回测和实盘共用，异步层留给 app）。
//!
//! 跨阶段可变状态收在 [`RoundState`]，瞬时的账本 / 挂单簿 / 行情 / 池成本追踪是 Engine 字段。

use crate::book::OrderBook;
use crate::config::{EngineConfig, Pool};
use domain::clock::Millis;
use domain::command::Command;
use domain::market::MarketSnapshot;
use domain::order::{Generation, OrderId, OrderIdGenerator};
use domain::round_state::RoundState;
use domain::state::RobotState;
use domain::types::{Money, Side};
use exchange::event::ExchangeEvent;
use risk::auditor::RiskAuditor;
use std::collections::HashMap;
use strategy::{
    BuildingStrategy, CircuitBreakerStrategy, DynamicHedgeStrategy, EvHedgeStrategy,
    PairingStrategy,
};

/// 单写者引擎。所有可变状态只有它一个写者。
pub struct Engine {
    pub(crate) cfg: EngineConfig,
    pub(crate) auditor: RiskAuditor,

    // 各阶段小策略（纯函数，无状态，构造一次复用）。
    pub(crate) building: BuildingStrategy,
    pub(crate) pairing: PairingStrategy,
    pub(crate) dynamic: DynamicHedgeStrategy,
    pub(crate) ev: EvHedgeStrategy,
    pub(crate) circuit: CircuitBreakerStrategy,

    // ---- 跨阶段可变状态 ----
    pub(crate) round: RoundState,

    // ---- 瞬时 / 账务状态 ----
    pub(crate) ledger: ledger::Ledger,
    pub(crate) book: OrderBook,
    pub(crate) id_gen: OrderIdGenerator,
    pub(crate) generation: Generation,
    pub(crate) market: MarketSnapshot,

    /// 各池已成交累计成本（剩余 = 池总额 − 已成交 − 活跃挂单名义）。
    pub(crate) filled_cost: HashMap<Pool, Money>,
    /// 订单 → 所属池（算活跃挂单按池分摊）。
    pub(crate) order_pool: HashMap<OrderId, Pool>,

    pub(crate) now: Millis,
    pub(crate) time_to_expiry: Millis,

    // 诊断：本场曾到达的最深阶段（回测分析用）。
    pub(crate) deepest_phase: u8,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Self {
        let auditor = RiskAuditor::new(cfg.pools, cfg.cash_guard_ratio);
        let s = cfg.strategy.clone();
        Self {
            building: BuildingStrategy::new(s.clone()),
            pairing: PairingStrategy::new(s.clone()),
            dynamic: DynamicHedgeStrategy::new(s.clone()),
            ev: EvHedgeStrategy::new(s.clone()),
            circuit: CircuitBreakerStrategy::new(s),
            auditor,
            cfg,
            round: RoundState::new(),
            ledger: ledger::Ledger::new(),
            book: OrderBook::new(),
            id_gen: OrderIdGenerator::new(),
            generation: Generation::new(),
            market: MarketSnapshot::default(),
            filled_cost: HashMap::new(),
            order_pool: HashMap::new(),
            now: 0,
            time_to_expiry: 0,
            deepest_phase: 0,
        }
    }

    /// 当前状态。
    pub fn state(&self) -> RobotState {
        self.round.state
    }

    /// 账本只读引用。
    pub fn ledger(&self) -> &ledger::Ledger {
        &self.ledger
    }

    /// 主战场侧（建仓首笔成交后锁定）。
    pub fn main_field(&self) -> Option<Side> {
        self.round.main_field
    }

    /// 本场曾到达的最深阶段标签（回测诊断用）。
    pub fn deepest_phase_label(&self) -> &'static str {
        match self.deepest_phase {
            0 => "Building",
            1 => "Pairing",
            2 => "DynamicHedge",
            3 => "EvHedge",
            _ => "Unknown",
        }
    }

    /// 处理一个事件。`now` 与 `time_to_expiry` 由调用方（时钟）给出。
    /// 返回要下发给交易所的指令列表。
    pub fn handle_event(
        &mut self,
        event: ExchangeEvent,
        now: Millis,
        time_to_expiry: Millis,
    ) -> Vec<Command> {
        self.now = now;
        self.time_to_expiry = time_to_expiry;

        // ① 更新事实（账本 / 挂单簿 / 行情），得出本次触发类型。
        let trigger = self.apply_event_facts(&event);
        // ② 组装只读上下文 → 路由 → 小策略决策。
        let decision = self.decide(trigger);
        // ③ 落地决策：分配 ID、过风控、产出指令、更新镜像、应用跳转与全局量更新。
        self.apply_decision(decision)
    }

    /// 世代推进：阶段切换时调用，隔离旧世代成交不触发重算（由小策略 trigger 控制）。
    pub fn bump_generation(&mut self) {
        self.generation = self.generation.next();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::market::{BookTop, MarketSnapshot};
    use domain::order::{Fill, OrderDirection};
    use domain::types::{OrderRole, Price};
    use rust_decimal_macros::dec;

    fn engine() -> Engine {
        Engine::new(EngineConfig::with_capital(dec!(1000)))
    }

    fn book(bid: Option<Price>, ask: Option<Price>) -> BookTop {
        BookTop {
            best_bid: bid,
            best_ask: ask,
            last_trade: None,
        }
    }

    fn book_update(up_ask: Price, down_ask: Price) -> ExchangeEvent {
        ExchangeEvent::BookUpdate(MarketSnapshot {
            up: book(Some(up_ask - dec!(0.02)), Some(up_ask)),
            down: book(Some(down_ask - dec!(0.02)), Some(down_ask)),
        })
    }

    fn fill(order_id: u64, side: Side, price: Price, qty: domain::types::Qty) -> ExchangeEvent {
        ExchangeEvent::Filled(Fill {
            order_id: OrderId(order_id),
            side,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price,
            filled_qty: qty,
            cash: price * qty,
            generation: Generation::new(),
        })
    }

    #[test]
    fn starts_in_building() {
        assert_eq!(engine().state(), RobotState::Building);
    }

    #[test]
    fn first_book_update_deploys_three_rungs() {
        let mut e = engine();
        let cmds = e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1000, 600_000);
        let submits: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, Command::SubmitOrder(_)))
            .collect();
        assert_eq!(submits.len(), 3);
        // 挂单簿登记了 3 笔。
        assert_eq!(e.book.len(), 3);
    }

    #[test]
    fn fill_locks_main_field_and_enters_pairing() {
        let mut e = engine();
        e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1000, 600_000);
        e.handle_event(fill(0, Side::Up, dec!(0.39), dec!(10)), 1100, 600_000);
        assert_eq!(e.main_field(), Some(Side::Up));
        assert_eq!(e.state(), RobotState::Pairing);
        assert_eq!(e.ledger().snapshot().up_qty, dec!(10));
        // 成交后从挂单簿移除。
        assert!(!e.book.is_empty()); // 另外两笔还在。
    }

    #[test]
    fn time_red_line_cancels_all_and_settles() {
        let mut e = engine();
        e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1000, 600_000);
        e.handle_event(fill(0, Side::Up, dec!(0.39), dec!(10)), 1100, 600_000);
        // 剩余 30s < 1min → 时间红线。
        let cmds = e.handle_event(book_update(dec!(0.40), dec!(0.62)), 900_000, 30_000);
        assert!(cmds.contains(&Command::CancelAll));
        assert_eq!(e.state(), RobotState::SettlementWait);
    }

    #[test]
    fn circuit_breaker_trips_on_wide_spread() {
        let mut e = engine();
        e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1000, 600_000);
        // Down spread 爆宽。
        let crash = ExchangeEvent::BookUpdate(MarketSnapshot {
            up: book(Some(dec!(0.40)), Some(dec!(0.41))),
            down: book(Some(dec!(0.20)), Some(dec!(0.55))),
        });
        let cmds = e.handle_event(crash, 2000, 600_000);
        assert!(cmds.contains(&Command::CancelAll));
        assert_eq!(e.state(), RobotState::CircuitBreaker);
    }

    #[test]
    fn pairing_fill_posts_pair_order() {
        let mut e = engine();
        e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1000, 600_000);
        e.handle_event(fill(0, Side::Up, dec!(0.39), dec!(10)), 1100, 600_000);
        // 再来行情 + 主战场成交触发配对。
        e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1200, 600_000);
        let cmds = e.handle_event(fill(1, Side::Up, dec!(0.39), dec!(50)), 1300, 600_000);
        let has_pair = cmds.iter().any(|c| {
            matches!(
                c,
                Command::SubmitOrder(o) if o.side == Side::Down
            )
        });
        assert!(has_pair, "主战场成交应触发 Down 配对单");
    }
}
