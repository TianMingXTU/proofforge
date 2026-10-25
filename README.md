# ProofForge

**智能软件工程质量审查与验证系统**

ProofForge 独立于代码生成过程，回答一个完整的工程问题：

> 在有限资源下，如何判断一份软件实现是否符合需求、是否存在真实缺陷、
> 是否符合成熟工程规范，以及是否存在可以安全消除的不必要复杂性？

它面向人类工程师与 AI 编程工具（Claude Code、Codex、Cursor 等）生成的代码，
结合程序分析、工程规则、动态测试、反例搜索与可复现证据，对软件做独立质量评估。

完整需求规格见 [`需求文档.md`](需求文档.md)（16 章，FR-001 … FR-070）。
设计决策与已知边界见 [`docs/`](docs/)。

---

## 当前状态

| 阶段 | 范围 | 状态 |
|---|---|---|
| 一 | Rust workspace、Git 读取、Tree-sitter 解析、符号索引、规则引擎、CLI、JSON 报告 | ✅ 已完成 |
| 二 | 调用图 / 依赖图、重复代码、纯转发包装、无效抽象、注释审查、架构规则 | ✅ 已完成 |
| 三 | 测试执行器、沙箱、属性测试、模糊测试、反例保存与缩减 | ✅ 已完成 |
| 四 | 自研 Rust Agent Runtime、验证计划、策略调度、执行预算 | ✅ 已完成 |
| 五 | 简化候选、补丁生成、隔离工作区、行为差分、人工审查 | ✅ 已完成 |
| 六 | 工程规范导入与冲突检测、规则豁免、报告导出、历史趋势、审计追踪 | ⚠️ 部分完成 |

阶段六的范围比第十二章的任务清单窄：清单里的 **Axum API、Web 控制台与
CI/CD 集成没有实现**，已交付的是规范导入与冲突检测、规则豁免、报告导出
（JSON / HTML / SARIF）、历史趋势与审计追踪。理由见下方「尚未实现的需求」。

> **70 条 FR 中有 7 条未实现或只实现一部分**（FR-003、006、026、031、038、
> 040、041），逐条范围见
> [`docs/known-limitations.md`](docs/known-limitations.md) 的「尚未实现的需求」。
> 其中 FR-040 故障注入与 FR-041 并发测试**完全没有代码**——沙箱没有可注入的
> 故障点，也没有检测数据竞争的能力，凭空报「已模拟磁盘故障」是伪造证据。

阶段二已交付：调用图与模块依赖图、循环依赖、纯转发包装、无效抽象
（单一实现 trait / 无分支工厂）、注释质量、命名与文档、重复代码、过度配置、
过度模块拆分与样板代码、未引用依赖（FR-012）、资源管理（FR-010）、
使用者自定义的架构依赖方向（FR-024）、变更影响分析（FR-025）。

阶段三已交付：沙箱隔离、测试执行器、属性测试、反馈驱动的模糊测试、
反例缩减与复现配方。结论现在有了**动态证据**：`verify` 报出的每条违反都附带
一个已经跑过的输入和一条可直接执行的复现命令。同一套沙箱之上还有两个能力：
**变异测试**（FR-037，`mutate` 判断项目自己的测试能不能发现行为变化）与
**蜕变测试**（FR-039，`verify --metamorphic` 验证「输入变换 ↔ 输出关系」）。

阶段四已交付：`pf-agent` 由程序结构与测试反馈动态调整验证任务——决定跑哪些
性质、先跑哪个、什么时候停，并把每个决策的理由写进轨迹。每个任务都由 Runtime
真实执行（现有测试、性质验证、模糊测试都在隔离沙箱里跑），没有可执行输入域的
候选会如实报告「没有得到结论」。它**不读需求文档的自然语言**，也不调用外部模型：
默认的计划器完全由静态结构驱动，因此没有 API 密钥也能完整运行。

阶段五已交付：`pf-simplify` 生成候选、在隔离工作区打补丁、跑回归与行为比较、
比较工程指标，最后由人确认后才写目标仓库。

阶段六已交付：内置规范可渲染（`rules standard`）、项目规范可从 AGENTS.md /
Markdown / YAML / TOML 导入、规则冲突可检测、规则集版本可追溯、豁免可管理
（`docs/exemptions.md`）、报告可导出为 JSON / HTML / SARIF、历史趋势可查询。

