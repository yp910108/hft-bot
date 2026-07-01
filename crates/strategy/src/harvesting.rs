//! 阶段 3：收手变现。停新买单，继续止盈卖单收割，到时间完全停手。
//!
//! 逻辑：
//! - 不再挂新买单（停止补仓）。
//! - 继续对已有 Lot 挂止盈卖单（逻辑同 Cycling 的止盈扫描）。
//! - 止损也继续生效（不扛亏损到结算）。
//! - progress ≥ settle_start → 跳转 Settled（终态，什么也不做）。

use crate::context::{ActiveOrder, CommandIntent, Decision, DecisionContext};
use domain::order::{OrderDirection, TimeInForce};
use domain::phase::Phase;
use domain::types::{OrderRole, Side};
use inventory::lot::LotId;

/// 收手变现策略。
pub struct HarvestingStrategy;

impl HarvestingStrategy {
    pub fn decide(&self, ctx: &DecisionContext) -> Decision {
        // 到停手时间 → 跳转 Settled。
        if ctx.progress >= ctx.config.settle_start {
            return Decision {
                commands: vec![],
                transition: Some(Phase::Settled),
            };
        }

        let mut commands = Vec::new();
        let tp = ctx.config.tp(ctx.progress);
        let sl = ctx.config.sl(ctx.progress);

        // ── 止盈扫描（同 Cycling） ──
        for side in [Side::Up, Side::Down] {
            for lot in ctx.inventory.open_lots(side) {
                if has_sell_for_lot(ctx.active_orders, lot.lot_id) {
                    continue;
                }

                let Some(bid) = ctx.market.book(side).best_bid else {
                    continue;
                };

                let target_price = lot.buy_price + tp;
                if bid >= target_price {
                    let price = ctx.constraints.quantize_price_up(target_price);
                    commands.push(CommandIntent::SubmitSell {
                        lot_id: lot.lot_id,
                        side,
                        price,
                        qty: lot.qty,
                        role: OrderRole::Maker,
                        tif: TimeInForce::Gtc,
                    });
                }
            }
        }

        // ── 止损扫描（同 Cycling） ──
        for side in [Side::Up, Side::Down] {
            for lot in ctx.inventory.open_lots(side) {
                let Some(bid) = ctx.market.book(side).best_bid else {
                    continue;
                };

                let drawdown = lot.buy_price - bid;
                if drawdown >= sl {
                    if let Some(sell_order_id) = find_sell_for_lot(ctx.active_orders, lot.lot_id) {
                        commands.push(CommandIntent::Cancel(sell_order_id));
                    }
                    let price = ctx.constraints.quantize_price(bid);
                    commands.push(CommandIntent::SubmitSell {
                        lot_id: lot.lot_id,
                        side,
                        price,
                        qty: lot.qty,
                        role: OrderRole::Taker,
                        tif: TimeInForce::Ioc,
                    });
                }
            }
        }

        // ── 不补买（收手阶段停止新开买单）──

        Decision {
            commands,
            transition: None,
        }
    }
}

fn has_sell_for_lot(orders: &[ActiveOrder], lot_id: LotId) -> bool {
    orders
        .iter()
        .any(|o| o.lot_id == Some(lot_id) && o.direction == OrderDirection::Sell)
}

