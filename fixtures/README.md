# 测试夹具

四个样本项目，供集成测试与手动验证使用。它们是**普通文件**，可以被
`proofforge` 直接跑，也可以当普通代码阅读：

```bash
cargo run -p pf-cli -- inspect fixtures/over-engineered
cargo run -p pf-cli -- inspect fixtures/clean

# 行为验证：一条干净性质 + 一条已知缺陷
cargo run -p pf-cli -- verify \
    --target fixtures/verifiable \
    --properties fixtures/verifiable/properties.json

# 蜕变测试：两条干净关系 + 一条踩到已知缺陷的关系
cargo run -p pf-cli -- verify \
    --target fixtures/verifiable \
    --metamorphic fixtures/verifiable/relations.json

# 变异测试：有测试的函数与没有测试的函数各一半
cargo run -p pf-cli -- mutate --target fixtures/mutable --max-mutants 40
```

| 目录 | 用途 |
|---|---|
| [`over-engineered/`](over-engineered/) | 植入已知问题，覆盖需求文档 14.3 的「识别人工构造的过度设计案例」 |
| [`clean/`](clean/) | 没有已知问题，用于验证「干净项目应当通过门禁」 |
| [`verifiable/`](verifiable/) | 植入一条**可复现**的缺陷，供 `verify` 手动验证 |
| [`mutable/`](mutable/) | 一半函数有测试、一半没有，供 `mutate` 测量测试套件 |

四个目录都带 `[workspace]` 空表，因此它们**不参与** ProofForge 的 workspace
构建——夹具里的编译警告与 lint 结果是预期内容，不应影响 `cargo test
--workspace`。

## `over-engineered` 植入的问题

| 问题 | 规则 | 位置 |
|---|---|---|
| 公共 API 缺少文档 | `public-api-missing-docs`（FR-014） | `src/lib.rs` |
| 命名不符合惯例 | `naming-convention`（FR-007） | `src/lib.rs` |
| 只有一种实现的 trait | `single-implementation-trait`（FR-015） | `src/lib.rs`、`src/boilerplate.rs` |
| 纯转发包装 | `forwarding-wrapper`（FR-016） | `src/lib.rs` |
| 没有选择逻辑的工厂 | `needless-factory`（FR-017） | `src/lib.rs` |
| 签名被开关参数主导 | `over-configuration`（FR-018） | `src/config.rs` |
| 内容很少却横跨多个文件 | `over-modularization`（FR-019） | `src/thin.rs` |
| 空占位实现 / 逐字复制的分支 | `boilerplate-code`（FR-020） | `src/boilerplate.rs` |
| 逐字重复的实现 | `duplicate-code`（FR-011） | `src/config.rs` |
| 参数过多 | `too-many-parameters`（FR-008） | `src/lib.rs` |
| 复述代码的注释 | `redundant-comment`（FR-013） | `src/lib.rs` |
| 模块循环依赖 | `circular-dependency`（FR-023） | `src/alpha.rs` ↔ `src/beta.rs` |

`boilerplate-code` 的两条植入都只描述**代码质量**：占位函数不承载行为、复制
出来的分支没有区分情况。规则不会、也不应推断代码由谁编写（FR-020 的明确约束）。

## `verifiable` 植入的缺陷

`over-engineered` 植入的是**静态可见**的结构问题。`verify` 对它们无话可说——
它回答「程序在某个输入下做了什么」，不是「代码长什么样」。因此行为验证需要
另一类夹具：

| 函数 | 性质 | 预期结论 |
|---|---|---|
| `double` | 任意输入翻倍后仍是偶数 | 未观察到违反（干净路径） |
| `parse` | `output < 100usize` | 已复现违反（`value <= 0` 时 `try_into()` panic） |

`parse` 的缺陷是**故意保留**的：修好它会让这条回归失去守护能力。它的反例会从
`Int(-20)`（规模 20）被缩减到 `Int(-1)`（规模 1）。

