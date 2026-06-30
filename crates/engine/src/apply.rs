//! 落地决策：把小策略产出的指令意图变成真实指令（分配 ID、过风控、更新镜像），
//! 并应用状态跳转与全局量更新。这是 engine 唯一改挂单簿、状态机、全局量的地方。

use crate::config::Pool;
use crate::core::Engine;
use domain::command::Command;
use domain::order::Order;
use domain::state::RobotState;
use risk::auditor::Approval;
use strategy::context::{CommandIntent, Decision, OrderIntent};

impl Engine {
    /// 落地决策：逐条指令分配 ID、过风控、产出 Command、更新镜像；
    /// 再应用状态跳转与全局量更新。
    pub(crate) fn apply_decision(&mut self, decision: Decision) -> Vec<Command> {
        let mut commands = Vec::new();
        let state = self.round.state;
        let pool = Self::pool_for_state(state);

        for intent in &decision.commands {
            match intent {
                CommandIntent::Submit(order_intent) => {
                    if let Some(cmd) = self.try_submit(order_intent, pool) {
                        commands.push(cmd);
                    }
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

        // 应用显式全局量更新意图（strategy 表达、engine 无脑写入，不猜）。
        if decision.freeze_main_field {
            self.round.freeze_main_field();
        }
        if decision.mark_funds_exhausted {
            self.round.mark_funds_exhausted();
        }

        // 应用双边负计数更新意图。
        if let Some((count, was)) = decision.double_negative_update {
            self.round.update_double_negative(count, was);
        }

        // 应用状态跳转。跳转前校验合法性（安全网，非法跳转在开发期暴露）。
        // 跳转时推进世代号，隔离旧世代成交不误触新阶段逻辑。
        if let Some(target) = decision.transition {
            debug_assert!(
                fsm::is_legal_transition(self.round.state, target),
                "非法状态跳转: {:?} → {:?}",
                self.round.state,
                target,
            );
            self.round.state = target;
            self.generation = self.generation.next();
            // 诊断：更新本场曾达最深阶段。
            let depth = Self::phase_depth(target);
            if depth > self.deepest_phase {
                self.deepest_phase = depth;
            }
        }

        commands
    }

    /// 尝试提交一笔订单意图：过 Cash Guard，通过则分配 ID/世代、登记镜像、产出 Command。
    fn try_submit(&mut self, intent: &OrderIntent, pool: Pool) -> Option<Command> {
        let order_id = self.id_gen.next();
        let order = Order {
            order_id,
            side: intent.side,
            direction: intent.direction,
            price: intent.price,
            qty: intent.qty,
            role: intent.role,
            time_in_force: intent.time_in_force,
            generation: self.generation,
        };
        // Cash Guard：可用现金低于红线则拒发。
        if self.auditor.approve(&order, self.free_cash()) != Approval::Approved {
            return None;
        }
        self.book.insert(order);
        self.order_pool.insert(order_id, pool);
        // 对冲单记录时间戳（冷却用）。
        if matches!(pool, Pool::Dynamic | Pool::Ev) {
            self.round.record_hedge_at(self.now);
        }
        Some(Command::SubmitOrder(order))
    }

    /// 某状态下新挂单出自哪个池。
    fn pool_for_state(state: RobotState) -> Pool {
        match state {
            RobotState::Building | RobotState::Pairing => Pool::GridMaker,
            RobotState::DynamicHedge => Pool::Dynamic,
            RobotState::EvHedge => Pool::Ev,
            RobotState::CircuitBreaker | RobotState::SettlementWait => Pool::GridMaker,
        }
    }

    /// 阶段推进深度（诊断用）：Building 0 < Pairing 1 < DynamicHedge 2 < EvHedge 3。
    fn phase_depth(state: RobotState) -> u8 {
        match state {
            RobotState::Building => 0,
            RobotState::Pairing => 1,
            RobotState::DynamicHedge => 2,
            RobotState::EvHedge => 3,
            RobotState::CircuitBreaker | RobotState::SettlementWait => 0,
        }
    }
}