fn find_sell_for_lot(orders: &[ActiveOrder], lot_id: LotId) -> Option<domain::order::OrderId> {
    orders
        .iter()
        .find(|o| o.lot_id == Some(lot_id) && o.direction == OrderDirection::Sell)
        .map(|o| o.order_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StrategyConfig;
    use crate::context::{ActiveOrder, Trigger};
    use domain::market::{BookTop, MarketSnapshot};
    use domain::order::{OrderConstraints, OrderId};
    use inventory::Inventory;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use std::sync::LazyLock;

    static DEFAULT_CFG: LazyLock<StrategyConfig> = LazyLock::new(StrategyConfig::default);

    fn make_market(up_bid: Decimal, dn_bid: Decimal) -> MarketSnapshot {
        MarketSnapshot {
            up: BookTop {
                best_bid: Some(up_bid),
                best_ask: Some(up_bid + dec!(0.01)),
                last_trade: None,
            },
            down: BookTop {
                best_bid: Some(dn_bid),
                best_ask: Some(dn_bid + dec!(0.01)),
                last_trade: None,
            },
        }
    }

    fn ctx<'a>(
        inventory: &'a Inventory,
        active_orders: &'a [ActiveOrder],
        market: MarketSnapshot,
        progress: Decimal,
    ) -> DecisionContext<'a> {
        DecisionContext {
            trigger: Trigger::BookUpdate,
            progress,
            market,
            inventory,
            active_orders,
            free_cash: dec!(1000),
            constraints: OrderConstraints::default(),
            config: &DEFAULT_CFG,
        }
    }

    #[test]
    fn no_buy_orders_in_harvesting() {
        let inv = Inventory::new();
        let market = make_market(dec!(0.50), dec!(0.50));
        let c = ctx(&inv, &[], market, dec!(0.85));
        let strategy = HarvestingStrategy;
        let decision = strategy.decide(&c);

        let buys: Vec<_> = decision
            .commands
            .iter()
            .filter(|c| matches!(c, CommandIntent::SubmitBuy { .. }))
            .collect();
        assert!(buys.is_empty(), "收手阶段不应再买入");
    }

    #[test]
    fn still_submits_take_profit_sells() {
        let mut inv = Inventory::new();
        let _lot_id = inv.open_lot(Side::Up, dec!(0.40), dec!(10), dec!(4.00), 100);

        // bid 0.51 ≥ 0.40 + 0.10(Q4 tp, progress=0.85) = 0.50 → 触发止盈。
        let market = make_market(dec!(0.51), dec!(0.50));
        let c = ctx(&inv, &[], market, dec!(0.85));
        let strategy = HarvestingStrategy;
        let decision = strategy.decide(&c);

        let sells: Vec<_> = decision
            .commands
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    CommandIntent::SubmitSell {
                        role: OrderRole::Maker,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(sells.len(), 1);
    }

    #[test]
    fn transitions_to_settled_at_settle_start() {
        let inv = Inventory::new();
        let market = make_market(dec!(0.50), dec!(0.50));
        let c = ctx(&inv, &[], market, dec!(0.967));
        let strategy = HarvestingStrategy;
        let decision = strategy.decide(&c);

        assert_eq!(decision.transition, Some(Phase::Settled));
        assert!(decision.commands.is_empty());
    }

    #[test]
    fn stop_loss_still_works_in_harvesting() {
        let mut inv = Inventory::new();
        let lot_id = inv.open_lot(Side::Down, dec!(0.60), dec!(10), dec!(6.00), 200);

        // DN bid = 0.52, drawdown = 0.60 − 0.52 = 0.08 ≥ sl(Q4)=0.07 → 止损。
        let market = make_market(dec!(0.48), dec!(0.52));
        let existing = vec![ActiveOrder {
            order_id: OrderId(30),
            side: Side::Down,
            direction: OrderDirection::Sell,
            price: dec!(0.70),
            qty: dec!(10),
            role: OrderRole::Maker,
            lot_id: Some(lot_id),
        }];
        let c = ctx(&inv, &existing, market, dec!(0.90));
        let strategy = HarvestingStrategy;
        let decision = strategy.decide(&c);

        // 撤止盈 + Taker 止损卖出。
        let cancels: Vec<_> = decision
            .commands
            .iter()
            .filter(|c| matches!(c, CommandIntent::Cancel(_)))
            .collect();
        assert_eq!(cancels.len(), 1);

        let taker_sells: Vec<_> = decision
            .commands
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    CommandIntent::SubmitSell {
                        role: OrderRole::Taker,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(taker_sells.len(), 1);
    }
}
