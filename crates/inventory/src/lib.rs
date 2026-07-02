//! 逐笔持仓账本：每笔买入独立记录为 Lot，支持逐笔止盈/止损/聚合查询。
//!
//! 这是新策略的核心数据结构，替代旧 `ledger` 的聚合 `{qty, cost}` 模型。
//! 策略遍历 `open_lots(side)` 对每笔独立判断止盈止损条件，
//! Engine 在卖单成交后调 `close_lot` 精确平掉对应那笔。
//!
//! 账务恒等式：
//! - net_invested = cash_out − cash_in
//! - 全场 PnL = realized_pnl + settle_pnl(净持仓, winner)
//!   = winner 侧净持仓 − net_invested

pub mod lot;

use domain::clock::Millis;
use domain::pnl::PositionSnapshot;
use domain::types::{Money, Price, Qty, Side};
use lot::{Lot, LotId};
use rust_decimal::Decimal;

/// LotId 生成器：单调递增。
#[derive(Debug, Clone, Default)]
struct LotIdGen {
    next: u64,
}

impl LotIdGen {
    fn next(&mut self) -> LotId {
        let id = LotId(self.next);
        self.next += 1;
        id
    }
}

/// 单侧逐笔持仓。
#[derive(Debug, Clone, Default)]
struct SideInventory {
    /// 按买入时间顺序排列。LIFO 止盈从尾部开始扫描（最近买的先卖）。
    lots: Vec<Lot>,
}

impl SideInventory {
    /// 新增一笔买入持仓。
    fn open(&mut self, lot: Lot) {
        self.lots.push(lot);
    }

    /// 按 lot_id 找到并全额平仓，返回被平的 Lot。找不到返回 None。
    fn close(&mut self, lot_id: LotId) -> Option<Lot> {
        let pos = self.lots.iter().position(|l| l.lot_id == lot_id)?;
        Some(self.lots.remove(pos))
    }

    /// 某侧净持仓 = Σ lots.qty。
    fn net_qty(&self) -> Qty {
        self.lots.iter().map(|l| l.qty).sum()
    }

    /// 某侧净成本 = Σ (buy_price × qty)。
    fn net_cost(&self) -> Money {
        self.lots.iter().map(|l| l.cost()).sum()
    }

    /// 某侧净均价。无持仓返回 None。
    fn net_avg(&self) -> Option<Price> {
        let qty = self.net_qty();
        if qty > Decimal::ZERO {
            Some(self.net_cost() / qty)
        } else {
            None
        }
    }

    /// 遍历未平 Lot，最近买入的在前（LIFO：止盈优先卖刚买的便宜货）。
    fn iter(&self) -> impl Iterator<Item = &Lot> {
        self.lots.iter().rev()
    }

    /// 未平 Lot 数量。
    fn len(&self) -> usize {
        self.lots.len()
    }
}

/// 双边逐笔账本。
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    up: SideInventory,
    down: SideInventory,
    id_gen: LotIdGen,

    /// 累计已实现盈亏（循环卖出的差价利润）。
    realized_pnl: Money,
    /// 累计买入付出现金。
    cash_out: Money,
    /// 累计卖出回收现金。
    cash_in: Money,
}

impl Inventory {
    pub fn new() -> Self {
        Self::default()
    }

    // ─── 写操作 ───

    /// 买入成交 → 开一笔新 Lot。返回分配的 LotId。
    ///
    /// - `buy_price`：有效成本/股 = 实付现金 ÷ 净入仓股数。
    /// - `qty`：净入仓股数（已扣 Taker 费）。
    /// - `cash`：实付现金（= 名义股数 × 成交价）。
    /// - `at`：买入时刻。
    pub fn open_lot(
        &mut self,
        side: Side,
        buy_price: Price,
        qty: Qty,
        cash: Money,
        at: Millis,
    ) -> LotId {
        let lot_id = self.id_gen.next();
        let lot = Lot {
            lot_id,
            buy_price,
            qty,
            opened_at: at,
        };
        self.side_mut(side).open(lot);
        self.cash_out += cash;
        lot_id
    }

