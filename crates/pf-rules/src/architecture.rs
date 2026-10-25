//! 架构分析。
//!
//! 覆盖需求文档 5.4：模块依赖图（FR-022）、循环依赖检测（FR-023）与架构规则
//! （FR-024）。依赖图本身由 `pf-graph` 构建，本模块只负责把它翻译成可定位的发现。

use pf_core::config::{ArchitectureMode, ArchitectureRule};
use pf_core::finding::{Finding, FindingKind, Remediation};
use pf_core::ids::RuleId;
use pf_core::location::Location;
use pf_core::rule::{Rule, RuleSeverity};
use pf_symbol::SymbolIndex;

use super::{Checker, Context};

/// FR-023：模块循环依赖。
pub struct CircularDependencyChecker;

impl Checker for CircularDependencyChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("circular-dependency"),
            "模块之间不得存在循环依赖",
            RuleSeverity::Mandatory,
            "circular_dependency",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        context
            .module_graph
            .cycles()
            .into_iter()
            .map(|cycle| {
                let locations = cycle_locations(&cycle, context.index);
                // 环是依赖图上的确定性质：给同一份 import 集合，算法必然找到同一个环。
                super::confirmed(
                    Finding::new(
                        rule.id.clone(),
                        FindingKind::CircularDependency,
                        format!("检测到 {}-文件循环依赖", cycle.len()),
                        format!(
                            "以下文件互相依赖，形成环：{}。循环依赖会让构建顺序、\
                         测试隔离与独立复用都变得困难。",
                            cycle.join(" → ")
                        ),
                        locations,
                    )
                    .with_severity(rule.effective_severity())
                    .with_fact("cycle", cycle.join(" → "))
                    .with_remediation(Remediation::new(
                        "提取共享类型到独立模块，或用依赖反转打破环",
                    )),
                )
            })
            .collect()
    }
}

/// 为循环中的每个文件找出「形成环的那条 import」的位置。
///
/// 模块依赖图只记录文件级依赖，行号来自符号索引里的引用记录。匹配 import 路径的
/// **任意一段**与环成员，因此 `use crate::beta;`、`use crate::beta::Thing;` 与
/// `from . import beta` 都能命中。找不到匹配 import 的文件不产生位置，而不是编造
/// 一个行号 —— 每条发现都必须可定位（FR-064）。
fn cycle_locations(cycle: &[String], index: &SymbolIndex) -> Vec<Location> {
    let stems: Vec<&str> = cycle.iter().map(|path| file_stem(path)).collect();
    cycle
        .iter()
        .filter_map(|file| import_location(index, file, &stems))
        .collect()
}

/// 在某个文件的 import 引用里，找出第一条指向给定模块名集合的引用位置。
///
/// 判据是「路径的任意一段等于模块名」，而不是只比最后一段：`use crate::domain::InputDomain;`
/// 指向的模块是 `domain`，而最后一段是 `InputDomain`。只比最后一段会让这类
/// import——仓库里最常见的形式——全部匹配不上，于是发现一条位置都没有，
/// 而「不可定位的发现」在报告里等于没有证据（FR-064）。
fn import_location(index: &SymbolIndex, file: &str, stems: &[&str]) -> Option<Location> {
    index
        .imports_of(file)
        .find(|reference| path_segments(&reference.to_name).any(|segment| stems.contains(&segment)))
        .map(|reference| reference.location.clone())
}

/// 拆出一条 import 路径的各段。
///
/// `::` 与 `.` 都当分隔符：前者是 Rust，后者是 Python（`from . import beta` 的
/// `to_name` 是 `.beta`）。
fn path_segments(path: &str) -> impl Iterator<Item = &str> {
    path.split([':', '.']).filter(|segment| !segment.is_empty())
}

/// 取文件路径去掉目录与扩展名后的模块名（`src/beta.rs` → `beta`）。
fn file_stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".rs")
        .or_else(|| name.strip_suffix(".py"))
        .unwrap_or(name)
}

/// FR-021：从未被调用的公共 API（可能是死代码或入口点）。
pub struct UncalledPublicApiChecker;

impl Checker for UncalledPublicApiChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("uncalled-public-api"),
            "项目内没有任何调用者的公共 API 需要确认是入口点还是死代码",
            RuleSeverity::Advisory,
            "uncalled_public_api",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        context
            .index
            .public_api()
            .filter(|symbol| symbol.kind.is_callable())
            .filter(|symbol| context.call_graph.callers_of(&symbol.id).is_empty())
            .map(|symbol| {
                Finding::new(
                    rule.id.clone(),
                    FindingKind::MissingTestCoverage,
                    format!("公共 API `{}` 在项目内没有调用者", symbol.name),
                    format!(
                        "`{}` 是对外可见的{}，但项目内没有任何符号调用它。\
                         它可能是供外部使用的入口点，也可能是未被清理的死代码。",
                        symbol.qualified_name,
                        symbol.kind.display_name()
                    ),
                    vec![symbol.location.clone()],
                )
                .with_severity(rule.effective_severity())
                .with_fact("callers", "0")
                .with_remediation(Remediation::new(
                    "确认是否为对外入口点；若否，考虑删除或降级为私有",
                ))
            })
            .collect()
    }
}

