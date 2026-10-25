# 设计说明

本目录记录 ProofForge 的关键设计决策与已知边界。需求规格见仓库根的
[`需求文档.md`](../需求文档.md)；本目录回答的是「为什么这样实现」以及
「现在还不能做什么」。

| 文档 | 内容 |
|---|---|
| [architecture.md](architecture.md) | crate 职责、依赖方向与数据流 |
| [evidence-model.md](evidence-model.md) | 证据状态机、门禁状态与退出码约定 |
| [rule-engine.md](rule-engine.md) | 规则引擎的契约与内置规则的判定边界 |
| [exemptions.md](exemptions.md) | 规则豁免的字段、范围与失效语义（FR-062） |
| [known-limitations.md](known-limitations.md) | 当前实现的已知缺口 |

写代码前请先读对应文档；修改设计时请同步更新这里——这些文档是评审与排障的
第一现场，不是事后补的说明。
