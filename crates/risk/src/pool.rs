//! 三资金池划拨：把总资金切成三份，各管各的。
//!
//! 本模块只算各池的**额度上限**，不记余额（余额由账本管，避免重复记账）。
//!
//! | 池 | 默认比例 | 干什么 |
//! | --- | --- | --- |
//! | 备用金 Reserve | 25% | 红线，日常不碰 |
//! | 做市池 Grid_Maker | 52.5% | Maker 梯度铺单 |
//! | 对冲池 Hedge_Attack | 22.5% | Taker 追买 |

use domain::types::Money;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// 三池的比例配置。三者之和必须等于 1。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolRatios {
    pub reserve: Decimal,
    pub grid_maker: Decimal,
    pub hedge_attack: Decimal,
}

impl PoolRatios {
    /// 三个比例加起来。
    pub fn sum(&self) -> Decimal {
        self.reserve + self.grid_maker + self.hedge_attack
    }
}

impl Default for PoolRatios {
    /// 默认：备用金 25%、做市 52.5%、对冲 22.5%。
    fn default() -> Self {
        Self {
            reserve: dec!(0.25),
            grid_maker: dec!(0.525),
            hedge_attack: dec!(0.225),
        }
    }
}

/// 三池的绝对额度（= 总资金 × 比例）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapitalPools {
    total_capital: Money,
    ratios: PoolRatios,
    reserve: Money,
    grid_maker: Money,
    hedge_attack: Money,
}

impl CapitalPools {
    /// 按比例切分总资金。比例之和不为 1 则 panic。
    pub fn new(total_capital: Money, ratios: PoolRatios) -> Self {
        assert_eq!(
            ratios.sum(),
            Decimal::ONE,
            "三资金池划拨比例之和必须为 1，当前为 {}",
            ratios.sum()
        );
        Self {
            total_capital,
            ratios,
            reserve: total_capital * ratios.reserve,
            grid_maker: total_capital * ratios.grid_maker,
            hedge_attack: total_capital * ratios.hedge_attack,
        }
    }

    /// 用默认比例切分。
    pub fn with_default_ratios(total_capital: Money) -> Self {
        Self::new(total_capital, PoolRatios::default())
    }

    /// 总资金。
    pub fn total_capital(&self) -> Money {
        self.total_capital
    }

    /// 比例配置。
    pub fn ratios(&self) -> &PoolRatios {
        &self.ratios
    }

    /// 备用金池额度。
    pub fn reserve(&self) -> Money {
        self.reserve
    }

    /// 做市池额度。
    pub fn grid_maker(&self) -> Money {
        self.grid_maker
    }

    /// 对冲池额度。
    pub fn hedge_attack(&self) -> Money {
        self.hedge_attack
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ratios_sum_to_one() {
        assert_eq!(PoolRatios::default().sum(), Decimal::ONE);
    }

    #[test]
    fn default_pools_split_capital_by_ratio() {
        // 总资金 1000 → 备用金 250、核心做市 525、动量对冲 225。
        let pools = CapitalPools::with_default_ratios(dec!(1000));
        assert_eq!(pools.reserve(), dec!(250));
        assert_eq!(pools.grid_maker(), dec!(525));
        assert_eq!(pools.hedge_attack(), dec!(225));
    }

    #[test]
    fn pools_sum_back_to_total_capital() {
        let pools = CapitalPools::with_default_ratios(dec!(1000));
        let sum = pools.reserve() + pools.grid_maker() + pools.hedge_attack();
        assert_eq!(sum, pools.total_capital());
    }

    #[test]
    fn custom_ratios_are_honored() {
        let ratios = PoolRatios {
            reserve: dec!(0.3),
            grid_maker: dec!(0.5),
            hedge_attack: dec!(0.2),
        };
        let pools = CapitalPools::new(dec!(2000), ratios);
        assert_eq!(pools.reserve(), dec!(600));
        assert_eq!(pools.grid_maker(), dec!(1000));
        assert_eq!(pools.hedge_attack(), dec!(400));
    }

    #[test]
    #[should_panic(expected = "划拨比例之和必须为 1")]
    fn ratios_not_summing_to_one_panics() {
        let bad = PoolRatios {
            reserve: dec!(0.3),
            grid_maker: dec!(0.5),
            hedge_attack: dec!(0.3), // 和为 1.1
        };
        CapitalPools::new(dec!(1000), bad);
    }
}
