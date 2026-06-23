//! 基础数值类型和市场方向定义。
//!
//! 金额、价格、股数全部用 [`rust_decimal::Decimal`] 表示，不用 f64。
//! 原因：价格在 0~1 之间反复除法，浮点会积累误差，Decimal 能保证精确。

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// 价格，取值约 (0, 1)，单位美元。
pub type Price = Decimal;

/// 股数，即持有的合约份数。
pub type Qty = Decimal;

/// 金额，以美元计（成本、盈亏、资金等）。
pub type Money = Decimal;

/// 市场的两个方向：Up（看涨）和 Down（看跌）。
///
/// 交割时有且仅有一方胜出，胜出方每股兑付 1 美元。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Up,
    Down,
}

impl Side {
    /// 返回对面那一侧。
    pub fn opposite(self) -> Self {
        match self {
            Side::Up => Side::Down,
            Side::Down => Side::Up,
        }
    }
}

/// 订单的撮合角色，决定收哪档手续费。
///
/// Maker 是被动挂单（提供流动性），Taker 是主动吃单（消耗流动性）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderRole {
    Maker,
    Taker,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opposite_side_is_symmetric() {
        assert_eq!(Side::Up.opposite(), Side::Down);
        assert_eq!(Side::Down.opposite(), Side::Up);
        // 取两次对面应回到自身。
        assert_eq!(Side::Up.opposite().opposite(), Side::Up);
    }
}
