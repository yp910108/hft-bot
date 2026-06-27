//! 内存账本：记录 Up/Down 两侧各持有多少股、总共花了多少钱。
//!
//! 金额用 Decimal 精确表示，均价由股数和成本算出来，不做舍入。
//! 账本只管"现在持有什么"，不记已实现盈亏——卖出时按加权均价扣成本就完事。

use domain::order::{Fill, OrderDirection};
use domain::pnl::PositionSnapshot;
use domain::types::{Money, Price, Qty, Side};
use rust_decimal::Decimal;

/// 单侧账本：某一侧现在有多少股、总共花了多少钱。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct SideBook {
    /// 持有股数。
    qty: Qty,
    /// 买入这些股总共花的钱（含手续费）。
    cost: Money,
}

impl SideBook {
    /// 加权均价 = 总成本 / 股数。没持仓时返回 0。
    ///
    /// 保留精确值不舍入，因为盈亏计算都用原始 cost 和 qty，均价只是展示用的派生值。
    fn average_price(&self) -> Price {
        if self.qty.is_zero() {
            Decimal::ZERO
        } else {
            self.cost / self.qty
        }
    }

    /// 买入：加股数、加成本。
    ///
    /// 手续费已经在撮合层通过扣减股数来体现了，所以这里的 filled_qty 就是净到手股数，
    /// cash 就是实付现金，直接累加即可。
    fn apply_buy(&mut self, filled_qty: Qty, cash: Money) {
        self.qty += filled_qty;
        self.cost += cash;
    }

    /// 卖出：按加权均价扣减成本，剩余持仓的均价不变。
    ///
    /// 卖出手续费属于已实现盈亏，由资金层处理，不计入留存持仓成本——
    /// 否则会虚抬剩余持仓的均价。卖出股数超过持仓量时截断到持仓量，防止负持仓。
    fn apply_sell(&mut self, qty: Qty) {
        let sell_qty = qty.min(self.qty);
        if sell_qty.is_zero() {
            return;
        }
        // sell_qty 非零说明 self.qty 非零，可以安全算均价。
        let avg_before = self.average_price();
        self.qty -= sell_qty;
        // 只扣掉卖出部分对应的成本，剩余持仓均价自然不变。
        self.cost -= avg_before * sell_qty;
        // 全卖完了：把可能因除法产生的零头残留清零，
        // 免得下次重新建仓时均价被污染。
        if self.qty.is_zero() {
            self.cost = Decimal::ZERO;
        }
    }
}

/// 双边账本：汇总 Up/Down 两侧持仓，提供快照给盈亏计算用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ledger {
    up: SideBook,
    down: SideBook,
}

impl Ledger {
    /// 创建空账本。
    pub fn new() -> Self {
        Self::default()
    }

    /// 把一笔成交写入账本。
    pub fn apply_fill(&mut self, fill: &Fill) {
        let book = match fill.side {
            Side::Up => &mut self.up,
            Side::Down => &mut self.down,
        };
        match fill.direction {
            OrderDirection::Buy => book.apply_buy(fill.filled_qty, fill.cash),
            OrderDirection::Sell => book.apply_sell(fill.filled_qty),
        }
    }

    /// 查某一侧现在持有多少股。
    pub fn qty(&self, side: Side) -> Qty {
        match side {
            Side::Up => self.up.qty,
            Side::Down => self.down.qty,
        }
    }

    /// 查某一侧的加权均价，没持仓时为 0。
    pub fn average_price(&self, side: Side) -> Price {
        match side {
            Side::Up => self.up.average_price(),
            Side::Down => self.down.average_price(),
        }
    }

    /// 查某一侧的累计投入成本（含费），没持仓时为 0。
    pub fn cost(&self, side: Side) -> Money {
        match side {
            Side::Up => self.up.cost,
            Side::Down => self.down.cost,
        }
    }

    /// 双边累计净总成本，作为结算盈亏计算的总成本口径。
    pub fn total_cost(&self) -> Money {
        self.up.cost + self.down.cost
    }

