//! 场内阶段：一场 15 分钟周期里策略所处的四个阶段。
//!
//! 阶段由场内进度（progress）驱动，线性向前推进，不回退：
//! Building → Cycling → Harvesting → Settled。
//!
//! 本枚举只定义「有哪些阶段」和「哪些推进合法」，决策逻辑在 strategy crate。

use serde::{Deserialize, Serialize};

/// 场内阶段。
///
/// 推进顺序（progress 从小到大）：
/// - `Building`：开场建仓，双边铺 Maker 买单铺底仓。
/// - `Cycling`：循环做市，持续补买 + 逐笔止盈 + 快速止损。
/// - `Harvesting`：收手变现，停新买单、继续挂止盈卖单收割。
/// - `Settled`：完全停手，只保留净持仓等结算。终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Phase {
    #[default]
    Building,
    Cycling,
    Harvesting,
    Settled,
}

impl Phase {
    /// 推进序号：越大越靠后。用于判断推进方向是否合法。
    fn order(self) -> u8 {
        match self {
            Phase::Building => 0,
            Phase::Cycling => 1,
            Phase::Harvesting => 2,
            Phase::Settled => 3,
        }
    }

    /// 能否推进到目标阶段。
    ///
    /// 只准向前（含原地不动），不准回退。progress 可能因数据缺口跳跃，
    /// 故允许跨阶段前进（如 Building 直接到 Harvesting）。
    pub fn can_transition_to(self, target: Phase) -> bool {
        target.order() >= self.order()
    }

    /// 是否为终态（不再有任何出边）。
    pub fn is_terminal(self) -> bool {
        matches!(self, Phase::Settled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_building() {
        assert_eq!(Phase::default(), Phase::Building);
    }

    #[test]
    fn only_settled_is_terminal() {
        assert!(Phase::Settled.is_terminal());
        assert!(!Phase::Building.is_terminal());
        assert!(!Phase::Cycling.is_terminal());
        assert!(!Phase::Harvesting.is_terminal());
    }

    #[test]
    fn forward_transitions_are_legal() {
        assert!(Phase::Building.can_transition_to(Phase::Cycling));
        assert!(Phase::Cycling.can_transition_to(Phase::Harvesting));
        assert!(Phase::Harvesting.can_transition_to(Phase::Settled));
    }

    #[test]
    fn skipping_forward_is_legal() {
        // progress 跳跃时允许跨阶段前进。
        assert!(Phase::Building.can_transition_to(Phase::Harvesting));
        assert!(Phase::Building.can_transition_to(Phase::Settled));
        assert!(Phase::Cycling.can_transition_to(Phase::Settled));
    }

    #[test]
    fn staying_in_place_is_legal() {
        assert!(Phase::Cycling.can_transition_to(Phase::Cycling));
    }

    #[test]
    fn backward_transitions_are_illegal() {
        assert!(!Phase::Cycling.can_transition_to(Phase::Building));
        assert!(!Phase::Harvesting.can_transition_to(Phase::Cycling));
        assert!(!Phase::Settled.can_transition_to(Phase::Harvesting));
    }
}
