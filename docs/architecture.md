# 架构

## crate 职责与依赖方向

```text
命令行      pf-cli

编排层      pf-agent    pf-simplify   pf-review   pf-verify
           pf-report   pf-store

规则层      pf-rules    pf-adapters   pf-git

索引层      pf-symbol   pf-graph

解析层      pf-parser

领域模型    pf-core
```

每一层只依赖它下面的层，唯一的例外是同层的 `pf-graph → pf-symbol`：调用图建立在
符号索引之上，把它拆成两层只会得到两张各含一个 crate 的层。`pf-integration`
是测试专用 crate，不属于分层，它横向依赖 `pf-review` 与 `pf-report`。

依赖方向必须保持单向（AGENTS.md 第 3 节），完整边集：

```text
pf-cli      → pf-agent / pf-simplify / pf-review / pf-verify / pf-report / pf-store / pf-rules
pf-agent    → pf-review / pf-rules / pf-verify / pf-adapters / pf-symbol / pf-graph / pf-git
pf-simplify → pf-review / pf-rules / pf-verify / pf-adapters / pf-symbol / pf-graph
pf-review   → pf-rules / pf-symbol / pf-graph / pf-parser / pf-git
pf-rules    → pf-symbol / pf-graph / pf-parser
pf-symbol   → pf-parser
pf-graph    → pf-symbol
pf-verify   → pf-adapters
其余 crate  → pf-core
```

`pf-core` 不依赖任何内部 crate，也不做 I/O；`pf-report` 与 `pf-store` 只依赖
`pf-core`。这条约束的收益是可测试性：报告渲染与存储可以在没有语法树、没有
文件系统的情况下被单元测试。

`adapters/`（crate 名 `pf-adapters`）是独立的 workspace 成员，目录不带 `pf-`
前缀：它是唯一被允许拼出子进程命令的地方（AGENTS.md 安全红线 1），`pf-cli`、
`pf-verify`、`pf-simplify` 与 `pf-agent` 都通过它执行工具。它只依赖
`pf-core`。

`pf-verify` 只依赖 `pf-core` 与 `pf-adapters`，不依赖任何静态分析 crate。这条
边界让「验证」与「静态审查」可以分别运行、分别测试，也意味着只在需要动态证据
时才付出编译目标项目的代价。行为验证回答的是「这个程序在这个输入下做了什么」，
不是「这段代码长什么样」；把两者接在一起会让静态审查的发现被当成动态结论的
证据，那正是红线 5 要防的事。

`pf-simplify`（阶段五）与 `pf-agent`（阶段四）挂在静态分析与验证之上，只被
`pf-cli` 依赖。`pf-agent` 是唯一接触模型接缝的 crate，它的工具调用权限由 Rust
Runtime 强制检查，不依赖提示词约束（需求文档 9.3）。`pf-simplify` 的补丁一律
落在 `pf-verify::Sandbox` 的独立工作区里，只有显式授权才写目标仓库
（FR-055 / FR-056，AGENTS.md 安全红线 3）。

## 数据流

一次 `proofforge inspect <target>` 的执行顺序：

1. **`pf-git`** 读取目标版本：工作区、某个提交或两个版本之间的差异。产出
   `SnapshotId` 与文件清单。
2. **`pf-core::source`** 把文件清单变成 `SourceMap`（路径 + 内容 + 语言），
   并应用预算限制（最大文件数、单文件字节数、忽略规则）。
3. **`pf-parser`** 用 Tree-sitter 解析每个文件，产出 `ParseTree`。解析失败的
   文件不中断流程，记入 warnings。
4. **`pf-symbol`** 遍历语法树，提取符号定义与引用，产出 `SymbolIndex`。
5. **`pf-graph`** 基于 `SymbolIndex` 构建模块依赖图与调用图。
6. **`pf-rules`** 把 `SymbolIndex` + 语法树 + 两张图打包成 `Context`，逐条
   规则产出 `Finding`。
7. **`pf-review`** 先加载项目规范（FR-058）合并进规则集，再应用豁免
   （FR-062）：豁免只把发现标记为「已豁免」，不删除它。
8. **`pf-core::review`** 汇总发现，按四层质量模型分层，调用 `decide_gate`
   得到门禁状态。
