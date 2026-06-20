//! 事件循环层：单写者主循环，把账本、状态机、策略、风控、执行后端串成闭环。
//!
//! 对应策略说明书第三节「订单成交事件驱动」架构。本层为**同步核心**：
//! 不自持 channel 与后端，而是暴露 [`Engine::handle_event`]（喂一个交易所事件、返回应下发的指令）
//! 与 [`Engine::start`]（初始布阵），由上层（app）负责对接 mpsc 事件流与执行后端。
//! 这样核心逻辑可纯同步地单元测试，且天然满足单写者串行（一次只处理一个事件）。
//!
//! **竞态隔离（修复风险 #6）**：每批新挂单世代号自增；成交 / 撤单回报携带其所属世代，
//! 低于当前世代的回报视为过期，直接丢弃，避免基于过期状态误操作。

use domain::market::MarketSnapshot;
use domain::order::{Command, Generation, Order, OrderConstraints, OrderDirection, OrderIdGenerator};
use domain::state::RobotState;
use domain::types::{Money, OrderRole, Side};
use exchange::event::ExchangeEvent;
use fsm::{StateMachine, StepInputs, Thresholds};
use ledger::Ledger;
use risk::auditor::RiskAuditor;
use strategy::{FillContext, GradientLadder, PendingOrder};

/// 事件循环的运行参数。
#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// 核心做市池额度上限，供梯度布阵分配。
    pub grid_maker_pool: Money,
    /// 交易所最小量与精度约束。
    pub constraints: OrderConstraints,
}

/// 单写者事件循环。
pub struct Engine {
    ledger: Ledger,
    state_machine: StateMachine,
    ladder: GradientLadder,
    auditor: RiskAuditor,
    id_generator: OrderIdGenerator,
    /// 当前挂单世代号，每发起一批新挂单自增一次。
    current_generation: Generation,
    /// 当前可用现金，初始为总资金，随成交扣减。
    free_cash: Money,
    /// 最新市场快照，由 BookUpdate 事件刷新。
    market: MarketSnapshot,
    /// 本轮做市主战场侧（初始布阵时确定）：仅该侧的主动接低成交才触发跨侧重算，
    /// 对面侧的配对买入成交不触发，以断开「配对又触发配对」的仓位滚雪球。
    main_field: Option<Side>,
    /// 挂起的配对买单：配对价 ≥ 对面 Ask 时暂存，待 Ask 跌到价下方再补挂。
    pending_orders: Vec<PendingOrder>,
    /// 是否已收手：进入 FinalSettlement（利润锁定）后置位，此后停止一切新开仓，
    /// 已持仓扛到交割兑现，避免继续滚仓位被反向大单吃掉利润。
    settled: bool,
    config: EngineConfig,
}

impl Engine {
    /// 创建事件循环。`total_capital` 为账户总资金 V，初始即全部可用。
    pub fn new(
        total_capital: Money,
        thresholds: Thresholds,
        ladder: GradientLadder,
        auditor: RiskAuditor,
        config: EngineConfig,
    ) -> Self {
        Self {
            ledger: Ledger::new(),
            state_machine: StateMachine::new(thresholds),
            ladder,
            auditor,
            id_generator: OrderIdGenerator::new(),
            current_generation: Generation::new(),
            free_cash: total_capital,
            market: MarketSnapshot::default(),
            main_field: None,
            pending_orders: Vec::new(),
            settled: false,
            config,
        }
    }

    /// 当前账本（只读，供观测与测试）。
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// 当前状态机状态。
    pub fn state(&self) -> RobotState {
        self.state_machine.state()
    }

    /// 当前可用现金。
    pub fn free_cash(&self) -> Money {
        self.free_cash
    }

    /// 启动：完成初始化跳转并产出初始布阵指令。
    ///
    /// 需要先有行情（通过 [`Engine::handle_event`] 收到 `BookUpdate`）才能选主战场布阵；
    /// 无行情时返回空指令。
    pub fn start(&mut self) -> Vec<Command> {
        self.state_machine.finish_initialization();
        self.deploy_ladder()
    }

