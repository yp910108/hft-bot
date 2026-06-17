//! 有限状态机层：策略的「大脑」，定义状态流转规则与对冲触发判定。
//!
//! 纯逻辑、不触碰任何 IO，对应策略说明书第三节 FSM 流转架构。
//! 状态机由交易所成交事件驱动；每收到一次输入即调用 [`StateMachine::step`] 评估是否跳转。
//!
//! 阈值一律以**绝对金额**传入（由上层把「相对总资金 V 的比例」换算为绝对值，
//! 见策略风险修复项 #4），状态机本身不感知资金规模与比例。

use domain::market::MarketSnapshot;
use domain::pnl::PositionSnapshot;
use domain::state::RobotState;
use domain::types::{Money, Side};
use rust_decimal::Decimal;

/// 状态流转所需的绝对金额阈值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// 复合对冲触发的单边亏损线（正数，表示「亏损达到该值」即 `Side_PnL <= -loss_trigger`）。
    pub hedge_loss_trigger: Money,
    /// 复合对冲触发的安全价格线：某侧 Mark Price 低于此值才确认单边暴砸。
    pub hedge_safety_price: Decimal,
    /// 收手结算的利润达标线：两边条件 PnL 的较小者达到此值即锁定离场。
    pub profit_target: Money,
}

/// 单步评估所需的输入快照。
#[derive(Debug, Clone, Copy)]
pub struct StepInputs {
    /// 当前双边持仓快照，用于计算条件盈亏。
    pub position: PositionSnapshot,
    /// 当前市场快照，用于读取各侧 Mark Price。
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
    ///
    /// `Initialization → RangeBoundMaking` 这条边不依赖行情判定，而是初始布阵完成的信号，
    /// 故单独提供方法，不混入由盈亏驱动的 `step`。
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
            // 初始化阶段不由盈亏驱动跳转，等待 finish_initialization。
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
                    // 两边均负：第一次放大亏损上限、留在对冲；累计第二次升级到 EV 对冲。
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

            // 终态：不再跳转。
            RobotState::FinalSettlement => RobotState::FinalSettlement,
            RobotState::ChopMarketShutdown => RobotState::ChopMarketShutdown,
        }
    }

    /// 两边条件 PnL 的较小者是否达到利润目标（达标即可锁定离场）。
    fn profit_target_reached(&self, position: &PositionSnapshot) -> bool {
        position.is_profit_locked()
            && position.up_win_pnl().min(position.down_win_pnl()) >= self.thresholds.profit_target
    }

    /// 复合对冲边界：某边亏损穿透风险线，且该边 Mark Price 跌破安全价。
    fn hedge_boundary_triggered(&self, inputs: &StepInputs) -> bool {
        self.side_hedge_triggered(Side::Up, inputs) || self.side_hedge_triggered(Side::Down, inputs)
    }

    /// 单侧的复合对冲判定：该侧胜出 PnL ≤ -亏损线，且该侧 Mark Price < 安全价。
    fn side_hedge_triggered(&self, side: Side, inputs: &StepInputs) -> bool {
        let side_pnl = match side {
            Side::Up => inputs.position.up_win_pnl(),
            Side::Down => inputs.position.down_win_pnl(),
        };
        let pnl_breached = side_pnl <= -self.thresholds.hedge_loss_trigger;
        let price_breached = match inputs.market.mark_price(side) {
            Some(mark) => mark < self.thresholds.hedge_safety_price,
            None => false,
        };
        pnl_breached && price_breached
    }
}

