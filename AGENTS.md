# AGENTS.md

本文件是 ProofForge 仓库对 AI 编程助手（Claude Code、Codex、Cursor 等）的
**强制性工作约定**。人类贡献者请参阅 `README.md` 与 `docs/`。

---

## 1. 项目一句话

ProofForge 是一个用 Rust 编写的**软件工程质量审查与自主验证系统**：它独立
于代码生成过程，判断一份实现是否符合需求、是否存在真实缺陷、是否存在可以
安全消除的不必要复杂性，并为每个结论保留可复现证据。

完整需求见 `需求文档.md`（16 章，功能需求编号 FR-001 … FR-070）。

---

## 2. 开始工作前必读

1. `需求文档.md` 的相关章节 —— 特别是你将要改动的那一部分。
2. `docs/` 下对应的设计说明：架构见 `architecture.md`，证据与门禁见
   `evidence-model.md`，规则契约见 `rule-engine.md`，豁免语义见
   `exemptions.md`，当前做不到什么见 `known-limitations.md`。
3. 本文件第 4 节的编码与注释规范。
4. 目标 crate 的 `src/lib.rs` 顶部文档注释（说明了该 crate 的职责边界）。

---

## 3. 架构与目录

```text
proofforge/
├── Cargo.toml            workspace 根：依赖版本、lint 基线、release profile
├── crates/
│   ├── pf-core/          领域模型：ID、定位、Finding、证据、规则、门禁（零 I/O）
│   ├── pf-git/           Git 读取（复用 gix）
│   ├── pf-parser/        多语言语法解析（复用 tree-sitter grammar）
│   ├── pf-symbol/        符号索引与引用提取
│   ├── pf-graph/         依赖图 / 调用图（复用 petgraph 的图算法）
│   ├── pf-rules/         工程质量规则引擎、内置规则集、规范导入与冲突检测
│   ├── pf-review/        审查编排、质量分层、规范导入与豁免应用
│   ├── pf-verify/        行为验证：沙箱、测试执行器、属性与模糊测试、反例缩减
│   ├── pf-simplify/      简化候选、隔离补丁、回归与行为比较、安全应用
│   ├── pf-agent/         Agent Runtime：工具权限、验证计划、策略、预算、轨迹
│   ├── pf-report/        报告渲染（terminal / JSON / SARIF / HTML）
│   ├── pf-store/         SQLite 结果、证据、趋势与规则集版本历史
│   ├── pf-cli/           命令行入口（含 tests/cli.rs 端到端测试）
│   └── pf-integration/   跨 crate 集成测试（tests/integration.rs）
├── adapters/             语言与工具链适配器（cargo / pytest / ruff）
├── configs/              示例配置（proofforge.toml、exemptions.example.toml）
├── exemptions.toml       本仓库对自己的规则豁免（fixtures/ 里植入的样本）
├── fixtures/             测试用样本项目（over-engineered / clean / verifiable）
├── docs/                 设计说明（架构、证据模型、规则契约、豁免、已知限制）
└── 需求文档.md           需求规格（16 章，FR-001 … FR-070）
```

跨 crate 集成测试放在 `crates/pf-integration/tests/`，**不放在仓库根的
`tests/`**：它们是普通 crate，能依赖 workspace 的其他成员并参与
`cargo test --workspace`。仓库根的 `tests/` 目录不存在，不要在那里新建。

**依赖方向必须保持单向**，完整边集见
[`docs/architecture.md`](docs/architecture.md)。要点：

- `pf-core` 不依赖任何内部 crate，也不做 I/O；`pf-report`、`pf-store`、
  `pf-git`、`pf-parser`、`pf-verify` 只依赖 `pf-core`（`pf-verify` 另依赖
  `pf-adapters`）。
- 静态分析链 `pf-rules → pf-symbol → pf-graph → pf-parser` 只向上依赖。
- `pf-simplify` 与 `pf-agent` 只被 `pf-cli` 依赖，没有别的 crate 依赖它们：
  会改用户代码的能力、以及会接触模型的能力，都必须是管线的终点。

违反此方向会破坏可测试性，CI 不会替你发现。

---

## 4. 编码规范（Google Rust Style 为基准）

### 4.1 命名

| 对象 | 风格 | 示例 |
|---|---|---|
| crate / module | snake_case | `pf_symbol`、`call_graph` |
| type / trait / enum 变体 | PascalCase | `SymbolIndex`、`FindingKind` |
| function / method / variable | snake_case | `build_index`、`line_count` |
| constant / static | SCREAMING_SNAKE_CASE | `MAX_LEN` |
| 布尔值 | `is_` / `has_` / `should_` 前缀 | `is_blocking`、`has_errors` |
| getter | 不加 `get_` 前缀 | `fn name(&self)` 而非 `fn get_name` |