    /// 产出当前持仓快照，供 domain 层计算盈亏与数学期望。
    pub fn snapshot(&self) -> PositionSnapshot {
        PositionSnapshot {
            up_qty: self.up.qty,
            down_qty: self.down.qty,
            up_cost: self.up.cost,
            down_cost: self.down.cost,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::order::{Generation, OrderId};
    use domain::types::OrderRole;
    use rust_decimal_macros::dec;

    /// 构造一笔成交回报的测试辅助函数。
    ///
    /// `filled_qty` 为扣费后的净入仓股数，`cash` 为实付现金。
    fn fill(
        side: Side,
        direction: OrderDirection,
        price: Price,
        filled_qty: Qty,
        cash: Money,
    ) -> Fill {
        Fill {
            order_id: OrderId(0),
            side,
            direction,
            role: OrderRole::Maker,
            price,
            filled_qty,
            cash,
            generation: Generation::new(),
        }
    }

    #[test]
    fn new_ledger_is_empty() {
        let ledger = Ledger::new();
        assert_eq!(ledger.qty(Side::Up), Decimal::ZERO);
        assert_eq!(ledger.qty(Side::Down), Decimal::ZERO);
        assert_eq!(ledger.average_price(Side::Up), Decimal::ZERO);
        assert_eq!(ledger.total_cost(), Decimal::ZERO);
    }

    #[test]
    fn single_buy_sets_qty_and_average_price() {
        let mut ledger = Ledger::new();
        // 零费买入：净入仓 100 股、实付现金 40 → 均价 0.4，成本 40。
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Buy,
            dec!(0.4),
            dec!(100),
            dec!(40),
        ));
        assert_eq!(ledger.qty(Side::Up), dec!(100));
        assert_eq!(ledger.average_price(Side::Up), dec!(0.4000));
        assert_eq!(ledger.total_cost(), dec!(40));
    }

