//! 现金安全哨兵（Cash Guard）：任何下单前的强制资产审计关卡。
//!
//! 对应策略说明书第一节。任何执行模块在提交订单前必须先经本审计器校验：
//! 若当前可用现金低于总资金 V 的红线比例（默认 25%），立即拒绝一切新开仓挂单，
//! 守住底层资金链安全（见架构决策：可用现金由调用方传入，审计器只做校验）。

use crate::pool::CapitalPools;
use domain::order::Order;
use domain::types::Money;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// 下单被审计器拒绝的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// 可用现金低于 Cash Guard 红线，拒绝一切新开仓。
    CashGuardBlocked,
}

/// 一次下单审计的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// 通过审计，允许下单。
    Approved,
    /// 未通过审计，附带拒绝原因。
    Rejected(RejectReason),
}

impl Approval {
    /// 是否通过审计。
    pub fn is_approved(self) -> bool {
        matches!(self, Approval::Approved)
    }
}

/// 资产审计器：执行 Cash Guard 校验。
#[derive(Debug, Clone, Copy)]
pub struct RiskAuditor {
    /// 三资金池（含总资金 V）。
    pools: CapitalPools,
    /// Cash Guard 红线比例（相对总资金 V），低于此比例的可用现金即触发拦截。
    cash_guard_ratio: Decimal,
}

impl RiskAuditor {
    /// 以指定资金池与红线比例创建审计器。
    pub fn new(pools: CapitalPools, cash_guard_ratio: Decimal) -> Self {
        Self {
            pools,
            cash_guard_ratio,
        }
    }

    /// 以默认 25% 红线比例创建审计器。
    pub fn with_default_guard(pools: CapitalPools) -> Self {
        Self::new(pools, dec!(0.25))
    }

    /// Cash Guard 红线的绝对金额 = 红线比例 × 总资金 V。
    pub fn cash_guard_floor(&self) -> Money {
        self.pools.total_capital() * self.cash_guard_ratio
    }

    /// 审计一笔新开仓订单。
    ///
    /// 若可用现金 `free_cash` 低于 Cash Guard 红线，拒绝下单；否则通过。
    /// `_order` 暂未参与判定（本阶段仅做现金红线校验），保留以备后续按池限额扩展。
    pub fn approve(&self, _order: &Order, free_cash: Money) -> Approval {
        if free_cash < self.cash_guard_floor() {
            Approval::Rejected(RejectReason::CashGuardBlocked)
        } else {
            Approval::Approved
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::order::{Generation, OrderDirection, OrderId};
    use domain::types::{OrderRole, Side};

    /// 构造一笔测试订单（内容不影响 Cash Guard 判定）。
    fn sample_order() -> Order {
        Order {
            order_id: OrderId(1),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.4),
            qty: dec!(100),
            role: OrderRole::Maker,
            generation: Generation::first(),
        }
    }

    /// 总资金 1000、默认 25% 红线 → 红线绝对值 250。
    fn auditor() -> RiskAuditor {
        RiskAuditor::with_default_guard(CapitalPools::with_default_ratios(dec!(1000)))
    }

    #[test]
    fn cash_guard_floor_is_ratio_times_capital() {
        assert_eq!(auditor().cash_guard_floor(), dec!(250));
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
}