    /// 处理一个交易所事件，返回应下发的指令。
    pub fn handle_event(&mut self, event: ExchangeEvent) -> Vec<Command> {
        match event {
            ExchangeEvent::BookUpdate(snapshot) => {
                self.market = snapshot;
                self.drive_state_machine();
                if let Some(settle) = self.enter_settlement_if_locked() {
                    return settle;
                }
                // 已收手则不再补挂；否则检查挂起的配对单：对面 Ask 跌到目标价下方则补挂。
                if self.settled {
                    Vec::new()
                } else {
                    self.try_post_pending()
                }
            }
            ExchangeEvent::Filled(fill) => {
                // 成交是不可撤销的事实：无论属于哪个世代，一律入账并扣减现金，
                // 否则账本会与交易所真实持仓不一致（撤单只对未成交挂单生效，对已成交无效）。
                let is_current = fill.generation >= self.current_generation;
                self.ledger.apply_fill(&fill);
                self.free_cash -= fill.cash;
                self.drive_state_machine();
                if let Some(settle) = self.enter_settlement_if_locked() {
                    return settle;
                }
                // 已收手则停止一切新开仓；否则仅对「当前世代 + 主战场侧（主动接低）」的买入成交
                // 触发跨侧重算；对面配对成交不触发，断开正反馈滚雪球；旧世代成交亦不触发（修复 #6）。
                let is_main_field = self.main_field == Some(fill.side);
                if !self.settled && is_current && is_main_field && fill.direction == OrderDirection::Buy
                {
                    self.recompute_after_fill(&fill)
                } else {
                    Vec::new()
                }
            }
            // 拒单与撤单确认暂不触发新指令。
            ExchangeEvent::Rejected { .. } | ExchangeEvent::Canceled(_) => Vec::new(),
        }
    }

    /// 利润锁定即停手闸：状态机首次进入 [`RobotState::FinalSettlement`] 时一次性收手——
    /// 撤销全部活跃挂单、清空待补挂队列、置位 `settled`，此后停止一切新开仓。
    ///
    /// 返回 `Some(收手指令)` 表示本次刚进入收手；`None` 表示无需处理（未达成或早已收手）。
    fn enter_settlement_if_locked(&mut self) -> Option<Vec<Command>> {
        if self.settled {
            return None;
        }
        if self.state_machine.state() == RobotState::FinalSettlement {
            self.settled = true;
            self.pending_orders.clear();
            Some(vec![Command::CancelAll])
        } else {
            None
        }
    }

    /// 本边买入成交后触发跨侧配对重算：世代号自增、产出经风控的指令，并记录挂起的配对单。
    fn recompute_after_fill(&mut self, fill: &domain::order::Fill) -> Vec<Command> {
        self.current_generation = self.current_generation.next();
        let context = FillContext {
            filled_side: fill.side,
            filled_price: fill.price,
            own_qty: self.ledger.qty(fill.side),
            opposite_qty: self.ledger.qty(fill.side.opposite()),
            own_average_price: self.ledger.average_price(fill.side),
            opposite_best_ask: self.market.book(fill.side.opposite()).best_ask,
        };
        let result = self.ladder.recompute_after_fill(
            &context,
            self.config.grid_maker_pool,
            &self.config.constraints,
            &mut self.id_generator,
            self.current_generation,
        );
        if let Some(pending) = result.pending {
            self.pending_orders.push(pending);
        }
        result
            .commands
            .into_iter()
            .filter(|command| self.approve_command(command))
            .collect()
    }

    /// 检查挂起的配对单：对面 Ask 已跌到目标价下方的，转为正式 Maker 买单补挂。
    fn try_post_pending(&mut self) -> Vec<Command> {
        let mut commands = Vec::new();
        let mut still_pending = Vec::new();
        for pending in std::mem::take(&mut self.pending_orders) {
            let postable = matches!(
                self.market.book(pending.side).best_ask,
                Some(ask) if pending.price < ask
            );
            if postable {
                let order = Order {
                    order_id: self.id_generator.next(),
                    side: pending.side,
                    direction: OrderDirection::Buy,
                    price: pending.price,
                    qty: pending.qty,
                    role: OrderRole::Maker,
                    generation: self.current_generation,
                };
                let command = Command::SubmitOrder(order);
                if self.approve_command(&command) {
                    commands.push(command);
                }
            } else {
                still_pending.push(pending);
            }
        }
        self.pending_orders = still_pending;
        commands
    }

    /// 依据当前账本与行情驱动状态机评估一次流转。
    fn drive_state_machine(&mut self) {
        let inputs = StepInputs {
            position: self.ledger.snapshot(),
            market: self.market,
        };
        self.state_machine.step(&inputs);
    }

