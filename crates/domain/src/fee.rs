//! 手续费模型：按角色（Maker/Taker）扣减到手股数。
//!
//! 实测 Taker 费率约 4%，Maker 为 0%。
//!
//! 扣费方式：不额外扣现金，而是减少到手股数。
//! 下单 100 股实际入仓 100×(1-费率) 股，花的钱不变仍是 100×价格。
//! 交割时每股值 1 美元，所以少拿的股数就是实际损失。

use crate::types::{OrderRole, Qty};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// 手续费模型，存放 Maker 和 Taker 各自的费率。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeModel {
    /// Maker 费率，例如 `0.00` 表示免费。
    pub maker_fee_rate: Decimal,
    /// Taker 费率，例如 `0.04` 表示 4%。
    pub taker_fee_rate: Decimal,
}

impl FeeModel {
    /// 创建零费率模型（Maker 和 Taker 都不收费）。
    pub fn zero() -> Self {
        Self {
            maker_fee_rate: Decimal::ZERO,
            taker_fee_rate: Decimal::ZERO,
        }
    }

    /// 查询指定角色的费率。
    pub fn rate_for(&self, role: OrderRole) -> Decimal {
        match role {
            OrderRole::Maker => self.maker_fee_rate,
            OrderRole::Taker => self.taker_fee_rate,
        }
    }

    /// 算出扣完手续费后实际到手的股数 = `gross_qty × (1 - 费率)`。
    ///
    /// `gross_qty` 是名义成交股数，返回值是真正入仓的净股数。
    /// 花的钱不在这里算，调用方自己记账。
    pub fn net_qty(&self, role: OrderRole, gross_qty: Qty) -> Qty {
        gross_qty * (Decimal::ONE - self.rate_for(role))
    }
}

impl Default for FeeModel {
    /// 默认值：Taker 4%、Maker 0%（实测确认，偏保守）。
    fn default() -> Self {
        Self {
            maker_fee_rate: Decimal::ZERO,
            taker_fee_rate: dec!(0.04),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn zero_model_keeps_full_qty() {
        let model = FeeModel::zero();
        // 零费时净股数等于名义股数。
        assert_eq!(model.net_qty(OrderRole::Taker, dec!(100)), dec!(100));
    }

    #[test]
    fn default_model_uses_four_percent_taker() {
        let model = FeeModel::default();
        assert_eq!(model.maker_fee_rate, dec!(0.00));
        assert_eq!(model.taker_fee_rate, dec!(0.04));
    }

    #[test]
    fn taker_net_qty_deducts_rate_from_shares() {
        let model = FeeModel::default();
        // 下单 100 股，Taker 4% → 实际入仓 100 × 0.96 = 96 股。
        assert_eq!(model.net_qty(OrderRole::Taker, dec!(100)), dec!(96.00));
    }

    #[test]
    fn maker_net_qty_keeps_full_qty_by_default() {
        let model = FeeModel::default();
        // 默认 Maker 零费 → 净股数不减。
        assert_eq!(model.net_qty(OrderRole::Maker, dec!(100)), dec!(100));
    }
}
