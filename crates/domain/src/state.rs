//! 机器人有限状态机的状态枚举。
//!
//! 对应策略说明书的四阶段 + 两个全局态（共 6 状态）。所有状态切换由单写者事件循环串行驱动，
//! 每个 tick 先过全局优先级链（见 strategy::router）再进入当前阶段逻辑。
//!
//! 本枚举只定义「有哪些状态」，合法转移校验在 fsm crate，决策逻辑在 strategy crate。

use serde::{Deserialize, Serialize};

/// 机器人状态。
///
/// 优先级链（高 → 低）：时间红线(SettlementWait) > 熔断(CircuitBreaker)
/// > EvHedge > DynamicHedge > Building/Pairing。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RobotState {
    /// 建仓态：开局在主战场侧铺三档梯度单，等首笔成交。
    #[default]
    Building,
    /// 配对态：首笔成交后常驻，主战场成交触发配对重算。
    Pairing,
    /// 动态对冲态：Maker 织网补少仓侧摽齐（Delta Neutral）。
    ///
    /// 无载荷的干净状态。「双边负」边沿计数等跨阶段风险量提升为全局上下文，由 engine 统一维护
    /// （见 strategy::context），不再塞进状态载荷——避免每个转移点手工搬运易漏。
    /// 观察（pnl 在安全区间、或敞口撞红线、或资金耗尽）时留在本状态、策略层 Skip，
    /// 不再单独设 Observing 状态——和 EV 装死、做市挂机的模式统一。
    DynamicHedge,
    /// EV 对冲态：IOC Taker 顺势单边押注（战略翻转）。
    EvHedge,
    /// 熔断求生态：Spread 崩溃，CancelAll 后等流动性恢复重走全局路由。
    CircuitBreaker,
    /// 等待结算态：终态。撤所有单、扛持仓到 15 分钟交割。
    SettlementWait,
}

impl RobotState {
    /// 是否为终态（不再有任何出边）。
    pub fn is_terminal(self) -> bool {
        matches!(self, RobotState::SettlementWait)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_building() {
        assert_eq!(RobotState::default(), RobotState::Building);
    }

    #[test]
    fn only_settlement_wait_is_terminal() {
        assert!(RobotState::SettlementWait.is_terminal());
        assert!(!RobotState::Building.is_terminal());
        assert!(!RobotState::DynamicHedge.is_terminal());
        assert!(!RobotState::EvHedge.is_terminal());
        assert!(!RobotState::CircuitBreaker.is_terminal());
    }
}
