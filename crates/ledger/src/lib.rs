//! 账本层：记录 Up/Down 两侧各持有多少股、花了多少钱。
//!
//! 内存账本（[`Ledger`]）负责实时计算，预写日志（[`wal::Wal`]）负责落盘。
//! 每笔成交先写日志再入账，崩溃重启后重放日志即可恢复到崩溃前的状态。

pub mod ledger;
pub mod wal;

pub use ledger::Ledger;
