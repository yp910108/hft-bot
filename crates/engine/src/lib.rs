//! Engine：整个机器人的"大脑"，把所有模块串起来的事件循环。
//!
//! 工作方式很简单：
//! 1. 外部喂进一个事件（行情更新 / 成交回报）
//! 2. Engine 内部更新账本、驱动状态机、决定该干什么
//! 3. 返回一组指令（下单 / 撤单），由外部发给交易所
//!
//! 入口是 [`Engine::handle_event`] 和 [`Engine::start`]。
//! 所有逻辑纯同步、单线程，不存在并发竞态。

use domain::market::MarketSnapshot;
use domain::order::{
    Command, Fill, Generation, Order, OrderConstraints, OrderDirection, OrderIdGenerator,
};
use domain::state::RobotState;
use domain::types::{Money, OrderRole, Side};
use exchange::event::ExchangeEvent;
use fsm::{StateMachine, StepInputs, Thresholds, Transition};
use ledger::Ledger;
use risk::auditor::RiskAuditor;
use rust_decimal::Decimal;
use std::mem;
use strategy::{FillContext, GradientLadder, HedgeDecision, HedgePhase, PendingOrder};

/// Engine 的配置参数。
#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// 做市池额度，梯度布阵从这里出钱。
    pub grid_maker_pool: Money,
    /// 对冲池额度，Taker 追买从这里出钱。
    pub hedge_attack_pool: Money,
    /// 每步 Taker 花对冲池多少比例（0.2 = 每步花 20%）。
    pub hedge_step_fraction: Decimal,
    /// 每个对冲阶段最多打几步 Taker，打完停手等交割。
    pub max_taker_steps: u32,
    /// 交易所的最小下单量和精度。
    pub constraints: OrderConstraints,
}

/// 机器人的事件循环主体。
///
/// 外部每收到一个交易所事件（行情/成交/撤单），就调 `handle_event` 喂进来，
/// 它返回一组指令（下单/撤单）由外部发出去。内部维护账本、状态机、风控等全部状态。
pub struct Engine {
    ledger: Ledger,
    state_machine: StateMachine,
    ladder: GradientLadder,
    auditor: RiskAuditor,
    id_generator: OrderIdGenerator,
    /// 世代号：每发一批新挂单自增。用来区分"这笔成交属于哪批挂单"。
    current_generation: Generation,
    /// 剩余可用现金（初始=总资金，每成交一笔就扣一笔）。
    free_cash: Money,
    /// 最新的市场盘口快照。
    market: MarketSnapshot,
    /// 本场做市的主战场是哪一侧（Up 或 Down）。
    /// 只有主战场侧的成交才触发跨侧配对重算，对面的配对成交不触发（防滚雪球）。
    main_field: Option<Side>,
    /// 暂时挂不出去的配对单（配对价 ≥ 对面 Ask，等 Ask 跌下来再补挂）。
    pending_orders: Vec<PendingOrder>,
    /// 当前对冲阶段分类，用来检测"刚进入对冲"从而重置步数。
    hedge_phase: HedgePhase,
    /// 本对冲阶段已发了几步 Taker，达上限就停。
    taker_steps: u32,
    /// 本 tick 是否已发过对冲 Taker（每次 BookUpdate 重置为 false）。
    hedge_fired_this_tick: bool,
    /// 对冲池剩余可用额度（初始 = hedge_attack_pool，每笔对冲成交扣减，跨阶段不重置）。
    hedge_budget_remaining: Money,
    /// 是否已收手（利润锁定后置 true，之后不再开新仓）。
    settled: bool,
    config: EngineConfig,
}

impl Engine {
    /// 创建 Engine。`total_capital` = 账户总资金，初始全部可用。
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
            hedge_phase: HedgePhase::None,
            taker_steps: 0,
            hedge_fired_this_tick: false,
            hedge_budget_remaining: config.hedge_attack_pool,
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

    /// 启动：状态机跳出初始化，铺好第一批梯度挂单。
    /// 调用前需先喂一次行情（BookUpdate），否则选不出主战场，返回空。
    pub fn start(&mut self) -> Vec<Command> {
        self.state_machine.finish_initialization();
        self.deploy_ladder()
    }

