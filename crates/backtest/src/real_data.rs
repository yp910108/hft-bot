//! 真实历史行情加载：解析 `logs/` 下的逐秒盘口 CSV，转为回测可用的 [`SyntheticMarket`]。
//!
//! CSV 格式（每行一秒快照）：
//! `elapsed_secs,pm_price,ptb,up_bid,up_ask,down_bid,down_ask,up_bid_size,down_bid_size`
//! - `pm_price`：BTC 现货价；`ptb`：结算基准价（price-to-beat），仅末段出现。
//! - 胜负判定：末个有效 `pm_price > ptb` → Up 赢，否则 Down 赢（已用 933 个真实文件验证，
//!   与盘口收敛方向一致）。
//! - 空盘口字段映射为 `None`（约 10% 的行存在单边缺失，由下游 Mark Price 逻辑自然处理）。
//! - `last_trade` 真实数据无对应列，一律填 `None`（不硬造、不失真）。
//!
//! 产出 [`SyntheticMarket`]，与合成行情同构，可直接喂入 `driver::run`。

use crate::market::SyntheticMarket;
use domain::market::{BookTop, MarketSnapshot};
use domain::types::{Price, Side};
use rust_decimal::Decimal;
use std::path::Path;
use std::str::FromStr;

/// 加载 CSV 时可能发生的错误。
#[derive(Debug)]
pub enum LoadError {
    /// 文件读取失败。
    Io(std::io::Error),
    /// CSV 内容无法解析为有效行情（无表头、无数据行、或缺结算基准价）。
    Malformed(String),
}

impl From<std::io::Error> for LoadError {
    fn from(e: std::io::Error) -> Self {
        LoadError::Io(e)
    }
}

/// CSV 各列的下标（与表头顺序对应）。
struct Columns {
    pm_price: usize,
    ptb: usize,
    up_bid: usize,
    up_ask: usize,
    down_bid: usize,
    down_ask: usize,
}

impl Columns {
    /// 从表头行解析各列下标。
    fn from_header(header: &str) -> Result<Self, LoadError> {
        let names: Vec<&str> = header.split(',').map(str::trim).collect();
        let idx = |name: &str| {
            names
                .iter()
                .position(|&n| n == name)
                .ok_or_else(|| LoadError::Malformed(format!("缺少列 {name}")))
        };
        Ok(Self {
            pm_price: idx("pm_price")?,
            ptb: idx("ptb")?,
            up_bid: idx("up_bid")?,
            up_ask: idx("up_ask")?,
            down_bid: idx("down_bid")?,
            down_ask: idx("down_ask")?,
        })
    }
}

/// 解析单元格为价格；空串或解析失败返回 `None`。
fn cell_price(fields: &[&str], idx: usize) -> Option<Price> {
    fields
        .get(idx)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .and_then(|s| Decimal::from_str(s).ok())
}

/// 从一个 CSV 文件加载一场真实行情。
pub fn load_market<P: AsRef<Path>>(path: P) -> Result<SyntheticMarket, LoadError> {
    let content = std::fs::read_to_string(path)?;
    let mut lines = content.lines();

    let header = lines
        .next()
        .ok_or_else(|| LoadError::Malformed("空文件".into()))?;
    let cols = Columns::from_header(header)?;

    let mut snapshots = Vec::new();
    let mut last_pm_price: Option<Decimal> = None;
    let mut last_ptb: Option<Decimal> = None;

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();

        let up = BookTop {
            best_bid: cell_price(&fields, cols.up_bid),
            best_ask: cell_price(&fields, cols.up_ask),
            last_trade: None,
        };
        let down = BookTop {
            best_bid: cell_price(&fields, cols.down_bid),
            best_ask: cell_price(&fields, cols.down_ask),
            last_trade: None,
        };
        snapshots.push(MarketSnapshot { up, down });

        if let Some(p) = cell_price(&fields, cols.pm_price) {
            last_pm_price = Some(p);
        }
        if let Some(p) = cell_price(&fields, cols.ptb) {
            last_ptb = Some(p);
        }
    }

    if snapshots.is_empty() {
        return Err(LoadError::Malformed("无数据行".into()));
    }
    // 胜负：末个有效 pm_price > ptb → Up 赢。缺基准价无法判定胜负。
    let (pm_price, ptb) = match (last_pm_price, last_ptb) {
        (Some(pm), Some(ptb)) => (pm, ptb),
        _ => return Err(LoadError::Malformed("缺少 pm_price 或 ptb，无法判定胜负".into())),
    };
    let winner = if pm_price > ptb { Side::Up } else { Side::Down };

    Ok(SyntheticMarket { snapshots, winner })
}

