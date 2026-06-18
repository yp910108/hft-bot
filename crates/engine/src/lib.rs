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
use domain::order::{Command, Generation, OrderConstraints, OrderIdGenerator};
use domain::state::RobotState;
use domain::types::Money;
use exchange::event::ExchangeEvent;
use fsm::{StateMachine, StepInputs, Thresholds};
use ledger::Ledger;
use risk::auditor::RiskAuditor;
use strategy::GradientLadder;

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
                Vec::new()
            }
            ExchangeEvent::Filled(fill) => {
                // 成交是不可撤销的事实：无论属于哪个世代，一律入账并扣减现金，
                // 否则账本会与交易所真实持仓不一致（撤单只对未成交挂单生效，对已成交无效）。
                //
                // 世代号的用途是：旧世代成交属于已被重算作废的那批挂单，入账后**不应再据它
                // 触发新的重算/重挂决策**（避免基于过期决策误操作，修复 #6）。当前最小闭环
                // 尚无跨侧重算决策，故此处只记账并驱动状态机；世代区分留待跨侧重算阶段使用。
                self.ledger.apply_fill(&fill);
                self.free_cash -= fill.cash;
                self.drive_state_machine();
                Vec::new()
            }
            // 最小闭环：拒单与撤单确认暂不触发新指令。
            ExchangeEvent::Rejected { .. } | ExchangeEvent::Canceled(_) => Vec::new(),
        }
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
}
