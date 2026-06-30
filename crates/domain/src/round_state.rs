//! 跨阶段可变状态：整场比赛持久维护、跨状态跳转不丢失的量。
//!
//! engine 持有并维护一份 [`RoundState`]，strategy 通过 `DecisionContext` 中的
//! `&RoundState` 只读引用访问。两者共享同一个结构定义，消除平铺字段的冗余。

use crate::clock::Millis;
use crate::state::RobotState;
use crate::types::Side;

/// 一场比赛的跨阶段可变状态。
///
/// engine 持有、可读可写；strategy 通过 DecisionContext 中的 `&RoundState` 只读访问。
#[derive(Debug, Clone)]
pub struct RoundState {
    /// 当前 FSM 状态。
    pub state: RobotState,
    /// 主战场侧（首笔成交锁定，一轮不换）。建仓前为 None。
    pub main_field: Option<Side>,
    /// 主战场侧是否已永久停铺（做市阶段敞口曾超限，本阶段不再铺）。
    pub main_field_frozen: bool,
    /// 上次对冲动作的时间戳（冷却判定用）。从未对冲为 None。
    pub last_hedge_at: Option<Millis>,
    /// 资金耗尽标志位：动态对冲池资金耗尽后置 true，黏住本场不再重启对冲。
    pub funds_exhausted: bool,
    /// 「双边负」边沿计数。
    pub double_negative_count: u8,
    /// 上一 tick 是否处于双边负状态（边沿检测用）。
    pub was_double_negative: bool,
    /// 熔断态下 spread 持续低于恢复阈值的起始时刻；尚未平静为 None。
    pub calm_since: Option<Millis>,
}

impl RoundState {
    /// 开局初始状态：建仓态，其余皆空 / 零 / false。
    pub fn new() -> Self {
        Self {
            state: RobotState::Building,
            main_field: None,
            main_field_frozen: false,
            last_hedge_at: None,
            funds_exhausted: false,
            double_negative_count: 0,
            was_double_negative: false,
            calm_since: None,
        }
    }

    /// 首笔成交锁定主战场（仅在尚未锁定时生效，一轮不换）。
    pub fn lock_main_field(&mut self, side: Side) {
        if self.main_field.is_none() {
            self.main_field = Some(side);
        }
    }

    /// 做市阶段敞口超限：永久停铺主战场。
    pub fn freeze_main_field(&mut self) {
        self.main_field_frozen = true;
    }

    /// 标记资金耗尽（黏住本场）。
    pub fn mark_funds_exhausted(&mut self) {
        self.funds_exhausted = true;
    }

    /// 记录一次对冲动作的时间戳（冷却起点）。
    pub fn record_hedge_at(&mut self, now: Millis) {
        self.last_hedge_at = Some(now);
    }

    /// 更新双边负边沿计数（由 strategy 的 Decision 意图驱动）。
    pub fn update_double_negative(&mut self, count: u8, was: bool) {
        self.double_negative_count = count;
        self.was_double_negative = was;
    }

    /// 更新熔断恢复迟滞计时：平静则记起点（已记则保持），不平静则清空。
    pub fn update_calm(&mut self, is_calm: bool, now: Millis) {
        if is_calm {
            if self.calm_since.is_none() {
                self.calm_since = Some(now);
            }
        } else {
            self.calm_since = None;
        }
    }
}

impl Default for RoundState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_in_building_with_empty_state() {
        let s = RoundState::new();
        assert_eq!(s.state, RobotState::Building);
        assert_eq!(s.main_field, None);
        assert!(!s.main_field_frozen);
        assert_eq!(s.last_hedge_at, None);
        assert!(!s.funds_exhausted);
        assert_eq!(s.double_negative_count, 0);
        assert!(!s.was_double_negative);
        assert_eq!(s.calm_since, None);
    }

    #[test]
    fn lock_main_field_only_once() {
        let mut s = RoundState::new();
        s.lock_main_field(Side::Up);
        assert_eq!(s.main_field, Some(Side::Up));
        s.lock_main_field(Side::Down);
        assert_eq!(s.main_field, Some(Side::Up));
    }

    #[test]
    fn update_double_negative_writes_both() {
        let mut s = RoundState::new();
        s.update_double_negative(2, true);
        assert_eq!(s.double_negative_count, 2);
        assert!(s.was_double_negative);
    }

    #[test]
    fn update_calm_records_start_then_holds() {
        let mut s = RoundState::new();
        s.update_calm(true, 1000);
        assert_eq!(s.calm_since, Some(1000));
        s.update_calm(true, 2000);
        assert_eq!(s.calm_since, Some(1000));
        s.update_calm(false, 3000);
        assert_eq!(s.calm_since, None);
    }
}
