//! 有限状态机层：策略的「大脑」，定义状态流转规则与对冲触发判定。
//!
//! 纯逻辑、不触碰任何 IO。状态机由交易所成交事件驱动；
//! 每收到一次输入即调用 [`StateMachine::step`] 评估是否跳转。
//!
//! 阈值一律以绝对金额传入（由上层把相对总资金的比例换算为绝对值），
//! 状态机本身不感知资金规模与比例。

use domain::market::MarketSnapshot;
use domain::pnl::PositionSnapshot;
use domain::state::RobotState;
use domain::types::{Money, Qty, Side};

/// 状态流转所需的阈值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// 对冲触发的单边亏损线（正数，表示 `Side_PnL <= -loss_trigger`）。
    pub hedge_loss_trigger: Money,
    /// 对冲触发的最小总成交股数：总持仓(up_qty+down_qty)达到此值才允许触发对冲，
    /// 防止开局首笔成交导致的虚假报警。
    pub hedge_min_qty: Qty,
    /// 收手结算的利润达标线：两边条件 PnL 的较小者达到此值即锁定离场。
    pub profit_target: Money,
}

/// 单步评估所需的输入快照。
#[derive(Debug, Clone, Copy)]
pub struct StepInputs {
    /// 当前双边持仓快照，用于计算条件盈亏。
    pub position: PositionSnapshot,
    /// 当前市场快照，用于 EV 对冲阶段读取 Mark Price 估算胜出概率。
    pub market: MarketSnapshot,
}

/// 一次 `step` 的结果：状态是否发生跳转。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// 状态未变。
    Unchanged(RobotState),
    /// 状态已跳转，携带新状态。
    Moved(RobotState),
}

impl Transition {
    /// 取跳转后的状态（无论是否变化）。
    pub fn state(self) -> RobotState {
        match self {
            Transition::Unchanged(state) | Transition::Moved(state) => state,
        }
    }

    /// 是否发生了跳转。
    pub fn is_moved(self) -> bool {
        matches!(self, Transition::Moved(_))
    }
}

/// 策略有限状态机：持有当前状态，依据输入评估并执行流转。
#[derive(Debug, Clone)]
pub struct StateMachine {
    state: RobotState,
    thresholds: Thresholds,
}

impl StateMachine {
    /// 以初始状态 [`RobotState::Initialization`] 创建状态机。
    pub fn new(thresholds: Thresholds) -> Self {
        Self {
            state: RobotState::default(),
            thresholds,
        }
    }

    /// 返回当前状态。
    pub fn state(&self) -> RobotState {
        self.state
    }

    /// 返回状态机的阈值配置（供上层读取对冲亏损线等参数）。
    pub fn thresholds(&self) -> Thresholds {
        self.thresholds
    }

    /// 依据输入评估流转，更新并返回结果。
    pub fn step(&mut self, inputs: &StepInputs) -> Transition {
        let next = self.next_state(inputs);
        if next != self.state {
            self.state = next;
            Transition::Moved(next)
        } else {
            Transition::Unchanged(next)
        }
    }

    /// 部署完初始梯度单后，由上层显式驱动进入常规做市。
    pub fn finish_initialization(&mut self) -> Transition {
        if self.state == RobotState::Initialization {
            self.state = RobotState::RangeBoundMaking;
            Transition::Moved(self.state)
        } else {
            Transition::Unchanged(self.state)
        }
    }

    /// 依据当前状态与输入计算应处的下一状态（纯判定，不改自身）。
    fn next_state(&self, inputs: &StepInputs) -> RobotState {
        match self.state {
            RobotState::Initialization => RobotState::Initialization,

            RobotState::RangeBoundMaking => {
                if self.profit_target_reached(&inputs.position) {
                    RobotState::FinalSettlement
                } else if self.hedge_boundary_triggered(inputs) {
                    RobotState::DynamicHedging {
                        double_negative_count: 0,
                    }
                } else {
                    RobotState::RangeBoundMaking
                }
            }

            RobotState::DynamicHedging {
                double_negative_count,
            } => {
                if self.profit_target_reached(&inputs.position) {
                    RobotState::FinalSettlement
                } else if inputs.position.both_sides_negative() {
                    if double_negative_count >= 1 {
                        RobotState::EvHedging
                    } else {
                        RobotState::DynamicHedging {
                            double_negative_count: double_negative_count + 1,
                        }
                    }
                } else {
                    RobotState::DynamicHedging {
                        double_negative_count,
                    }
                }
            }

            RobotState::EvHedging => {
                if expected_value_non_negative(&inputs.position, &inputs.market) {
                    RobotState::FinalSettlement
                } else {
                    RobotState::EvHedging
                }
            }

            RobotState::FinalSettlement => RobotState::FinalSettlement,
            RobotState::ChopMarketShutdown => RobotState::ChopMarketShutdown,
        }
    }