    #[test]
    fn taker_fee_shows_as_reduced_qty_not_extra_cost() {
        let mut ledger = Ledger::new();
        // Taker 下单 100 股、价 0.5、费率 4%：实付现金 50（100×0.5），
        // 但净入仓仅 96 股（手续费体现为股数扣减）。
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Buy,
            dec!(0.5),
            dec!(96),
            dec!(50),
        ));
        assert_eq!(ledger.qty(Side::Up), dec!(96));
        assert_eq!(ledger.total_cost(), dec!(50));
        // 均价 = 50/96 = 0.5208333...，账本保留精确值而非舍入到 4 位。
        assert_eq!(ledger.average_price(Side::Up), dec!(50) / dec!(96));
    }

    #[test]
    fn multiple_buys_compute_weighted_average() {
        let mut ledger = Ledger::new();
        // 第一笔：净 100 股、现金 40。
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Buy,
            dec!(0.4),
            dec!(100),
            dec!(40),
        ));
        // 第二笔：净 200 股、现金 60。累计成本 100，股数 300，均价 = 100/300 = 0.3333...。
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Buy,
            dec!(0.3),
            dec!(200),
            dec!(60),
        ));
        assert_eq!(ledger.qty(Side::Up), dec!(300));
        assert_eq!(ledger.average_price(Side::Up), dec!(100) / dec!(300));
        assert_eq!(ledger.total_cost(), dec!(100));
    }

    #[test]
    fn average_price_keeps_full_precision_without_rounding() {
        let mut ledger = Ledger::new();
        // 净 3 股、现金 10 → 均价 = 10/3 = 3.3333...（无限循环小数）。
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Buy,
            dec!(3.3333),
            dec!(3),
            dec!(10),
        ));
        // 账本保留精确除法结果，不卡死在 4 位小数。
        assert_eq!(ledger.average_price(Side::Up), dec!(10) / dec!(3));
        // 精确值小数位数远多于 4 位，借此确认未发生 4 位舍入。
        assert!(ledger.average_price(Side::Up).scale() > 4);
    }

    #[test]
    fn both_sides_accumulate_independently() {
        let mut ledger = Ledger::new();
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Buy,
            dec!(0.4),
            dec!(100),
            dec!(40),
        ));
        ledger.apply_fill(&fill(
            Side::Down,
            OrderDirection::Buy,
            dec!(0.55),
            dec!(100),
            dec!(55),
        ));
        assert_eq!(ledger.average_price(Side::Up), dec!(0.4000));
        assert_eq!(ledger.average_price(Side::Down), dec!(0.5500));
        // 总成本 = 40 + 55 = 95。
        assert_eq!(ledger.total_cost(), dec!(95));
    }

    #[test]
    fn sell_reduces_qty_keeping_average_price() {
        let mut ledger = Ledger::new();
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Buy,
            dec!(0.4),
            dec!(100),
            dec!(40),
        ));
        // 卖出 40 股，均价应保持 0.4，剩余 60 股，成本 = 60×0.4 = 24。
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Sell,
            dec!(0.5),
            dec!(40),
            Decimal::ZERO,
        ));
        assert_eq!(ledger.qty(Side::Up), dec!(60));
        assert_eq!(ledger.average_price(Side::Up), dec!(0.4));
        assert_eq!(ledger.total_cost(), dec!(24));
    }

    #[test]
    fn selling_entire_position_resets_book() {
        let mut ledger = Ledger::new();
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Buy,
            dec!(0.4),
            dec!(100),
            dec!(40),
        ));
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Sell,
            dec!(0.5),
            dec!(100),
            Decimal::ZERO,
        ));
        assert_eq!(ledger.qty(Side::Up), Decimal::ZERO);
        assert_eq!(ledger.average_price(Side::Up), Decimal::ZERO);
        assert_eq!(ledger.total_cost(), Decimal::ZERO);
    }

    #[test]
    fn oversell_is_clamped_to_held_qty() {
        let mut ledger = Ledger::new();
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Buy,
            dec!(0.4),
            dec!(100),
            dec!(40),
        ));
        // 试图卖出 150 股，仅持有 100 股 → 最多卖出 100，持仓清零不为负。
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Sell,
            dec!(0.5),
            dec!(150),
            Decimal::ZERO,
        ));
        assert_eq!(ledger.qty(Side::Up), Decimal::ZERO);
    }

    #[test]
    fn snapshot_feeds_pnl_calculation() {
        let mut ledger = Ledger::new();
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Buy,
            dec!(0.4),
            dec!(100),
            dec!(40),
        ));
        ledger.apply_fill(&fill(
            Side::Down,
            OrderDirection::Buy,
            dec!(0.45),
            dec!(100),
            dec!(45),
        ));
        let snapshot = ledger.snapshot();
        // up_qty=100, down_qty=100, total_cost=85。
        // 两边股数 100 均大于成本 85 → 已锁定双向利润。
        assert_eq!(snapshot.up_qty, dec!(100));
        assert_eq!(snapshot.down_qty, dec!(100));
        assert_eq!(snapshot.total_cost(), dec!(85));
        assert_eq!(snapshot.up_cost, dec!(40));
        assert_eq!(snapshot.down_cost, dec!(45));
        // 两侧结算 pnl 均为正 → 锁定利润。
        assert!(snapshot.settle_pnl(Side::Up) > Money::ZERO);
        assert!(snapshot.settle_pnl(Side::Down) > Money::ZERO);
    }

    #[test]
    fn cost_accessor_returns_per_side_cost() {
        let mut ledger = Ledger::new();
        ledger.apply_fill(&fill(
            Side::Up,
            OrderDirection::Buy,
            dec!(0.4),
            dec!(100),
            dec!(40),
        ));
        assert_eq!(ledger.cost(Side::Up), dec!(40));
        assert_eq!(ledger.cost(Side::Down), Money::ZERO);
    }
}