> 诚实说明：每条发现都标注了 `EvidenceStatus`，强度与措辞严格对应。
> `inspect` 只能给出 `suspected` 与 `static_confirmed`——后者只表示「这段代码
> 确实长这样」，不表示缺陷已复现；要拿到 `reproduced` 必须跑 `verify` 或
> `agent`。已知的假阳性与漏检范围列在
> [`docs/known-limitations.md`](docs/known-limitations.md)，包括「写在
> `assert_eq!` 宏参数里的调用不可见」这类会影响 `uncalled-public-api` 判断的
> 缺口。

---

## 构建

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

要求 Rust 1.99 或更新版本（`rust-version` 已固定）。

## 使用

```bash
# 工程质量审查
cargo run -p pf-cli -- inspect .

# 审查仓库自带的样本项目
cargo run -p pf-cli -- inspect fixtures/over-engineered
cargo run -p pf-cli -- inspect fixtures/clean

# 输出 JSON 报告
cargo run -p pf-cli -- inspect . --format json

# 输出 SARIF（GitHub 代码扫描可直接消费）
cargo run -p pf-cli -- inspect . --format sarif --output report.sarif

# 输出 HTML（单文件、内联样式、不引用外部资源）
cargo run -p pf-cli -- inspect . --format html --output report.html

# 增量分析：只审查相对某个引用发生变化的文件（FR-005）
# 附带变更影响分析（FR-025）：报告会列出「可能波及哪些文件」
cargo run -p pf-cli -- inspect . --baseline main

# 指定阻断阈值与文件预算
cargo run -p pf-cli -- inspect . --block-on medium --max-files 500

# 导入项目规范（AGENTS.md / Markdown / YAML / TOML）并记录其版本
cargo run -p pf-cli -- inspect . --standard AGENTS.md

# 用配置文件跑一次审查：导入规范 + 应用豁免，一步到位
cargo run -p pf-cli -- inspect . --config configs/proofforge.toml

# 应用豁免：压制门禁，但不删除发现（FR-062）
cargo run -p pf-cli -- inspect . --exemption exemptions.toml

# 把运行写入本地历史
cargo run -p pf-cli -- inspect . --store .proofforge/history.db
cargo run -p pf-cli -- report --latest

# 看质量随时间的变化（FR-069）
cargo run -p pf-cli -- report --trend --store .proofforge/history.db --project .
cargo run -p pf-cli -- report --rulesets --store .proofforge/history.db --project .

# 行为验证（阶段三）：跑一份显式的性质规格文件
cargo run -p pf-cli -- verify \
    --target fixtures/verifiable \
    --properties fixtures/verifiable/properties.json

# JSON 输出（含反例、缩减规模与复现命令）
cargo run -p pf-cli -- verify --target fixtures/verifiable \
    --properties fixtures/verifiable/properties.json --format json

# 蜕变测试（FR-039）：验证「输入变换 ↔ 输出关系」，不需要现成的测试套件
cargo run -p pf-cli -- verify --target fixtures/verifiable \
    --metamorphic fixtures/verifiable/relations.json

# 变异测试（FR-037）：项目自己的测试能不能发现一次行为变化
cargo run -p pf-cli -- mutate --target fixtures/mutable
cargo run -p pf-cli -- mutate --target . --max-mutants 40 --format json

# 自主验证循环（阶段四）：由程序结构与测试反馈调整验证任务
cargo run -p pf-cli -- agent --target . --include-existing-tests
cargo run -p pf-cli -- agent --target fixtures/verifiable \
    --properties fixtures/verifiable/properties.json --fuzz

# 简化分析（阶段五）：默认只读，看过报告后用 --yes 才写目标仓库
cargo run -p pf-cli -- simplify --target .
cargo run -p pf-cli -- simplify --target . --yes

# 工程规范与豁免管理（阶段六）
cargo run -p pf-cli -- rules standard
cargo run -p pf-cli -- rules import AGENTS.md
cargo run -p pf-cli -- rules conflicts AGENTS.md
cargo run -p pf-cli -- rules exemptions exemptions.toml
```

> 在本仓库上跑 `inspect .`（不带 `--config`）会得到退出码 1：`fixtures/` 里
> 故意植入的样本问题被如实报告并参与门禁。带上 `--config configs/proofforge.toml`
> 才会加载 `exemptions.toml` 把样本豁免掉。原因见
> [`docs/known-limitations.md`](docs/known-limitations.md) 的「没有用户可配置的
> 路径排除清单」。

