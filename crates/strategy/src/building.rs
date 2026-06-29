//! 建仓态：开局在主战场侧铺三档梯度 Maker 买单，等首笔成交跳配对。
//!
//! 纯函数：没有「我铺过没」的记忆，靠读 ctx 推断——
//! 主战场无活跃挂单且无持仓 → 铺三档；一旦有持仓（首笔成交）→ 跳配对态。

use crate::PhaseStrategy;
use crate::config::StrategyConfig;
use crate::context::{CommandIntent, Decision, DecisionContext, OrderIntent};
use domain::market::MarketSnapshot;
use domain::state::RobotState;
use domain::types::{Money, Price, Qty, Side};
use rust_decimal::Decimal;

/// 建仓态小策略。
#[derive(Debug, Clone)]
pub struct BuildingStrategy {
    cfg: StrategyConfig,
}

impl BuildingStrategy {
    pub fn new(cfg: StrategyConfig) -> Self {
        Self { cfg }
    }

    /// 选主战场：best_ask < 阈值的一侧，两侧皆满足取更便宜者；都不满足 None。
    pub fn select_main_field(&self, market: &MarketSnapshot) -> Option<Side> {
        let threshold = self.cfg.main_field_max_ask;
        let up = market.book(Side::Up).best_ask.filter(|&a| a < threshold);
        let down = market.book(Side::Down).best_ask.filter(|&a| a < threshold);
        match (up, down) {
            (Some(u), Some(d)) => Some(if u <= d { Side::Up } else { Side::Down }),
            (Some(_), None) => Some(Side::Up),
            (None, Some(_)) => Some(Side::Down),
            (None, None) => None,
        }
    }

    /// 单档股数 = 池总额 × 占比 ÷ 挂单价。
    fn rung_qty(&self, grid_maker_total: Money, pool_fraction: Decimal, price: Price) -> Qty {
        grid_maker_total * pool_fraction / price
    }
}

impl PhaseStrategy for BuildingStrategy {
    fn decide(&self, ctx: &DecisionContext) -> Decision {
        // 已有任意持仓 → 首笔成交发生过 → 跳配对态。
        if ctx.position.up_qty > Qty::ZERO || ctx.position.down_qty > Qty::ZERO {
            return Decision::transition(RobotState::Pairing);
        }

        // 已经铺了单还没成交 → 等着，别重复铺。
        if !ctx.active_orders.is_empty() {
            return Decision::skip();
        }

        // 选主战场；选不出（无便宜侧）→ 这一 tick 空过。
        let Some(side) = self.select_main_field(&ctx.market) else {
            return Decision::skip();
        };
        let best_ask = match ctx.market.book(side).best_ask {
            Some(a) => a,
            None => return Decision::skip(),
        };

        // 铺三档，价格向下取整、股数向下量化后校验最小量，不够的档跳过。
        let mut decision = Decision::skip();
        for rung in &self.cfg.building_rungs {
            let price = ctx.constraints.quantize_price(best_ask - rung.price_offset);
            if price <= Price::ZERO {
                continue;
            }
            let qty = ctx.constraints.quantize_qty(self.rung_qty(
                ctx.pools.grid_maker_total,
                rung.pool_fraction,
                price,
            ));
            if !ctx.constraints.is_satisfied(qty, price) {
                continue;
            }
            decision = decision.with(CommandIntent::Submit(OrderIntent::maker_buy(
                side, price, qty,
            )));
        }
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ActiveOrder, PoolBudgets, Trigger};
    use domain::market::BookTop;
    use domain::order::{OrderConstraints, OrderDirection, OrderId};
    use domain::pnl::PositionSnapshot;
    use domain::types::OrderRole;
    use rust_decimal_macros::dec;

    fn book(bid: Option<Price>, ask: Option<Price>) -> BookTop {
        BookTop {
            best_bid: bid,
            best_ask: ask,
            last_trade: None,
        }
    }