    /// 喂入一个交易所事件，返回该发给交易所的指令。
    pub fn handle_event(&mut self, event: ExchangeEvent) -> Vec<Command> {
        match event {
            ExchangeEvent::BookUpdate(snapshot) => {
                self.market = snapshot;
                self.hedge_fired_this_tick = false;
                self.drive_state_machine();
                let mut commands = self.sync_hedge_phase();
                if let Some(settle) = self.enter_settlement_if_locked() {
                    return settle;
                }
                // 已收手 → 啥也不干。
                // 对冲阶段 → 不在行情 tick 里主动发 Taker（防止一个 tick 连环打光步数）。
                // 常规阶段 → 看看之前挂起的配对单能不能补上。
                if self.settled {
                    Vec::new()
                } else if self.hedge_phase.is_hedging() {
                    commands
                } else if commands.contains(&Command::CancelAll) {
                    // 刚从对冲回退到做市，立即重铺梯度。
                    commands.extend(self.deploy_ladder());
                    commands
                } else {
                    commands.extend(self.try_post_pending());
                    commands
                }
            }
            ExchangeEvent::Filled(fill) => {
                // 每笔成交都是新的决策点，允许发下一步对冲（不受同 tick 限流）。
                self.hedge_fired_this_tick = false;
                // 无论新旧世代，成交是既成事实，必须入账（否则账本和交易所不一致）。
                let is_current = fill.generation >= self.current_generation;
                let is_hedging = self.hedge_phase.is_hedging();
                self.ledger.apply_fill(&fill);
                self.free_cash -= fill.cash;
                // 对冲阶段的成交同时扣减对冲池预算。
                if is_hedging {
                    self.hedge_budget_remaining -= fill.cash;
                }
                self.drive_state_machine();
                let mut phase_commands = self.sync_hedge_phase();
                if let Some(settle) = self.enter_settlement_if_locked() {
                    return settle;
                }
                if self.settled {
                    return Vec::new();
                }
                // 对冲阶段：每笔成交后评估要不要再打一步 Taker。
                if self.hedge_phase.is_hedging() {
                    phase_commands.extend(self.hedge_step());
                    return phase_commands;
                }
                // 刚从对冲回退到做市（phase_commands 含 CancelAll），立即重铺梯度。
                if phase_commands.contains(&Command::CancelAll) {
                    phase_commands.extend(self.deploy_ladder());
                    return phase_commands;
                }
                // 常规阶段：只对"本世代 + 主战场侧"的买入成交触发跨侧配对重算。
                // 对面配对成交不触发（防滚雪球），旧世代成交也不触发。
                let is_main_field = self.main_field == Some(fill.side);
                if is_current && is_main_field && fill.direction == OrderDirection::Buy {
                    phase_commands.extend(self.recompute_after_fill(&fill));
                }
                phase_commands
            }
            // 拒单和撤单确认目前不需要做什么。
            ExchangeEvent::Rejected { .. } | ExchangeEvent::Canceled(_) => Vec::new(),
        }
    }

    /// 检测对冲阶段是否切换了（None↔Dynamic↔Ev），切换时重置 Taker 步数并清场。
    /// 阶段切换时产出 CancelAll（撤掉上一阶段的挂单，从干净状态开始新阶段）。
    fn sync_hedge_phase(&mut self) -> Vec<Command> {
        let phase = HedgePhase::of(self.state_machine.state());
        if phase != self.hedge_phase {
            self.hedge_phase = phase;
            self.taker_steps = 0;
            self.pending_orders.clear();
            vec![Command::CancelAll]
        } else {
            Vec::new()
        }
    }

    /// 利润锁定时一次性收手：撤全部挂单、清待补队列、标记 settled。
    /// 返回 Some(撤单指令) 表示刚收手，None 表示没变化。
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

