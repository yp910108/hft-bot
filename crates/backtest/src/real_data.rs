//! 真实历史行情加载：解析 `logs/` 下的逐秒盘口 CSV，转为回测可用的 [`Market`]。
//!
//! CSV 格式（每行一秒快照）：
//! `elapsed_secs,pm_price,ptb,up_bid,up_ask,down_bid,down_ask,up_bid_size,down_bid_size`
//! - `pm_price`：BTC 现货价；`ptb`：结算基准价（price-to-beat），仅末段出现。
//! - 胜负判定：末个有效 `pm_price > ptb` → Up 赢，否则 Down 赢。
//! - 空盘口字段映射为 `None`（约 10% 的行存在单边缺失）。
//! - `last_trade` 真实数据无对应列，一律填 `None`。

use crate::market::Market;
use domain::market::{BookTop, MarketSnapshot};
use domain::types::{Price, Side};
use rust_decimal::Decimal;
use std::fs;
use std::io::Error;
use std::path::Path;
use std::str::FromStr;

/// 加载 CSV 时可能发生的错误。
#[derive(Debug)]
pub enum LoadError {
    /// 文件读取失败。
    Io(Error),
    /// CSV 内容无法解析为有效行情（无表头、无数据行、或缺结算基准价）。
    Malformed(String),
}

impl From<Error> for LoadError {
    fn from(e: Error) -> Self {
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
fn parse_price(field: &str) -> Option<Price> {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return None;
    }
    Decimal::from_str(trimmed).ok()
}

/// 加载单个 CSV 文件为一场回测市场数据。
pub fn load_market<P: AsRef<Path>>(path: P) -> Result<Market, LoadError> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)?;
    let mut lines = content.lines();

    let header = lines
        .next()
        .ok_or_else(|| LoadError::Malformed("空文件".into()))?;
    let cols = Columns::from_header(header)?;

    let mut snapshots = Vec::new();
    let mut last_pm_price: Option<Decimal> = None;
    let mut last_ptb: Option<Decimal> = None;

    for line in lines {
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() <= cols.down_ask {
            continue;
        }

        if let Some(pm) = parse_price(fields[cols.pm_price]) {
            last_pm_price = Some(pm);
        }
        if let Some(ptb) = parse_price(fields[cols.ptb]) {
            last_ptb = Some(ptb);
        }

        let up = BookTop {
            best_bid: parse_price(fields[cols.up_bid]),
            best_ask: parse_price(fields[cols.up_ask]),
            last_trade: None,
        };
        let down = BookTop {
            best_bid: parse_price(fields[cols.down_bid]),
            best_ask: parse_price(fields[cols.down_ask]),
            last_trade: None,
        };

        snapshots.push(MarketSnapshot { up, down });
    }

    if snapshots.is_empty() {
        return Err(LoadError::Malformed("无数据行".into()));
    }

    let ptb = last_ptb.ok_or_else(|| LoadError::Malformed("缺结算基准价 ptb".into()))?;
    let pm = last_pm_price.unwrap_or(ptb);
    let winner = if pm > ptb { Side::Up } else { Side::Down };

    Ok(Market {
        snapshots,
        winner,
        title: market_title(path),
        source_file: path.to_string_lossy().into_owned(),
    })
}

/// 从文件路径拼出场标题，如 "BTC 15m - 2026-05-22 07:30"。
/// 目录名当日期，文件名 `07_30_508877.csv` 取前两段当时刻。
fn market_title(path: &Path) -> String {
    let date = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let parts: Vec<&str> = stem.split('_').collect();
    let time = if parts.len() >= 2 {
        format!("{}:{}", parts[0], parts[1])
    } else {
        stem.to_string()
    };
    format!("BTC 15m - {date} {time}")
}

/// 加载指定目录下的所有 CSV 文件为市场数据列表（递归子目录）。
pub fn load_dir<P: AsRef<Path>>(dir: P) -> Result<Vec<Market>, LoadError> {
    let mut paths = Vec::new();
    collect_csv(dir.as_ref(), &mut paths)?;
    paths.sort();
    let mut markets = Vec::new();
    for path in paths {
        if let Ok(market) = load_market(&path) {
            markets.push(market);
        }
    }
    Ok(markets)
}

/// 递归收集目录下所有 .csv 文件路径。
fn collect_csv(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), LoadError> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_csv(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "csv") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_csv(tag: &str, content: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("hft_real_test_{tag}.csv"));
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn parses_snapshots_and_up_wins_when_price_above_ptb() {
        let csv = "\
elapsed_secs,pm_price,ptb,up_bid,up_ask,down_bid,down_ask,up_bid_size,down_bid_size
6,100.5,100.0,0.55,0.56,0.44,0.45,1000,800
7,101.0,100.0,0.60,0.61,0.39,0.40,1200,600
";
        let path = write_csv("up_win", csv);
        let market = load_market(&path).unwrap();
        assert_eq!(market.snapshots.len(), 2);
        assert_eq!(market.winner, Side::Up);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn down_wins_when_price_below_ptb() {
        let csv = "\
elapsed_secs,pm_price,ptb,up_bid,up_ask,down_bid,down_ask,up_bid_size,down_bid_size
6,99.0,100.0,0.40,0.41,0.59,0.60,500,1000
";
        let path = write_csv("down_win", csv);
        let market = load_market(&path).unwrap();
        assert_eq!(market.winner, Side::Down);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_book_fields_map_to_none() {
        let csv = "\
elapsed_secs,pm_price,ptb,up_bid,up_ask,down_bid,down_ask,up_bid_size,down_bid_size
6,100.5,100.0,,0.56,,0.45,0,0
";
        let path = write_csv("empty_book", csv);
        let market = load_market(&path).unwrap();
        let snap = &market.snapshots[0];
        assert!(snap.up.best_bid.is_none());
        assert!(snap.down.best_bid.is_none());
        assert!(snap.up.best_ask.is_some());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_ptb_is_malformed() {
        let csv = "\
elapsed_secs,pm_price,ptb,up_bid,up_ask,down_bid,down_ask,up_bid_size,down_bid_size
6,100.5,,0.55,0.56,0.44,0.45,1000,800
";
        let path = write_csv("no_ptb", csv);
        assert!(matches!(load_market(&path), Err(LoadError::Malformed(_))));
        fs::remove_file(&path).ok();
    }
}
