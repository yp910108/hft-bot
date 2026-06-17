//! 三资金池划拨：开盘时按总资金 V 将资本切分为三个独立额度上限。
//!
//! 对应策略说明书第一节「资金池精细化治理」。本模块只负责依据比例算出各池的
//! **额度上限**（见架构决策：池 = 额度上限，不单独记每池余额，以免与账本重复记账）。
//!
//! | 子资金池 | 默认比例 | 用途 |
//! | --- | --- | --- |
//! | 备用金池 Reserve | 25% | 红线，日常禁动 |
//! | 核心做市池 Grid_Maker | 52.5% | Maker 梯度铺单 |
//! | 动量对冲池 Hedge_Attack | 22.5% | Taker 对冲 |

use domain::types::Money;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// 三资金池的划拨比例（相对总资金 V）。三者之和应为 1。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolRatios {
    /// 备用金池比例。
    pub reserve: Decimal,
    /// 核心做市池比例。
    pub grid_maker: Decimal,
    /// 动量对冲池比例。
    pub hedge_attack: Decimal,
}

impl PoolRatios {
    /// 三个比例之和。
    pub fn sum(&self) -> Decimal {
        self.reserve + self.grid_maker + self.hedge_attack
    }
}

impl Default for PoolRatios {
    /// 策略默认划拨：备用金 25%、核心做市 52.5%、动量对冲 22.5%。
    fn default() -> Self {
        Self {
            reserve: dec!(0.25),
            grid_maker: dec!(0.525),
            hedge_attack: dec!(0.225),
        }
    }
}

/// 三资金池的额度上限（按总资金 V 与比例算出的绝对金额）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapitalPools {
    /// 总资金 V。
    total_capital: Money,
    /// 备用金池额度上限。
    reserve: Money,
    /// 核心做市池额度上限。
    grid_maker: Money,
    /// 动量对冲池额度上限。
    hedge_attack: Money,
}

impl CapitalPools {
    /// 按总资金与划拨比例切分出三池额度上限。
    ///
    /// # Panics
    /// 当比例之和不为 1 时 panic——划拨比例必须恰好分配全部资金，否则属配置错误。
    pub fn new(total_capital: Money, ratios: PoolRatios) -> Self {
        assert_eq!(
            ratios.sum(),
            Decimal::ONE,
            "三资金池划拨比例之和必须为 1，当前为 {}",
            ratios.sum()
        );
        Self {
            total_capital,
            reserve: total_capital * ratios.reserve,
            grid_maker: total_capital * ratios.grid_maker,
            hedge_attack: total_capital * ratios.hedge_attack,
        }
    }

    /// 以默认比例（25% / 52.5% / 22.5%）切分。
    pub fn with_default_ratios(total_capital: Money) -> Self {
        Self::new(total_capital, PoolRatios::default())
    }

    /// 总资金 V。
    pub fn total_capital(&self) -> Money {
        self.total_capital
    }

    /// 备用金池额度上限。
    pub fn reserve(&self) -> Money {
        self.reserve
    }

    /// 核心做市池额度上限。
    pub fn grid_maker(&self) -> Money {
        self.grid_maker
    }

    /// 动量对冲池额度上限。
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