缩减**不会**停在 `0` 上：`InputValue::size()` 规定每个在场的元素至少占 1
（否则删除零值元素不改变规模，缩减就没有动力把它删掉），于是 `-1` 与 `0` 同等
大小，先被问到的那个赢。两者都触发同一条缺陷，报告中的规模 `20 → 1` 才是
「压得多小」的可信度量。

### `verifiable` 的蜕变关系

`relations.json` 覆盖蜕变测试的三条结论路径，与 `properties.json` 的分工一致：

| 关系 | 域 | 预期结论 |
|---|---|---|
| 翻倍与输入符号无关 | 标量 | 未观察到违反 |
| 加法满足交换律 | 元组 | 未观察到违反 |
| `parse` 的平移量恒为 1 | 标量（含非正值） | 已复现违反（`value <= 0` 时 panic） |

元组域那条同时守护一个容易写错的契约：变换填的是 `{args}` 槽位，要展开成
**逗号分隔的实参列表**（`{input}.1, {input}.0`），写成字面元组会让第二次调用
只收到一个参数，编译失败后整条关系只剩「证据不足」——看不出是规格写错了。
`add` 这个双参数函数的存在理由就是这个形状匹配。

## `mutable` 植入的覆盖缺口

变异测试测量的是**测试套件**，因此夹具要一半有测试、一半没有：

| 函数 | 测试 | 变异体的预期命运 |
|---|---|---|
| `add` | `tests/behavior.rs` | 被杀死 |
| `is_even` | 同上 | 被杀死 |
| `clamp` | 同上 | 被杀死 |
| `unchecked_discount` | **无** | 幸存——没有测试能发现它的行为变化 |
| `odd_offset` | **无** | 幸存 |

`tests/behavior.rs` 自己也会产生变异体（断言里的字面量）。它们幸存说明的是
**断言强度**：测试选的输入落在不变式成立的区间里，改动因此被掩盖。报告把测试
文件与生产代码里的幸存分开统计——把前者读成「生产代码没被覆盖」是误读。

`--max-mutants` 的默认值是 12，手动验证时给大一点（例如 40）才能看到全部
幸存条目。

跑这个夹具会看到少数 `invalid` 条目：`assert_eq!(add(-1, 1), 0)` 里的 `-` 被
换成 `+` 得到 `+1`，而 Rust 不接受前导正号。它们**不计入通过率的分母**——
编译不过的变异体没有发生过任何比较，算成已杀死会凭空抬高通过率。

## 修改夹具前

`crates/pf-integration/tests/integration.rs` 与 `crates/pf-cli/tests/cli.rs`
都对这些目录的内容做了断言。改夹具请先跑：

```bash
cargo test -p pf-integration
cargo test -p pf-cli --test cli
cargo test -p pf-verify
```

新增植入问题时，请在两个测试里都补上对应断言——夹具的价值就在于它守护的
行为是明确的。

## `fixtures/` 会出现在自我审查报告里

CI 的 `self-review` 任务会跑 `proofforge inspect . --config configs/proofforge.toml`，
因此 `over-engineered` 里植入的问题会出现在报告与 SARIF 产物中。这是预期的：
它们不是回归，而是被守护的行为样本。

植入的 `public-api-missing-docs` 是 `StaticConfirmed` + High，**会**阻断门禁，
因此仓库根的 `exemptions.toml` 按文件范围把它们豁免掉，理由与批准人写在文件里。
豁免不删除发现——它们仍在报告里，只是不计入门禁，所以 CI 额外断言一条：
**被豁免的发现必须全部位于 `fixtures/` 下**。真实代码里出现被豁免的发现说明有人在
压问题，那必须让 CI 红掉。

改动植入内容时请同步更新 `exemptions.toml`：新增一个带阻断级发现的文件，就要有
一条对应的豁免，否则 `self-review` 会失败。新增 Advisory 级的植入不需要动它。