    /// 两边条件 PnL 的较小者是否达到利润目标。
    fn profit_target_reached(&self, position: &PositionSnapshot) -> bool {
        position.is_profit_locked()
            && position.up_win_pnl().min(position.down_win_pnl()) >= self.thresholds.profit_target
    }

    /// 复合对冲边界：某边亏损穿透风险线，且总成交股数达到最小规模。
    fn hedge_boundary_triggered(&self, inputs: &StepInputs) -> bool {
        let total_qty = inputs.position.up_qty + inputs.position.down_qty;
        if total_qty < self.thresholds.hedge_min_qty {
            return false;
        }
        let up_pnl = inputs.position.up_win_pnl();
        let down_pnl = inputs.position.down_win_pnl();
        up_pnl <= -self.thresholds.hedge_loss_trigger
            || down_pnl <= -self.thresholds.hedge_loss_trigger
    }
}

/// 以当前 Up 侧 Mark Price 作为胜出概率估计，判断交割数学期望是否非负。
fn expected_value_non_negative(position: &PositionSnapshot, market: &MarketSnapshot) -> bool {
    match market.mark_price(Side::Up) {
        Some(up_probability) => position.expected_value(up_probability) >= Money::ZERO,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::market::BookTop;
    use rust_decimal_macros::dec;

    fn thresholds() -> Thresholds {
        Thresholds {
            hedge_loss_trigger: dec!(30),
            hedge_min_qty: dec!(100),
            profit_target: dec!(15),
        }
    }

    fn position(up_qty: Money, down_qty: Money, total_cost: Money) -> PositionSnapshot {
        PositionSnapshot {
            up_qty,
            down_qty,
            total_cost,
        }
    }

    fn market_with_up_mid(up_mid: Qty) -> MarketSnapshot {
        MarketSnapshot {
            up: BookTop {
                best_bid: Some(up_mid),
                best_ask: Some(up_mid),
                last_trade: None,
            },
            down: BookTop::default(),
        }
    }

    fn neutral_market() -> MarketSnapshot {
        MarketSnapshot::default()
    }

    #[test]
    fn starts_in_initialization() {
        let machine = StateMachine::new(thresholds());
        assert_eq!(machine.state(), RobotState::Initialization);
    }

    #[test]
    fn thresholds_getter_returns_configured_values() {
        let machine = StateMachine::new(thresholds());
        let t = machine.thresholds();
        assert_eq!(t.hedge_loss_trigger, dec!(30));
        assert_eq!(t.hedge_min_qty, dec!(100));
        assert_eq!(t.profit_target, dec!(15));
    }

    #[test]
    fn initialization_does_not_move_on_step() {
        let mut machine = StateMachine::new(thresholds());
        let inputs = StepInputs {
            position: position(dec!(100), dec!(100), dec!(50)),
            market: neutral_market(),
        };
        let transition = machine.step(&inputs);
        assert!(!transition.is_moved());
        assert_eq!(machine.state(), RobotState::Initialization);
    }

    #[test]
    fn finish_initialization_enters_range_bound_making() {
        let mut machine = StateMachine::new(thresholds());
        let transition = machine.finish_initialization();
        assert!(transition.is_moved());
        assert_eq!(machine.state(), RobotState::RangeBoundMaking);
    }

    #[test]
    fn range_bound_making_to_final_settlement_when_profit_locked() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        let inputs = StepInputs {
            position: position(dec!(100), dec!(100), dec!(80)),
            market: neutral_market(),
        };
        let transition = machine.step(&inputs);
        assert_eq!(transition.state(), RobotState::FinalSettlement);
    }

    #[test]
    fn range_bound_profit_not_reached_when_below_target() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        let inputs = StepInputs {
            position: position(dec!(100), dec!(100), dec!(90)),
            market: neutral_market(),
        };
        let transition = machine.step(&inputs);
        assert_eq!(transition.state(), RobotState::RangeBoundMaking);
    }

    #[test]
    fn hedge_triggered_when_loss_breached_and_qty_sufficient() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        // up_pnl = 5 - 50 = -45 ≤ -30，总持仓 5+60=65 < 100 → 不触发。
        let inputs = StepInputs {
            position: position(dec!(5), dec!(60), dec!(50)),
            market: neutral_market(),
        };
        let transition = machine.step(&inputs);
        assert_eq!(transition.state(), RobotState::RangeBoundMaking);

        // 总持仓增加到 150 ≥ 100 → 触发。
        let inputs2 = StepInputs {
            position: position(dec!(50), dec!(100), dec!(180)),
            market: neutral_market(),
        };
        // up_pnl = 50 - 180 = -130 ≤ -30, total_qty = 150 ≥ 100 → 触发对冲。
        let transition2 = machine.step(&inputs2);
        assert_eq!(
            transition2.state(),
            RobotState::DynamicHedging {
                double_negative_count: 0
            }
        );
    }

    #[test]
    fn hedge_not_triggered_when_qty_insufficient() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        // up_pnl = 10 - 80 = -70 ≤ -30，但总持仓 10+40=50 < 100 → 不触发。
        let inputs = StepInputs {
            position: position(dec!(10), dec!(40), dec!(80)),
            market: neutral_market(),
        };
        let transition = machine.step(&inputs);
        assert_eq!(transition.state(), RobotState::RangeBoundMaking);
    }

    #[test]
    fn dynamic_hedging_first_double_negative_increments_count() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        // 先进入对冲（总持仓 150 ≥ 100，up_pnl = -130）。
        machine.step(&StepInputs {
            position: position(dec!(50), dec!(100), dec!(180)),
            market: neutral_market(),
        });
        // 两边均负：up_pnl = 40-100=-60，down_pnl = 50-100=-50。第一次 → 计数 1。
        let transition = machine.step(&StepInputs {
            position: position(dec!(40), dec!(50), dec!(100)),
            market: neutral_market(),
        });
        assert_eq!(
            transition.state(),
            RobotState::DynamicHedging {
                double_negative_count: 1
            }
        );
    }

    #[test]
    fn dynamic_hedging_second_double_negative_escalates_to_ev() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        machine.step(&StepInputs {
            position: position(dec!(50), dec!(100), dec!(180)),
            market: neutral_market(),
        });
        let double_negative = StepInputs {
            position: position(dec!(40), dec!(50), dec!(100)),
            market: neutral_market(),
        };
        machine.step(&double_negative);
        let transition = machine.step(&double_negative);
        assert_eq!(transition.state(), RobotState::EvHedging);
    }

    #[test]
    fn dynamic_hedging_to_final_settlement_when_profit_locked() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        machine.step(&StepInputs {
            position: position(dec!(50), dec!(100), dec!(180)),
            market: neutral_market(),
        });
        let transition = machine.step(&StepInputs {
            position: position(dec!(100), dec!(100), dec!(80)),
            market: neutral_market(),
        });
        assert_eq!(transition.state(), RobotState::FinalSettlement);
    }

    #[test]
    fn ev_hedging_to_final_settlement_when_ev_non_negative() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        machine.step(&StepInputs {
            position: position(dec!(50), dec!(100), dec!(180)),
            market: neutral_market(),
        });
        let double_negative = StepInputs {
            position: position(dec!(40), dec!(50), dec!(100)),
            market: neutral_market(),
        };
        machine.step(&double_negative);
        machine.step(&double_negative);
        assert_eq!(machine.state(), RobotState::EvHedging);
        // EV ≥ 0：up=120/down=80/cost=100，up概率0.6 → EV = 0.6×20 + 0.4×(-20) = 4 ≥ 0。
        let transition = machine.step(&StepInputs {
            position: position(dec!(120), dec!(80), dec!(100)),
            market: market_with_up_mid(dec!(0.6)),
        });
        assert_eq!(transition.state(), RobotState::FinalSettlement);
    }

    #[test]
    fn final_settlement_is_terminal() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        machine.step(&StepInputs {
            position: position(dec!(100), dec!(100), dec!(80)),
            market: neutral_market(),
        });
        assert_eq!(machine.state(), RobotState::FinalSettlement);
        let transition = machine.step(&StepInputs {
            position: position(dec!(200), dec!(200), dec!(50)),
            market: neutral_market(),
        });
        assert!(!transition.is_moved());
    }
}