    /// 发起一批梯度布阵：世代号自增，产出经风控通过的下单指令。
    fn deploy_ladder(&mut self) -> Vec<Command> {
        // 记录本轮主战场侧：后续仅该侧的主动接低成交触发跨侧重算。
        self.main_field = self.ladder.select_main_field(&self.market);
        self.current_generation = self.current_generation.next();
        let commands = self.ladder.deploy(
            &self.market,
            self.config.grid_maker_pool,
            &self.config.constraints,
            &mut self.id_generator,
            self.current_generation,
        );
        // 逐条过风控：Cash Guard 拒绝的指令不下发。
        commands
            .into_iter()
            .filter(|command| self.approve_command(command))
            .collect()
    }

    /// 风控审计单条指令；非下单指令（撤单）默认放行。
    fn approve_command(&self, command: &Command) -> bool {
        match command {
            Command::SubmitOrder(order) => {
                self.auditor.approve(order, self.free_cash).is_approved()
            }
            Command::CancelOrder(_) | Command::CancelSide(_) | Command::CancelAll => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::market::BookTop;
    use domain::order::{Fill, OrderDirection, OrderId};
    use domain::state::RobotState;
    use domain::types::Side;
    use risk::pool::CapitalPools;
    use rust_decimal_macros::dec;

    /// 构造测试用事件循环：总资金 1000，默认布阵 / 约束 / 风控。
    fn engine() -> Engine {
        let total_capital = dec!(1000);
        let thresholds = Thresholds {
            hedge_loss_trigger: dec!(30),
            hedge_safety_price: dec!(0.5),
            profit_target: dec!(15),
        };
        let pools = CapitalPools::with_default_ratios(total_capital);
        let config = EngineConfig {
            grid_maker_pool: pools.grid_maker(),
            constraints: OrderConstraints::default(),
        };
        Engine::new(
            total_capital,
            thresholds,
            GradientLadder::with_default_config(),
            RiskAuditor::with_default_guard(pools),
            config,
        )
    }

    /// 构造仅设 Up 侧 best_ask 的市场快照。
    fn book_update(up_ask: Money) -> ExchangeEvent {
        ExchangeEvent::BookUpdate(MarketSnapshot {
            up: BookTop {
                best_bid: None,
                best_ask: Some(up_ask),
                last_trade: None,
            },
            down: BookTop::default(),
        })
    }

    #[test]
    fn starts_in_initialization() {
        let engine = engine();
        assert_eq!(engine.state(), RobotState::Initialization);
        assert_eq!(engine.free_cash(), dec!(1000));
    }

    #[test]
    fn start_after_book_update_deploys_ladder() {
        let mut engine = engine();
        // 先喂行情：主战场 Up，best_ask 0.40。
        engine.handle_event(book_update(dec!(0.40)));
        let commands = engine.start();
        // 初始化跳转到区间做市，并产出三层布阵指令。
        assert_eq!(engine.state(), RobotState::RangeBoundMaking);
        assert_eq!(commands.len(), 3);
        assert!(
            commands
                .iter()
                .all(|c| matches!(c, Command::SubmitOrder(_)))
        );
    }

    #[test]
    fn deployed_orders_carry_incremented_generation() {
        let mut engine = engine();
        engine.handle_event(book_update(dec!(0.40)));
        let commands = engine.start();
        // 初始世代为 0，第一批布阵世代自增为 1。
        for command in &commands {
            if let Command::SubmitOrder(order) = command {
                assert_eq!(order.generation, Generation(1));
            }
        }
    }

    #[test]
    fn fill_updates_ledger_and_reduces_cash() {
        let mut engine = engine();
        engine.handle_event(book_update(dec!(0.40)));
        engine.start();
        // 模拟一笔成交：净入仓 50 份、花费 20。
        let fill = Fill {
            order_id: OrderId(0),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.39),
            filled_qty: dec!(50),
            cash: dec!(20),
            generation: Generation(1),
        };
        engine.handle_event(ExchangeEvent::Filled(fill));
        assert_eq!(engine.ledger().qty(Side::Up), dec!(50));
        assert_eq!(engine.free_cash(), dec!(980));
    }

    #[test]
    fn older_generation_fill_is_still_booked() {
        let mut engine = engine();
        engine.handle_event(book_update(dec!(0.40)));
        engine.start(); // 当前世代 → 1
        // 一笔世代号为 0（旧世代）的成交：成交是不可撤销的事实，必须入账，
        // 否则账本会与交易所真实持仓不一致。世代号只影响是否触发后续决策，不影响记账。
        let older = Fill {
            order_id: OrderId(99),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.39),
            filled_qty: dec!(50),
            cash: dec!(20),
            generation: Generation(0),
        };
        engine.handle_event(ExchangeEvent::Filled(older));
        // 旧世代成交同样入账、同样扣减现金。
        assert_eq!(engine.ledger().qty(Side::Up), dec!(50));
        assert_eq!(engine.free_cash(), dec!(980));
    }

