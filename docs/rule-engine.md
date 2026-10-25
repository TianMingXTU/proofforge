# 规则引擎

## 契约

每条规则都是一个实现了 `Checker` 的单元：

```rust
pub trait Checker: Send + Sync {
    fn rule(&self) -> Rule;
    fn check(&self, context: &Context<'_>) -> Vec<Finding>;
}
```

三条不可协商的约定：

1. **`check` 是纯函数**。不读环境变量、不读当前时间、不产生随机数。相同的
   `Context` 必须产出完全相同的 `Finding` 列表（包括顺序）。
2. **输出结构事实，不输出裁决**。规则可以说「这个 trait 只有一种实现」，
   不可以说「这个 trait 应该被删除」。语义判断留给人类（AGENTS.md 设计原则）。
3. **不修改用户代码**。规则只读语法树与符号索引（需求文档 1.3）。

### `EvidenceStatus`：哪条规则可以标 `StaticConfirmed`

`StaticConfirmed` 的判据只有一条：**规则能在语法树上直接指出违反位置**——
那一行确实是 `unwrap()`、那个标识符确实不符合惯例、那个函数确实有 120 行。
这类事实可复算，因此是有证据的；但它不代表缺陷已被复现，所以不是
`Reproduced`（`docs/evidence-model.md`）。

需要结合业务上下文才能结论的规则必须留在 `Suspected`：单一实现的 trait 是否
多余、两个函数是否算重复、公共 API 是否真的没人用，都不取决于代码长什么样。
把猜测标成已验证是 AGENTS.md 安全红线 5 禁止的事。

`pf-rules` 用 `super::confirmed(finding)` 统一标记，新增规则时按上面这条判据
决定要不要包一层。

### 规则集与门禁的关系

门禁只数「未被豁免 + 有证据支持 + 严重程度达到 `block_on`」的发现
（`LayerSummary::blocking`）。因此：

- 标了 `StaticConfirmed` 的强制性规则（默认严重程度 High）在默认 `block_on=high`
  下会阻断合并；
- Advisory 级规则（默认 Low）要把 `block_on` 降到 `low` 或 `info` 才阻断；
- 已豁免的发现不阻断，但仍在报告里（见 `docs/exemptions.md`）。

### 为什么 `Finding` 的 ID 是内容派生的

`FindingId::derive()` 用「规则 ID + 类型 + 标题 + 位置」拼出 SHA-256 前
128 位。这让同一个问题在两次独立运行中得到同一个 ID，报告因此可以 diff、
可以归档、可以在 CI 里做「只报告新增问题」。

代价是：修改标题文案会改变所有相关发现的 ID。改文案时请意识到这是一次
破坏性变更。

## 测试代码不是库代码

规则必须区分测试代码与库代码，否则报告会被噪音淹没：

| 判定 | 覆盖范围 |
|---|---|
| `is_test_path()` | `tests/`、`benches/`、`*_test.rs`、`test_utils.rs` |
| `is_rust_test_code()` | 源码内的 `#[cfg(test)] mod tests`、`#[test]` 函数 |

`is_rust_test_code()` 逐 token 判断属性，而不是整段 `contains("test")`：
后者会把 `#[cfg(feature = "latest")]` 误判成测试代码，从而漏报真实问题。
回归测试是 `cfg_feature_named_like_test_is_not_test_code`。

这条区分有规范依据：AGENTS.md 4.3 只禁止**库代码**使用 `unwrap()` /
`expect()`，测试代码里这是惯用写法。

## 各规则的判定边界

### `forwarding-wrapper`（FR-016）

目标是需求文档 3.4.1 的「无业务处理的代理」。**不**报告以下三类，因为它们
的转发是契约或语言惯例要求的：

| 不报告 | 理由 |
|---|---|
| trait 实现方法 | `Display::fmt` 只能调用 `f.write_str(...)`，转发是 trait 契约 |
| 零参数访问器 | `len()` / `is_empty()` / `as_str()` 返回派生值，是 Rust 暴露不变量的方式 |
| 测试代码 | 薄包装是惯用的测试助手 |

判据是「除 `self` 之外还有被原样转发的参数」。`RuleSet::push` 这种一行转发
**会**被报告——它确实是代理层，是否保留由人决定。

### `single-implementation-trait`（FR-015）

只输出结构事实（实现数量），并附带 `needs_human_confirmation=true`。需求文档
7.3 明确禁止把单一加权分数当作确定性判据：一个只有一种实现的 trait 可能是
为了测试隔离、为了未来的第二种实现、为了划分模块边界而存在的。

