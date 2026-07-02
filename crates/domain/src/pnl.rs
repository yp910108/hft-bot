//! 盈亏口径：结算盈亏、浮亏。纯函数无 IO。
//!
//! 新策略靠循环做市 + 净持仓扛结算赚钱，PnL 分两块：
//! - **已实现盈亏**：场内循环卖出赚的差价，由 inventory 累计，不在本模块。
//! - **结算盈亏**：收手时保留的净持仓，交割时兑现 = 赢家侧净持仓 − 净持仓成本。
//!
//! 二元市场胜方每股值 1 美元、败方 0。阈值比较放在 strategy 层，本模块只出原始数值。

use crate::types::{Money, Price, Qty, Side};

/// 某一时点的净持仓快照。按侧记录净股数与净成本，供结算与诊断用。
///
/// 「净」指已扣除卖出减仓后的剩余：net_qty = 买入 − 卖出，net_cost 同步扣减。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionSnapshot {
    /// Up 侧净持仓股数。
    pub up_qty: Qty,
    /// Down 侧净持仓股数。
    pub down_qty: Qty,
    /// Up 侧净持仓成本。
    pub up_cost: Money,
    /// Down 侧净持仓成本。
    pub down_cost: Money,
}

impl PositionSnapshot {
    /// 取指定侧的净持仓股数。
    pub fn qty(&self, side: Side) -> Qty {
        match side {
            Side::Up => self.up_qty,
            Side::Down => self.down_qty,
        }
    }

    /// 取指定侧的净持仓成本。
    pub fn cost(&self, side: Side) -> Money {
        match side {
            Side::Up => self.up_cost,
            Side::Down => self.down_cost,
        }
    }

    /// 双边净持仓总成本。
    pub fn total_cost(&self) -> Money {
        self.up_cost + self.down_cost
    }

    /// 指定侧的净持仓均价 = 该侧成本 ÷ 该侧股数。无持仓返回 None。
    pub fn average_price(&self, side: Side) -> Option<Price> {
        let qty = self.qty(side);
        if qty > Qty::ZERO {
            Some(self.cost(side) / qty)
        } else {
            None
        }
    }

    /// sum_avg = 两侧净持仓均价之和。< 1 时无论谁赢结算都赚。
    ///
    /// 任一侧无持仓时该侧均价按 0 计（单边持仓不构成双赢结构，sum_avg 意义有限，
    /// 但仍返回有持仓侧的均价便于诊断）。
    pub fn sum_avg(&self) -> Price {
        let up = self.average_price(Side::Up).unwrap_or(Money::ZERO);
        let down = self.average_price(Side::Down).unwrap_or(Money::ZERO);
        up + down
    }

    /// 结算盈亏：给定赢家侧，交割兑现的盈亏 = 赢家侧净持仓 − 双边净持仓总成本。
    ///
    /// 胜方每股兑付 1 美元，故赢家侧回收金额即其净持仓股数。
    /// 这是**净持仓部分**的盈亏，完整场盈亏还需加上 inventory 的已实现盈亏。
    pub fn settle_pnl(&self, winner: Side) -> Money {
        self.qty(winner) - self.total_cost()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn pos(up_qty: Qty, down_qty: Qty, up_cost: Money, down_cost: Money) -> PositionSnapshot {
        PositionSnapshot {
            up_qty,
            down_qty,
            up_cost,
            down_cost,
        }
    }

    #[test]
    fn settle_pnl_is_winner_qty_minus_total_cost() {
        // 总成本 = 40 + 50 = 90。Up 赢拿回 100 → +10；Down 赢拿回 80 → −10。
        let p = pos(dec!(100), dec!(80), dec!(40), dec!(50));
        assert_eq!(p.total_cost(), dec!(90));
        assert_eq!(p.settle_pnl(Side::Up), dec!(10));
        assert_eq!(p.settle_pnl(Side::Down), dec!(-10));
    }

    #[test]
    fn average_price_is_cost_over_qty() {
        let p = pos(dec!(100), dec!(0), dec!(45), dec!(0));
        assert_eq!(p.average_price(Side::Up), Some(dec!(0.45)));
        assert_eq!(p.average_price(Side::Down), None);
    }

    #[test]
    fn sum_avg_adds_both_side_averages() {
        // Up 均价 0.45，Down 均价 0.50 → sum_avg = 0.95 < 1，双赢结构。
        let p = pos(dec!(100), dec!(100), dec!(45), dec!(50));
        assert_eq!(p.sum_avg(), dec!(0.95));
    }

    #[test]
    fn sum_avg_counts_missing_side_as_zero() {
        // 只有 Up 持仓 → sum_avg = Up 均价。
        let p = pos(dec!(100), dec!(0), dec!(45), dec!(0));
        assert_eq!(p.sum_avg(), dec!(0.45));
    }
}
