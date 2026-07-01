//! 阶段 2：循环做市（核心阶段）。
//!
//! 三个并行决策流：
//! 1. 止盈扫描：遍历未平 Lot，对尚未挂卖单的 Lot，若 bid ≥ buy_price + tp → 挂 Maker 卖单。
//! 2. 止损扫描：遍历未平 Lot，若 buy_price − bid ≥ sl → 撤止盈卖单、Taker 止损卖出。
//! 3. 补买：两侧保持活跃 Maker 买单，便宜侧优先。

use crate::context::{ActiveOrder, CommandIntent, Decision, DecisionContext};
use domain::order::{OrderDirection, TimeInForce};
use domain::phase::Phase;
use domain::types::{OrderRole, Price, Side};
use inventory::lot::LotId;

/// 循环做市策略。
pub struct CyclingStrategy;

impl CyclingStrategy {
    pub fn decide(&self, ctx: &DecisionContext) -> Decision {
        // 到收手时间 → 跳转 Harvesting。
        if ctx.progress >= ctx.config.harvest_start {
            return Decision {
                commands: vec![],
                transition: Some(Phase::Harvesting),
            };
        }

        let mut commands = Vec::new();
        let tp = ctx.config.tp(ctx.progress);
        let sl = ctx.config.sl(ctx.progress);

        // ── 止盈扫描 ──
        for side in [Side::Up, Side::Down] {
            for lot in ctx.inventory.open_lots(side) {
                // 已有卖单绑定此 Lot → 跳过。
                if has_sell_for_lot(ctx.active_orders, lot.lot_id) {
                    continue;
                }

                let Some(bid) = ctx.market.book(side).best_bid else {
                    continue;
                };

                let target_price = lot.buy_price + tp;

                if bid >= target_price {
                    // bid 已穿越止盈线 → 挂 Maker 卖单在 target_price。
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

        // ── 止损扫描 ──
        for side in [Side::Up, Side::Down] {
            for lot in ctx.inventory.open_lots(side) {
                let Some(bid) = ctx.market.book(side).best_bid else {
                    continue;
                };

                let drawdown = lot.buy_price - bid;
                if drawdown >= sl {
                    // 浮亏超过止损线 → 撤该 Lot 的止盈卖单 + Taker 卖出止损。
                    if let Some(sell_order_id) = find_sell_for_lot(ctx.active_orders, lot.lot_id) {
                        commands.push(CommandIntent::Cancel(sell_order_id));
                    }
                    // 以 bid 作为限价下限，Taker 卖出（IOC）。
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

        // ── 补买 ──
        let (cheap, expensive) = cheaper_side(ctx);
        for &side in &[cheap, expensive] {
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

/// 某 Lot 是否已有对应的止盈卖单挂着。
fn has_sell_for_lot(orders: &[ActiveOrder], lot_id: LotId) -> bool {
    orders
        .iter()
        .any(|o| o.lot_id == Some(lot_id) && o.direction == OrderDirection::Sell)
}

/// 找到某 Lot 对应的止盈卖单的 order_id（止损时需撤它）。
fn find_sell_for_lot(orders: &[ActiveOrder], lot_id: LotId) -> Option<domain::order::OrderId> {
    orders
        .iter()
        .find(|o| o.lot_id == Some(lot_id) && o.direction == OrderDirection::Sell)
        .map(|o| o.order_id)
}

/// 某侧是否已有活跃买单。
fn has_active_buy(orders: &[ActiveOrder], side: Side) -> bool {
    orders
        .iter()
        .any(|o| o.side == side && o.direction == OrderDirection::Buy)
}

/// 构造 Maker 买单：bid 价挂 lot_qty。现金不够或无 bid 时返回 None。
fn make_buy_intent(ctx: &DecisionContext, side: Side) -> Option<CommandIntent> {
    let bid = ctx.market.book(side).best_bid?;
    let price = ctx.constraints.quantize_price(bid);
    let qty = ctx.config.lot_qty;
    let notional = price * qty;

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

/// 哪边便宜（bid 更低的先买）。
fn cheaper_side(ctx: &DecisionContext) -> (Side, Side) {
    let up_bid = ctx.market.book(Side::Up).best_bid;
    let dn_bid = ctx.market.book(Side::Down).best_bid;
    match (up_bid, dn_bid) {
        (Some(u), Some(d)) if d < u => (Side::Down, Side::Up),
        _ => (Side::Up, Side::Down),
    }
}

/// 取便宜侧 bid 价格（给止损定价用的内部辅助，不对外暴露）。
#[allow(dead_code)]
fn _cheaper_bid(ctx: &DecisionContext) -> Option<Price> {
    let (side, _) = cheaper_side(ctx);
    ctx.market.book(side).best_bid
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StrategyConfig;
    use crate::context::Trigger;
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

    fn ctx_with_inventory<'a>(
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
    fn submits_take_profit_sell_when_bid_exceeds_threshold() {
        let mut inv = Inventory::new();
        // 买入 UP@0.40。
        let lot_id = inv.open_lot(Side::Up, dec!(0.40), dec!(10), dec!(4.00), 100);

        // 当前 UP bid = 0.46 ≥ 0.40 + 0.05(Q1 tp) = 0.45 → 触发止盈。
        let market = make_market(dec!(0.46), dec!(0.54));
        let ctx = ctx_with_inventory(&inv, &[], market, dec!(0.10));
        let strategy = CyclingStrategy;
        let decision = strategy.decide(&ctx);

        let sell_cmds: Vec<_> = decision
            .commands
            .iter()
            .filter(|c| matches!(c, CommandIntent::SubmitSell { .. }))
            .collect();
        assert!(!sell_cmds.is_empty());
        match sell_cmds[0] {
            CommandIntent::SubmitSell {
                lot_id: id,
                side,
                price,
                qty,
                role,
                ..
            } => {
                assert_eq!(*id, lot_id);
                assert_eq!(*side, Side::Up);
                assert_eq!(*price, dec!(0.45)); // buy_price + tp = 0.40 + 0.05
                assert_eq!(*qty, dec!(10));
                assert_eq!(*role, OrderRole::Maker);
            }
            _ => panic!("应为 SubmitSell"),
        }
    }

    #[test]
    fn does_not_submit_sell_when_bid_below_threshold() {
        let mut inv = Inventory::new();
        inv.open_lot(Side::Up, dec!(0.40), dec!(10), dec!(4.00), 100);

        // UP bid = 0.44 < 0.40 + 0.05 = 0.45 → 不触发止盈。
        let market = make_market(dec!(0.44), dec!(0.54));
        let ctx = ctx_with_inventory(&inv, &[], market, dec!(0.10));
        let strategy = CyclingStrategy;
        let decision = strategy.decide(&ctx);

        let sell_cmds: Vec<_> = decision
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
        assert!(sell_cmds.is_empty());
    }

    #[test]
    fn does_not_duplicate_sell_for_same_lot() {
        let mut inv = Inventory::new();
        let lot_id = inv.open_lot(Side::Up, dec!(0.40), dec!(10), dec!(4.00), 100);

        // 已有该 Lot 的止盈卖单挂着。
        let existing = vec![ActiveOrder {
            order_id: OrderId(99),
            side: Side::Up,
            direction: OrderDirection::Sell,
            price: dec!(0.45),
            qty: dec!(10),
            role: OrderRole::Maker,
            lot_id: Some(lot_id),
        }];

        let market = make_market(dec!(0.46), dec!(0.54));
        let ctx = ctx_with_inventory(&inv, &existing, market, dec!(0.10));
        let strategy = CyclingStrategy;
        let decision = strategy.decide(&ctx);

        // 不重复挂。
        let sell_cmds: Vec<_> = decision
            .commands
            .iter()
            .filter(|c| matches!(c, CommandIntent::SubmitSell { .. }))
            .collect();
        assert!(sell_cmds.is_empty());
    }

    #[test]
    fn triggers_stop_loss_when_drawdown_exceeds_sl() {
        let mut inv = Inventory::new();
        let lot_id = inv.open_lot(Side::Up, dec!(0.50), dec!(10), dec!(5.00), 100);

        // UP bid = 0.45, drawdown = 0.50 − 0.45 = 0.05 ≥ sl(Q1) = 0.04 → 止损。
        let market = make_market(dec!(0.45), dec!(0.54));

        // 已有止盈卖单要撤。
        let existing = vec![ActiveOrder {
            order_id: OrderId(50),
            side: Side::Up,
            direction: OrderDirection::Sell,
            price: dec!(0.55),
            qty: dec!(10),
            role: OrderRole::Maker,
            lot_id: Some(lot_id),
        }];

        let ctx = ctx_with_inventory(&inv, &existing, market, dec!(0.10));
        let strategy = CyclingStrategy;
        let decision = strategy.decide(&ctx);

        // 应有：Cancel(50) + SubmitSell(Taker)。
        let cancel_cmds: Vec<_> = decision
            .commands
            .iter()
            .filter(|c| matches!(c, CommandIntent::Cancel(_)))
            .collect();
        assert_eq!(cancel_cmds.len(), 1);
        match cancel_cmds[0] {
            CommandIntent::Cancel(id) => assert_eq!(*id, OrderId(50)),
            _ => panic!("应为 Cancel"),
        }

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
        match taker_sells[0] {
            CommandIntent::SubmitSell {
                lot_id: id,
                price,
                role,
                tif,
                ..
            } => {
                assert_eq!(*id, lot_id);
                assert_eq!(*price, dec!(0.45));
                assert_eq!(*role, OrderRole::Taker);
                assert_eq!(*tif, TimeInForce::Ioc);
            }
            _ => panic!("应为 SubmitSell"),
        }
    }

    #[test]
    fn transitions_to_harvesting_at_harvest_start() {
        let inv = Inventory::new();
        let market = make_market(dec!(0.50), dec!(0.50));
        let ctx = ctx_with_inventory(&inv, &[], market, dec!(0.83));
        let strategy = CyclingStrategy;
        let decision = strategy.decide(&ctx);
        assert_eq!(decision.transition, Some(Phase::Harvesting));
    }

    #[test]
    fn submits_buy_when_no_active_buy() {
        let inv = Inventory::new();
        let market = make_market(dec!(0.55), dec!(0.44));
        let ctx = ctx_with_inventory(&inv, &[], market, dec!(0.20));
        let strategy = CyclingStrategy;
        let decision = strategy.decide(&ctx);

        let buy_cmds: Vec<_> = decision
            .commands
            .iter()
            .filter(|c| matches!(c, CommandIntent::SubmitBuy { .. }))
            .collect();
        // 两侧各一笔。
        assert_eq!(buy_cmds.len(), 2);
    }
}