/// FR-024：使用者定义的架构依赖规则。
///
/// 与 [`CircularDependencyChecker`] 的分工：后者检测一个**客观**的结构性质（有环
/// 还是没环），前者执行项目自己定下的**约定**（谁允许依赖谁）。约定没有默认值——
/// 工具不预设分层，因此规则为空时这个检查器什么都不报。
///
/// 规则是路径通配符而不是 crate 名：模块依赖图按文件建边，规则必须能落到文件上
/// 才可定位（FR-064）。
pub struct ArchitectureRuleChecker {
    /// 项目定义的全部规则。
    rules: Vec<ArchitectureRule>,
}

impl Clone for ArchitectureRuleChecker {
    fn clone(&self) -> Self {
        Self::new(self.rules.clone())
    }
}

impl ArchitectureRuleChecker {
    /// 用一组规则构造检查器。
    #[must_use]
    pub const fn new(rules: Vec<ArchitectureRule>) -> Self {
        Self { rules }
    }
}

impl Checker for ArchitectureRuleChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("architecture-rule"),
            "依赖方向必须符合项目定义的架构规则",
            RuleSeverity::Mandatory,
            "architecture_rule",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        for entry in &self.rules {
            findings.extend(Self::check_one(context, &rule, entry));
        }
        findings
    }
}

impl ArchitectureRuleChecker {
    /// 检查单条规则，返回它命中的发现。
    fn check_one(context: &Context<'_>, rule: &Rule, entry: &ArchitectureRule) -> Vec<Finding> {
        match entry.mode {
            ArchitectureMode::Forbidden => Self::forbidden_findings(context, rule, entry),
            ArchitectureMode::Allowed => Self::allowed_findings(context, rule, entry),
        }
    }

    /// `forbidden` 模式：找出所有命中 `from → to` 的边。
    fn forbidden_findings(
        context: &Context<'_>,
        rule: &Rule,
        entry: &ArchitectureRule,
    ) -> Vec<Finding> {
        let mut findings = Vec::new();
        for from in context.module_graph.files() {
            if !pf_core::config::matches_pattern(&entry.from, from) {
                continue;
            }
            for to in context.module_graph.dependencies_of(from) {
                if !entry.applies_to(from, to) {
                    continue;
                }
                findings.push(edge_finding(context, rule, entry, from, to));
            }
        }
        findings
    }

    /// `allowed` 模式：`from` 的每个目标都必须在白名单里。
    ///
    /// 与 `forbidden` 的关键差别：这里要把**没被这条规则允许**的边报出来，因此
    /// 每条 `from` 规则都要看它的全部出边，而不是只看匹配 `to` 的那些。
    fn allowed_findings(
        context: &Context<'_>,
        rule: &Rule,
        entry: &ArchitectureRule,
    ) -> Vec<Finding> {
        let mut findings = Vec::new();
        for from in context.module_graph.files() {
            if !pf_core::config::matches_pattern(&entry.from, from) {
                continue;
            }
            for to in context.module_graph.dependencies_of(from) {
                if entry.permits(from, to) {
                    continue;
                }
                findings.push(edge_finding(context, rule, entry, from, to));
            }
        }
        findings
    }
}

/// 为一条被禁止的依赖边构造发现。
///
/// 位置取「形成这条边的那条 import」的引用记录，与 [`cycle_locations`] 同一套做法：
/// 文件级依赖本身没有位置，行号来自符号索引里的引用。
fn edge_finding(
    context: &Context<'_>,
    rule: &Rule,
    entry: &ArchitectureRule,
    from: &str,
    to: &str,
) -> Finding {
    let direction = match entry.mode {
        ArchitectureMode::Forbidden => format!(
            "架构规则 `{}` 禁止 `{}` 依赖 `{}`",
            entry.name, entry.from, entry.to
        ),
        ArchitectureMode::Allowed => format!(
            "架构规则 `{}` 只允许 `{}` 依赖 `{}`",
            entry.name, entry.from, entry.to
        ),
    };
    let reason = entry
        .reason
        .as_deref()
        .map_or_else(|| "（这条规则没有写理由）".to_owned(), ToOwned::to_owned);
    let locations = edge_locations(context.index, from, to);
    Finding::new(
        rule.id.clone(),
        FindingKind::ArchitectureViolation,
        format!("`{from}` 依赖 `{to}`，违反架构规则 `{}`", entry.name),
        format!(
            "{direction}，但 `{from}` 里有一条指向 `{to}` 的导入。规则理由：{reason}。\
             依赖方向是项目自己定下的约定，违反它不会让程序出错，但会让分层逐渐失效。",
        ),
        locations,
    )
    .with_severity(rule.effective_severity())
    .with_fact("rule", entry.name.clone())
    .with_fact("from", from.to_owned())
    .with_fact("to", to.to_owned())
    .with_fact(
        "mode",
        match entry.mode {
            ArchitectureMode::Forbidden => "forbidden",
            ArchitectureMode::Allowed => "allowed",
        },
    )
    .with_remediation(Remediation::new(format!(
        "把 `{from}` 对 `{to}` 的引用反转或下沉；若这条依赖确实必要，\
         修改规则 `{}` 并更新其理由",
        entry.name
    )))
}

