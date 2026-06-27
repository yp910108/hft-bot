//! 引擎配置与资金池标签。

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
    /// 默认配置：按默认四池比例切分给定总资金。
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
