//! 条件盈亏（Conditional PnL）与数学期望（EV）计算。
//!
//! 系统所有状态切换与执行决策的底层数据源，纯函数无 IO。
//!
//! 判定「双向盈利」一律以真实盈亏公式 `min(Q_up, Q_down) > C_total` 为准，
//! 不使用「双边均价之和 < 1」这一仅在两边股数相等时才等价的近似。

use crate::types::{Money, Qty};

/// 某一时点的持仓快照，用于计算条件盈亏与数学期望。
///
/// `total_cost` 是双边累计投入的净总成本，已包含所有已发生的手续费。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionSnapshot {
    /// 持有的 Up 侧股数。
    pub up_qty: Qty,
    /// 持有的 Down 侧股数。
    pub down_qty: Qty,
    /// 双边累计净总成本（含手续费）。
    pub total_cost: Money,
}

impl PositionSnapshot {
    /// 若最终 Up 侧胜出的条件盈亏 = Up 股数 × 1 − 总成本。
    ///
    /// 每股胜出侧在交割时兑付 1 美元，故 Up 胜出时回收金额即为 `up_qty`。
    pub fn up_win_pnl(&self) -> Money {
        self.up_qty - self.total_cost
    }

    /// 若最终 Down 侧胜出的条件盈亏 = Down 股数 × 1 − 总成本。
    pub fn down_win_pnl(&self) -> Money {
        self.down_qty - self.total_cost
    }

    /// 是否已锁定双向利润：无论哪一侧胜出，条件盈亏均严格为正。
    ///
    /// 等价于真实条件 `min(up_qty, down_qty) > total_cost`。
    pub fn is_profit_locked(&self) -> bool {
        self.up_win_pnl() > Money::ZERO && self.down_win_pnl() > Money::ZERO
    }

    /// 是否两边条件 PnL 同时为负：无论哪一侧胜出均亏损。
    ///
    /// 与 [`Self::is_profit_locked`] 对称，用于对冲阶段判定双边瘸腿恶化。
    pub fn both_sides_negative(&self) -> bool {
        self.up_win_pnl() < Money::ZERO && self.down_win_pnl() < Money::ZERO
    }

    /// 计算交割数学期望（EV）。
    ///
    /// `up_win_probability` 为 Up 侧最终胜出的概率 p（取值 [0, 1]），
    /// Down 侧胜出概率为 q = 1 − p：
    ///
    /// `EV = p × up_win_pnl + (1 − p) × down_win_pnl`
    pub fn expected_value(&self, up_win_probability: Money) -> Money {
        let q = Money::ONE - up_win_probability;
        up_win_probability * self.up_win_pnl() + q * self.down_win_pnl()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn up_and_down_win_pnl_are_qty_minus_cost() {
        let pos = PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(80),
            total_cost: dec!(90),
        };
        assert_eq!(pos.up_win_pnl(), dec!(10));
        assert_eq!(pos.down_win_pnl(), dec!(-10));
    }

    #[test]
    fn profit_is_locked_only_when_both_sides_positive() {
        // 两边股数均高于总成本 → 双向锁定利润。
        let locked = PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(95),
            total_cost: dec!(90),
        };
        assert!(locked.is_profit_locked());

        // 仅 Up 侧为正、Down 侧为负 → 未锁定。
        let one_sided = PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(80),
            total_cost: dec!(90),
        };
        assert!(!one_sided.is_profit_locked());
    }

    #[test]
    fn profit_not_locked_when_min_qty_equals_cost() {
        // 边界：min(qty) 恰等于成本时，该侧 PnL 为 0，不算严格为正。
        let pos = PositionSnapshot {
            up_qty: dec!(90),
            down_qty: dec!(90),
            total_cost: dec!(90),
        };
        assert!(!pos.is_profit_locked());
    }

    #[test]
    fn both_sides_negative_only_when_both_pnl_below_zero() {
        // 两边股数均低于成本 → 双边皆负。
        let both_negative = PositionSnapshot {
            up_qty: dec!(40),
            down_qty: dec!(50),
            total_cost: dec!(100),
        };
        assert!(both_negative.both_sides_negative());

        // 仅一侧为负 → 不算双边皆负。
        let one_sided = PositionSnapshot {
            up_qty: dec!(120),
            down_qty: dec!(80),
            total_cost: dec!(100),
        };
        assert!(!one_sided.both_sides_negative());
    }

    #[test]
    fn both_sides_negative_false_when_pnl_equals_zero() {
        // 边界：某侧 PnL 恰为 0 时不算严格为负。
        let pos = PositionSnapshot {
            up_qty: dec!(100),
            down_qty: dec!(40),
            total_cost: dec!(100),
        };
        // up_win_pnl = 0（非负），故不构成双边皆负。
        assert!(!pos.both_sides_negative());
    }

    #[test]
    fn expected_value_weighted_by_probability() {
        let pos = PositionSnapshot {
            up_qty: dec!(120),
            down_qty: dec!(80),
            total_cost: dec!(100),
        };
        // up_win_pnl = 20, down_win_pnl = -20。
        // p = 0.6 → EV = 0.6×20 + 0.4×(−20) = 12 − 8 = 4。
        assert_eq!(pos.expected_value(dec!(0.6)), dec!(4.0));
    }

    #[test]
    fn expected_value_at_certain_outcomes() {
        let pos = PositionSnapshot {
            up_qty: dec!(120),
            down_qty: dec!(80),
            total_cost: dec!(100),
        };
        // p = 1 → EV 退化为 up_win_pnl；p = 0 → EV 退化为 down_win_pnl。
        assert_eq!(pos.expected_value(dec!(1)), dec!(20));
        assert_eq!(pos.expected_value(dec!(0)), dec!(-20));
    }
}