退出码遵循需求文档 14.3 的约定，四种状态互不相同：

| 退出码 | 含义 |
|---|---|
| 0 | 通过 —— 检查完整执行，未发现阻断级问题 |
| 1 | 未通过 —— 检查完整执行，发现阻断级问题 |
| 2 | 证据不足 —— 检查未能完整执行，不能据此判断 |
| 3 | 已取消 |

`inspect` 的门禁只看**已被证据支持且未被豁免**的阻断级发现：`suspected` 的
发现不会让门禁失败，被豁免的发现也不会（但它仍然出现在报告里）。工具缺失、
解析全部失败时门禁返回证据不足，不会伪装成通过。

`proofforge init` 生成一份配置模板，`inspect --config <FILE>` 读它。优先级是
**命令行 > 配置文件 > 内置默认值**，只有显式给出的参数才覆盖配置——所以
`--block-on` 不传时用的是配置里的值。配置文件里的相对路径按该文件所在目录解析
（放在 `configs/` 下也能指向仓库根）；`rule_files` 与 `exemption_files` 例外，
它们按目标根目录解析。`verify` / `simplify` / `agent` 不读配置文件，设置都通过
各自的参数给出。

---

## 它检查什么

内置规则集中在 MVP 的第一个场景（需求文档 14.1 场景一）：

| 规则 | 对应需求 | 说明 |
|---|---|---|
| `public-api-missing-docs` | FR-014 | 公共 API 缺少文档注释或 docstring |
| `naming-convention` | FR-007 | 命名不符合语言惯例 |
| `no-unwrap-in-library` | FR-009 | 库代码中裸 `unwrap` / `expect` |
| `no-empty-error-handler` | FR-009 | 空的异常处理块吞没错误 |
| `function-too-long` | FR-008 | 函数超过 80 行 |
| `too-many-parameters` | FR-008 | 参数超过 5 个 |
| `redundant-comment` | FR-013 | 注释只是复述代码文本 |
| `duplicate-code` | FR-011 | 逐字重复或形状一致但不改名 |
| `single-implementation-trait` | FR-015 | 只有一种实现的 trait（需人工确认） |
| `forwarding-wrapper` | FR-016 | 纯转发包装 |
| `needless-factory` | FR-017 | 没有选择逻辑的工厂函数 |
| `over-configuration` | FR-018 | 签名被 `bool` / `Option` 开关参数主导 |
| `over-modularization` | FR-019 | 内容很少却横跨多个文件的模块 |
| `boilerplate-code` | FR-020 | 空占位实现与逐字复制的分支 |
| `circular-dependency` | FR-023 | 模块循环依赖 |
| `uncalled-public-api` | FR-021 | 项目内无调用者的公共 API |
| `unused-dependency` | FR-012 | 清单声明却无源码引用的依赖（需人工确认） |
| `resource-management` | FR-010 | 跳过析构或在库代码里终止进程 |
| `architecture-rule` | FR-024 | 使用者定义的依赖方向约束（由配置给出，无配置则不检查） |

### 关于「过度设计」判定的边界

需求文档 7.3 明确要求：不得把单一加权分数直接作为确定性判据。因此
`single-implementation-trait`、`needless-factory` 等规则只输出**结构事实**
（实现数量、是否存在分支、调用者数量），并附带
`needs_human_confirmation=true`，由人结合测试隔离、模块边界与业务变化预期
做最终判断。阶段四的 `pf-agent` 能做的是按结构事实决定**验证顺序**，不能替代
这个判断。

`duplicate-code` 的判据只有两条：函数体的**形状签名**（标识符与字面量归一化
后的节点种类序列）与**词法签名**（保留标识符、归一化字面量）。前者让「改个
变量名」躲不过检测，后者用来区分「复制粘贴」与「照着形状重写」。低于
14 个语法节点或 3 条语句的函数体不参与判定——访问器级别的实现在任何代码库
里都会大量撞形状，报出来只会淹没真正值得合并的逻辑。

`boilerplate-code` 刻意不推断作者。FR-020 明确禁止「判断某段代码是否由 AI
编写」，因此它只报告可以独立论证的低价值结构：函数体只有 `todo!()` /
`unimplemented!()`，或同一个 `match` 里有三个以上分支体逐字相同。

