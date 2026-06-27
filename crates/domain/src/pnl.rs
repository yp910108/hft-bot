//! 盈亏口径：结算盈亏、浮亏、数学期望。纯函数无 IO。
//!
//! 两个独立口径，抓两件不同的事（见策略说明书第三节）：
//! - **结算 pnl**：该侧若赢能拿回多少 = 该侧股数 − 双边总成本。二元市场胜方每股值 1。
//! - **浮亏 pnl**：该侧现在按 best_bid 强平能拿回多少 = 该侧股数 × bid − 该侧成本。
//!   逐侧算、只对有持仓的侧算、用 bid（最坏情况），是趋势盘的底线防守信号。
//!
//! 阈值（利润锁定线、亏损触发线等都相对总资金 V）的比较放在 strategy 层，
//! 本模块只产出原始盈亏数值，不含任何阈值。

use crate::types::{Money, Price, Qty, Side};

/// 某一时点的持仓快照。按侧分别记录股数与成本，便于算逐侧浮亏。
///
/// 成本是该侧累计投入的净现金（含已发生手续费）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionSnapshot {
    /// Up 侧净入仓股数。
    pub up_qty: Qty,
    /// Down 侧净入仓股数。
    pub down_qty: Qty,
    /// Up 侧累计投入成本（含费）。
    pub up_cost: Money,
    /// Down 侧累计投入成本（含费）。
    pub down_cost: Money,
}

impl PositionSnapshot {
    /// 取指定侧的持仓股数。
    pub fn qty(&self, side: Side) -> Qty {
        match side {
            Side::Up => self.up_qty,
            Side::Down => self.down_qty,
        }
    }

    /// 取指定侧的投入成本。
    pub fn cost(&self, side: Side) -> Money {
        match side {
            Side::Up => self.up_cost,
            Side::Down => self.down_cost,
        }
    }

    /// 双边总成本 = 两侧成本之和。结算 pnl 的减数。
    pub fn total_cost(&self) -> Money {
        self.up_cost + self.down_cost
    }

    /// 指定侧的成交均价 = 该侧成本 ÷ 该侧股数。无持仓返回 None。
    pub fn average_price(&self, side: Side) -> Option<Price> {
        let qty = self.qty(side);
        if qty > Qty::ZERO {
            Some(self.cost(side) / qty)
        } else {
            None
        }
    }

    /// 结算 pnl：该侧若赢能拿回多少 = 该侧股数 − 双边总成本。
    ///
    /// 胜方每股交割兑付 1 美元，故 Up 赢时回收金额即 up_qty。
    pub fn settle_pnl(&self, side: Side) -> Money {
        self.qty(side) - self.total_cost()
    }

    /// 浮亏 pnl：该侧现在按 best_bid 强平能拿回多少 = 该侧股数 × bid − 该侧成本。
    ///
    /// 用本侧成本、用 bid（现在立即割肉的最坏价）。只对有持仓的侧有意义，
    /// 无持仓返回 None（没仓位谈不上浮亏）。
    pub fn float_pnl(&self, side: Side, best_bid: Price) -> Option<Money> {
        let qty = self.qty(side);
        if qty > Qty::ZERO {
            Some(qty * best_bid - self.cost(side))
        } else {
            None
        }
    }

    /// 两侧结算 pnl 是否同时为负：无论哪侧赢都亏。
    ///
    /// EV 升级计数器的判定依据（双边同时亏说明 sum_avg > 1 且两侧高位套牢）。
    pub fn both_sides_settle_negative(&self) -> bool {
        self.settle_pnl(Side::Up) < Money::ZERO && self.settle_pnl(Side::Down) < Money::ZERO
    }

    /// 结算 pnl 更小（亏损更大）的一侧。两侧相等返回 None。
    ///
    /// 动态对冲补「亏损大侧」摊薄均价时用它选边。
    pub fn weaker_side(&self) -> Option<Side> {
        let up = self.settle_pnl(Side::Up);
        let down = self.settle_pnl(Side::Down);
        if up < down {
            Some(Side::Up)
        } else if down < up {
            Some(Side::Down)
        } else {
            None
        }
    }

    /// 未配对保护成本 = max(0, 主战场侧股数 − 对面股数) × 主战场侧均价。
    ///
    /// 度量主战场侧比对面多出来、没被配对保护的裸露头寸现价值，用于最大敞口判定。
    /// 主战场侧无持仓时无裸露，返回 0。
    pub fn unpaired_cost(&self, main_side: Side) -> Money {
        let gap = self.qty(main_side) - self.qty(main_side.opposite());
        match self.average_price(main_side) {
            Some(avg) if gap > Qty::ZERO => gap * avg,
            _ => Money::ZERO,
        }
    }

