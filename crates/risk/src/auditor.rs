//! Cash Guard：下单前的现金红线校验。
//!
//! 规则很简单：可用现金 < 总资金 × 红线比例 → 拒绝一切新开仓。
//! 红线比例默认等于备用金池比例（25%），也可以设更高。
//! 审计器本身不管钱在哪，调用方把当前可用现金传进来就行。

use crate::pool::CapitalPools;
use domain::order::Order;
use domain::types::Money;
use rust_decimal::Decimal;

/// 拒绝原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// 现金低于红线，不让开新仓。
    CashGuardBlocked,
    /// 该侧敞口会超过最大单边敞口上限。
    MaxExposureExceeded,
}

/// 审计结果：通过或拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// 通过，可以下单。
    Approved,
    /// 被拒，附带原因。
    Rejected(RejectReason),
}

impl Approval {
    /// 是否通过。
    pub fn is_approved(self) -> bool {
        matches!(self, Approval::Approved)
    }
}

/// Cash Guard 审计器。
#[derive(Debug, Clone, Copy)]
pub struct RiskAuditor {
    pools: CapitalPools,
    /// 红线比例（相对总资金），低于这个比例的现金就拦截。
    cash_guard_ratio: Decimal,
}

impl RiskAuditor {
    /// 创建审计器。red_line 比例必须 >= 备用金池比例，否则 panic。
    pub fn new(pools: CapitalPools, cash_guard_ratio: Decimal) -> Self {
        assert!(
            cash_guard_ratio >= pools.ratios().reserve,
            "cash_guard_ratio({}) 必须 >= reserve 比例({})",
            cash_guard_ratio,
            pools.ratios().reserve
        );
        Self {
            pools,
            cash_guard_ratio,
        }
    }

    /// 用备用金池比例当红线（最低有效配置）。
    pub fn with_default_guard(pools: CapitalPools) -> Self {
        Self::new(pools, pools.ratios().reserve)
    }

    /// 红线的绝对金额 = 比例 × 总资金。
    pub fn cash_guard(&self) -> Money {
        self.pools.total_capital() * self.cash_guard_ratio
    }

    /// 审计一笔下单：现金低于红线就拒绝，否则放行。
    /// `_order` 目前没用到，留着以后做按池限额扩展。
    pub fn approve(&self, _order: &Order, free_cash: Money) -> Approval {
        if free_cash < self.cash_guard() {
            Approval::Rejected(RejectReason::CashGuardBlocked)
        } else {
            Approval::Approved
        }
    }

    /// 最大单边敞口上限（绝对金额）。
    pub fn max_exposure(&self) -> Money {
        self.pools.max_exposure()
    }

    /// 校验某侧敞口是否会超过最大单边敞口上限。
    ///
    /// `projected_exposure` = 未配对保护成本 + 该侧活跃挂单金额 + 拟发新单金额。
    /// 超过 11.25%V 返回拒绝，否则放行。动态对冲撞此线时原地挂机（Skip），不升级 EV。
    pub fn check_exposure(&self, projected_exposure: Money) -> Approval {
        if projected_exposure > self.max_exposure() {
            Approval::Rejected(RejectReason::MaxExposureExceeded)
        } else {
            Approval::Approved
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::order::{Generation, OrderDirection, OrderId, TimeInForce};
    use domain::types::{OrderRole, Side};
    use rust_decimal_macros::dec;

    /// 构造一笔测试订单（内容不影响 Cash Guard 判定）。
    fn sample_order() -> Order {
        Order {
            order_id: OrderId(1),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.4),
            qty: dec!(100),
            role: OrderRole::Maker,
            time_in_force: TimeInForce::Gtc,
            generation: Generation::new(),
        }
    }

    /// 总资金 1000、默认 25% 红线 → 红线绝对值 250。
    fn auditor() -> RiskAuditor {
        RiskAuditor::with_default_guard(CapitalPools::with_default_ratios(dec!(1000)))
    }

    #[test]
    fn cash_guard_is_ratio_times_capital() {
        assert_eq!(auditor().cash_guard(), dec!(250));
    }

    #[test]
    fn approves_when_cash_above_floor() {
        // 可用现金 300 ≥ 红线 250 → 通过。
        let approval = auditor().approve(&sample_order(), dec!(300));
        assert_eq!(approval, Approval::Approved);
        assert!(approval.is_approved());
    }

    #[test]
    fn rejects_when_cash_below_floor() {
        // 可用现金 200 < 红线 250 → 拒绝。
        let approval = auditor().approve(&sample_order(), dec!(200));
        assert_eq!(approval, Approval::Rejected(RejectReason::CashGuardBlocked));
        assert!(!approval.is_approved());
    }

    #[test]
    fn approves_when_cash_exactly_at_floor() {
        // 边界：可用现金恰等于红线 250，不低于红线 → 通过。
        let approval = auditor().approve(&sample_order(), dec!(250));
        assert_eq!(approval, Approval::Approved);
    }

    #[test]
    #[should_panic(expected = "cash_guard_ratio")]
    fn panics_when_guard_ratio_below_reserve() {
        let pools = CapitalPools::with_default_ratios(dec!(1000));
        // reserve 比例 = 0.25，设 guard = 0.20 → 应 panic。
        RiskAuditor::new(pools, dec!(0.20));
    }

    #[test]
    fn max_exposure_is_eleven_point_two_five_percent() {
        // 总资金 1000，动态对冲池 225 → 最大敞口 112.5。
        assert_eq!(auditor().max_exposure(), dec!(112.5));
    }

    #[test]
    fn exposure_approved_below_limit() {
        // 预计敞口 100 < 112.5 → 放行。
        assert_eq!(auditor().check_exposure(dec!(100)), Approval::Approved);
    }

    #[test]
    fn exposure_approved_at_limit() {
        // 边界：恰等于 112.5，不超过 → 放行。
        assert_eq!(auditor().check_exposure(dec!(112.5)), Approval::Approved);
    }

    #[test]
    fn exposure_rejected_above_limit() {
        // 预计敞口 120 > 112.5 → 拒绝。
        assert_eq!(
            auditor().check_exposure(dec!(120)),
            Approval::Rejected(RejectReason::MaxExposureExceeded)
        );
    }
}
