//! 账本层：维护双边持仓的股数、累计净成本与加权均价，并提供预写日志持久化。
//!
//! 对应策略说明书第八节「高频账本高可用（HA）」：纯内存账本（[`Ledger`]）+
//! 预写日志（[`wal::Wal`]）——每笔成交先落盘再入账，panic 重启后重放日志完美复原。

pub mod ledger;
pub mod wal;

pub use ledger::Ledger;
