# 规则豁免（FR-062）

豁免让「具有权限的用户为特定问题提供有理由、有范围的豁免」成为可能，同时不
让「我们到底压掉了多少问题」变得不可审计。

## 一条豁免必须带什么

```toml
[[exemptions]]
rule_id = "builtin.single-implementation-trait"
scope = { kind = "lines", path = "src/store.rs", start_line = 40, end_line = 96 }
reason = "这一层是测试替身，真实实现里会有第二个 impl；见 issue #1234"
granted_by = "bob"
expires_on = "2027-01-01"
```

| 字段 | 约束 | 为什么 |
|---|---|---|
| `rule_id` | 必填 | 豁免按规则 ID 索引；ID 必须稳定（AGENTS.md 第 6 节） |
| `scope` | 必填 | 范围越小越安全：`rule` > `file` > `lines` |
| `reason` | 必填，不可为空白 | 说不清理由的豁免只是把发现藏起来，解析时直接报错 |
| `granted_by` | 必填，不可为空白 | 豁免必须可归属到人（FR-070 审计追踪） |
| `expires_on` | 可选 | 省略表示长期有效；过期豁免自动失效并在报告里说明 |

YAML 写法等价（`configs/exemptions.example.toml` 里有完整示例）。

## 三种范围

| `kind` | 字段 | 覆盖 |
|---|---|---|
| `rule` | — | 该规则在所有位置的命中 |
| `file` | `path` | 该文件内的命中 |
| `lines` | `path`、`start_line`、`end_line` | 该文件该行范围内的命中 |

没有位置的发现只能用 `rule` 级豁免覆盖——给它编一个位置等于伪造证据。

## 豁免做什么、不做什么

**做**：把发现标记为「已豁免」，在报告里附上理由、批准人与范围；让它不再计入
`LayerSummary::blocking`，因此不再阻断门禁。

**不做**：

- 不删除发现。它仍在 `findings` 列表里，JSON 里是 `"exempted": true`，
  SARIF 里是带 `suppressions` 的结果，HTML 里是多出来的一段说明。
- 不修改标题、位置、严重程度或证据状态。
- 不让「没有位置」的发现被文件级或行级豁免覆盖。

## 过期与失效

`is_active_on(reference_date)` 按 ISO 日期字符串比较（`YYYY-MM-DD` 字典序即
时间序）。参考日期由调用方传入而不是在库里读时钟，因此「同一份代码 + 同一个
日期 ⇒ 同一份报告」（FR-061）。CLI 用 `--reference-date` 覆盖，缺省取今天。

过期豁免会发生两件事：

1. 它覆盖的发现重新出现在报告里，并重新参与门禁判定；
2. 报告 warnings 里多一条「该豁免已于 X 过期」，让人知道有条豁免悄悄失效了。

## 加载失败不会让审查失败

豁免文件读不到、格式不支持、或某条豁免缺字段时，审查**照样跑完**，只是在
warnings 里说明「本该生效的豁免没有生效」。一份写错的豁免文件不应该让整个仓库
失去审查结果；但静默忽略它同样不行——那会让使用者以为豁免还在生效
（AGENTS.md 安全红线 4）。

## 用法

```bash
# 命令行
proofforge inspect . --exemption exemptions.toml --reference-date 2026-10-25

# 或写进配置文件（相对路径按被审查项目的根目录解析）
exemption_files = ["exemptions.toml"]

# 用配置文件跑一次审查；显式给出的 CLI 参数优先于配置文件
proofforge inspect . --config configs/proofforge.toml

# 查看一个豁免文件的状态（--reference-date 缺省取今天）
proofforge rules exemptions exemptions.toml
```

`--config` 目前只对 `inspect` 生效；`verify` / `simplify` / `agent` 的设置都通过
各自的参数给出。配置文件里的相对路径按**该配置文件所在目录**解析，因此配置放在
`configs/` 下也能指向仓库根；`rule_files` 与 `exemption_files` 例外，它们按目标
根目录解析，因为规范与豁免是跟着项目走的决策。

`report --trend` 按项目过滤，缺省取当前工作目录；查别处的历史用 `--project`。

## 存储

`pf-store` 的 `runs` 表记 `exempted_count`，`findings` 表记 `exempted`。
趋势查询（`report --trend`）因此能同时显示发现数与豁免数——「发现数下降」
可能只是因为豁免变多了，两个数字放在一起才看得出质量在往哪边走（FR-069）。