/// 加载一个日期目录下的全部 CSV 行情；跳过无法解析的文件。
pub fn load_dir<P: AsRef<Path>>(dir: P) -> Result<Vec<SyntheticMarket>, LoadError> {
    let mut markets = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "csv"))
        .collect();
    entries.sort();
    for path in entries {
        if let Ok(market) = load_market(&path) {
            markets.push(market);
        }
    }
    Ok(markets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 写一个临时 CSV 并返回路径。
    fn write_csv(tag: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("hft_real_test_{tag}.csv"));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn parses_snapshots_and_up_wins_when_price_above_ptb() {
        let csv = "elapsed_secs,pm_price,ptb,up_bid,up_ask,down_bid,down_ask,up_bid_size,down_bid_size\n\
                   6,100.0,,0.43,0.44,0.56,0.57,10,10\n\
                   7,101.0,,0.45,0.46,0.54,0.55,10,10\n\
                   8,102.0,99.0,0.99,0.99,0.01,0.02,10,10\n";
        let path = write_csv("up_win", csv);
        let market = load_market(&path).unwrap();
        assert_eq!(market.snapshots.len(), 3);
        // 末价 102 > ptb 99 → Up 赢。
        assert_eq!(market.winner, Side::Up);
        // 首行盘口正确解析。
        assert_eq!(market.snapshots[0].up.best_ask, Some(Decimal::from_str("0.44").unwrap()));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn down_wins_when_price_below_ptb() {
        let csv = "elapsed_secs,pm_price,ptb,up_bid,up_ask,down_bid,down_ask,up_bid_size,down_bid_size\n\
                   6,100.0,,0.43,0.44,0.56,0.57,10,10\n\
                   8,98.0,99.0,0.01,0.02,0.98,0.99,10,10\n";
        let path = write_csv("down_win", csv);
        let market = load_market(&path).unwrap();
        // 末价 98 < ptb 99 → Down 赢。
        assert_eq!(market.winner, Side::Down);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_book_fields_map_to_none() {
        let csv = "elapsed_secs,pm_price,ptb,up_bid,up_ask,down_bid,down_ask,up_bid_size,down_bid_size\n\
                   6,100.0,,,0.44,0.56,,10,10\n\
                   8,102.0,99.0,0.5,0.5,0.5,0.5,10,10\n";
        let path = write_csv("empty_book", csv);
        let market = load_market(&path).unwrap();
        // 首行 up_bid 与 down_ask 为空 → None。
        assert_eq!(market.snapshots[0].up.best_bid, None);
        assert_eq!(market.snapshots[0].down.best_ask, None);
        // last_trade 一律 None。
        assert_eq!(market.snapshots[0].up.last_trade, None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_ptb_is_malformed() {
        let csv = "elapsed_secs,pm_price,ptb,up_bid,up_ask,down_bid,down_ask,up_bid_size,down_bid_size\n\
                   6,100.0,,0.43,0.44,0.56,0.57,10,10\n";
        let path = write_csv("no_ptb", csv);
        // 无结算基准价 → 无法判胜负 → Malformed。
        assert!(matches!(load_market(&path), Err(LoadError::Malformed(_))));
        std::fs::remove_file(&path).ok();
    }
}