9. **`pf-report`** 渲染为终端 / JSON / SARIF / HTML。
10. **`pf-store`**（可选）把运行、发现与证据写入 SQLite。

豁免与规范的加载失败都以 warnings 形式进入报告，不会让审查中断：一份写错的
`exemptions.toml` 不应该让整个仓库失去审查结果，但静默忽略它同样不行
（AGENTS.md 安全红线 4）。详见 [exemptions.md](exemptions.md) 与
[rule-engine.md](rule-engine.md) 的「规范管理与豁免」。

## 验证数据流

一次 `proofforge verify --target <dir> --properties <file>` 的执行顺序：

1. **`pf-verify::spec`** 读入 JSON 规格文件，校验每条性质的输入域是否可执行
   （`min <= max`、长度非零）。空文件与矛盾域在这里就被拒绝——它们不是
   「验证通过」。
2. **`pf-verify::sandbox`** 把目标项目复制进独立临时工作区，跳过 `target/`、
   `.git/` 与超预算文件，不跟随符号链接。原仓库从此只读。
3. **`pf-verify::domain`** 用固定种子生成样本，**以字面量**写进驱动程序。
4. **`pf-verify::driver`** 渲染 Rust / Python 驱动程序，经 `pf-adapters` 的
   `Tool::SingleTest` 执行（不带 `--all-targets`，否则项目自己的测试失败会
   污染驱动程序的结论），并在剔除凭据的环境里运行。
5. **`pf-verify::property`** 解读结果：违反标记在 stdout、完成标记在 stdout，
   两者都读不到就记证据不足。找到违反后进入
   **`pf-verify::reduce`** 的 ddmin 式缩减，一轮编译执行淘汰一批候选。
6. **`pf-verify::report`** 汇总为报告与门禁结论，反例附带复现配方
   （驱动源码 + 命令），`pf-cli` 决定退出码。

### 变异测试与蜕变测试为什么也在 `pf-verify`

`mutate`（FR-037）与 `verify --metamorphic`（FR-039）复用同一条链的每一环：
沙箱、驱动程序渲染、执行解读、反例缩减、复现配方。差别只在被测的命题：

| 能力 | 断言的形式 | 需要什么 |
|---|---|---|
| 性质测试 | `P(f(x))` | 一条对输出的不变式 |
| 蜕变测试 | `R(f(x), f(T(x)))` | 一个输入变换 + 一条输出关系 |
| 变异测试 | `f(x)` 改动后测试是否失败 | 项目自己的测试套件 |

前两者由 ProofForge 渲染驱动程序并直接执行目标函数；后者**不渲染任何断言**，
它把变异后的源码写进沙箱，跑的是项目自己的 `cargo test` / `pytest`。因此变异
测试的结论只关于测试套件，与「程序是否正确」无关——这条区别写在
[`known-limitations.md`](known-limitations.md)，也写在 CLI 的输出里。

变异算子因此是**词法**的：`pf-verify` 不依赖 `pf-parser`（见上文依赖方向），
拿不到语法树。改控制流、删函数体这类变异要等这个约束变化后才能做。

分类上有一处必须显式区分：`cargo test` 在**编译失败**与**测试失败**时同样非零
退出。因此测试一失败就再问一次编译器（`Tool::Build`），由它决定这个变异体记
`Killed` 还是 `Invalid`。只读退出码会把编译不过的变异体算成「测试发现了
问题」，凭空抬高通过率——那是红线 4 禁止的结论。代价是失败的那批变异体各多
一次 `cargo check`；通过率的分母因此只包含真实发生过的比较。

### 为什么「有没有跑起来」不靠退出码判定

编译失败、测试目标不存在、参数写错都会让 `cargo test` 以非零退出码结束，而
输出里没有任何测试执行记录。只靠退出码判定，这三种情况与「全部样本都满足」
无法区分——后者是退出码 0，前者是非零，看起来可分，但只要驱动程序里有一条
项目自身的编译警告被 `-D warnings` 拦住，「满足」也会变成非零。

因此驱动程序在跑完时打印一个完成标记，判定规则读它而不是读退出码。这不是
防御性编程：它是唯一能把「没跑」和「跑过且通过」分开的信号。

### 为什么驱动程序不解析标准输出找回反例

反例的下标由 ProofForge 在渲染时记录，驱动程序只打印下标标签。解析 `Debug`
输出回 `InputValue` 既脆弱，又给目标类型增加 `Debug` 约束。

