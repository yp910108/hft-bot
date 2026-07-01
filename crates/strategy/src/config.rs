//! 策略参数配置。
//!
//! 所有可调参数集中在 [`StrategyConfig`]，由回测校准。
//! 止盈/止损幅度按场内进度分段取值。

use domain::types::{Price, Qty};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// 全部策略参数。
#[derive(Debug, Clone)]
pub struct StrategyConfig {
    /// 入场秒数（数据可用即入场）。
    pub entry_second: u64,
    /// 单笔下单股数。
    pub lot_qty: Qty,
    /// 建仓期结束 progress（0~1）。
    pub building_end: Decimal,
    /// 收手期开始 progress。
    pub harvest_start: Decimal,
    /// 完全停手 progress（终态）。
    pub settle_start: Decimal,
    /// 止盈幅度分段 [Q1, Q2, Q3, Q4]。
    pub tp_by_quartile: [Price; 4],
    /// 止损幅度分段 [Q1, Q2, Q3, Q4]。
    pub sl_by_quartile: [Price; 4],
    // ── 第二版可选项（默认关闭）──
    /// 单侧净持仓上限。None = 不限制。
    pub inventory_cap: Option<Qty>,
    /// 双侧不平衡上限。None = 不限制。
    pub imbalance_cap: Option<Qty>,
    /// 收手对称补仓阈值。None = 不启用。
    pub symmetry_threshold: Option<Qty>,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            entry_second: 6,
            lot_qty: dec!(10),
            building_end: dec!(0.08),
            harvest_start: dec!(0.83),
            settle_start: dec!(0.967),
            tp_by_quartile: [dec!(0.05), dec!(0.06), dec!(0.08), dec!(0.10)],
            sl_by_quartile: [dec!(0.04), dec!(0.04), dec!(0.05), dec!(0.07)],
            inventory_cap: None,
            imbalance_cap: None,
            symmetry_threshold: None,
        }
    }
}

impl StrategyConfig {
    /// 根据场内进度（0~1）取止盈幅度。分四段取对应值。
    pub fn tp(&self, progress: Decimal) -> Price {
        self.tp_by_quartile[quartile_index(progress)]
    }

    /// 根据场内进度（0~1）取止损幅度。分四段取对应值。
    pub fn sl(&self, progress: Decimal) -> Price {
        self.sl_by_quartile[quartile_index(progress)]
    }
}

/// 把 progress (0~1) 映射到四分位下标 0/1/2/3。
fn quartile_index(progress: Decimal) -> usize {
    if progress < dec!(0.25) {
        0
    } else if progress < dec!(0.50) {
        1
    } else if progress < dec!(0.75) {
        2
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_expected_values() {
        let cfg = StrategyConfig::default();
        assert_eq!(cfg.lot_qty, dec!(10));
        assert_eq!(cfg.building_end, dec!(0.08));
        assert_eq!(cfg.harvest_start, dec!(0.83));
        assert_eq!(cfg.settle_start, dec!(0.967));
    }

    #[test]
    fn tp_returns_quartile_value() {
        let cfg = StrategyConfig::default();
        assert_eq!(cfg.tp(dec!(0.10)), dec!(0.05)); // Q1
        assert_eq!(cfg.tp(dec!(0.30)), dec!(0.06)); // Q2
        assert_eq!(cfg.tp(dec!(0.60)), dec!(0.08)); // Q3
        assert_eq!(cfg.tp(dec!(0.80)), dec!(0.10)); // Q4
    }

    #[test]
    fn sl_returns_quartile_value() {
        let cfg = StrategyConfig::default();
        assert_eq!(cfg.sl(dec!(0.10)), dec!(0.04)); // Q1
        assert_eq!(cfg.sl(dec!(0.50)), dec!(0.05)); // Q3 (0.50 进第三段)
        assert_eq!(cfg.sl(dec!(0.90)), dec!(0.07)); // Q4
    }

    #[test]
    fn quartile_boundaries() {
        // 0.25 精确落在 Q2 起点。
        assert_eq!(quartile_index(dec!(0.25)), 1);
        assert_eq!(quartile_index(dec!(0.75)), 3);
        assert_eq!(quartile_index(dec!(0.00)), 0);
        assert_eq!(quartile_index(dec!(1.00)), 3);
    }
}