    /// 构造双边均设 best_ask 的市场快照。
    fn book_update_both(up_ask: Money, down_ask: Money) -> ExchangeEvent {
        ExchangeEvent::BookUpdate(MarketSnapshot {
            up: BookTop {
                best_bid: None,
                best_ask: Some(up_ask),
                last_trade: None,
            },
            down: BookTop {
                best_bid: None,
                best_ask: Some(down_ask),
                last_trade: None,
            },
        })
    }

    /// 构造当前世代（1）的本边买入成交。
    fn current_fill(side: Side, price: Money, qty: Money, cash: Money) -> ExchangeEvent {
        ExchangeEvent::Filled(Fill {
            order_id: OrderId(0),
            side,
            direction: OrderDirection::Buy,
            price,
            filled_qty: qty,
            cash,
            generation: Generation(1),
        })
    }

    #[test]
    fn current_generation_fill_triggers_recompute() {
        let mut engine = engine();
        // Up 主战场 0.40，对面 Down Ask 0.65。
        engine.handle_event(book_update_both(dec!(0.40), dec!(0.65)));
        engine.start(); // 世代 → 1
        // Up 侧买入成交 100 股、花费 40 → 触发跨侧重算。
        let commands = engine.handle_event(current_fill(Side::Up, dec!(0.40), dec!(100), dec!(40)));
        // 重算应撤销对面 Down 侧全部活跃挂单。
        assert!(commands.contains(&Command::CancelSide(Side::Down)));
        // 并产出对面 Down 的配对买单（配对价 1-0.40-0.02=0.58 < Down Ask 0.65 → 直接挂）。
        assert!(commands.iter().any(|c| matches!(
            c,
            Command::SubmitOrder(o) if o.side == Side::Down && o.price == dec!(0.58)
        )));
    }

    #[test]
    fn pending_pair_is_posted_when_ask_drops() {
        let mut engine = engine();
        // 对面 Down Ask 仅 0.55，配对价 0.58 ≥ 0.55 → 挂起，不立即下发 Down 单。
        engine.handle_event(book_update_both(dec!(0.40), dec!(0.55)));
        engine.start();
        let commands = engine.handle_event(current_fill(Side::Up, dec!(0.40), dec!(100), dec!(40)));
        assert!(!commands.iter().any(|c| matches!(
            c,
            Command::SubmitOrder(o) if o.side == Side::Down
        )));
        // 行情更新：Down Ask 跌到 0.60 > 配对价 0.58 → 补挂挂起的配对单。
        let posted = engine.handle_event(book_update_both(dec!(0.40), dec!(0.60)));
        assert!(posted.iter().any(|c| matches!(
            c,
            Command::SubmitOrder(o) if o.side == Side::Down && o.price == dec!(0.58)
        )));
    }

    #[test]
    fn entering_settlement_cancels_all_and_stops_new_orders() {
        let mut engine = engine();
        engine.handle_event(book_update_both(dec!(0.40), dec!(0.60)));
        engine.start();
        // 先在两侧各建仓 100 股、双边总成本 80：min(Q)=100 > 成本 80，
        // 且两边 PnL 均 = 100 - 80 = 20 ≥ 利润目标 15 → 利润锁定，应进入收手。
        engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(0),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.40),
            filled_qty: dec!(100),
            cash: dec!(40),
            generation: Generation(1),
        }));
        let settle = engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(1),
            side: Side::Down,
            direction: OrderDirection::Buy,
            price: dec!(0.40),
            filled_qty: dec!(100),
            cash: dec!(40),
            generation: Generation(1),
        }));
        // 利润锁定瞬间：状态转入 FinalSettlement，并一次性发出 CancelAll 收手。
        assert_eq!(engine.state(), RobotState::FinalSettlement);
        assert!(settle.contains(&Command::CancelAll));

        // 收手后：任何后续事件都不再产出新开仓指令（扛到交割）。
        let after_book = engine.handle_event(book_update_both(dec!(0.30), dec!(0.30)));
        assert!(after_book.is_empty());
        let after_fill = engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(2),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.30),
            filled_qty: dec!(50),
            cash: dec!(15),
            generation: Generation(1),
        }));
        assert!(after_fill.is_empty());
    }
}