/// 以当前 Up 侧 Mark Price 作为胜出概率估计，判断交割数学期望是否非负。
///
/// 此处藏有 fsm 层的策略假设——用 Up 侧 Mark Price 近似 Up 胜出概率
/// （二元市场中价格 ≈ 概率），故归属 fsm 而非 domain；若无法取得则保守视为期望为负。
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

    /// 测试用阈值：对冲亏损线 30、安全价 0.5、利润目标 15。
    fn thresholds() -> Thresholds {
        Thresholds {
            hedge_loss_trigger: dec!(30),
            hedge_safety_price: dec!(0.5),
            profit_target: dec!(15),
        }
    }

    /// 构造持仓快照。
    fn position(up_qty: Money, down_qty: Money, total_cost: Money) -> PositionSnapshot {
        PositionSnapshot {
            up_qty,
            down_qty,
            total_cost,
        }
    }

    /// 构造仅设定某侧中间价的市场快照（买一、卖一对称取均价）。
    fn market_with_up_mid(up_mid: Decimal) -> MarketSnapshot {
        MarketSnapshot {
            up: BookTop {
                best_bid: Some(up_mid),
                best_ask: Some(up_mid),
                last_trade: None,
            },
            down: BookTop::default(),
        }
    }

    /// 构造空盘口的中性市场快照。
    fn neutral_market() -> MarketSnapshot {
        MarketSnapshot::default()
    }

    #[test]
    fn starts_in_initialization() {
        let machine = StateMachine::new(thresholds());
        assert_eq!(machine.state(), RobotState::Initialization);
    }

    #[test]
    fn initialization_does_not_move_on_step() {
        let mut machine = StateMachine::new(thresholds());
        let inputs = StepInputs {
            position: position(dec!(100), dec!(100), dec!(50)),
            market: neutral_market(),
        };
        // 初始化阶段不由盈亏驱动跳转。
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
        // up_pnl = 100-80 = 20，down_pnl = 100-80 = 20，较小者 20 ≥ 利润目标 15。
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
        // 两边 pnl 均为 10，达成双向盈利但 10 < 利润目标 15，不应离场。
        let inputs = StepInputs {
            position: position(dec!(100), dec!(100), dec!(90)),
            market: neutral_market(),
        };
        let transition = machine.step(&inputs);
        assert_eq!(transition.state(), RobotState::RangeBoundMaking);
        assert!(!transition.is_moved());
    }

    #[test]
    fn range_bound_to_dynamic_hedging_on_composite_boundary() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        // Up 侧瘸腿触发复合对冲边界：up_pnl = 5 - 50 = -45 ≤ -30（亏损穿透），
        // 且 Up 侧 mark 0.3 < 安全价 0.5（价格暴砸）。Down 侧此时 down_pnl = 60 - 50 = +10。
        let inputs = StepInputs {
            position: position(dec!(5), dec!(60), dec!(50)),
            market: market_with_up_mid(dec!(0.3)),
        };
        let transition = machine.step(&inputs);
        assert_eq!(
            transition.state(),
            RobotState::DynamicHedging {
                double_negative_count: 0
            }
        );
    }

    #[test]
    fn hedge_not_triggered_when_price_not_breached() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        // up_pnl = -45 ≤ -30 满足亏损线，但 Up mark 0.6 ≥ 安全价 0.5，复合判定不成立。
        let inputs = StepInputs {
            position: position(dec!(5), dec!(60), dec!(50)),
            market: market_with_up_mid(dec!(0.6)),
        };
        let transition = machine.step(&inputs);
        assert_eq!(transition.state(), RobotState::RangeBoundMaking);
    }

    #[test]
    fn dynamic_hedging_first_double_negative_increments_count() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        // 先进入对冲。
        machine.step(&StepInputs {
            position: position(dec!(5), dec!(60), dec!(50)),
            market: market_with_up_mid(dec!(0.3)),
        });
        // 两边均负：up_pnl = 40-100 = -60，down_pnl = 50-100 = -50。第一次 → 计数变 1，仍在对冲。
        let transition = machine.step(&StepInputs {
            position: position(dec!(40), dec!(50), dec!(100)),
            market: market_with_up_mid(dec!(0.3)),
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
            position: position(dec!(5), dec!(60), dec!(50)),
            market: market_with_up_mid(dec!(0.3)),
        });
        let double_negative = StepInputs {
            position: position(dec!(40), dec!(50), dec!(100)),
            market: market_with_up_mid(dec!(0.3)),
        };
        // 第一次双负 → count = 1。
        machine.step(&double_negative);
        // 第二次双负 → 升级到 EV 对冲。
        let transition = machine.step(&double_negative);
        assert_eq!(transition.state(), RobotState::EvHedging);
    }

    #[test]
    fn dynamic_hedging_to_final_settlement_when_profit_locked() {
        let mut machine = StateMachine::new(thresholds());
        machine.finish_initialization();
        machine.step(&StepInputs {
            position: position(dec!(5), dec!(60), dec!(50)),
            market: market_with_up_mid(dec!(0.3)),
        });
        // 对冲后两边 pnl 均达标（各 20 ≥ 15）→ 收手。
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
        // 驱动进入 EV 对冲。
        machine.step(&StepInputs {
            position: position(dec!(5), dec!(60), dec!(50)),
            market: market_with_up_mid(dec!(0.3)),
        });
        let double_negative = StepInputs {
            position: position(dec!(40), dec!(50), dec!(100)),
            market: market_with_up_mid(dec!(0.3)),
        };
        machine.step(&double_negative);
        machine.step(&double_negative);
        assert_eq!(machine.state(), RobotState::EvHedging);
        // EV ≥ 0：持仓 up=120/down=80/成本100，up 概率 0.6 → EV = 0.6×20 + 0.4×(-20) = 4 ≥ 0。
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
        // 终态不再因任何输入跳转。
        let transition = machine.step(&StepInputs {
            position: position(dec!(5), dec!(60), dec!(50)),
            market: market_with_up_mid(dec!(0.3)),
        });
        assert!(!transition.is_moved());
        assert_eq!(machine.state(), RobotState::FinalSettlement);
    }
}
