//! 梯度接低模块：初始布阵（Gradient Low-Catching, Initial Deployment）。
//!
//! 对应策略说明书第四节。场开始时：
//! 1. **选主战场**：对比双边 `best_ask`，选 `best_ask < 0.5` 的一侧作为做市主战场；
//!    若两侧皆满足则取更低者，皆不满足则放弃布阵（返回空指令）。
//! 2. **三层非饱和铺单**：以该侧 `best_ask` 为基准向下平铺三层 Maker 买单，
//!    层距与池占比可配（默认 1 层 ask-0.01 用 2%、2 层 ask-0.02 用 3%、3 层 ask-0.03 用 5%），
//!    4 层及以下保持真空。
//!
//! 本模块为纯逻辑：输入行情与池额度，产出一组 [`Command`]，不直接触碰交易所
//! （见架构决策：策略产出指令列表）。跨侧配对重算留待后续阶段。

use domain::market::MarketSnapshot;
use domain::order::{
    Command, Generation, Order, OrderConstraints, OrderDirection, OrderIdGenerator,
};
use domain::types::{Money, OrderRole, Price, Qty, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// 单层梯度的布阵参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderRung {
    /// 相对该侧 best_ask 的向下价格偏移（正数，挂单价 = best_ask - offset）。
    pub price_offset: Decimal,
    /// 动用核心做市池的比例（如 0.02 表示 2%）。
    pub pool_fraction: Decimal,
}

/// 梯度接低的初始布阵配置：三层梯度 + 主战场判定阈值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderConfig {
    /// 三层梯度，从首档到末档。
    pub rungs: [LadderRung; 3],
    /// 主战场选择阈值：仅 best_ask 低于此值的一侧才可作为做市主战场。
    pub main_field_max_ask: Price,
}

impl Default for LadderConfig {
    /// 策略默认布阵：三层偏移 0.01/0.02/0.03，池占比 2%/3%/5%，主战场阈值 0.5。
    fn default() -> Self {
        Self {
            rungs: [
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
            main_field_max_ask: dec!(0.5),
        }
    }
}

/// 梯度接低初始布阵器。
#[derive(Debug, Clone)]
pub struct GradientLadder {
    config: LadderConfig,
}

impl GradientLadder {
    /// 以指定配置创建布阵器。
    pub fn new(config: LadderConfig) -> Self {
        Self { config }
    }

    /// 以策略默认配置创建布阵器。
    pub fn with_default_config() -> Self {
        Self::new(LadderConfig::default())
    }

    /// 依据行情选出本轮做市主战场。
    ///
    /// 取双边中 `best_ask` 低于阈值且更小的一侧；两侧皆不满足时返回 `None`。
    pub fn select_main_field(&self, market: &MarketSnapshot) -> Option<Side> {
        let up_ask = market.book(Side::Up).best_ask;
        let down_ask = market.book(Side::Down).best_ask;
        let threshold = self.config.main_field_max_ask;

        match (up_ask, down_ask) {
            (Some(up), Some(down)) => {
                let up_ok = up < threshold;
                let down_ok = down < threshold;
                match (up_ok, down_ok) {
                    (true, true) => Some(if up <= down { Side::Up } else { Side::Down }),
                    (true, false) => Some(Side::Up),
                    (false, true) => Some(Side::Down),
                    (false, false) => None,
                }
            }
            (Some(up), None) if up < threshold => Some(Side::Up),
            (None, Some(down)) if down < threshold => Some(Side::Down),
            _ => None,
        }
    }

    /// 生成初始布阵的下单指令。
    ///
    /// `grid_maker_pool` 为核心做市池额度上限；`id_generator` 为每层分配订单标识；
    /// `generation` 为本批挂单的世代号；`constraints` 为交易所最小量约束。
    /// 无主战场时返回空指令列表；某档不满足最小份数/金额约束时跳过该档（不上调，保持资金纪律）。
    pub fn deploy(
        &self,
        market: &MarketSnapshot,
        grid_maker_pool: Money,
        constraints: &OrderConstraints,
        id_generator: &mut OrderIdGenerator,
        generation: Generation,
    ) -> Vec<Command> {
        let Some(side) = self.select_main_field(market) else {
            return Vec::new();
        };
        let best_ask = market.book(side).best_ask.expect("主战场必有 best_ask");

        let mut commands = Vec::with_capacity(self.config.rungs.len());
        for rung in &self.config.rungs {
            // 价格按交易所精度向下量化（买单向下取整不抬高买价）。
            let price = constraints.quantize_price(best_ask - rung.price_offset);
            // 价格非正的档位无意义，跳过（极端薄盘下的保护）。
            if price <= Decimal::ZERO {
                continue;
            }
            // 股数先按精度向下量化，再以量化后的实际下单值校验最小量约束。
            let qty =
                constraints.quantize_qty(self.rung_qty(grid_maker_pool, rung.pool_fraction, price));
            // 不满足交易所最小份数/金额约束的档位直接跳过（不上调，避免破坏池占比与超支）。
            if !constraints.is_satisfied(qty, price) {
                continue;
            }
            let order = Order {
                order_id: id_generator.next_id(),
                side,
                direction: OrderDirection::Buy,
                price,
                qty,
                role: OrderRole::Maker,
                generation,
            };
            commands.push(Command::SubmitOrder(order));
        }
        commands
    }

