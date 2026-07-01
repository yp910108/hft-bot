//! 逐笔持仓单元：每笔买入独立记为一个 [`Lot`]，是逐笔止盈的最小跟踪单位。
//!
//! 策略对每个未平 Lot 单独判断「涨够了就挂卖单止盈」，卖单成交后精确平掉这一笔。
//! 这和只记聚合均价的账本根本不同——聚合均价无法回答「哪一笔涨了」。

use domain::clock::Millis;
use domain::types::{Price, Qty};

/// Lot 标识：单调递增，唯一指认一笔买入持仓。
///
/// 卖单挂出时携带要平的 Lot 标识，成交回来据此找到并平掉对应那笔。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LotId(pub u64);

/// 一笔尚未平掉的买入持仓。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lot {
    /// 唯一标识。
    pub lot_id: LotId,
    /// 有效买入成本（每股），= 买入现金 ÷ 净入仓股数。
    ///
    /// Maker 买入零费时等于挂单价；Taker 买入含费时略高于挂单价，
    /// 正好是止盈应超过的真实成本线。既作止盈止损触发基准，也作成本记账。
    pub buy_price: Price,
    /// 剩余未平股数（部分平仓后减少）。
    pub qty: Qty,
    /// 买入时刻（自场开始起毫秒），用于持有时长诊断。
    pub opened_at: Millis,
}

impl Lot {
    /// 这笔持仓的成本基础 = 有效买入成本 × 剩余股数。
    pub fn cost(&self) -> Price {
        self.buy_price * self.qty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn cost_is_price_times_qty() {
        let lot = Lot {
            lot_id: LotId(0),
            buy_price: dec!(0.45),
            qty: dec!(10),
            opened_at: 0,
        };
        assert_eq!(lot.cost(), dec!(4.50));
    }

    #[test]
    fn lot_id_orders_monotonically() {
        assert!(LotId(2) > LotId(1));
        assert_eq!(LotId(3), LotId(3));
    }
}
