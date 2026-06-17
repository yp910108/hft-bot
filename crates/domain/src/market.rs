//! 市场行情类型：单侧盘口顶部、双边市场快照与 Mark Price 计算。
//!
//! 对应策略风险修复项 #8：策略多处使用 Mark Price 却从未定义其口径。
//! 本模块明确将 Mark Price 定义为盘口中间价 `(best_bid + best_ask) / 2`；
//! 当某一侧盘口缺失（只有买价或只有卖价）时，回退到最近一笔成交价。

use crate::types::{Price, Side};
use rust_decimal::Decimal;

/// 单侧资产的盘口顶部快照。
///
/// 三个字段均为 `Option`，因为开盘瞬间或流动性枯竭时盘口可能缺失。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BookTop {
    /// 买一价（最高买价）。
    pub best_bid: Option<Price>,
    /// 卖一价（最低卖价）。
    pub best_ask: Option<Price>,
    /// 最近一笔成交价，用于盘口单边缺失时的 Mark Price 回退。
    pub last_trade: Option<Price>,
}

impl BookTop {
    /// 计算该侧的 Mark Price。
    ///
    /// 优先取中间价 `(best_bid + best_ask) / 2`，返回**精确值**不做舍入
    /// （Mark Price 为派生值，仅用于与阈值比较，精确值更准确）；
    /// 若买价或卖价任一缺失，则回退到最近成交价；两者皆无时返回 `None`。
    pub fn mark_price(&self) -> Option<Price> {
        match (self.best_bid, self.best_ask) {
            (Some(bid), Some(ask)) => Some((bid + ask) / Decimal::TWO),
            _ => self.last_trade,
        }
    }
}

/// 双边市场快照：同时持有 Up 与 Down 两侧的盘口顶部。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MarketSnapshot {
    /// Up 侧盘口。
    pub up: BookTop,
    /// Down 侧盘口。
    pub down: BookTop,
}

impl MarketSnapshot {
    /// 返回指定侧的盘口顶部。
    pub fn book(&self, side: Side) -> &BookTop {
        match side {
            Side::Up => &self.up,
            Side::Down => &self.down,
        }
    }

    /// 返回指定侧的 Mark Price。
    pub fn mark_price(&self, side: Side) -> Option<Price> {
        self.book(side).mark_price()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn mark_price_is_mid_when_both_quotes_present() {
        let top = BookTop {
            best_bid: Some(dec!(0.40)),
            best_ask: Some(dec!(0.44)),
            last_trade: Some(dec!(0.99)),
        };
        // 中间价 = (0.40 + 0.44) / 2 = 0.42，优先于最近成交价。
        assert_eq!(top.mark_price(), Some(dec!(0.4200)));
    }

    #[test]
    fn mark_price_falls_back_to_last_trade_when_bid_missing() {
        let top = BookTop {
            best_bid: None,
            best_ask: Some(dec!(0.44)),
            last_trade: Some(dec!(0.41)),
        };
        assert_eq!(top.mark_price(), Some(dec!(0.41)));
    }

    #[test]
    fn mark_price_falls_back_to_last_trade_when_ask_missing() {
        let top = BookTop {
            best_bid: Some(dec!(0.40)),
            best_ask: None,
            last_trade: Some(dec!(0.41)),
        };
        assert_eq!(top.mark_price(), Some(dec!(0.41)));
    }

    #[test]
    fn mark_price_is_none_when_no_data() {
        let top = BookTop::default();
        assert_eq!(top.mark_price(), None);
    }

    #[test]
    fn mark_price_keeps_full_precision_without_rounding() {
        let top = BookTop {
            best_bid: Some(dec!(0.4001)),
            best_ask: Some(dec!(0.4004)),
            last_trade: None,
        };
        // (0.4001 + 0.4004) / 2 = 0.40025，保留精确值，不舍入到 4 位小数。
        assert_eq!(top.mark_price(), Some(dec!(0.40025)));
    }

    #[test]
    fn snapshot_routes_to_correct_side() {
        let snapshot = MarketSnapshot {
            up: BookTop {
                best_bid: Some(dec!(0.40)),
                best_ask: Some(dec!(0.42)),
                last_trade: None,
            },
            down: BookTop {
                best_bid: Some(dec!(0.56)),
                best_ask: Some(dec!(0.60)),
                last_trade: None,
            },
        };
        assert_eq!(snapshot.mark_price(Side::Up), Some(dec!(0.4100)));
        assert_eq!(snapshot.mark_price(Side::Down), Some(dec!(0.5800)));
    }
}