### 4.2 文档注释（强制）

- 所有 `pub` 项必须有 `///` 文档注释。`missing_docs` 已在 workspace lint 中
  设为 `warn`，新增未文档化的公开 API 会产生警告。
- 注释解释**为什么**与**约束**，不复述代码。反例：
  ```rust
  // 坏：复述代码
  /// 返回计数。
  pub fn count(&self) -> usize

  // 好：说明契约
  /// 返回当前已索引的符号数量。
  ///
  /// 该值在索引构建后不再变化；并发访问请自行加锁。
  pub fn count(&self) -> usize
  ```
- 公开函数若可能返回错误，用 `# Errors` 小节说明错误条件；若可能 panic，用
  `# Panics` 小节说明（workspace lint 目前允许省略，但新增 panic 路径必须写）。
- 测试函数同样需要注释，说明它守护的是哪一条行为。

### 4.3 错误处理

- 库代码返回具体的错误类型（`thiserror` 派生的 enum），**不要**返回
  `String` 或 `Box<dyn Error>`。
- 二进制层（`pf-cli`）才使用 `anyhow`。
- 禁止在库代码中 `unwrap()` / `expect()` / `panic!()`；确有不变量保证时，
  用 `debug_assert!` 并在注释中说明理由。
- 错误信息面向使用者，包含可操作信息（哪个路径、哪个引用、如何修正）。

### 4.4 所有权与生命周期

- 优先传 `&str` / `&[T]`；小尺寸 Copy 类型（≤ 16 字节）按值传递。
- 返回 `impl Iterator + '_` 而不是 `Vec`，除非调用方确实需要随机访问。
- 避免为绕过借用检查而使用 `Box::leak`；测试中请用回调
  （参考 `pf-rules` 的 `with_context`）。

### 4.5 结构与抽象

- **不提前抽象**。不要为「将来可能有多实现」创建 trait；单一实现就用具体
 类型（这条规则对我们自己的代码同样适用 —— 我们就是在检测这种问题）。
- 优先复用成熟 crate：Git 用 `gix`、解析用 `tree-sitter`、图算法用
  `petgraph`、存储用 `sqlx`。不要重新实现这些能力。
- 函数超过 80 行、参数超过 5 个时，`pf-rules` 会报告 —— 请真的拆分。
- 模块超过约 800 行时考虑拆文件，但拆分的依据是**职责**而非行数。

---

## 5. 测试要求

- 每个 crate 的测试放在同文件的 `#[cfg(test)] mod tests` 中；跨 crate 的
  集成测试放 `tests/`。
- 公开 API 至少有一条「正常路径」测试；每个 `match` 分支都要有测试覆盖。
- 修复 bug 时**先写失败测试**，再修复。
- 测试必须确定性：不依赖网络、当前时间、随机数或执行顺序。
- 表驱动测试优先于重复的断言块。

运行：

```bash
cargo test --workspace          # 全部测试
cargo test -p pf-rules          # 单个 crate
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

---

## 6. 需求可追溯性

- 实现某条 FR 时，在相关类型的文档注释中引用编号，例如
  `//! 需求文档 FR-023：模块循环依赖。`
- 新增内置规则时，在 `pf-rules/src/lib.rs` 的 `builtin_checkers()` 中追加，
  并使用**稳定可读的规则 ID**（`RuleId::builtin("kebab-case-name")`）。
  规则 ID 会出现在报告、豁免配置与审计记录中，不得随机生成。

---

## 7. 安全红线

以下行为在任何情况下都不允许，即使是为了「让测试通过」：

1. 执行来自被审查仓库的任意字符串命令。只能运行由 `pf-adapters` 的
   [`ToolAdapter`] 生成的命令。
2. 把模型 API 密钥、`GITHUB_TOKEN` 等凭据传入子进程环境。
3. 未经用户明确授权修改被审查仓库的文件。默认只读；`simplify` 的补丁必须
   落在独立工作区，并由人工确认后才应用（FR-055 / FR-056）。
4. 把「检查未完成」或「工具缺失」报告为「通过」。必须返回
   `GateStatus::InsufficientEvidence`。
5. 把模型输出的猜测标注为已验证结论。必须区分
   `EvidenceStatus::Suspected` 与 `Reproduced`。

---

## 8. 提交前自查

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

三者全部通过才提交。提交信息用祈使句，说明「为什么」而不只是「做了什么」。
