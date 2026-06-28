//! 有限状态机：只管「状态间的转移合不合法」，不含任何决策逻辑。
//!
//! 纯逻辑零 IO。决策（该不该跳、跳去哪）由 strategy::router 按策略规则算出，
//! 这里只负责校验：router 想要的跳转是不是一条合法的边。把「决策」和「合法性」分开，
//! 让非法跳转在开发期就被测试逮住，而不是埋进策略逻辑里。
//!
//! 合法转移图（对应策略说明书的优先级链与阶段流转）：
//! - 时间红线：任意非终态 → SettlementWait（最高优先级硬中断）。
//! - 熔断：任意非终态、非熔断态 → CircuitBreaker。
//! - 熔断恢复：CircuitBreaker → 任意阶段态（带记忆重走全局路由）。
//! - 阶段推进：Building → Pairing → DynamicHedge → EvHedge。

use domain::state::RobotState;

/// 校验从 `from` 到 `to` 的转移是否合法。
///
/// `to == from` 视为合法（原地不动）。终态 SettlementWait 无任何出边。
pub fn is_legal_transition(from: RobotState, to: RobotState) -> bool {
    use RobotState::*;

    // 原地不动总是合法。
    if from == to {
        return true;
    }

    // 终态没有出边。
    if from.is_terminal() {
        return false;
    }

    // 时间红线：任意非终态都能进结算等待。
    if to == SettlementWait {
        return true;
    }

    // 熔断：任意非终态、非熔断态都能进熔断求生。
    if to == CircuitBreaker {
        return from != CircuitBreaker;
    }

    match (from, to) {
        // 建仓 → 配对（首笔成交）。
        (Building, Pairing) => true,

        // 配对 → 动态对冲（结算/浮亏穿线 或 尾盘规则）。
        (Pairing, DynamicHedge { .. }) => true,

        // 动态对冲 → EV（TTE<5min 且双边负2次/尾盘破线）。
        (DynamicHedge { .. }, EvHedge) => true,

        // 配对 → EV（TTE<5min 尾盘规则可直接从配对态进 EV）。
        (Pairing, EvHedge) => true,

        // 熔断恢复：带记忆重走路由，可落到任意阶段态。
        (CircuitBreaker, Building) => true,
        (CircuitBreaker, Pairing) => true,
        (CircuitBreaker, DynamicHedge { .. }) => true,
        (CircuitBreaker, EvHedge) => true,

        _ => false,
    }
}

/// 状态机：记住当前状态，按合法转移图迁移。
#[derive(Debug, Clone)]
pub struct StateMachine {
    state: RobotState,
}

impl StateMachine {
    /// 创建状态机，初始为 [`RobotState::Building`]。
    pub fn new() -> Self {
        Self {
            state: RobotState::default(),
        }
    }

    /// 当前状态。
    pub fn state(&self) -> RobotState {
        self.state
    }

    /// 尝试迁移到目标状态。合法则迁移并返回 true，非法则保持原状返回 false。
    pub fn transition_to(&mut self, to: RobotState) -> bool {
        if is_legal_transition(self.state, to) {
            self.state = to;
            true
        } else {
            false
        }
    }
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RobotState::*;

    fn dynamic(n: u8) -> RobotState {
        DynamicHedge {
            double_negative_count: n,
        }
    }

    #[test]
    fn starts_in_building() {
        assert_eq!(StateMachine::new().state(), Building);
    }

    #[test]
    fn same_state_is_always_legal() {
        assert!(is_legal_transition(Building, Building));
        assert!(is_legal_transition(EvHedge, EvHedge));
        assert!(is_legal_transition(SettlementWait, SettlementWait));
    }

    #[test]
    fn settlement_wait_is_terminal_no_exit() {
        assert!(!is_legal_transition(SettlementWait, Building));
        assert!(!is_legal_transition(SettlementWait, EvHedge));
        assert!(!is_legal_transition(SettlementWait, CircuitBreaker));
    }

    #[test]
    fn time_red_line_reaches_settlement_from_any_phase() {
        for from in [Building, Pairing, dynamic(0), EvHedge, CircuitBreaker] {
            assert!(
                is_legal_transition(from, SettlementWait),
                "{from:?} 应能进 SettlementWait"
            );
        }
    }

    #[test]
    fn circuit_breaker_reachable_from_any_phase_except_itself() {
        for from in [Building, Pairing, dynamic(0), EvHedge] {
            assert!(
                is_legal_transition(from, CircuitBreaker),
                "{from:?} 应能进 CircuitBreaker"
            );
        }
    }

    #[test]
    fn normal_phase_progression_is_legal() {
        assert!(is_legal_transition(Building, Pairing));
        assert!(is_legal_transition(Pairing, dynamic(0)));
        assert!(is_legal_transition(dynamic(1), EvHedge));
        // 配对态也能直接进 EV（尾盘规则）。
        assert!(is_legal_transition(Pairing, EvHedge));
    }

    #[test]
    fn circuit_breaker_recovery_reaches_any_phase() {
        for to in [Building, Pairing, dynamic(0), EvHedge] {
            assert!(
                is_legal_transition(CircuitBreaker, to),
                "熔断恢复应能到 {to:?}"
            );
        }
    }

    #[test]
    fn illegal_skips_are_rejected() {
        // 建仓不能直接跳对冲（必须先进配对）。
        assert!(!is_legal_transition(Building, dynamic(0)));
        // 建仓不能直接跳 EV。
        assert!(!is_legal_transition(Building, EvHedge));
        // EV 不能退回动态对冲（认输后不回头）。
        assert!(!is_legal_transition(EvHedge, dynamic(0)));
        // 配对不能退回建仓。
        assert!(!is_legal_transition(Pairing, Building));
    }

    #[test]
    fn transition_to_applies_only_legal_moves() {
        let mut machine = StateMachine::new();
        assert!(machine.transition_to(Pairing));
        assert_eq!(machine.state(), Pairing);
        // 非法跳转保持原状。
        assert!(!machine.transition_to(Building));
        assert_eq!(machine.state(), Pairing);
        // 合法跳转生效。
        assert!(machine.transition_to(dynamic(0)));
        assert_eq!(machine.state(), dynamic(0));
    }
}