### `uncalled-public-api`（FR-021）

报告「项目内没有任何调用者」的公共 API。注意它的措辞是**项目内**——对一个
库来说，公共 API 没有内部调用者是正常状态，这条规则因此是 Advisory 级，
只提示「需要确认是入口点还是死代码」。

测试代码里的调用算作调用者：至少有一个消费者，就说明这个 API 被使用过。

### `duplicate-code`（FR-011）

两级归一化签名，缺一不可：

| 签名 | 归一化 | 用途 |
|---|---|---|
| 形状签名 | 标识符、字面量、字段名一律替换为 `_`，只留节点种类序列 | 分组：改个变量名躲不过检测 |
| 词法签名 | 保留标识符文本，字面量替换为 `_` | 区分「复制粘贴」与「照着形状重写」 |

组内词法签名全部相同 → `duplication=exact`；否则 → `structural` 并要求人工
确认。函数名**不**参与词法签名：复制一个函数时最自然会改的就是名字，把改名
判成「只是相似」会让真正的复制粘贴逃过 `exact` 归类。

阈值：函数体少于 14 个语法节点或 3 条语句不参与判定。访问器级别的实现在任何
代码库里都会大量撞形状，报出来只会淹没真正值得合并的逻辑。

### `over-configuration`（FR-018）

两个可静态论证的信号：

1. **签名被开关参数主导**：参数 ≥ 4 且其中 ≥ 3 个是 `bool` / `Option<_>`。
2. **假的可选参数**：`Option` 形参在函数体内被**无条件** `unwrap_or(...)` 兜底。

第二个信号刻意跳过 `if` / `match` / `while` / `for` 体内的兜底：分支里的兜底
是真实的配置行为，调用方的选择确实改变了结果。形参文本按尖括号、圆括号、
方括号的深度切分——`HashMap<String, Vec<u8>>` 与 `impl Fn(u32, u32) -> u32`
里的逗号不是参数分隔符。

只支持 Rust：两个信号都依赖形参列表的文本形状与方法调用解析，其他语言上没有
同等可靠的判据，宁可不报。

### `over-modularization`（FR-019）

判据是「项目内依赖数 ≥ 3 且本文件定义的顶层符号 ≤ 2」。依赖数来自模块图
（只数能解析到项目内文件的 `use`），符号数来自符号索引（只数 `parent` 为空的
顶层定义——方法归属于 impl 块，不算文件内容）。

入口文件（`lib.rs` / `main.rs` / `mod.rs`）不参与判定：按惯例它们承担重新导出
与子模块声明，符号少而依赖多是本职。

### `boilerplate-code`（FR-020）

需求文档 FR-020 明确禁止「判断某段代码是否由 AI 编写」，因此这条规则只报告
可以独立论证的低价值结构，并在说明里显式否认作者推断：

| 信号 | 判据 |
|---|---|
| 空占位函数 | 函数体**只有**一条 `todo!()` / `unimplemented!()` / `unreachable!()` |
| 逐字复制的分支 | 同一个 `match` 里 ≥ 3 个分支体在去掉注释与首尾空白后完全相同 |

「像 AI 写的」不是结论，「这段代码当前不承载信息」才是。混合了其他逻辑的
函数即便包含 `todo!()` 也不报告。

### `unused-dependency`（FR-012）

把 `Cargo.toml` 里声明的依赖与源码里实际出现的路径根对一下，差值是候选。
判据有两条，命中任意一条即算「已引用」：它作为路径根出现在某条 import 引用里，
或它的名字在源码里以路径根的形式出现（标识符后紧跟 `::` 或 `.`）。

第二条不是冗余：Rust 允许完全限定调用而不写 `use`，只看 import 会把用着的
依赖报成没人用。相反方向的已知漏报（`include!`、build script 生成的代码）是
安全的——漏报好过误报。

清单解析是词法的，两处必须特殊处理：

- `[workspace.dependencies]` **不算**本包依赖。那是一份供成员 crate 用
  `.workspace = true` 继承的版本目录，把它算进来会在每个 Cargo workspace 的
  根清单上产出成片假阳性。
- `name.workspace = true` 要认出依赖名是 `name`。不认它会让 workspace 里每个
  成员 crate 的依赖都从清单上消失，规则随之失去意义。

每条发现都是 `Suspected` + `needs_human_confirmation`：**「没被 import」不等于
「不必要」**，宏、重新导出、条件编译分支都让依赖在源码里不出现。

