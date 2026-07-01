//! 阶段 1：开场建仓。双边铺 Maker 买单建底仓。
//!
//! 逻辑：
//! - 在 UP 和 DN 两侧各挂 Maker 买单（bid 价排队）。
//! - 优先便宜侧（price 更低的先挂）。
//! - 每侧同时只保留一个活跃买单（成交后再挂下一笔）。
//! - 超过 building_end 时触发阶段跳转到 Cycling。

use crate::context::{ActiveOrder, CommandIntent, Decision, DecisionContext};
use domain::order::{OrderDirection, TimeInForce};
use domain::phase::Phase;
use domain::types::{OrderRole, Side};

/// 开场建仓策略。
pub struct BuildingStrategy;

impl BuildingStrategy {
    pub fn decide(&self, ctx: &DecisionContext) -> Decision {
        // 到时间了 → 跳转 Cycling。
        if ctx.progress >= ctx.config.building_end {
            return Decision {
                commands: vec![],
                transition: Some(Phase::Cycling),
            };
        }

        let mut commands = Vec::new();

        // 两侧各最多保持一个活跃 Maker 买单。没有就挂。
        let (cheap_side, expensive_side) = cheaper_side(ctx);

        // 优先便宜侧。
        for &side in &[cheap_side, expensive_side] {
            if has_active_buy(ctx.active_orders, side) {
                continue;
            }
            if let Some(cmd) = make_buy_intent(ctx, side) {
                commands.push(cmd);
            }
        }

        Decision {
            commands,
            transition: None,
        }
    }
}

/// 判断某侧是否已有活跃买单。
fn has_active_buy(orders: &[ActiveOrder], side: Side) -> bool {
    orders
        .iter()
        .any(|o| o.side == side && o.direction == OrderDirection::Buy)
}

/// 构造一笔 Maker 买单意图：在该侧 bid 价挂 lot_qty。
/// 盘口无 bid 时不挂（无法定价）。
fn make_buy_intent(ctx: &DecisionContext, side: Side) -> Option<CommandIntent> {
    let bid = ctx.market.book(side).best_bid?;
    let price = ctx.constraints.quantize_price(bid);
    let qty = ctx.config.lot_qty;
    let notional = price * qty;

    // 现金不够则不挂。
    if notional > ctx.free_cash {
        return None;
    }

    Some(CommandIntent::SubmitBuy {
        side,
        price,
        qty,
        role: OrderRole::Maker,
        tif: TimeInForce::Gtc,
    })
}

/// 哪边更便宜（bid 更低的先买）。两边一样则 UP 优先。
fn cheaper_side(ctx: &DecisionContext) -> (Side, Side) {
    let up_bid = ctx.market.book(Side::Up).best_bid;
    let dn_bid = ctx.market.book(Side::Down).best_bid;
    match (up_bid, dn_bid) {
        (Some(u), Some(d)) if d < u => (Side::Down, Side::Up),
        _ => (Side::Up, Side::Down),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StrategyConfig;
    use domain::market::{BookTop, MarketSnapshot};
    use domain::order::OrderConstraints;
    use inventory::Inventory;
    use rust_decimal_macros::dec;
    use std::sync::LazyLock;

    static DEFAULT_CFG: LazyLock<StrategyConfig> = LazyLock::new(StrategyConfig::default);

    fn basic_ctx<'a>(
        progress: rust_decimal::Decimal,
        inventory: &'a Inventory,
        active_orders: &'a [ActiveOrder],
        market: MarketSnapshot,
    ) -> DecisionContext<'a> {
        DecisionContext {
            trigger: crate::context::Trigger::BookUpdate,
            progress,
            market,
            inventory,
            active_orders,
            free_cash: dec!(1000),
            constraints: OrderConstraints::default(),
            config: &DEFAULT_CFG,
        }
    }

    fn market(
        up_bid: Option<rust_decimal::Decimal>,
        dn_bid: Option<rust_decimal::Decimal>,
    ) -> MarketSnapshot {
        MarketSnapshot {
            up: BookTop {
                best_bid: up_bid,
                best_ask: up_bid.map(|b| b + dec!(0.01)),
                last_trade: None,
            },
            down: BookTop {
                best_bid: dn_bid,
                best_ask: dn_bid.map(|b| b + dec!(0.01)),
                last_trade: None,
            },
        }
    }

    #[test]
    fn submits_buy_on_both_sides() {
        let inv = Inventory::new();
        let ctx = basic_ctx(
            dec!(0.02),
            &inv,
            &[],
            market(Some(dec!(0.55)), Some(dec!(0.44))),
        );
        let strategy = BuildingStrategy;
        let decision = strategy.decide(&ctx);

        // 两侧各一笔买单。
        assert_eq!(decision.commands.len(), 2);
        // 便宜侧（DN 0.44）先挂。
        match decision.commands[0] {
            CommandIntent::SubmitBuy { side, price, .. } => {
                assert_eq!(side, Side::Down);
                assert_eq!(price, dec!(0.44));
            }
            _ => panic!("应为 SubmitBuy"),
        }
    }

    #[test]
    fn skips_side_with_existing_buy() {
        let inv = Inventory::new();
        let existing = vec![ActiveOrder {
            order_id: domain::order::OrderId(1),
            side: Side::Up,
            direction: OrderDirection::Buy,
            price: dec!(0.50),
            qty: dec!(10),
            role: OrderRole::Maker,
            lot_id: None,
        }];
        let ctx = basic_ctx(
            dec!(0.02),
            &inv,
            &existing,
            market(Some(dec!(0.55)), Some(dec!(0.44))),
        );
        let strategy = BuildingStrategy;
        let decision = strategy.decide(&ctx);

        // UP 已有买单，只挂 DN。
        assert_eq!(decision.commands.len(), 1);
        match decision.commands[0] {
            CommandIntent::SubmitBuy { side, .. } => assert_eq!(side, Side::Down),
            _ => panic!("应为 SubmitBuy"),
        }
    }

    #[test]
    fn transitions_to_cycling_when_progress_reaches_threshold() {
        let inv = Inventory::new();
        let ctx = basic_ctx(
            dec!(0.08),
            &inv,
            &[],
            market(Some(dec!(0.50)), Some(dec!(0.50))),
        );
        let strategy = BuildingStrategy;
        let decision = strategy.decide(&ctx);

        assert_eq!(decision.transition, Some(Phase::Cycling));
        assert!(decision.commands.is_empty());
    }

    #[test]
    fn no_buy_when_insufficient_cash() {
        let inv = Inventory::new();
        let ctx = DecisionContext {
            trigger: crate::context::Trigger::BookUpdate,
            progress: dec!(0.02),
            market: market(Some(dec!(0.50)), Some(dec!(0.50))),
            inventory: &inv,
            active_orders: &[],
            free_cash: dec!(3), // 0.50 × 10 = 5 > 3
            constraints: OrderConstraints::default(),
            config: &DEFAULT_CFG,
        };
        let strategy = BuildingStrategy;
        let decision = strategy.decide(&ctx);
        assert!(decision.commands.is_empty());
    }
}