    /// 卖出成交 → 全额平掉指定 Lot，记录已实现盈亏。
    ///
    /// - `sell_price`：卖出成交价（每股回收金额）。
    /// - `cash_in_amount`：实际回收现金（卖 Maker 时 = sell_price×qty；卖 Taker 时扣费后金额）。
    ///
    /// 返回该笔已实现盈亏。找不到 Lot 返回 None。
    pub fn close_lot(
        &mut self,
        side: Side,
        lot_id: LotId,
        cash_in_amount: Money,
    ) -> Option<Money> {
        let lot = self.side_mut(side).close(lot_id)?;
        let pnl = cash_in_amount - lot.cost();
        self.realized_pnl += pnl;
        self.cash_in += cash_in_amount;
        Some(pnl)
    }

    // ─── 读操作 ───

    /// 某侧净持仓股数。
    pub fn net_qty(&self, side: Side) -> Qty {
        self.side(side).net_qty()
    }

    /// 某侧净持仓成本。
    pub fn net_cost(&self, side: Side) -> Money {
        self.side(side).net_cost()
    }

    /// 某侧净持仓均价。无持仓返回 None。
    pub fn net_avg(&self, side: Side) -> Option<Price> {
        self.side(side).net_avg()
    }

    /// sum_avg = UP 净均价 + DN 净均价。< 1 时双赢结构。
    /// 任一侧无持仓时该侧按 0 计。
    pub fn sum_avg(&self) -> Price {
        let up = self.net_avg(Side::Up).unwrap_or(Decimal::ZERO);
        let dn = self.net_avg(Side::Down).unwrap_or(Decimal::ZERO);
        up + dn
    }

    /// 遍历某侧未平 Lot 的只读引用（策略挂止盈卖单时用）。
    pub fn open_lots(&self, side: Side) -> impl Iterator<Item = &Lot> {
        self.side(side).iter()
    }

    /// 某侧未平 Lot 数量。
    pub fn lot_count(&self, side: Side) -> usize {
        self.side(side).len()
    }

    /// 累计已实现盈亏。
    pub fn realized_pnl(&self) -> Money {
        self.realized_pnl
    }

    /// 净投入 = 累计买入现金 − 累计卖出回收现金。
    /// 供现金哨兵和结算用。
    pub fn net_invested(&self) -> Money {
        self.cash_out - self.cash_in
    }

    /// 产出聚合快照（供结算和报告用）。
    pub fn snapshot(&self) -> PositionSnapshot {
        PositionSnapshot {
            up_qty: self.net_qty(Side::Up),
            down_qty: self.net_qty(Side::Down),
            up_cost: self.net_cost(Side::Up),
            down_cost: self.net_cost(Side::Down),
        }
    }

    // ─── 内部辅助 ───

    fn side(&self, side: Side) -> &SideInventory {
        match side {
            Side::Up => &self.up,
            Side::Down => &self.down,
        }
    }