/// 为一条依赖边找出「形成它的那条 import」的位置。
///
/// 与 [`cycle_locations`] 共用 [`import_location`]：同一套匹配规则，两处才不会
/// 给出不同的定位结论。找不到时不产生位置，而不是编造一个行号。
fn edge_locations(index: &SymbolIndex, from: &str, to: &str) -> Vec<Location> {
    import_location(index, from, &[file_stem(to)])
        .map(|location| vec![location])
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::config::ArchitectureRule;

    use crate::testing::with_context;

    /// 报告层不得依赖规则层。
    ///
    /// 路径写成 `src/report.rs` 与 `src/rules.rs`：模块依赖图把 `crate::rules`
    /// 解析成 `src/rules.rs`，用 `crates/pf-rules/src/lib.rs` 这种真实路径反而
    /// 解析不到，测试会变成在测解析器而不是在测规则。
    fn forbidden() -> ArchitectureRule {
        ArchitectureRule {
            name: "报告层不得依赖规则层".to_owned(),
            from: "src/report.rs".to_owned(),
            to: "src/rules.rs".to_owned(),
            mode: ArchitectureMode::Forbidden,
            reason: Some("报告渲染应当可以被单独测试".to_owned()),
        }
    }

    #[test]
    fn a_forbidden_edge_is_reported_with_its_import_location() {
        with_context(
            &[
                ("src/report.rs", "use crate::rules::Rule;\npub fn r() {}\n"),
                ("src/rules.rs", "pub struct Rule;\n"),
            ],
            |context| {
                let findings = ArchitectureRuleChecker::new(vec![forbidden()]).check(context);
                assert_eq!(findings.len(), 1, "{findings:?}");
                assert_eq!(findings[0].kind, FindingKind::ArchitectureViolation);
                assert_eq!(
                    findings[0]
                        .facts
                        .iter()
                        .find(|(k, _)| k == "rule")
                        .map(|(_, v)| v.as_str()),
                    Some("报告层不得依赖规则层")
                );
                assert_eq!(
                    findings[0]
                        .facts
                        .iter()
                        .find(|(k, _)| k == "from")
                        .map(|(_, v)| v.as_str()),
                    Some("src/report.rs")
                );
                assert_eq!(
                    findings[0]
                        .facts
                        .iter()
                        .find(|(k, _)| k == "to")
                        .map(|(_, v)| v.as_str()),
                    Some("src/rules.rs")
                );
                assert!(
                    findings[0]
                        .primary_location()
                        .is_some_and(|location| location.path == "src/report.rs"),
                    "发现必须指向形成这条边的那条 import：{findings:?}"
                );
            },
        );
    }

    #[test]
    fn an_edge_outside_the_rules_scope_is_not_reported() {
        with_context(
            &[
                ("src/cli.rs", "use crate::rules::Rule;\npub fn r() {}\n"),
                ("src/rules.rs", "pub struct Rule;\n"),
            ],
            |context| {
                let findings = ArchitectureRuleChecker::new(vec![forbidden()]).check(context);
                assert!(findings.is_empty(), "{findings:?}");
            },
        );
    }

    #[test]
    fn an_allowed_rule_reports_every_target_outside_the_whitelist() {
        let rule = ArchitectureRule {
            name: "入口只能依赖核心".to_owned(),
            from: "src/cli.rs".to_owned(),
            to: "src/core.rs".to_owned(),
            mode: ArchitectureMode::Allowed,
            reason: None,
        };
        with_context(
            &[
                (
                    "src/cli.rs",
                    "use crate::core::run;\nuse crate::rules::Rule;\npub fn r() {}\n",
                ),
                ("src/core.rs", "pub fn run() {}\n"),
                ("src/rules.rs", "pub struct Rule;\n"),
            ],
            |context| {
                let findings = ArchitectureRuleChecker::new(vec![rule]).check(context);
                assert_eq!(findings.len(), 1, "{findings:?}");
                assert_eq!(
                    findings[0]
                        .facts
                        .iter()
                        .find(|(k, _)| k == "to")
                        .map(|(_, v)| v.as_str()),
                    Some("src/rules.rs"),
                    "白名单外的每个目标都要报出来：{findings:?}"
                );
                assert!(
                    findings[0].detail.contains("没有写理由"),
                    "规则缺理由时报告要自己说明：{}",
                    findings[0].detail
                );
            },
        );
    }

    #[test]
    fn an_empty_rule_set_reports_nothing() {
        // 工具不预设分层：没有规则就不检查依赖方向。
        with_context(
            &[
                ("src/report.rs", "use crate::rules::Rule;\npub fn r() {}\n"),
                ("src/rules.rs", "pub struct Rule;\n"),
            ],
            |context| {
                assert_eq!(
                    ArchitectureRuleChecker::new(Vec::new()).check(context),
                    Vec::new()
                );
            },
        );
    }
}
