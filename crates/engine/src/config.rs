//! 引擎配置与内部资金池标签。

use domain::order::OrderConstraints;
use domain::types::Money;
use risk::pool::CapitalPools;
use strategy::StrategyConfig;

/// 资金池标签：每笔挂单出自哪个池，用于算各池剩余。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Pool {
    GridMaker,
    Dynamic,
    Ev,
}

/// 引擎配置：总资金、池划拨、策略阈值、风控。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub pools: CapitalPools,
    pub strategy: StrategyConfig,
    pub cash_guard_ratio: rust_decimal::Decimal,
    pub constraints: OrderConstraints,
}

impl EngineConfig {
    /// 默认配置：按默认四池比例切分给定总资金，Cash Guard 取备用金池比例。
    pub fn with_capital(total_capital: Money) -> Self {
        let pools = CapitalPools::with_default_ratios(total_capital);
        Self {
            pools,
            strategy: StrategyConfig::default(),
            cash_guard_ratio: pools.ratios().reserve,
            constraints: OrderConstraints::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn with_capital_splits_four_pools() {
        let cfg = EngineConfig::with_capital(dec!(1000));
        // 默认四池：备用 250 / 做市 150 / 动态 225 / EV 375。
        assert_eq!(cfg.pools.grid_maker(), dec!(150));
        assert_eq!(cfg.pools.dynamic(), dec!(225));
        assert_eq!(cfg.pools.ev(), dec!(375));
    }

    #[test]
    fn cash_guard_defaults_to_reserve_ratio() {
        let cfg = EngineConfig::with_capital(dec!(1000));
        // Cash Guard 红线比例 = 备用金池比例 25%。
        assert_eq!(cfg.cash_guard_ratio, dec!(0.25));
    }
}