    fn side_mut(&mut self, side: Side) -> &mut SideInventory {
        match side {
            Side::Up => &mut self.up,
            Side::Down => &mut self.down,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn new_inventory_is_empty() {
        let inv = Inventory::new();
        assert_eq!(inv.net_qty(Side::Up), Decimal::ZERO);
        assert_eq!(inv.net_qty(Side::Down), Decimal::ZERO);
        assert_eq!(inv.net_cost(Side::Up), Decimal::ZERO);
        assert_eq!(inv.realized_pnl(), Decimal::ZERO);
        assert_eq!(inv.net_invested(), Decimal::ZERO);
    }

    #[test]
    fn open_lot_creates_lot_and_tracks_cash() {
        let mut inv = Inventory::new();
        // 买入 UP 10 股 @ 有效成本 0.45/股，付出现金 $4.50。
        let id = inv.open_lot(Side::Up, dec!(0.45), dec!(10), dec!(4.50), 1000);
        assert_eq!(id, LotId(0));
        assert_eq!(inv.net_qty(Side::Up), dec!(10));
        assert_eq!(inv.net_cost(Side::Up), dec!(4.50));
        assert_eq!(inv.net_avg(Side::Up), Some(dec!(0.45)));
        assert_eq!(inv.net_invested(), dec!(4.50));
    }

    #[test]
    fn close_lot_removes_and_records_pnl() {
        let mut inv = Inventory::new();
        let id = inv.open_lot(Side::Up, dec!(0.45), dec!(10), dec!(4.50), 1000);
        // 卖出回收 $5.00（Maker 零费 @ 0.50 × 10 股）。
        let pnl = inv.close_lot(Side::Up, id, dec!(5.00));
        // 盈亏 = 回收 5.00 − 成本 4.50 = +0.50。
        assert_eq!(pnl, Some(dec!(0.50)));
        assert_eq!(inv.realized_pnl(), dec!(0.50));
        assert_eq!(inv.net_qty(Side::Up), Decimal::ZERO);
        assert_eq!(inv.net_invested(), dec!(-0.50)); // 回收多于投入
    }

    #[test]
    fn close_lot_returns_none_for_unknown_id() {
        let mut inv = Inventory::new();
        assert_eq!(inv.close_lot(Side::Up, LotId(99), dec!(5.00)), None);
    }

    #[test]
    fn multiple_lots_and_sum_avg() {
        let mut inv = Inventory::new();
        // UP: 买 10 股 @0.45，再买 10 股 @0.40 → 均价 = (4.5+4.0)/20 = 0.425。
        inv.open_lot(Side::Up, dec!(0.45), dec!(10), dec!(4.50), 100);
        inv.open_lot(Side::Up, dec!(0.40), dec!(10), dec!(4.00), 200);
        // DN: 买 10 股 @0.50。
        inv.open_lot(Side::Down, dec!(0.50), dec!(10), dec!(5.00), 150);

        assert_eq!(inv.net_qty(Side::Up), dec!(20));
        assert_eq!(inv.net_avg(Side::Up), Some(dec!(0.425)));
        assert_eq!(inv.net_avg(Side::Down), Some(dec!(0.50)));
        // sum_avg = 0.425 + 0.50 = 0.925 < 1 → 双赢结构。
        assert_eq!(inv.sum_avg(), dec!(0.925));
    }

    #[test]
    fn open_lots_iterates_most_recent_first() {
        let mut inv = Inventory::new();
        let id0 = inv.open_lot(Side::Up, dec!(0.45), dec!(10), dec!(4.50), 100);
        let id1 = inv.open_lot(Side::Up, dec!(0.40), dec!(10), dec!(4.00), 200);
        // LIFO：最近买入的 id1 在前。
        let ids: Vec<LotId> = inv.open_lots(Side::Up).map(|l| l.lot_id).collect();
        assert_eq!(ids, vec![id1, id0]);
    }

    #[test]
    fn snapshot_matches_aggregates() {
        let mut inv = Inventory::new();
        inv.open_lot(Side::Up, dec!(0.45), dec!(10), dec!(4.50), 100);
        inv.open_lot(Side::Down, dec!(0.55), dec!(8), dec!(4.40), 200);
        let snap = inv.snapshot();
        assert_eq!(snap.up_qty, dec!(10));
        assert_eq!(snap.down_qty, dec!(8));
        assert_eq!(snap.up_cost, dec!(4.50));
        assert_eq!(snap.down_cost, dec!(4.40));
    }

    #[test]
    fn realized_pnl_accumulates_across_multiple_closes() {
        let mut inv = Inventory::new();
        let id0 = inv.open_lot(Side::Up, dec!(0.40), dec!(10), dec!(4.00), 100);
        let id1 = inv.open_lot(Side::Up, dec!(0.42), dec!(10), dec!(4.20), 200);
        // 平 id0: 回收 $4.50 → pnl +0.50
        inv.close_lot(Side::Up, id0, dec!(4.50));
        // 平 id1: 回收 $4.80 → pnl +0.60
        inv.close_lot(Side::Up, id1, dec!(4.80));
        assert_eq!(inv.realized_pnl(), dec!(1.10)); // 0.50 + 0.60
    }

    #[test]
    fn net_invested_is_cash_out_minus_cash_in() {
        let mut inv = Inventory::new();
        inv.open_lot(Side::Up, dec!(0.45), dec!(10), dec!(4.50), 100);
        inv.open_lot(Side::Down, dec!(0.55), dec!(10), dec!(5.50), 200);
        // 总买入: 4.50 + 5.50 = 10.00
        assert_eq!(inv.net_invested(), dec!(10.00));
        // 卖出一笔 UP: 回收 5.00
        let id = LotId(0);
        inv.close_lot(Side::Up, id, dec!(5.00));
        assert_eq!(inv.net_invested(), dec!(5.00)); // 10 − 5
    }
}