    /// 单层股数 = 池额度 × 占比 ÷ 挂单价（把分配金额换算为股数）。
    fn rung_qty(&self, grid_maker_pool: Money, pool_fraction: Decimal, price: Price) -> Qty {
        let budget = grid_maker_pool * pool_fraction;
        budget / price
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::market::BookTop;

    /// 构造指定双边 best_ask 的市场快照。
    fn market(up_ask: Option<Price>, down_ask: Option<Price>) -> MarketSnapshot {
        MarketSnapshot {
            up: BookTop {
                best_bid: None,
                best_ask: up_ask,
                last_trade: None,
            },
            down: BookTop {
                best_bid: None,
                best_ask: down_ask,
                last_trade: None,
            },
        }
    }

    #[test]
    fn selects_only_side_below_threshold() {
        let ladder = GradientLadder::with_default_config();
        // 仅 Up 侧 0.4 < 0.5，Down 侧 0.7 不满足 → 选 Up。
        let side = ladder.select_main_field(&market(Some(dec!(0.4)), Some(dec!(0.7))));
        assert_eq!(side, Some(Side::Up));
    }

    #[test]
    fn selects_lower_ask_when_both_below_threshold() {
        let ladder = GradientLadder::with_default_config();
        // 两侧皆 < 0.5，取更低者 Down(0.3)。
        let side = ladder.select_main_field(&market(Some(dec!(0.45)), Some(dec!(0.3))));
        assert_eq!(side, Some(Side::Down));
    }

    #[test]
    fn no_main_field_when_neither_below_threshold() {
        let ladder = GradientLadder::with_default_config();
        let side = ladder.select_main_field(&market(Some(dec!(0.6)), Some(dec!(0.7))));
        assert_eq!(side, None);
    }

    #[test]
    fn deploy_produces_three_descending_rungs() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 主战场 Up，best_ask = 0.40，核心做市池 1000。
        let commands = ladder.deploy(
            &market(Some(dec!(0.40)), Some(dec!(0.80))),
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::first(),
        );
        assert_eq!(commands.len(), 3);

        // 逐层校验价格与股数：股数 = 池额度 × 占比 ÷ 挂单价，再按精度向下量化。
        // 保留完整算式以体现推导链（如 1 层 = 1000 × 2% / 0.39 = 51.282… → 量化 51.28）。
        let constraints = OrderConstraints::default();
        let expected = [
            (dec!(0.39), constraints.quantize_qty(dec!(1000) * dec!(0.02) / dec!(0.39))), // 1 层：ask-0.01，2%
            (dec!(0.38), constraints.quantize_qty(dec!(1000) * dec!(0.03) / dec!(0.38))), // 2 层：ask-0.02，3%
            (dec!(0.37), constraints.quantize_qty(dec!(1000) * dec!(0.05) / dec!(0.37))), // 3 层：ask-0.03，5%
        ];
        for (command, (exp_price, exp_qty)) in commands.iter().zip(expected) {
            match command {
                Command::SubmitOrder(order) => {
                    assert_eq!(order.side, Side::Up);
                    assert_eq!(order.direction, OrderDirection::Buy);
                    assert_eq!(order.role, OrderRole::Maker);
                    assert_eq!(order.price, exp_price);
                    assert_eq!(order.qty, exp_qty);
                }
                _ => panic!("应为 SubmitOrder 指令"),
            }
        }
    }

    #[test]
    fn deploy_assigns_unique_increasing_order_ids() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        let commands = ladder.deploy(
            &market(Some(dec!(0.40)), None),
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::first(),
        );
        let ids: Vec<_> = commands
            .iter()
            .map(|c| match c {
                Command::SubmitOrder(o) => o.order_id,
                _ => panic!("应为 SubmitOrder"),
            })
            .collect();
        // 三个标识互不相同且递增。
        assert!(ids[0] < ids[1] && ids[1] < ids[2]);
    }

    #[test]
    fn deploy_skips_rung_below_min_order_size() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        // 小核心做市池 105、主战场 best_ask = 0.45：
        // 1 层 价0.44 预算105×2%=2.1 股数≈4.77 < 5 份 → 跳过；
        // 2 层 价0.43 预算105×3%=3.15 股数≈7.33 ≥5 且金额≥1 → 保留；
        // 3 层 价0.42 预算105×5%=5.25 股数≈12.5 → 保留。
        let commands = ladder.deploy(
            &market(Some(dec!(0.45)), None),
            dec!(105),
            &OrderConstraints::default(),
            &mut generator,
            Generation::first(),
        );
        // 首档因不足 5 份被跳过，仅保留 2 档。
        assert_eq!(commands.len(), 2);
        // 保留档的价格应为 2 层与 3 层（0.43、0.42），首档 0.44 缺席。
        let prices: Vec<_> = commands
            .iter()
            .map(|c| match c {
                Command::SubmitOrder(o) => o.price,
                _ => panic!("应为 SubmitOrder"),
            })
            .collect();
        assert_eq!(prices, vec![dec!(0.43), dec!(0.42)]);
    }

    #[test]
    fn deploy_empty_when_no_main_field() {
        let ladder = GradientLadder::with_default_config();
        let mut generator = OrderIdGenerator::new();
        let commands = ladder.deploy(
            &market(Some(dec!(0.6)), Some(dec!(0.7))),
            dec!(1000),
            &OrderConstraints::default(),
            &mut generator,
            Generation::first(),
        );
        assert!(commands.is_empty());
    }
}
