//! Engine 配置。

use domain::order::OrderConstraints;
use domain::types::Money;
use rust_decimal_macros::dec;
use strategy::StrategyConfig;

/// Engine 配置。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// 总资金（一场可用的现金总量）。
    pub total_capital: Money,
    /// 精度约束。
    pub constraints: OrderConstraints,
    /// 策略参数。
    pub strategy: StrategyConfig,
}

impl EngineConfig {
    /// 按总资金快速构造（其余取默认）。
    pub fn with_capital(capital: Money) -> Self {
        Self {
            total_capital: capital,
            constraints: OrderConstraints::default(),
            strategy: StrategyConfig::default(),
        }
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self::with_capital(dec!(1000))
    }
}
