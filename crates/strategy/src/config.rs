//! 策略全部可配阈值，集中一处。所有小策略共享同一份配置。
//!
//! 比例类阈值（如 ±%V）以小数存储，运行时乘总资金 V 换算成绝对金额。
//! 时间类阈值以毫秒存储（自场开始计）。价格类阈值就是价格小数。

use exchange::clock::Millis;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// 建仓三档之一：相对 best_ask 的向下偏移 + 占核心做市池的比例。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderRung {
    pub price_offset: Decimal,
    pub pool_fraction: Decimal,
}

/// 动态对冲单步三档之一：相对 best_ask 的向下偏移 + 占本步资金的比例。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HedgeRung {
    pub price_offset: Decimal,
    pub step_fraction: Decimal,
}

/// 策略阈值配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyConfig {
    // ---- 建仓 / 配对 ----
    /// 主战场阈值：只有 best_ask < 此值的一侧能当主战场。
    pub main_field_max_ask: Decimal,
    /// 建仓三档。
    pub building_rungs: [LadderRung; 3],
    /// 配对价的利润空间：配对价 = 1 − 主战场均价 − margin。
    pub profit_margin: Decimal,
    /// 续挂追低步长：主战场成交价下方此偏移再挂一档。
    pub follow_offset: Decimal,
    /// 续挂占核心做市池的比例。
    pub follow_fraction: Decimal,
    /// 精细订单管理的价格容差：新旧配对价偏差 ≤ 此值则不撤单保排队。
    pub repair_tolerance: Decimal,

    // ---- 盈亏线（相对总资金 V 的比例）----
    /// 做市利润锁定线 +0.5%V。
    pub making_profit_lock: Decimal,
    /// 动态对冲微利逃生线 +0.25%V。
    pub dynamic_escape: Decimal,
    /// 观察线 −0.5%V。
    pub observe_line: Decimal,
    /// 亏损触发线 −2%V。
    pub loss_trigger: Decimal,
    /// 单边持仓门槛 3%V（结算 pnl 触发的对侧成本门槛）。
    pub single_side_threshold: Decimal,

    // ---- 动态对冲 ----
    /// 单步资金占对冲池剩余的比例 7.5%。
    pub dynamic_step_fraction: Decimal,
    /// 单步三档。
    pub dynamic_rungs: [HedgeRung; 3],
    /// 单步生命周期（毫秒），超时强制结束。
    pub dynamic_step_lifetime: Millis,
    /// 两步之间冷却（毫秒）。
    pub dynamic_cooldown: Millis,
    /// 深海死单判定：挂单价偏离 best_ask 超此值则失效撤销。
    pub deep_sea_deviation: Decimal,

    // ---- EV 对冲 ----
    /// 单步资金占 EV 池剩余的比例 25%。
    pub ev_step_fraction: Decimal,
    /// IOC 保护上限价。
    pub ev_price_cap: Decimal,
    /// 两步之间冷却（毫秒）。
    pub ev_cooldown: Millis,
    /// 出手甜区：剩余 >5min 时优势方概率区间。
    pub ev_sweet_far: (Decimal, Decimal),
    /// 出手甜区：剩余 5min~1min 时优势方概率区间。
    pub ev_sweet_near: (Decimal, Decimal),
    /// 反转退出线：优势方概率跌破此值立即收手。
    pub ev_reversal: Decimal,

    // ---- 熔断 ----
    /// 熔断触发：任一侧 Spread_Ratio > 此值。
    pub circuit_trigger_ratio: Decimal,
    /// 熔断恢复：Spread_Ratio < 此值。
    pub circuit_recover_ratio: Decimal,
    /// 熔断恢复需持续稳定的时长（毫秒）。
    pub circuit_recover_stable: Millis,

    // ---- 时间红线 ----
    /// 最后阶段阈值：剩余 < 此值时动态对冲失效（5min）。
    pub last_phase_window: Millis,
    /// 时间红线：剩余 < 此值时无条件收手扛结算（1min）。
    pub time_red_line: Millis,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            main_field_max_ask: dec!(0.5),
            building_rungs: [
                LadderRung {
                    price_offset: dec!(0.01),
                    pool_fraction: dec!(0.02),
                },
                LadderRung {
                    price_offset: dec!(0.02),
                    pool_fraction: dec!(0.03),
                },
                LadderRung {
                    price_offset: dec!(0.03),
                    pool_fraction: dec!(0.05),
                },
            ],
            profit_margin: dec!(0.02),
            follow_offset: dec!(0.01),
            follow_fraction: dec!(0.02),
            repair_tolerance: dec!(0.01),

            making_profit_lock: dec!(0.005),
            dynamic_escape: dec!(0.0025),
            observe_line: dec!(-0.005),
            loss_trigger: dec!(-0.02),
            single_side_threshold: dec!(0.03),

            dynamic_step_fraction: dec!(0.075),
            dynamic_rungs: [
                HedgeRung {
                    price_offset: dec!(0.01),
                    step_fraction: dec!(0.40),
                },
                HedgeRung {
                    price_offset: dec!(0.02),
                    step_fraction: dec!(0.30),
                },
                HedgeRung {
                    price_offset: dec!(0.03),
                    step_fraction: dec!(0.30),
                },
            ],
            dynamic_step_lifetime: 3_000,
            dynamic_cooldown: 1_000,
            deep_sea_deviation: dec!(0.03),

            ev_step_fraction: dec!(0.25),
            ev_price_cap: dec!(0.85),
            ev_cooldown: 2_000,
            ev_sweet_far: (dec!(0.60), dec!(0.75)),
            ev_sweet_near: (dec!(0.75), dec!(0.85)),
            ev_reversal: dec!(0.55),

            circuit_trigger_ratio: dec!(0.30),
            circuit_recover_ratio: dec!(0.10),
            circuit_recover_stable: 5_000,

            last_phase_window: 300_000,
            time_red_line: 60_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_building_rungs_sum_to_ten_percent() {
        let cfg = StrategyConfig::default();
        let sum: Decimal = cfg.building_rungs.iter().map(|r| r.pool_fraction).sum();
        // 2% + 3% + 5% = 10%（占核心做市池）。
        assert_eq!(sum, dec!(0.10));
    }

    #[test]
    fn dynamic_rungs_sum_to_full_step() {
        let cfg = StrategyConfig::default();
        let sum: Decimal = cfg.dynamic_rungs.iter().map(|r| r.step_fraction).sum();
        // 40% + 30% + 30% = 100%（占本步资金）。
        assert_eq!(sum, dec!(1.00));
    }

    #[test]
    fn profit_lines_ordered() {
        let cfg = StrategyConfig::default();
        // 亏损线 < 观察线 < 0 < 微利逃生 < 做市锁定。
        assert!(cfg.loss_trigger < cfg.observe_line);
        assert!(cfg.observe_line < Decimal::ZERO);
        assert!(Decimal::ZERO < cfg.dynamic_escape);
        assert!(cfg.dynamic_escape < cfg.making_profit_lock);
    }

    #[test]
    fn time_windows_ordered() {
        let cfg = StrategyConfig::default();
        // 时间红线 1min < 最后阶段窗口 5min。
        assert!(cfg.time_red_line < cfg.last_phase_window);
    }
}
