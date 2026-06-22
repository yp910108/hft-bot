//! 手续费模型：统一描述 Maker / Taker 的费率，并将其换算为净到手股数。
//!
//! 费率抽象为可配置参数，实测 Taker 约 4%、Maker 0%。
//!
//! **扣费口径**：手续费体现为到手股数的扣减，而非额外扣减现金。
//! 即下单 `gross_qty` 股，实际入仓 `gross_qty × (1 - rate)` 股，所花现金仍为 `gross_qty × price`。
//! 交割时每股兑付 1 美元，故股数损耗直接决定回收金额。

use crate::types::{OrderRole, Qty};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// 手续费模型，描述按成交股数比例扣减的 Maker / Taker 费率。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeModel {
    /// Maker（被动挂单）费率，例如 `0.00` 表示零费。
    pub maker_fee_rate: Decimal,
    /// Taker（主动吃单）费率，例如 `0.04` 表示 4%。
    pub taker_fee_rate: Decimal,
}

impl FeeModel {
    /// 构造一个零手续费模型（Maker 与 Taker 均为 0）。
    pub fn zero() -> Self {
        Self {
            maker_fee_rate: Decimal::ZERO,
            taker_fee_rate: Decimal::ZERO,
        }
    }

    /// 返回指定角色适用的费率。
    pub fn rate_for(&self, role: OrderRole) -> Decimal {
        match role {
            OrderRole::Maker => self.maker_fee_rate,
            OrderRole::Taker => self.taker_fee_rate,
        }
    }

    /// 计算扣除手续费后的净到手股数 = `gross_qty × (1 - rate)`。
    ///
    /// `gross_qty` 为下单成交的名义股数，返回实际入仓的净股数。
    /// 所花现金不受影响，仍为 `gross_qty × price`，由调用方单独记账。
    pub fn net_qty(&self, role: OrderRole, gross_qty: Qty) -> Qty {
        gross_qty * (Decimal::ONE - self.rate_for(role))
    }
}

impl Default for FeeModel {
    /// 默认采用实测确认的 Taker 4%、Maker 0%（略偏保守口径）。
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
        // 下单 100 股，4% Taker 费 → 净入仓 100 × 0.96 = 96 股。
        assert_eq!(model.net_qty(OrderRole::Taker, dec!(100)), dec!(96.00));
    }

    #[test]
    fn maker_net_qty_keeps_full_qty_by_default() {
        let model = FeeModel::default();
        // 默认 Maker 零费 → 净股数不减。
        assert_eq!(model.net_qty(OrderRole::Maker, dec!(100)), dec!(100));
    }
}