    /// 主战场侧成交后，重算对面的配对挂单。
    fn recompute_after_fill(&mut self, fill: &Fill) -> Vec<Command> {
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

    /// 检查挂起的配对单：配对价 < 对面 Ask 了，说明能以 Maker 挂出去，补挂。
    fn try_post_pending(&mut self) -> Vec<Command> {
        let mut commands = Vec::new();
        let mut still_pending = Vec::new();
        for pending in mem::take(&mut self.pending_orders) {
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

    /// 用当前账本和行情让状态机评估一次，看要不要跳转状态。
    fn drive_state_machine(&mut self) -> Transition {
        let step_budget = self.config.hedge_attack_pool * self.config.hedge_step_fraction;
        // 步数用完且还有预算 → 告诉 FSM 可以回退做市再来一轮。
        // 预算耗尽则不回退，扛到交割。
        let hedge_exhausted = self.hedge_phase.is_hedging()
            && self.taker_steps >= self.config.max_taker_steps
            && self.hedge_budget_remaining >= step_budget;
        let inputs = StepInputs {
            position: self.ledger.snapshot(),
            market: self.market,
            hedge_exhausted,
        };
        self.state_machine.step(&inputs)
    }

    /// 对冲单步：向 strategy 要决策，然后执行。
    /// 每个 tick 最多打一步（防止同价连环打光），达步数上限也停。
    /// 只有真的发出了指令才计步（被风控拦截不算）。
    fn hedge_step(&mut self) -> Vec<Command> {
        if self.taker_steps >= self.config.max_taker_steps {
            return Vec::new();
        }
        if self.hedge_fired_this_tick {
            return Vec::new();
        }

        let step_budget = self.config.hedge_attack_pool * self.config.hedge_step_fraction;
        if self.hedge_budget_remaining < step_budget {
            return Vec::new();
        }

        // 策略决策：追买哪侧、量的上限。
        let decision = self.ladder.decide_hedge_step(
            self.hedge_phase,
            &self.ledger.snapshot(),
            &self.market,
            &self.state_machine.thresholds(),
        );
        let (side, cap) = match decision {
            HedgeDecision::Execute { side, cap } => (side, cap),
            HedgeDecision::Skip => return Vec::new(),
        };

        // 执行。
        self.current_generation = self.current_generation.next();
        let commands: Vec<Command> = self
            .ladder
            .hedge_gradient_step(
                side,
                cap,
                &self.market,
                step_budget,
                &self.config.constraints,
                &mut self.id_generator,
                self.current_generation,
            )
            .into_iter()
            .filter(|command| self.approve_command(command))
            .collect();
        if !commands.is_empty() {
            self.taker_steps += 1;
            self.hedge_fired_this_tick = true;
        }
        commands
    }

    /// 铺梯度挂单：选主战场、世代号+1、生成挂单指令、过风控。
    fn deploy_ladder(&mut self) -> Vec<Command> {
        self.main_field = self.ladder.select_main_field(&self.market);
        self.current_generation = self.current_generation.next();
        let commands = self.ladder.deploy(
            &self.market,
            self.config.grid_maker_pool,
            &self.config.constraints,
            &mut self.id_generator,
            self.current_generation,
        );
        commands
            .into_iter()
            .filter(|command| self.approve_command(command))
            .collect()
    }

    /// 过风控：下单指令要过 Cash Guard 审计，撤单指令直接放行。
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
    use domain::types::{OrderRole, Side};
    use risk::pool::CapitalPools;
    use rust_decimal_macros::dec;

    /// 构造测试用事件循环：总资金 1000，默认布阵 / 约束 / 风控。
    fn engine() -> Engine {
        let total_capital = dec!(1000);
        let thresholds = Thresholds {
            hedge_loss_trigger: dec!(30),
            hedge_min_qty: dec!(100),
            profit_target: dec!(15),
        };
        let pools = CapitalPools::with_default_ratios(total_capital);
        let config = EngineConfig {
            grid_maker_pool: pools.grid_maker(),
            hedge_attack_pool: pools.hedge_attack(),
            hedge_step_fraction: dec!(0.2),
            max_taker_steps: 5,
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
            role: OrderRole::Maker,
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
            role: OrderRole::Maker,
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
            role: OrderRole::Maker,
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
        // Up 侧买入成交 50 股、花费 20 → 总持仓 50 < hedge_min_qty 100，不触发对冲，走跨侧重算。
        let commands = engine.handle_event(current_fill(Side::Up, dec!(0.40), dec!(50), dec!(20)));
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
        // 50 股成交 → 总持仓 50 < 100，不触发对冲。
        let commands = engine.handle_event(current_fill(Side::Up, dec!(0.40), dec!(50), dec!(20)));
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
            role: OrderRole::Maker,
            price: dec!(0.40),
            filled_qty: dec!(100),
            cash: dec!(40),
            generation: Generation(1),
        }));
        let settle = engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(1),
            side: Side::Down,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
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
            role: OrderRole::Maker,
            price: dec!(0.30),
            filled_qty: dec!(50),
            cash: dec!(15),
            generation: Generation(1),
        }));
        assert!(after_fill.is_empty());
    }

    /// 构造带完整买卖盘的双边市场快照（Mark Price 可由中间价计算）。
    fn full_book(up_bid: Money, up_ask: Money, down_bid: Money, down_ask: Money) -> ExchangeEvent {
        ExchangeEvent::BookUpdate(MarketSnapshot {
            up: BookTop {
                best_bid: Some(up_bid),
                best_ask: Some(up_ask),
                last_trade: None,
            },
            down: BookTop {
                best_bid: Some(down_bid),
                best_ask: Some(down_ask),
                last_trade: None,
            },
        })
    }

    #[test]
    fn fill_into_dynamic_hedging_emits_taker_step() {
        let mut engine = engine();
        // Up 盘口 ask=0.10，Down 盘口 ask=0.60。主战场 Up（ask < 0.5）。
        engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        engine.start();
        // Down 侧重仓成交 100 股、花 60 → 总持仓 100 ≥ min_qty，
        // up_win_pnl = 0 - 60 = -60 ≤ -30 且 down_win_pnl = 100 - 60 = 40 > 0（单边穿线）。
        // 触发动态对冲，追买瘸腿侧 Up（PnL 更小的一侧）。
        let commands = engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(0),
            side: Side::Down,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.60),
            filled_qty: dec!(100),
            cash: dec!(60),
            generation: Generation(1),
        }));
        assert!(matches!(engine.state(), RobotState::DynamicHedging { .. }));
        // 阶段切换产出 CancelAll + 对冲梯度指令。
        // cap = 100 - 0 = 100（摽齐缺口）。预算 45，Taker: min(45×0.40/0.10=180, 100)=100 股。
        // 剩余 cap=0 → Maker 不铺。所以 CancelAll + 1 Taker = 2 条。
        assert_eq!(commands.len(), 2);
        // 第 1 条：阶段切换的 CancelAll。
        assert_eq!(commands[0], Command::CancelAll);
        // 第 2 条：Up 侧 Taker。
        match &commands[1] {
            Command::SubmitOrder(o) => {
                assert_eq!(o.side, Side::Up);
                assert_eq!(o.role, OrderRole::Taker);
                assert_eq!(o.price, dec!(0.10));
                assert_eq!(o.qty, dec!(100));
            }
            _ => panic!("应为对冲 Taker SubmitOrder"),
        }
    }

    #[test]
    fn hedging_book_update_does_not_emit_taker() {
        let mut engine = engine();
        engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        engine.start();
        // 先进入动态对冲。
        engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(0),
            side: Side::Down,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.60),
            filled_qty: dec!(100),
            cash: dec!(60),
            generation: Generation(1),
        }));
        assert!(matches!(engine.state(), RobotState::DynamicHedging { .. }));
        // 对冲态下，行情更新不发 Taker（对冲推进只由成交驱动，防止连环失控）。
        let on_book =
            engine.handle_event(full_book(dec!(0.05), dec!(0.11), dec!(0.55), dec!(0.60)));
        assert!(on_book.is_empty());
    }

    #[test]
    fn taker_steps_capped_at_max_in_ev_phase() {
        let mut engine = engine();
        // 直接置于 EV 对冲阶段（cap=None，每步纯预算封顶、不会因摽齐自然停手，便于验证步数上限）。
        // Up mark = (0.58+0.62)/2 = 0.60 > 0.5 → 优势方 Up。
        engine.market = MarketSnapshot {
            up: BookTop {
                best_bid: Some(dec!(0.58)),
                best_ask: Some(dec!(0.62)),
                last_trade: None,
            },
            down: BookTop::default(),
        };
        engine.hedge_phase = HedgePhase::Ev;
        engine.taker_steps = 0;
        // 每个 tick 最多打一步，需要在步与步之间插入 BookUpdate 重置标记。
        let mut emitted = 0;
        for _ in 0..7 {
            engine.hedge_fired_this_tick = false; // 模拟新 tick 到来
            if !engine.hedge_step().is_empty() {
                emitted += 1;
            }
        }
        assert_eq!(emitted, 5);
        assert_eq!(engine.taker_steps, 5);
    }

    #[test]
    fn cash_guard_blocks_hedge_taker() {
        let mut engine = engine();
        engine.market = MarketSnapshot {
            up: BookTop {
                best_bid: Some(dec!(0.58)),
                best_ask: Some(dec!(0.62)),
                last_trade: None,
            },
            down: BookTop::default(),
        };
        engine.hedge_phase = HedgePhase::Ev;
        // 可用现金压到红线 250 以下 → Cash Guard 拦截对冲 Taker，不产指令、不计步。
        engine.free_cash = dec!(200);
        let commands = engine.hedge_step();
        assert!(commands.is_empty());
        assert_eq!(engine.taker_steps, 0);
    }

    #[test]
    fn hedge_step_limited_to_one_per_tick() {
        let mut engine = engine();
        engine.market = MarketSnapshot {
            up: BookTop {
                best_bid: Some(dec!(0.58)),
                best_ask: Some(dec!(0.62)),
                last_trade: None,
            },
            down: BookTop::default(),
        };
        engine.hedge_phase = HedgePhase::Ev;
        engine.taker_steps = 0;
        // 第一步：产出梯度指令（1 Taker + N Maker）。
        let first = engine.hedge_step();
        assert!(!first.is_empty());
        assert_eq!(engine.taker_steps, 1);
        // 同一 tick 第二步：被标记拦截，不产指令。
        let second = engine.hedge_step();
        assert!(second.is_empty());
        assert_eq!(engine.taker_steps, 1);
    }

    #[test]
    fn hedge_step_resumes_after_book_update() {
        let mut engine = engine();
        let book = MarketSnapshot {
            up: BookTop {
                best_bid: Some(dec!(0.58)),
                best_ask: Some(dec!(0.62)),
                last_trade: None,
            },
            down: BookTop::default(),
        };
        engine.market = book;
        engine.hedge_phase = HedgePhase::Ev;
        engine.taker_steps = 0;
        // 第一步成功。
        assert!(!engine.hedge_step().is_empty());
        // 同 tick 被拦。
        assert!(engine.hedge_step().is_empty());
        // 模拟新 tick 到来（BookUpdate 会重置标记）。
        engine.hedge_fired_this_tick = false;
        // 下一步又能打了。
        assert!(!engine.hedge_step().is_empty());
        assert_eq!(engine.taker_steps, 2);
    }

    #[test]
    fn hedge_budget_blocks_when_exhausted() {
        let mut engine = engine();
        engine.market = MarketSnapshot {
            up: BookTop {
                best_bid: Some(dec!(0.58)),
                best_ask: Some(dec!(0.62)),
                last_trade: None,
            },
            down: BookTop::default(),
        };
        engine.hedge_phase = HedgePhase::Ev;
        engine.taker_steps = 0;
        // 对冲池剩余设为比单步预算（225×0.2=45）少 → 打不出去。
        engine.hedge_budget_remaining = dec!(10);
        let commands = engine.hedge_step();
        assert!(commands.is_empty());
        // 步数没增（没真的打出去）。
        assert_eq!(engine.taker_steps, 0);
    }

    #[test]
    fn hedge_budget_deducted_on_fill_in_hedging_phase() {
        let mut engine = engine();
        engine.handle_event(book_update(dec!(0.40)));
        engine.start();
        // 手动进入对冲阶段。
        engine.hedge_phase = HedgePhase::Dynamic;
        let initial_budget = engine.hedge_budget_remaining;
        // 模拟一笔对冲阶段的成交，花费 20。
        engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(99),
            side: Side::Up,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.40),
            filled_qty: dec!(48),
            cash: dec!(20),
            generation: Generation(1),
        }));
        // 对冲池剩余应减少 20。
        assert_eq!(engine.hedge_budget_remaining, initial_budget - dec!(20));
    }

    #[test]
    fn hedge_budget_not_deducted_in_normal_phase() {
        let mut engine = engine();
        engine.handle_event(book_update(dec!(0.40)));
        engine.start();
        // 常规做市阶段（非对冲），对冲池不扣。
        let initial_budget = engine.hedge_budget_remaining;
        engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(0),
            side: Side::Up,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.39),
            filled_qty: dec!(50),
            cash: dec!(20),
            generation: Generation(1),
        }));
        assert_eq!(engine.hedge_budget_remaining, initial_budget);
    }

    #[test]
    fn phase_switch_to_dynamic_emits_cancel_all() {
        let mut engine = engine();
        engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        engine.start();
        assert_eq!(engine.state(), RobotState::RangeBoundMaking);
        // 触发进入 DynamicHedging：单边穿线。
        let commands = engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(0),
            side: Side::Down,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.60),
            filled_qty: dec!(100),
            cash: dec!(60),
            generation: Generation(1),
        }));
        assert!(matches!(engine.state(), RobotState::DynamicHedging { .. }));
        // 第一条指令应为阶段切换的 CancelAll。
        assert_eq!(commands[0], Command::CancelAll);
    }

    #[test]
    fn phase_switch_to_ev_emits_cancel_all() {
        let mut engine = engine();
        engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        engine.start();
        // 先进入 DynamicHedging。
        engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(0),
            side: Side::Down,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.60),
            filled_qty: dec!(100),
            cash: dec!(60),
            generation: Generation(1),
        }));
        assert!(matches!(engine.state(), RobotState::DynamicHedging { .. }));
        // 制造双边穿线直接进 Ev（从 RangeBoundMaking 双边穿线直达 Ev 的路径）。
        // 这里通过两次 both_sides_negative 升级：第一次 count 0→1，第二次升 Ev。
        // 构造 both_sides_negative 数据：up_pnl = 100-200 = -100, down_pnl = 50-200 = -150。
        // 但要注意当前持仓是 down=100, cost=60。再加一笔让双边负。
        // 直接手动设置状态来简化测试。
        engine.hedge_phase = HedgePhase::Dynamic;
        // 模拟一笔成交让状态升级到 EvHedging：
        // 当前 down=100, cost=60。再买 Up 50 股花 50 → up=50, down=100, cost=110。
        // up_pnl = 50-110=-60, down_pnl = 100-110=-10 → both_sides_negative → count 0→1。
        engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(1),
            side: Side::Up,
            direction: OrderDirection::Buy,
            role: OrderRole::Taker,
            price: dec!(0.10),
            filled_qty: dec!(50),
            cash: dec!(50),
            generation: Generation(2),
        }));
        // count 从 0 到 1，仍在 Dynamic，HedgePhase 没变。
        assert!(matches!(
            engine.state(),
            RobotState::DynamicHedging {
                double_negative_count: 1
            }
        ));
        // 第二次 both_sides_negative → 升级到 Ev。
        // 再买 Up 20 股花 20 → up=70, down=100, cost=130。
        // up_pnl = 70-130=-60, down_pnl = 100-130=-30 → both_sides_negative → 升 Ev。
        let commands = engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(2),
            side: Side::Up,
            direction: OrderDirection::Buy,
            role: OrderRole::Taker,
            price: dec!(0.10),
            filled_qty: dec!(20),
            cash: dec!(20),
            generation: Generation(2),
        }));
        assert_eq!(engine.state(), RobotState::EvHedging);
        // Dynamic → Ev 切换，应包含 CancelAll。
        assert!(commands.contains(&Command::CancelAll));
    }

    #[test]
    fn phase_switch_clears_pending_orders() {
        let mut engine = engine();
        // Up Ask 0.40，Down Ask 仅 0.55（配对价 0.58 ≥ 0.55 → 产生 Pending）。
        engine.handle_event(book_update_both(dec!(0.40), dec!(0.55)));
        engine.start();
        // 成交触发配对挂起。
        engine.handle_event(current_fill(Side::Up, dec!(0.40), dec!(50), dec!(20)));
        // 此时应有 pending order。
        assert!(!engine.pending_orders.is_empty());
        // 触发阶段切换（制造穿线进入 DynamicHedging）。
        // 再来一笔大额 Down 成交让 up_pnl 穿线。
        // 当前 up=50, cost=20。再 Down 成交 100 股花 60 → up=50, down=100, cost=80。
        // up_pnl = 50-80=-30 ≤ -30 且总持仓 150 ≥ 100 → 触发。
        engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(5),
            side: Side::Down,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.55),
            filled_qty: dec!(100),
            cash: dec!(60),
            generation: Generation(1),
        }));
        assert!(matches!(engine.state(), RobotState::DynamicHedging { .. }));
        // 阶段切换后 pending_orders 应被清空。
        assert!(engine.pending_orders.is_empty());
    }

    #[test]
    fn double_negative_count_increment_does_not_cancel() {
        let mut engine = engine();
        engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        engine.start();
        // 进入 DynamicHedging。
        engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(0),
            side: Side::Down,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.60),
            filled_qty: dec!(100),
            cash: dec!(60),
            generation: Generation(1),
        }));
        assert!(matches!(
            engine.state(),
            RobotState::DynamicHedging {
                double_negative_count: 0
            }
        ));
        // 制造 both_sides_negative 让 count 从 0 升到 1（不切换 HedgePhase）。
        // 当前 down=100, cost=60。买 Up 50 花 50 → up=50, down=100, cost=110。
        // up_pnl=-60, down_pnl=-10 → both_sides_negative → count 1。
        let commands = engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(1),
            side: Side::Up,
            direction: OrderDirection::Buy,
            role: OrderRole::Taker,
            price: dec!(0.10),
            filled_qty: dec!(50),
            cash: dec!(50),
            generation: Generation(2),
        }));
        assert!(matches!(
            engine.state(),
            RobotState::DynamicHedging {
                double_negative_count: 1
            }
        ));
        // count 自增不触发 CancelAll（HedgePhase 仍是 Dynamic，没切换）。
        assert!(!commands.contains(&Command::CancelAll));
    }

    #[test]
    fn hedge_exhausted_triggers_cancel_and_redeploy() {
        let mut engine = engine();
        engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        engine.start();
        // 进入 DynamicHedging。
        engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(0),
            side: Side::Down,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.60),
            filled_qty: dec!(100),
            cash: dec!(60),
            generation: Generation(1),
        }));
        assert!(matches!(engine.state(), RobotState::DynamicHedging { .. }));
        // 步数设满 + 预算充足 → hedge_exhausted=true → 回退 RangeBoundMaking。
        engine.taker_steps = engine.config.max_taker_steps;
        assert!(engine.hedge_budget_remaining >= engine.config.hedge_attack_pool * engine.config.hedge_step_fraction);
        let commands =
            engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        assert_eq!(engine.state(), RobotState::RangeBoundMaking);
        // 应包含 CancelAll（阶段切换清场）+ SubmitOrder（重铺梯度）。
        assert!(commands.contains(&Command::CancelAll));
        assert!(commands.iter().any(|c| matches!(c, Command::SubmitOrder(_))));
    }

    #[test]
    fn budget_exhausted_stays_in_hedging() {
        let mut engine = engine();
        engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        engine.start();
        // 进入 DynamicHedging。
        engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(0),
            side: Side::Down,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.60),
            filled_qty: dec!(100),
            cash: dec!(60),
            generation: Generation(1),
        }));
        assert!(matches!(engine.state(), RobotState::DynamicHedging { .. }));
        // 步数满 + 预算耗尽 → 不回退。
        engine.taker_steps = engine.config.max_taker_steps;
        engine.hedge_budget_remaining = dec!(0);
        let commands =
            engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        // 应留在 DynamicHedging（扛到交割）。
        assert!(matches!(engine.state(), RobotState::DynamicHedging { .. }));
        assert!(!commands.contains(&Command::CancelAll));
    }

    #[test]
    fn reentry_after_return_resets_steps() {
        let mut engine = engine();
        engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        engine.start();
        // 进入 DynamicHedging。
        engine.handle_event(ExchangeEvent::Filled(Fill {
            order_id: OrderId(0),
            side: Side::Down,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price: dec!(0.60),
            filled_qty: dec!(100),
            cash: dec!(60),
            generation: Generation(1),
        }));
        // 步数设满 → 触发回退。
        engine.taker_steps = engine.config.max_taker_steps;
        engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        assert_eq!(engine.state(), RobotState::RangeBoundMaking);
        // 再触发进入 DynamicHedging（仍穿线：up_pnl=0-60=-60 ≤ -30）。
        engine.handle_event(full_book(dec!(0.05), dec!(0.10), dec!(0.55), dec!(0.60)));
        assert!(matches!(engine.state(), RobotState::DynamicHedging { .. }));
        // 步数应从 0 重新开始。
        assert_eq!(engine.taker_steps, 0);
    }
}
