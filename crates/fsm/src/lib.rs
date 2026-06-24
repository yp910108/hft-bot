//! 策略状态机：根据持仓盈亏决定当前该做市、对冲还是收手。
//!
//! 纯逻辑，不碰 IO。上层每次喂入持仓快照，调用 [`StateMachine::step`] 判断要不要跳状态。
//! 阈值以绝对金额传入，状态机不关心总资金多大。

use domain::market::MarketSnapshot;
use domain::pnl::PositionSnapshot;
use domain::state::RobotState;
use domain::types::{Money, Qty, Side};

/// 状态跳转用到的几个门槛值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// 单边亏多少钱就触发对冲（正数）。
    pub hedge_loss_trigger: Money,
    /// 总持仓（up+down）至少要这么多股才允许触发对冲，防止刚开局就误报。
    pub hedge_min_qty: Qty,
    /// 两边条件 PnL 的较小者达到这个数就收手结算。
    pub profit_target: Money,
}

/// 每次 step 需要的输入：当前持仓和盘口。
#[derive(Debug, Clone, Copy)]
pub struct StepInputs {
    /// 当前持仓，用来算盈亏。
    pub position: PositionSnapshot,
    /// 当前盘口，EV 对冲阶段用它估算胜出概率。
    pub market: MarketSnapshot,
}

/// step 的返回值：告诉调用方状态有没有变。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// 没变。
    Unchanged(RobotState),
    /// 跳了，新状态在里面。
    Moved(RobotState),
}

impl Transition {
    /// 取当前状态，不管有没有跳转。
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

/// 策略状态机：记住当前状态，每次喂入数据就判断要不要跳。
#[derive(Debug, Clone)]
pub struct StateMachine {
    state: RobotState,
    thresholds: Thresholds,
}

impl StateMachine {
    /// 创建状态机，初始状态为 [`RobotState::Initialization`]。
    pub fn new(thresholds: Thresholds) -> Self {
        Self {
            state: RobotState::default(),
            thresholds,
        }
    }

    /// 当前状态。
    pub fn state(&self) -> RobotState {
        self.state
    }

    /// 读取阈值配置。
    pub fn thresholds(&self) -> Thresholds {
        self.thresholds
    }

    /// 喂入一次数据，判断要不要跳状态，跳了就更新自身。
    pub fn step(&mut self, inputs: &StepInputs) -> Transition {
        let next = self.next_state(inputs);
        if next != self.state {
            self.state = next;
            Transition::Moved(next)
        } else {
            Transition::Unchanged(next)
        }
    }

    /// 初始挂单完成后，上层调用这个方法让状态机进入正式做市。
    pub fn finish_initialization(&mut self) -> Transition {
        if self.state == RobotState::Initialization {
            self.state = RobotState::RangeBoundMaking;
            Transition::Moved(self.state)
        } else {
            Transition::Unchanged(self.state)
        }
    }

    /// 根据当前状态和输入，算出下一个状态应该是什么（不改自身）。
    fn next_state(&self, inputs: &StepInputs) -> RobotState {
        match self.state {
            RobotState::Initialization => RobotState::Initialization,

            RobotState::RangeBoundMaking => {
                if self.profit_target_reached(&inputs.position) {
                    RobotState::FinalSettlement
                } else if self.hedge_boundary_triggered(&inputs.position) {
                    // 两边同时亏 → 补哪边都没用，直接进 EV 对冲按胜率追买。
                    if inputs.position.both_sides_negative() {
                        RobotState::EvHedging
                    } else {
                        RobotState::DynamicHedging {
                            double_negative_count: 0,
                        }
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

    /// 两边条件 PnL 的较小者是否够到利润目标。
    fn profit_target_reached(&self, position: &PositionSnapshot) -> bool {
        position.is_profit_locked()
            && position.up_win_pnl().min(position.down_win_pnl()) >= self.thresholds.profit_target
    }

    /// 是否该触发对冲：某边亏损穿线，且总持仓够大（排除开局误报）。
    fn hedge_boundary_triggered(&self, position: &PositionSnapshot) -> bool {
        position
            .breached_side(self.thresholds.hedge_loss_trigger, self.thresholds.hedge_min_qty)
            .is_some()
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

        // 总持仓 ≥ 100，仅单边穿线（up_pnl = 50-180 = -130 ≤ -30），
        // 但 down_pnl = 150-180 = -30，不算严格穿透（要 ≤ -30 才算，边界不触发 both_sides_negative 因为 down > 0 不成立...）
        // 换个数据：up_pnl = 20-80 = -60 ≤ -30, down_pnl = 100-80 = 20 > 0。单边穿线。
        let inputs2 = StepInputs {
            position: position(dec!(20), dec!(100), dec!(80)),
            market: neutral_market(),
        };
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
        // 单边穿线进入 DynamicHedging：up_pnl = 20-80 = -60 ≤ -30, down_pnl = 100-80 = 20 > 0。
        machine.step(&StepInputs {
            position: position(dec!(20), dec!(100), dec!(80)),
            market: neutral_market(),
        });
        assert_eq!(
            machine.state(),
            RobotState::DynamicHedging { double_negative_count: 0 }
        );
        // 两边均负：up_pnl = 40-100=-60，down_pnl = 50-100=-50。第一次 → 计数 1。
        let transition = machine.step(&StepInputs {
            position: position(dec!(40), dec!(50), dec!(100)),
            market: neutral_market(),
        });
        assert!(transition.is_moved());
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
        // 单边穿线进入 DynamicHedging。
        machine.step(&StepInputs {
            position: position(dec!(20), dec!(100), dec!(80)),
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
        // 单边穿线进入 DynamicHedging。
        machine.step(&StepInputs {
            position: position(dec!(20), dec!(100), dec!(80)),
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
        // 单边穿线进入 DynamicHedging。
        machine.step(&StepInputs {
            position: position(dec!(20), dec!(100), dec!(80)),
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

    #[test]
    fn both_sides_breached_skips_dynamic_goes_to_ev() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        // 双边同时穿线且 both_sides_negative：
        // up_pnl = 50-180 = -130, down_pnl = 100-180 = -80，都 ≤ -30，且都 < 0。
        // 应直接跳 EvHedging，不经过 DynamicHedging。
        let transition = machine.step(&StepInputs {
            position: position(dec!(50), dec!(100), dec!(180)),
            market: neutral_market(),
        });
        assert_eq!(transition.state(), RobotState::EvHedging);
    }
}
