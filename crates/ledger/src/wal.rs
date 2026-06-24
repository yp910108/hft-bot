//! 预写日志（Write-Ahead Log, WAL）：账本的原子落盘与重启复原。
//!
//! 对应策略说明书第八节「高频账本高可用（HA）」。每笔成交在入账前先以一行 JSON
//! 追加写入日志文件并刷盘（write-ahead）；程序 panic 重启后，重放整个日志逐笔重建账本，
//! 即可完美复原持仓、成本与均价。
//!
//! 之所以记录不可变的成交事实（[`Fill`]）而非账本快照：成交是 append-only 的历史，
//! 只追加不修改，天然适合原子写；而账本状态是成交序列的确定性纯函数（`apply_fill`），
//! 重放必得一致结果。这也呼应「账本是唯一数据源、不存第二本账」的原则。

use crate::Ledger;
use domain::order::Fill;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Result, Write};
use std::path::{Path, PathBuf};

/// 预写日志：以 JSONL（每行一个 JSON）格式持久化成交序列。
#[derive(Debug)]
pub struct Wal {
    path: PathBuf,
    /// 以追加模式打开的写句柄。
    writer: File,
}

impl Wal {
    /// 打开（或创建）指定路径的日志文件，以追加模式准备写入。
    ///
    /// 已存在的日志内容会被保留，新记录追加在末尾。
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let writer = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self { path, writer })
    }

    /// 追加一笔成交并刷盘。
    ///
    /// 序列化为一行 JSON 写入后，调用 `sync_all` 确保数据真正落到磁盘
    /// （而非停留在操作系统缓冲区），保证 panic 时已写入的记录不丢失。
    pub fn append(&mut self, fill: &Fill) -> Result<()> {
        let line = serde_json::to_string(fill)?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.writer.sync_all()?;
        Ok(())
    }

    /// 重放日志，按写入顺序返回全部成交记录。
    ///
    /// 跳过空行；任一行解析失败即返回错误（日志损坏应显式暴露，不静默丢弃）。
    pub fn replay(&self) -> Result<Vec<Fill>> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut fills = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let fill: Fill = serde_json::from_str(&line)?;
            fills.push(fill);
        }
        Ok(fills)
    }

    /// 重放日志并重建账本：对每笔成交依次 `apply_fill`。
    pub fn rebuild_ledger(&self) -> Result<Ledger> {
        let mut ledger = Ledger::new();
        for fill in self.replay()? {
            ledger.apply_fill(&fill);
        }
        Ok(ledger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::order::{Generation, OrderDirection, OrderId};
    use domain::types::{Money, OrderRole, Price, Qty, Side};
    use rust_decimal_macros::dec;
    use std::env;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 生成一个进程内唯一的临时日志路径（避免测试间互相干扰）。
    fn temp_wal_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("hft_bot_wal_test_{tag}_{nanos}.jsonl"))
    }

    /// 构造一笔成交回报。
    fn fill(side: Side, price: Price, filled_qty: Qty, cash: Money) -> Fill {
        Fill {
            order_id: OrderId(0),
            side,
            direction: OrderDirection::Buy,
            role: OrderRole::Maker,
            price,
            filled_qty,
            cash,
            generation: Generation::new(),
        }
    }

    #[test]
    fn append_then_replay_returns_same_fills() {
        let path = temp_wal_path("replay");
        let f1 = fill(Side::Up, dec!(0.40), dec!(100), dec!(40));
        let f2 = fill(Side::Down, dec!(0.55), dec!(80), dec!(44));
        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append(&f1).unwrap();
            wal.append(&f2).unwrap();
        }
        let wal = Wal::open(&path).unwrap();
        let replayed = wal.replay().unwrap();
        assert_eq!(replayed, vec![f1, f2]);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn rebuild_ledger_recovers_positions() {
        let path = temp_wal_path("rebuild");
        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append(&fill(Side::Up, dec!(0.40), dec!(100), dec!(40)))
                .unwrap();
            wal.append(&fill(Side::Down, dec!(0.45), dec!(100), dec!(45)))
                .unwrap();
        }
        // 模拟「重启」：用一个全新 Wal 句柄从冷数据复原账本。
        let recovered = Wal::open(&path).unwrap().rebuild_ledger().unwrap();
        assert_eq!(recovered.qty(Side::Up), dec!(100));
        assert_eq!(recovered.qty(Side::Down), dec!(100));
        // 总成本 40 + 45 = 85，与落盘前账本一致。
        assert_eq!(recovered.total_cost(), dec!(85));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn recovered_ledger_matches_live_ledger() {
        let path = temp_wal_path("match");
        let fills = [
            fill(Side::Up, dec!(0.40), dec!(100), dec!(40)),
            fill(Side::Up, dec!(0.30), dec!(200), dec!(60)),
            fill(Side::Down, dec!(0.50), dec!(50), dec!(25)),
        ];
        // 一边写 WAL、一边维护一个实时账本，最后比对二者完全一致。
        let mut live = Ledger::new();
        {
            let mut wal = Wal::open(&path).unwrap();
            for f in &fills {
                wal.append(f).unwrap();
                live.apply_fill(f);
            }
        }
        let recovered = Wal::open(&path).unwrap().rebuild_ledger().unwrap();
        assert_eq!(recovered, live);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn append_preserves_existing_records_across_reopen() {
        let path = temp_wal_path("reopen");
        // 第一次打开写入一笔。
        Wal::open(&path)
            .unwrap()
            .append(&fill(Side::Up, dec!(0.40), dec!(100), dec!(40)))
            .unwrap();
        // 第二次打开（模拟重启后继续运行）再写一笔，旧记录应保留。
        Wal::open(&path)
            .unwrap()
            .append(&fill(Side::Up, dec!(0.30), dec!(100), dec!(30)))
            .unwrap();
        let replayed = Wal::open(&path).unwrap().replay().unwrap();
        assert_eq!(replayed.len(), 2);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn replay_empty_log_yields_no_fills() {
        let path = temp_wal_path("empty");
        let wal = Wal::open(&path).unwrap();
        assert!(wal.replay().unwrap().is_empty());
        fs::remove_file(&path).ok();
    }
}
