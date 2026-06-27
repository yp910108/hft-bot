//! 命令式外壳：单写者事件循环，串起账本、挂单簿、FSM、策略路由、风控、资金池。
//!
//! 纯函数核心（strategy）只算决策；engine 负责一切副作用：更新事实、组装只读上下文、
//! 按优先级链路由到小策略、给订单意图分配 ID/世代、过风控、产出指令、更新本地镜像、应用状态跳转。
//!
//! 时间不自持：调用方（回测虚拟时钟 / 实盘系统时钟）每次把 now 与剩余时间喂进来。
//!
//! 本文件只当门面：Engine 结构定义、构造、访问器、事件主循环。
//! 具体职责拆到各子模块：
//! - [`event`]：事件事实更新（账本/挂单簿/行情）。
//! - [`decide`]：组装上下文 + 全局路由 + 派发小策略。
//! - [`apply`]：落地决策（分配 ID、风控、改镜像、状态跳转）。
//! - [`budget`]：资金池剩余 / 可用现金 / 活跃挂单视图。
//! - [`book`]：活跃挂单簿 + 订单生命周期。

pub mod apply;
pub mod book;
pub mod budget;
pub mod config;
pub mod decide;
pub mod event;

pub use config::EngineConfig;

use book::OrderBook;
use config::Pool;
use domain::market::MarketSnapshot;
use domain::order::{Command, Generation, OrderId, OrderIdGenerator};
use domain::state::RobotState;
use domain::types::{Money, Side};
use exchange::clock::Millis;
use exchange::event::ExchangeEvent;
use fsm::StateMachine;
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

    // ---- 可变状态 ----
    pub(crate) machine: StateMachine,
    pub(crate) ledger: ledger::Ledger,
    pub(crate) book: OrderBook,
    pub(crate) id_gen: OrderIdGenerator,
    pub(crate) generation: Generation,
    pub(crate) market: MarketSnapshot,

    pub(crate) main_field: Option<Side>,
    pub(crate) main_field_frozen: bool,
    pub(crate) last_hedge_at: Option<Millis>,
    pub(crate) calm_since: Option<Millis>,

    // 各池已成交累计成本（剩余 = 池总额 − 已成交 − 活跃挂单名义）。
    pub(crate) filled_cost: HashMap<Pool, Money>,
    // 订单 → 所属池（算活跃挂单按池分摊）。
    pub(crate) order_pool: HashMap<OrderId, Pool>,

    pub(crate) now: Millis,
    pub(crate) time_to_expiry: Millis,

    // 诊断：本场曾到达的最深阶段（按推进深度排序），用于回测分析对冲触达率。
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
            machine: StateMachine::new(),
            ledger: ledger::Ledger::new(),
            book: OrderBook::new(),
            id_gen: OrderIdGenerator::new(),
            generation: Generation::new(),
            market: MarketSnapshot::default(),
            main_field: None,
            main_field_frozen: false,
            last_hedge_at: None,
            calm_since: None,
            filled_cost: HashMap::new(),
            order_pool: HashMap::new(),
            now: 0,
            time_to_expiry: 0,
            deepest_phase: 0,
        }
    }

    /// 当前状态。
    pub fn state(&self) -> RobotState {
        self.machine.state()
    }

    /// 账本只读引用。
    pub fn ledger(&self) -> &ledger::Ledger {
        &self.ledger
    }

    /// 主战场侧（建仓首笔成交后锁定）。
    pub fn main_field(&self) -> Option<Side> {
        self.main_field
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

        // ① 更新事实（账本 / 挂单簿 / 行情），并得出本次触发类型。
        let trigger = self.apply_event_facts(&event);
        // ② 组装只读上下文 → 路由 → 小策略决策。
        let decision = self.decide(trigger);
        // ③ 落地决策：分配 ID、过风控、产出指令、更新镜像、应用跳转。
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
    use domain::market::BookTop;
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

    /// 构造一笔成交回报。
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
        // Up 便宜侧 ask 0.40 → 铺三档 Up 买单。
        let cmds = e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1000, 600_000);
        let submits: Vec<_> = cmds
            .iter()
            .filter(|c| matches!(c, Command::SubmitOrder(_)))
            .collect();
        assert_eq!(submits.len(), 3);
    }

    #[test]
    fn fill_locks_main_field_and_enters_pairing() {
        let mut e = engine();
        e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1000, 600_000);
        // Up 侧首档成交（id 1 是第一笔挂单）。
        e.handle_event(fill(1, Side::Up, dec!(0.39), dec!(10)), 1100, 600_000);
        assert_eq!(e.main_field(), Some(Side::Up));
        assert_eq!(e.state(), RobotState::Pairing);
        assert_eq!(e.ledger().snapshot().up_qty, dec!(10));
    }

    #[test]
    fn pairing_fill_posts_pair_order() {
        let mut e = engine();
        e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1000, 600_000);
        e.handle_event(fill(1, Side::Up, dec!(0.39), dec!(10)), 1100, 600_000);
        e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1200, 600_000);
        let cmds = e.handle_event(fill(2, Side::Up, dec!(0.39), dec!(50)), 1300, 600_000);
        let has_pair = cmds.iter().any(|c| matches!(
            c,
            Command::SubmitOrder(o) if o.side == Side::Down
        ));
        assert!(has_pair, "主战场成交应触发 Down 配对单");
    }

    #[test]
    fn time_red_line_cancels_all_and_settles() {
        let mut e = engine();
        e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1000, 600_000);
        e.handle_event(fill(1, Side::Up, dec!(0.39), dec!(10)), 1100, 600_000);
        // 剩余 30s < 1min → 时间红线。
        let cmds = e.handle_event(book_update(dec!(0.40), dec!(0.62)), 900_000, 30_000);
        assert!(cmds.contains(&Command::CancelAll));
        assert_eq!(e.state(), RobotState::SettlementWait);
    }

    #[test]
    fn circuit_breaker_trips_on_wide_spread() {
        let mut e = engine();
        e.handle_event(book_update(dec!(0.40), dec!(0.62)), 1000, 600_000);
        let crash = ExchangeEvent::BookUpdate(MarketSnapshot {
            up: book(Some(dec!(0.20)), Some(dec!(0.55))),
            down: book(Some(dec!(0.40)), Some(dec!(0.42))),
        });
        let cmds = e.handle_event(crash, 2000, 600_000);
        assert!(cmds.contains(&Command::CancelAll));
        assert_eq!(e.state(), RobotState::CircuitBreaker);
    }
}