### 为什么符号提取与规则分离

`pf-symbol` 只回答「这里有什么符号、谁引用了谁」，`pf-rules` 只回答「这个
结构是否可疑」。分离的收益：

- 规则可以独立测试：给一份 `SymbolIndex` 就能断言，不需要构造语法树。
- 新增语言只需在 `pf-symbol` 加一条提取路径，所有规则自动获得该语言的
  结构事实。
- 规则之间不共享可变状态，可以并行执行（阶段四的 `pf-agent` 用到了这一点）。

### 为什么调用图只做两级名字解析

`pf-graph::CallGraph` 先尝试完整限定名精确匹配，失败后退化为「按名字最后
一段匹配，且只有一个候选时才接受」。这是有意的保守设计：

- 多候选时宁可不解析，也不猜。猜错的边会让「循环依赖」「未调用的公共 API」
- 这类规则产生假结论，比漏报更糟。
- 未解析的调用被记录在 `unresolved_calls()` 里，可以作为证据说明分析的
  完整度，而不是被静默丢弃。

代价是跨作用域同名方法会漏检。这个取舍记录在
[known-limitations.md](known-limitations.md)。

## 简化数据流

一次 `proofforge simplify --target <dir>` 的执行顺序：

1. **`pf-simplify::candidate`** 从审查发现与静态结构里挑出「可以安全消除的
   不必要复杂性」，生成候选。需要语义判断的变换只产出建议，不产出补丁。
2. **`pf-simplify::workspace`** 在 `pf-verify::Sandbox` 里复制一份隔离工作区，
   原仓库从此只读。
3. **`pf-simplify::edit`** 把候选写成补丁，只落在隔离工作区里。
4. **`pf-simplify::regression`** 跑目标项目自带的测试套件作为回归基线；
   **`pf-simplify::behavior`** 用 `pf-verify` 的驱动程序做行为比较。
5. **`pf-simplify::metrics`** 比较规模指标（行数、符号数、依赖边），
   `pf-simplify::report` 汇总为报告，反例与残余风险一并列出。
6. **`pf-simplify::apply`** 是整条管线上唯一写目标仓库的环节，它要求显式授权
   （`--yes`，等价于 FR-055 的人工确认）且候选必须已通过回归与行为验证。

基线本就失败、工具缺失、或没给 `--properties` 时，行为比较只能说明覆盖范围有
缺口，门禁返回 `InsufficientEvidence`——不会因为「测试套件跑过了」就算通过
（红线 4）。

## Agent 数据流

一次 `proofforge agent --target <dir>` 的执行顺序：

1. **`pf-agent::plan`** 由静态结构（`pf-symbol` / `pf-graph`）生成初始验证
   计划：该验证哪些目标、每个目标配哪些性质。
2. **`pf-agent::strategy`** 按优先级与预算调度下一个任务。
3. **`pf-agent::tools`** 把每个任务变成一个真实的工具调用：`run_existing_tests`
   走 `pf_verify::TestRunner`，`verify_property` 走 `pf_verify::PropertyRunner`，
   `fuzz` 走 `pf_verify::FuzzRunner`，`run_static_check` 走 `pf_review::ReviewEngine`
   ——全部在 `pf_verify::Sandbox` 的隔离副本里执行。`pf-agent::tool` 在模型输出
   之后、工具执行之前做权限检查，检查由 Rust Runtime 强制，不依赖提示词约束
   （9.3）。工具集里**没有 Shell**，外部执行一律经 `pf-adapters`（红线 1）。
   静态发现却没有可执行输入域的目标（例如只命中 `no-unwrap-in-library` 的文件）
   会被报告成「没有得到结论」，而不是猜一个结论。
4. **`pf-agent::feedback`** 读回测试与编译反馈，`pf-agent::state`** 更新计划：
   失败的性质可能让相邻目标进入高优先级，跑过的目标则降级。
5. **`pf-agent::budget`** 在步骤数、时间或用例数耗尽时停循环；
   **`pf-agent::trace`** 记录每个决策的理由，供审计（FR-070）。

Runtime 定义 `LlmProvider` 接缝但不内置任何外部模型调用，默认的
`StructuralPlanner` 完全由静态结构驱动——因此没有 API 密钥也能完整运行，
测试也因此确定。