同样的边界也作用于 `forwarding-wrapper`：trait 实现方法、零参数访问器与
测试代码里的薄包装**不会**被报告——它们的转发是契约或语言惯例要求的。判定
依据见 [`docs/rule-engine.md`](docs/rule-engine.md)。

---

## 行为验证怎么用

`verify` 验证的是**程序在具体输入下做了什么**，不是代码长什么样。它接受一份
JSON 性质规格文件：

```json
{
  "language": "rust",
  "imports": ["mycrate"],
  "samples": 24,
  "properties": [
    {
      "name": "解析结果非负",
      "call_template": "mycrate::parse({args})",
      "domain": { "kind": "int", "min": -1000, "max": 1000 },
      "invariant": "output >= 0"
    }
  ]
}
```

- `call_template` 里的 `{args}` 会被替换成实参；元组域按位置展开。
- `invariant` 是一段对 `output` 取值的断言表达式，按**目标语言**书写。
- `domain` 支持 `int` / `bool` / `str` / `tuple` / `list`，样本由 ProofForge
  用固定种子生成后以字面量写进驱动程序——同一种子永远得到同一批输入。

每条性质都会经过「隔离沙箱 → 生成驱动程序 → 编译执行 → 反例缩减」四步。
报告只使用三种措辞，与证据强度严格对应：

| 结论 | 证据状态 | 退出码 |
|---|---|---|
| 未观察到违反 | `not_reproduced` | 0 |
| 已复现违反（附反例与复现命令） | `reproduced` | 1 |
| 证据不足（编译失败 / 工具缺失 / 超时） | `inconclusive` | 2 |

「未观察到违反」**不是**「程序正确」——样本是有限的，措辞不越过证据。
反例会被自动缩减（整数朝零收缩、列表成块删除），缩减过程的每一轮执行记录都
保留在报告里，复现命令可直接复制执行。

诚实的边界：**从自然语言需求自动抽出性质尚未实现**。`pf-agent` 的计划器读的是
程序结构与测试反馈，不是需求散文；因此 `--requirements` 只作为追溯信息记录进
报告，不参与生成性质。把一句需求猜测成一条性质、再把跑出来的结论说成「已验证」，
是这套系统最不能做的事。在此之前，请用 `--properties` 提供显式规格。完整边界见
[`docs/known-limitations.md`](docs/known-limitations.md)。

### 没有测试套件也能验证：蜕变测试（FR-039）

性质测试需要一个**预期**（oracle），很多场景下它并不存在——你不知道一次重构
后的输出「应该」是多少，但你知道它和原来是什么关系。蜕变测试把「预期」换成
「关系」：

```json
{
  "language": "rust",
  "imports": ["fixture"],
  "samples": 16,
  "relations": [
    {
      "name": "加法满足交换律",
      "call_template": "fixture::add({args})",
      "domain": { "kind": "tuple", "fields": [
          { "kind": "int", "min": -50, "max": 50 },
          { "kind": "int", "min": -50, "max": 50 } ] },
      "transform": "(input.1, input.0)",
      "relation": "output_a == output_b"
    }
  ]
}
```

`transform` 是输入变换（必须包含 `{input}`），`relation` 是输出关系（必须同时
提到 `output_a` 与 `output_b`）。两条都是**人写**的：工具负责把它们跑成可复现
证据，不负责推导它们。因此「未观察到违反」的含义是「人写的这条关系在这批输入上
成立」，不是「这个函数满足它的规格」。

### 测试套件够不够用：变异测试（FR-037）

`mutate` 回答一个不同的问题：**项目自己的测试能不能发现一次行为变化**。它对
被测代码做小的词法改动（运算符互换、布尔取反、字面量加一），逐个在隔离沙箱里
跑项目的测试：

| 结论 | 含义 |
|---|---|
| 已杀死 | 有测试失败了——测试确实在守这条行为 |
| 幸存 | 全部测试通过——**改动没人发现** |
| 无效 | 变异体编译不过，不计入通过率的分母 |
| 无结论 | 测试没能完整执行（超时、工具缺失） |

```bash
proofforge mutate --target fixtures/mutable
proofforge mutate --target . --max-mutants 40 --format json
```

通过率低**不等于代码有 bug**：它只说明改动没人发现。反过来，把测试文件里的
变异体幸存算成覆盖缺口是错的，报告会分开统计两者——测试文件里的幸存往往只说明
那条测试自身对改动不敏感。**基线测试本来就失败时，整次运行不给结论**（退出码 2）：
在红灯上算通过率，数字没有意义。

