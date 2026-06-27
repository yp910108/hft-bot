//! 四资金池划拨：把总资金切成四份，各管各的。
//!
//! 本模块只算各池的**额度上限**，不记余额（余额由账本管，避免重复记账）。
//!
//! | 池 | 默认比例 | 干什么 |
//! | --- | --- | --- |
//! | 备用金 Reserve | 25% | 红线，日常不碰，EV 也不碰 |
//! | 核心做市 Grid_Maker | 15% | 建仓 + 配对的 Maker 铺单 |
//! | 动态对冲 Dynamic | 22.5% | 动态对冲 Maker 织网 |
//! | EV 对冲 Ev | 37.5% | EV 对冲 IOC Taker 扫盘 |
//!
//! 最大单边敞口上限 = 动态对冲池的 50%（11.25%V），是「风险敞口」与「救火弹药」的硬链接。

use domain::types::Money;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// 四池的比例配置。四者之和必须等于 1。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolRatios {
    pub reserve: Decimal,
    pub grid_maker: Decimal,
    pub dynamic: Decimal,
    pub ev: Decimal,
}

impl PoolRatios {
    /// 四个比例加起来。
    pub fn sum(&self) -> Decimal {
        self.reserve + self.grid_maker + self.dynamic + self.ev
    }
}

impl Default for PoolRatios {
    /// 默认：备用金 25%、核心做市 15%、动态对冲 22.5%、EV 对冲 37.5%。
    fn default() -> Self {
        Self {
            reserve: dec!(0.25),
            grid_maker: dec!(0.15),
            dynamic: dec!(0.225),
            ev: dec!(0.375),
        }
    }
}

/// 四池的绝对额度（= 总资金 × 比例）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapitalPools {
    total_capital: Money,
    ratios: PoolRatios,
    reserve: Money,
    grid_maker: Money,
    dynamic: Money,
    ev: Money,
}

impl CapitalPools {
    /// 按比例切分总资金。比例之和不为 1 则 panic。
    pub fn new(total_capital: Money, ratios: PoolRatios) -> Self {
        assert_eq!(
            ratios.sum(),
            Decimal::ONE,
            "四资金池划拨比例之和必须为 1，当前为 {}",
            ratios.sum()
        );
        Self {
            total_capital,
            ratios,
            reserve: total_capital * ratios.reserve,
            grid_maker: total_capital * ratios.grid_maker,
            dynamic: total_capital * ratios.dynamic,
            ev: total_capital * ratios.ev,
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

    /// 核心做市池额度。
    pub fn grid_maker(&self) -> Money {
        self.grid_maker
    }

    /// 动态对冲池额度。
    pub fn dynamic(&self) -> Money {
        self.dynamic
    }

    /// EV 对冲池额度。
    pub fn ev(&self) -> Money {
        self.ev
    }

    /// 最大单边敞口上限 = 动态对冲池的 50%。
    ///
    /// 含义：最极端单边行情下，系统永远保留 2 倍于当前裸露敞口的对冲资金，是安全垫。
    pub fn max_exposure(&self) -> Money {
        self.dynamic * dec!(0.5)
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
        // 总资金 1000 → 备用金 250、核心做市 150、动态对冲 225、EV 375。
        let pools = CapitalPools::with_default_ratios(dec!(1000));
        assert_eq!(pools.reserve(), dec!(250));
        assert_eq!(pools.grid_maker(), dec!(150));
        assert_eq!(pools.dynamic(), dec!(225));
        assert_eq!(pools.ev(), dec!(375));
    }

    #[test]
    fn pools_sum_back_to_total_capital() {
        let pools = CapitalPools::with_default_ratios(dec!(1000));
        let sum = pools.reserve() + pools.grid_maker() + pools.dynamic() + pools.ev();
        assert_eq!(sum, pools.total_capital());
    }

    #[test]
    fn max_exposure_is_half_of_dynamic_pool() {
        // 动态对冲池 225 → 最大敞口 112.5（总资金 1000 的 11.25%）。
        let pools = CapitalPools::with_default_ratios(dec!(1000));
        assert_eq!(pools.max_exposure(), dec!(112.5));
    }

    #[test]
    fn custom_ratios_are_honored() {
        let ratios = PoolRatios {
            reserve: dec!(0.3),
            grid_maker: dec!(0.2),
            dynamic: dec!(0.2),
            ev: dec!(0.3),
        };
        let pools = CapitalPools::new(dec!(2000), ratios);
        assert_eq!(pools.reserve(), dec!(600));
        assert_eq!(pools.grid_maker(), dec!(400));
        assert_eq!(pools.dynamic(), dec!(400));
        assert_eq!(pools.ev(), dec!(600));
    }

    #[test]
    #[should_panic(expected = "划拨比例之和必须为 1")]
    fn ratios_not_summing_to_one_panics() {
        let bad = PoolRatios {
            reserve: dec!(0.3),
            grid_maker: dec!(0.5),
            dynamic: dec!(0.2),
            ev: dec!(0.2), // 和为 1.2
        };
        CapitalPools::new(dec!(1000), bad);
    }
}