    fn ctx<'a>(
        market: MarketSnapshot,
        position: PositionSnapshot,
        active: &'a [ActiveOrder],
    ) -> DecisionContext<'a> {
        DecisionContext {
            total_capital: dec!(1000),
            trigger: Trigger::BookUpdate,
            now: 0,
            time_to_expiry: 600_000,
            state: RobotState::Building,
            main_field: None,
            main_field_frozen: false,
            position,
            market,
            pools: PoolBudgets {
                grid_maker_total: dec!(150),
                grid_maker_remaining: dec!(150),
                dynamic_remaining: dec!(225),
                ev_remaining: dec!(375),
                max_exposure: dec!(112.5),
            },
            active_orders: active,
            last_hedge_at: None,
            funds_exhausted: false,
            double_negative_count: 0,
            was_double_negative: false,
            calm_since: None,
            constraints: OrderConstraints::default(),
        }
    }

    fn empty_pos() -> PositionSnapshot {
        PositionSnapshot {
            up_qty: dec!(0),
            down_qty: dec!(0),
            up_cost: dec!(0),
            down_cost: dec!(0),
        }
    }

    fn strat() -> BuildingStrategy {
        BuildingStrategy::new(StrategyConfig::default())
    }

    #[test]
    fn selects_cheaper_side_below_threshold() {
        let s = strat();
        let market = MarketSnapshot {
            up: book(Some(dec!(0.39)), Some(dec!(0.40))),
            down: book(Some(dec!(0.59)), Some(dec!(0.60))),
        };
        assert_eq!(s.select_main_field(&market), Some(Side::Up));
    }

    #[test]
    fn selects_down_when_down_cheaper() {
        let s = strat();
        let market = MarketSnapshot {
            up: book(Some(dec!(0.59)), Some(dec!(0.60))),
            down: book(Some(dec!(0.39)), Some(dec!(0.40))),
        };
        assert_eq!(s.select_main_field(&market), Some(Side::Down));
    }

    #[test]
    fn no_main_field_when_neither_below_threshold() {
        let s = strat();
        let market = MarketSnapshot {
            up: book(Some(dec!(0.55)), Some(dec!(0.56))),
            down: book(Some(dec!(0.55)), Some(dec!(0.56))),
        };
        assert_eq!(s.select_main_field(&market), None);
    }

    #[test]
    fn deploys_three_rungs_on_empty_book() {
        let market = MarketSnapshot {
            up: book(Some(dec!(0.39)), Some(dec!(0.40))),
            down: book(Some(dec!(0.59)), Some(dec!(0.60))),
        };
        let d = strat().decide(&ctx(market, empty_pos(), &[]));
        // 三档都满足最小量 → 三条 Submit。
        assert_eq!(d.commands.len(), 3);
        // 价格依次为 0.39/0.38/0.37（ask 0.40 减 0.01/0.02/0.03）。
        for (i, expected_price) in [dec!(0.39), dec!(0.38), dec!(0.37)].iter().enumerate() {
            match &d.commands[i] {
                CommandIntent::Submit(o) => {
                    assert_eq!(o.price, *expected_price);
                    assert_eq!(o.side, Side::Up);
                    assert_eq!(o.role, OrderRole::Maker);
                }
                _ => panic!("应为 Submit"),
            }
        }
    }

    #[test]
    fn transitions_to_pairing_when_position_exists() {
        let market = MarketSnapshot {
            up: book(Some(dec!(0.39)), Some(dec!(0.40))),
            down: BookTop::default(),
        };
        let pos = PositionSnapshot {
            up_qty: dec!(10),
            down_qty: dec!(0),
            up_cost: dec!(4),
            down_cost: dec!(0),
        };
        let d = strat().decide(&ctx(market, pos, &[]));
        assert_eq!(d.transition, Some(RobotState::Pairing));
    }

    #[test]
    fn does_not_redeploy_when_orders_active() {
        let market = MarketSnapshot {
            up: book(Some(dec!(0.39)), Some(dec!(0.40))),
            down: BookTop::default(),
        };
        let active = [ActiveOrder {
            order_id: OrderId(1),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.39),
            qty: dec!(10),
            role: OrderRole::Maker,
        }];
        let d = strat().decide(&ctx(market, empty_pos(), &active));
        assert!(d.is_skip());
    }

    #[test]
    fn skips_when_no_main_field() {
        let market = MarketSnapshot {
            up: book(Some(dec!(0.55)), Some(dec!(0.56))),
            down: book(Some(dec!(0.55)), Some(dec!(0.56))),
        };
        let d = strat().decide(&ctx(market, empty_pos(), &[]));
        assert!(d.is_skip());
    }
}
