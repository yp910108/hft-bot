//! 核心数值与市场侧的基础类型定义。
//!
//! 金额、价格、股数统一使用 [`rust_decimal::Decimal`]（10 进制定点数）表示，
//! 禁止用 f64 表示金钱量：价格在 0~1 间且反复做「成本 ÷ 股数」除法，浮点会累积误差，
//! 而 Decimal 保证十进制运算精确，无需靠舍入压制浮点噪声。

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// 价格：Polymarket 二元市场中某一侧的报价，取值区间约为 (0, 1)，单位为美元。
pub type Price = Decimal;

/// 股数：持有的某侧合约份额。
pub type Qty = Decimal;

/// 金额：成本、盈亏、资金等以美元计的货币量。
pub type Money = Decimal;

/// 市场的两个对立面。
///
/// BTC 15 分钟周期二元市场中，`Up` 代表「上涨胜出」，`Down` 代表「下跌胜出」，
/// 二者必有且仅有一方在交割时兑付 1 美元。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Up,
    Down,
}

impl Side {
    /// 返回对立的一侧。
    pub fn opposite(self) -> Self {
        match self {
            Side::Up => Side::Down,
            Side::Down => Side::Up,
        }
    }
}

/// 订单在撮合中扮演的角色，决定适用的手续费率。
///
/// - `Maker`：被动挂单、为盘口提供流动性。
/// - `Taker`：主动吃单、消耗盘口流动性。
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
        // 取两次对立面应回到自身。
        assert_eq!(Side::Up.opposite().opposite(), Side::Up);
    }
}
