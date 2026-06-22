# 项目约定（每个会话自动加载，无需重复说明）

## 交流语言
- **全程使用简体中文**回答与交流，包括分析、设计文档、提交说明、代码注释。

## 工作方式
- **质量第一，不追求速度**。在稳健的基础上一步一步完善功能，不为赶进度牺牲正确性。
- 重大设计决策先讨论清楚再动手；遇到逻辑漏洞或风险，先暴露问题，不闷头实现。
- **涉及交易所真实行为的事实（手续费、精度、最小量、撤单/成交回调、结算机制等），不得凭推理下定论**。先查 `docs/待查证事实.md`：已核实的按结论实现，未核实的标注清楚并提请用户确认。新增此类假设时同步登记到该文件。
- **文档和代码修改不留历史记录，直接替换成最新正确结果**。不需要"📌实现修订""曾经如何""见风险修复项#N"等批注和历史痕迹——需要看历史直接去 GitHub 提交记录。

## 代码要求
- **每完成一个独立功能，必须配套编写单元测试**，测试通过后再进入下一个功能。
- 命名必须**简洁易读、见名知意**：结构体（struct）、枚举（enum）、方法、函数、变量一律遵守此原则，避免缩写歧义和无意义命名。
- 代码风格与项目现有代码保持一致。
- **构造 `Decimal` 字面量统一使用 `dec!` 宏**（来自 `rust_decimal_macros`），不用 `Decimal::new(4, 2)` 这类写法。`dec!(0.04)` 比 `Decimal::new(4, 2)` 更直观、更易读，且全项目保持一致。`Decimal::ZERO`、`Decimal::ONE` 等具名常量可继续使用。
- **路径引入遵循 Rust 官方惯用 `use` 规范**（《The Rust Programming Language》第 7.4 节 "Creating Idiomatic use Paths"），禁止在代码体中内联完整路径（如 `crate::types::Money`）：
  - **类型**（结构体 struct、枚举 enum、类型别名 type、trait）：`use` 到**具体项**，代码中直接用短名。例：`use crate::types::Money;` 后写 `Money`。
  - **函数**：`use` 到其**父模块**，调用时带模块名。例：`use crate::math;` 后写 `math::compute()`，而非 `use crate::math::compute;`。这样既表明函数非本地定义，又不必重复完整路径。
  - **例外**：当不同模块的同名项同时引入造成命名冲突时，改为引入各自父模块来消歧义（或用 `as` 重命名）。
- **`lib.rs` 职责单一**：一旦 crate 拆出子模块（有 `pub mod xxx;`），`lib.rs` 就只当「门面/索引」——只放模块声明、`pub use` 重导出与 crate 级文档，**不再塞实质代码**。实质类型/逻辑一律放进各自的子模块文件（如账本放 `ledger.rs`、持久化放 `wal.rs`）。反例：曾经 `ledger/lib.rs` 既声明 `pub mod wal` 又自含 356 行账本代码，是身兼二职的「混合形态」，已拆分。单文件 crate（只有 `lib.rs`、无子模块，如 fsm/strategy/engine）不受此约束。
- **`pub use` 重导出按需使用，不强求**：判断标准是 crate 有没有「主类型」。
  - **有单一主角**：在 `lib.rs` 用 `pub use` 把主类型提到 crate 顶层，外部写 `crate_name::MainType`，避免 `crate_name::module::MainType` 的双重名绕口。例：`ledger` 的 `pub use ledger::Ledger;` → 外部用 `ledger::Ledger`。
  - **一堆平级类型（词汇表型）**：保留模块路径，**不要**用 `pub use` 平铺到顶层——模块名本身承载分类信息，平铺会丢失分类、污染顶层命名空间。例：`domain` 应保持 `domain::types::Money`、`domain::order::Fill`，`exchange`/`risk` 同理保留 `exchange::event::ExchangeEvent`、`risk::auditor::RiskAuditor`。

## 项目背景
- 这是一个 Polymarket BTC 15 分钟周期二元预测市场的自动化高频交易机器人。
- 策略全文见 `docs/策略.md`（“双模态自适应对冲”量化策略）。
- 技术栈：Rust + Tokio 异步运行时，当前采用 Cargo Workspace。