编译不过的变异体不计入分母：`cargo test` 在编译失败与测试失败时同样非零退出，
因此测试一失败就再问一次编译器来区分两者——把编译不过算成「已杀死」会凭空抬高
通过率。Python 项目没有构建步骤可问，因此它们产不出「无效」这一类，这是已知偏差。

---

## 规范与豁免怎么用

`inspect` 不只跑内置规则。把团队的规范文件喂给它，它会把里面可自动化的条目
变成规则、把不可自动化的条目标记为「需人工检查」，并记录这次审查用的是哪个
版本：

```bash
proofforge rules standard                           # 内置规范渲染成 Markdown
proofforge inspect . --standard AGENTS.md           # 导入并参与本次审查
proofforge inspect . --config configs/proofforge.toml   # 规范 + 豁免一次到位
proofforge rules conflicts AGENTS.md                # 两份规范是否互相矛盾
```

本仓库自己就是这么用它的：`configs/proofforge.toml` 导入 `AGENTS.md` 作为项目
规范，仓库根的 `exemptions.toml` 豁免 `fixtures/` 里植入的测试样本，CI 的
`self-review` 任务跑的就是这条命令（见 [`fixtures/README.md`](fixtures/README.md)）。

导入的 Markdown 规范**只会**以 `RuleCheck::Manual` 生效——它不会把一句自然语言
猜成一条可执行检查。冲突检测里的「同主题相反模态」是启发式，报告会自己写明
判断依据是措辞（详见 [`docs/rule-engine.md`](docs/rule-engine.md)）。

豁免让「这一处我们决定先不修」变成一个有理由、有范围、有批准人、可过期的决定，
而不是把发现藏起来：

```toml
[[exemptions]]
rule_id = "builtin.single-implementation-trait"
scope = { kind = "lines", path = "src/store.rs", start_line = 40, end_line = 96 }
reason = "这一层是测试替身，真实实现里会有第二个 impl；见 issue #1234"
granted_by = "bob"
```

豁免**不删除发现**：它仍然出现在报告、JSON、SARIF 与 HTML 里，只是不再计入门禁。
理由与批准人是必填项——说不清理由的豁免只是把问题藏起来。完整设计见
[`docs/exemptions.md`](docs/exemptions.md)，示例见
[`configs/exemptions.example.toml`](configs/exemptions.example.toml)。

---

## 简化怎么用

`simplify` 的默认姿态是**只读**：它生成候选、在隔离工作区打补丁、跑回归与
行为比较、比较工程指标，然后把报告交给人。只有显式传 `--yes` 才写目标仓库：

```bash
proofforge simplify --target .            # 只读：出报告，不动代码
proofforge simplify --target . --yes      # 看过报告后才应用
```

未通过回归或行为验证的候选即使有 `--yes` 也不会被应用。基线本就失败、工具链
缺失、或没给 `--properties` 时，行为比较只能说明覆盖范围有缺口，门禁返回证据
不足（退出码 2）——「没验证完」不是「通过」。

工程指标（FR-054）在**基线度量过的同一组文件**上比较：沙箱里还有回归与行为
比较生成的驱动程序，不固定文件集的话，一次删除会被报告成「代码规模 +31 行」。

---

## 设计原则

来自需求文档第一章，对代码与产品同时生效：

- **正确性优先** —— 任何简化都不能以破坏明确行为为代价。
- **证据优先** —— 推测与已验证结果必须区分。
- **简单性优先，但不追求机械减少代码**。
- **尊重工程上下文** —— 抽象是否合理要结合项目规模判断。
- **人类工程师拥有最终控制权** —— 默认不修改原始工程代码。
- **可审计与可复现** —— 结论可追溯到代码版本、规则集与执行日志。

## 贡献

请先阅读 [`AGENTS.md`](AGENTS.md)：其中规定了编码与注释规范、测试要求、
安全红线与提交前自查清单。对 AI 编程助手而言该文件是强制性的。

改设计前请读 [`docs/`](docs/)：其中记录了 crate 边界（`architecture.md`）、
证据状态机（`evidence-model.md`）、规则契约（`rule-engine.md`）、豁免设计
（`exemptions.md`）与当前实现的已知缺口（`known-limitations.md`）。文档与代码
不一致时，以代码为准并**立即修正文档**。
