//! 落地决策：把小策略产出的指令意图变成真实指令（分配 ID、过风控、更新镜像），
//! 并应用状态跳转。这是 engine 唯一改挂单簿与状态机的地方。

use crate::config::Pool;
use crate::Engine;
use domain::order::{Command, Order};
use domain::state::RobotState;
use risk::auditor::Approval;
use strategy::context::{CommandIntent, Decision, OrderIntent};

impl Engine {
    /// 落地决策：逐条指令分配 ID、过风控、产出 Command、更新镜像；最后应用状态跳转。
    pub(crate) fn apply_decision(&mut self, decision: Decision) -> Vec<Command> {
        let mut commands = Vec::new();
        let state = self.machine.state();
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
                    // 配对态因敞口撤主战场 → 本场永久停铺。
                    if state == RobotState::Pairing && Some(*side) == self.main_field {
                        self.main_field_frozen = true;
                    }
                    commands.push(Command::CancelSide(*side));
                }
                CommandIntent::CancelAll => {
                    self.book.mark_all_cancel_pending();
                    commands.push(Command::CancelAll);
                }
            }
        }

        // 应用状态跳转。
        if let Some(target) = decision.transition {
            self.machine.transition_to(target);
        }
        // 更新「曾达最深阶段」诊断。
        let depth = Self::phase_depth(self.machine.state());
        if depth > self.deepest_phase {
            self.deepest_phase = depth;
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
            self.last_hedge_at = Some(self.now);
        }
        Some(Command::SubmitOrder(order))
    }

    /// 阶段推进深度（用于诊断「曾达最深阶段」）。
    /// 建仓0 < 配对1 < 观察2 < 动态对冲3 < EV 4；熔断/结算不计深度。
    fn phase_depth(state: RobotState) -> u8 {
        match state {
            RobotState::Building => 0,
            RobotState::Pairing => 1,
            RobotState::Observing { .. } => 2,
            RobotState::DynamicHedge { .. } => 3,
            RobotState::EvHedge => 4,
            RobotState::CircuitBreaker | RobotState::SettlementWait => 0,
        }
    }

    /// 本场曾到达的最深阶段标签（回测诊断用）。
    pub fn deepest_phase_label(&self) -> &'static str {
        match self.deepest_phase {
            0 => "Building",
            1 => "Pairing",
            2 => "Observing",
            3 => "DynamicHedge",
            4 => "EvHedge",
            _ => "Unknown",
        }
    }
}