### `resource-management`（FR-010）

只覆盖四个可以在语法树上独立论证的信号，证据强度分两档：

| 信号 | 证据状态 | 理由 |
|---|---|---|
| `mem::forget` / `Box::leak` | `StaticConfirmed` | 那一行确实跳过了析构 |
| 库代码里的 `process::exit` | `StaticConfirmed` | 同上 |
| `Box::into_raw` 而无同文件 `Box::from_raw` | `Suspected` | 是否泄漏取决于调用方，而调用方在另一个文件里 |

**锁使用错误与生命周期风险没有覆盖**：前者需要知道哪个 guard 在哪个分支里被
持有，后者要跨函数跟踪引用的存活期。用文本匹配猜这两类问题会大量假阳性。

### `architecture-rule`（FR-024）

规则来自 `proofforge.toml` 的 `[[architecture_rules]]`，不是内置集——工具不
替项目预设分层。每条规则一个检查器实例，因此名称与理由能单独出现在证据里；
**规则为空时该检查器根本不进入规则集元数据**，一份写着「检查了架构规则」却
没有规则的规则集是误导。

模式是路径通配符，因为模块依赖图按文件建边：

- `**` 跨目录，`crates/pf-report/**` 覆盖 `crates/pf-report/src/lib.rs`；
- `*` 不跨目录，`src/*.rs` 不覆盖 `src/a/b.rs`。

`mode = "forbidden"` 报告落在模式内的边；`mode = "allowed"` 报告**不在**白名单
里的边。自环永远不适用：文件依赖自己不是架构问题。

边的存在性由 `pf-graph` 的解析能力决定，不由规则决定——它只解析 `crate::`
前缀的内部引用，因此规则要写到文件粒度才生效。

## 规范管理与豁免

阶段六把「规则从哪来、听谁的、能不能压掉」做成了显式能力，接口都在
`proofforge rules` 下，全部只读：

| 命令 | 对应 FR | 作用 |
|---|---|---|
| `rules standard` | FR-057 | 把内置规范渲染成 Markdown |
| `rules import <FILE>` | FR-058 | 导入 AGENTS.md / CLAUDE.md / Markdown / YAML / TOML |
| `rules conflicts [FILE...]` | FR-060 | 检测相互矛盾的规范 |
| `rules exemptions <FILE>` | FR-062 | 列出豁免及其生效状态 |

Markdown 导入的规范**只能人工审查**：它写的是意图，不是可执行查询。把它猜成
一条 `StructuralQuery` 再报告「已检查并通过」，是在伪造证据。因此这类规则的
`RuleCheck` 是 `Manual`，默认不参与自动执行，只在报告里作为人工审查项出现。

冲突检测的四类判据里，前两类（重复 ID、同 ID 不同定义）是确定性的，后两类
（同主题相反模态、强制与建议并存）是启发式，报告里必须自证是启发式。

`inspect --config <FILE>` 把 `rule_files` 与 `exemption_files` 一起读进来，因此
「导入哪些规范、豁免哪些发现」可以一次性在配置文件里说清，而不是靠一长串命令行
参数。ProofForge 自己的 `configs/proofforge.toml` + `exemptions.toml` 就是这个
用法的样本。

## 新增规则

1. 在 `pf-rules/src/<职责>.rs` 实现 `Checker`（模块按职责拆，参考 `style.rs`、
   `architecture.rs`、`duplication.rs`、`over_engineering.rs`、`semantic.rs`）。
2. 在 `builtin_checkers()` 注册。
3. 规则 ID 用稳定可读的 kebab-case：`RuleId::builtin("kebab-case-name")`。
   ID 会出现在报告、豁免配置与审计记录里，不得随机生成（AGENTS.md 第 6 节）。
4. 在 `README.md` 的规则表补一行，注明对应 FR 编号。
5. 补测试：正常路径至少一条，每个 `match` 分支都要覆盖（AGENTS.md 第 5 节）。
   测试用的上下文构造器在 `pf-rules/src/lib.rs` 的 `#[cfg(test)] mod testing`。
6. 如果规则涉及「是否过度设计」，在 `detail` 里写清它观察到的结构事实，
   并加上 `needs_human_confirmation`。
7. 如果给 `fixtures/over-engineered` 植入了样本，在 `fixtures/README.md` 的表格
   补一行，并在 `pf-integration` 与 `pf-cli` 的夹具断言里加上对应规则 ID。
