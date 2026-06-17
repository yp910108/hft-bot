use serde::{Deserialize, Serialize};

/// 机器人有限状态机的状态枚举。
///
/// 对应策略说明书第三节的 FSM 流转架构，所有状态切换均由
/// 交易所异步回报的成交事件驱动（单写者事件循环串行处理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RobotState {
    /// 初始化：场开始时部署初始梯度单。
    #[default]
    Initialization,
    /// 区间做市：常规梯度接低循环，纯 Maker 被动撮合。
    RangeBoundMaking,
    /// 动态对冲：单边瘸腿触发复合对冲边界后，Taker + Maker 交替防御。
    ///
    /// `double_negative_count` 记录在本状态下「两边条件 PnL 同时为负」已发生的次数：
    /// 第一次发生时放大亏损上限、继续对冲；累计第二次则升级进入 [`RobotState::EvHedging`]。
    DynamicHedging { double_negative_count: u8 },
    /// EV 对冲：连续两次双边均负后，转入数学期望兜底模式。
    EvHedging,
    /// 收手结算：达成终局边界后撤单停手，死扛至交割。
    FinalSettlement,
    /// 猴市熔断：极端多空双杀，一键 Taker 全盘清仓关机。
    ChopMarketShutdown,
}
