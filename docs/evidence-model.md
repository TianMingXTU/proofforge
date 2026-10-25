# 证据模型与门禁

需求文档第 3.7 节与第 7 章要求：每个结论都必须能追溯到证据，且**不能把猜测
说成已验证**。这个约束落在两个枚举上。

## `EvidenceStatus`

```text
                 ┌──────────────┐
                 │  Suspected   │  静态结构可疑，没有执行证据
                 └──────┬───────┘
                        │ 运行工具并复现
              ┌─────────┴──────────┐
              ▼                    ▼
    ┌─────────────────┐   ┌──────────────────┐
    │ StaticConfirmed │   │   Reproduced     │  有可复现的反例
    └─────────────────┘   └──────────────────┘
              │                    │
              │ 运行后未复现        │ 复现失败
              ▼                    ▼
       ┌──────────────┐     ┌──────────────┐
       │NotReproduced │     │ Inconclusive │
       └──────────────┘     └──────────────┘
                                  │ 人工确认不是问题
                                  ▼
                          ┌──────────────┐
                          │ FalsePositive│
                          └──────────────┘
```

两种路径产出的状态不同：

| 状态 | 何时产生 |
|---|---|
| `Suspected` | 规则匹配到结构事实，但没有执行任何验证（`inspect`） |
| `StaticConfirmed` | 规则能在语法树上直接指出违反位置，例如某一行确实是 `unwrap()`（`inspect`） |
| `Reproduced` | `verify` 观察到了不变式违反，并已缩减出可复现的反例 |
| `NotReproduced` | `verify` 跑完了全部样本，未观察到违反 |
| `Inconclusive` | 编译失败、工具缺失或超时——没有得到任何结论 |

`StaticConfirmed` **不代表缺陷已被复现**——它只代表「这段代码确实长这样」。
`NotReproduced` **不代表程序正确**——样本有限，措辞不越过证据。

只有 `verify` 能产出 `Reproduced`；`inspect` 永远不能。

`is_evidence_backed()` 与 `is_confirmed_problem()` 是两个独立的谓词。报告与
门禁只把 `is_confirmed_problem()` 为真的发现算作「已确认问题」；一条
`StaticConfirmed` 的发现既是有证据的，也不是已确认的问题。

## `GateStatus` 与退出码

| 状态 | 退出码 | 含义 |
|---|---|---|
| `Passed` | 0 | 检查完整执行，未发现阻断级问题 |
| `Failed` | 1 | 检查完整执行，发现阻断级问题 |
| `InsufficientEvidence` | 2 | 检查未能完整执行，不能据此判断 |
| `Cancelled` | 3 | 被用户或预算取消 |

### 四条不可协商的规则

1. **没有可分析文件时必须返回 `InsufficientEvidence`**，不能返回 `Passed`。
   空目录不是「没有问题的代码」，而是「没有看过代码」。
2. **超出任务预算时必须返回 `InsufficientEvidence`**。跑不完的检查与通过的
   检查是两件事。
3. **没有做完的子命令必须返回 `InsufficientEvidence`**：`simplify` 在候选未经
   回归与行为验证时、`agent` 在预算耗尽或取消时，都不得给出「通过」。
   **没有性质可验证的 `verify` 同样返回 `InsufficientEvidence`**。绝不为
   「没做检查」伪造退出码 0。
4. **`Cancelled` 不得与 `Failed` 混用**。取消是外部决定，失败是质量结论。

对应的实现在 `pf-core/src/review.rs` 的 `decide_gate()`，测试在
`crates/pf-integration/tests/integration.rs`。

## 四层质量模型

```text
QualityLayers
├── correctness     正确性与安全   PossibleLogicError / UnhandledError / ...
├── engineering     工程规范       NamingViolation / MissingDocumentation / ...
├── maintainability 可维护性与设计 CircularDependency / UselessAbstraction / ...
└── verification    验证可信度     MissingTestCoverage / VerificationIncomplete / ...
```

分层是**独立统计**的：一层的问题不会抵消另一层。测试
`quality_layers_are_populated_independently` 守护「各层计数之和等于发现总数」
这个不变量。

门禁数的是**每条发现**的 `LayerSummary::blocking`，而不是层的类别。一条发现计入
`blocking` 需要同时满足三件事：严重程度不低于 `block_on` 阈值、有证据支持
（`is_evidence_backed()`）、且未被豁免（`is_exempted()`）。四层的 `blocking`
相加即门禁结论。

这条设计让「层」只承担呈现职责，不承担裁决职责：`maintainability` 层的发现
当前都是建议级，因此在默认阈值下不阻断——但阈值是配置，不是层的属性，把
`--block-on` 调到 `low` 时它们同样会阻断。豁免的发现永远不计入
`LayerSummary::blocking`，因此不改变门禁。

## 证据的内容寻址

每条 `EvidenceArtifact` 都带 SHA-256 摘要。存储层用摘要作为主键，因此同一
份证据被多次写入只存一份，且 `verify_evidence()` 能独立复算校验。

这让「证据被篡改」可以被检测到：`crates/pf-store` 的测试
`evidence_is_content_addressed_and_verifiable` 断言篡改后的摘要不通过校验。