    /// 交割数学期望 EV = p × Up结算pnl + (1−p) × Down结算pnl。
    ///
    /// `up_win_probability` 为 Up 胜出概率 p（取 Up 侧 Mark Price 近似）。
    pub fn expected_value(&self, up_win_probability: Money) -> Money {
        let q = Money::ONE - up_win_probability;
        up_win_probability * self.settle_pnl(Side::Up) + q * self.settle_pnl(Side::Down)
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
    fn settle_pnl_is_qty_minus_total_cost() {
        // 总成本 = 40 + 50 = 90。Up 赢拿回 100 → +10；Down 赢拿回 80 → −10。
        let p = pos(dec!(100), dec!(80), dec!(40), dec!(50));
        assert_eq!(p.total_cost(), dec!(90));
        assert_eq!(p.settle_pnl(Side::Up), dec!(10));
        assert_eq!(p.settle_pnl(Side::Down), dec!(-10));
    }

    #[test]
    fn float_pnl_uses_side_cost_and_bid() {
        // Up 持仓 100、成本 45，bid 0.40 → 强平拿回 40 − 45 = −5。
        let p = pos(dec!(100), dec!(0), dec!(45), dec!(0));
        assert_eq!(p.float_pnl(Side::Up, dec!(0.40)), Some(dec!(-5.00)));
    }

    #[test]
    fn float_pnl_none_when_no_position() {
        // Down 侧空仓 → 无浮亏可言。
        let p = pos(dec!(100), dec!(0), dec!(45), dec!(0));
        assert_eq!(p.float_pnl(Side::Down, dec!(0.50)), None);
    }

    #[test]
    fn average_price_is_cost_over_qty() {
        let p = pos(dec!(100), dec!(0), dec!(45), dec!(0));
        assert_eq!(p.average_price(Side::Up), Some(dec!(0.45)));
        assert_eq!(p.average_price(Side::Down), None);
    }

    #[test]
    fn both_sides_settle_negative_only_when_both_below_zero() {
        // 总成本 100，两侧股数 40/50 都 < 100 → 双负。
        let both = pos(dec!(40), dec!(50), dec!(50), dec!(50));
        assert!(both.both_sides_settle_negative());
        // Up 120 > 100 → 不双负。
        let one = pos(dec!(120), dec!(80), dec!(50), dec!(50));
        assert!(!one.both_sides_settle_negative());
    }

    #[test]
    fn both_sides_negative_false_when_one_pnl_zero() {
        // 总成本 100，Up 恰 100 → settle pnl = 0，非严格负。
        let p = pos(dec!(100), dec!(40), dec!(50), dec!(50));
        assert!(!p.both_sides_settle_negative());
    }

    #[test]
    fn weaker_side_picks_smaller_settle_pnl() {
        // 总成本 80。Up 20 → −60，Down 100 → +20。Up 更弱。
        let p = pos(dec!(20), dec!(100), dec!(40), dec!(40));
        assert_eq!(p.weaker_side(), Some(Side::Up));
    }

    #[test]
    fn weaker_side_none_when_equal() {
        // 两侧股数相等 → 结算 pnl 相等。
        let p = pos(dec!(50), dec!(50), dec!(40), dec!(40));
        assert_eq!(p.weaker_side(), None);
    }

    #[test]
    fn unpaired_cost_is_exposed_shares_times_avg() {
        // Up 120、Down 80，主战场 Up，均价 = 60/120 = 0.5。
        // 裸露 = 120 − 80 = 40 股 × 0.5 = 20。
        let p = pos(dec!(120), dec!(80), dec!(60), dec!(40));
        assert_eq!(p.unpaired_cost(Side::Up), dec!(20.0));
    }

    #[test]
    fn unpaired_cost_zero_when_aligned_or_opposite_larger() {
        // Up 80 < Down 120 → 主战场 Up 无裸露。
        let p = pos(dec!(80), dec!(120), dec!(40), dec!(60));
        assert_eq!(p.unpaired_cost(Side::Up), Money::ZERO);
    }

    #[test]
    fn expected_value_weighted_by_probability() {
        // 总成本 100，Up 120 → +20，Down 80 → −20。p=0.6 → 0.6×20+0.4×(−20)=4。
        let p = pos(dec!(120), dec!(80), dec!(50), dec!(50));
        assert_eq!(p.expected_value(dec!(0.6)), dec!(4.0));
    }
}
