//! 过度工程化检测（无效抽象与多余层次）。
//!
//! 覆盖需求文档 5.5：无效抽象（FR-015）、无业务处理的委托链（FR-016）与
//! 模式滥用（FR-017）。
//!
//! 三条规则都只输出**结构事实**并附带 `needs_human_confirmation`：测试替身、
//! 依赖替换与预期的多实现需求都是合理的存在理由，静态层无法区分。

use std::collections::HashMap;

use pf_core::finding::{Finding, FindingKind, Remediation};
use pf_core::ids::RuleId;
use pf_core::rule::{Rule, RuleSeverity};
use pf_symbol::SymbolKind;

use super::{
    find_node, forwards_arguments, is_inside, is_rust_test_code, is_trait_contract_node,
    location_for, Checker, Context,
};

/// FR-015：只有一种实现的 trait。
pub struct SingleImplementationTraitChecker;

impl Checker for SingleImplementationTraitChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("single-implementation-trait"),
            "只有一种实现且没有运行时选择逻辑的 trait 可能是无效抽象",
            RuleSeverity::Advisory,
            "single_implementation_trait",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        // 统计每个 trait 名对应的 impl 数量。
        let mut impl_counts: HashMap<&str, usize> = HashMap::new();
        for symbol in context.index.symbols() {
            if symbol.kind != SymbolKind::Impl {
                continue;
            }
            // impl 符号的名字形如 `impl <Type>`；trait 实现形如 `impl <Trait> for <Type>`。
            let target = symbol.name.trim_start_matches("impl ");
            if let Some((trait_name, _)) = target.split_once(" for ") {
                *impl_counts.entry(trait_name.trim()).or_insert(0) += 1;
            }
        }

        context
            .index
            .symbols()
            .iter()
            .filter(|symbol| symbol.kind == SymbolKind::Trait)
            .filter(|symbol| impl_counts.get(symbol.name.as_str()).copied().unwrap_or(0) == 1)
            .map(|symbol| {
                Finding::new(
                    rule.id.clone(),
                    FindingKind::UselessAbstraction,
                    format!("trait `{}` 只有一种实现", symbol.name),
                    format!(
                        "trait `{}` 在项目内只有一个实现。如果不存在测试替身、\
                         依赖替换或预期的多种实现需求，这层抽象只增加了间接调用。",
                        symbol.name
                    ),
                    vec![symbol.location.clone()],
                )
                .with_severity(rule.effective_severity())
                .with_fact("implementations", "1")
                .with_fact("needs_human_confirmation", "true")
                .with_remediation(
                    Remediation::new(
                        "确认是否存在测试隔离或依赖替换需求；若没有，考虑直接使用具体类型",
                    )
                    .with_code(format!("// 直接使用具体类型替代 {} 抽象", symbol.name)),
                )
            })
            .collect()
    }
}

/// FR-016：纯转发包装（连续包装但没有增加行为）。
pub struct ForwardingWrapperChecker;

impl ForwardingWrapperChecker {
    /// 触发告警的转发调用次数阈值（函数体内只有一次调用）。
    const THRESHOLD_FORWARDS: usize = 1;
}

impl Checker for ForwardingWrapperChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("forwarding-wrapper"),
            "只把参数原样转发给另一个函数的包装没有增加行为",
            RuleSeverity::Advisory,
            "forwarding_wrapper",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        for symbol in context.index.symbols() {
            if !symbol.kind.is_callable() {
                continue;
            }
            let Some(tree) = context.tree(&symbol.file) else {
                continue;
            };
            let Some(node) = find_node(tree, &symbol.span) else {
                continue;
            };
            // 测试代码里的薄包装是惯用的测试助手，不算过度设计。
            if is_rust_test_code(tree, &node) {
                continue;
            }
            // trait 的实现方法必须转发：`Display::fmt` 只能调用
            // `f.write_str(...)`，`From::from` 只能构造目标类型。转发是契约
            // 要求，不是可以消除的多余层次。
            if is_trait_contract_node(tree, &node) {
                continue;
            }
            // 零参数访问器（`len`、`is_empty`、`as_str`）返回的是派生值，
            // 是 Rust 暴露不变量的惯用方式；本规则针对的是「把参数原样转手」的
            // 代理层（需求文档 3.4.1 的「无业务处理的代理」）。
            if !forwards_arguments(tree, &node) {
                continue;
            }
            let calls: Vec<_> = tree
                .descendants()
                .filter(|candidate| candidate.kind() == "call_expression")
                .filter(|candidate| is_inside(candidate, &node))
                .collect();
            if calls.len() != Self::THRESHOLD_FORWARDS {
                continue;
            }
            // 仅当函数体只有这一条语句时才视为纯转发；带有校验、日志或状态
            // 变更的包装是有意义的中间层。
            let Some(body) = node.child_by_field_name("body") else {
                continue;
            };
            let mut body_cursor = body.walk();
            if body.named_children(&mut body_cursor).count() != 1 {
                continue;
            }
            let Some(file) = context.source(&symbol.file) else {
                continue;
            };
            let callee = calls[0]
                .child_by_field_name("function")
                .and_then(|callee| tree.node_text(&callee).ok())
                .unwrap_or_default()
                .to_owned();
            findings.push(
                Finding::new(
                    rule.id.clone(),
                    FindingKind::ForwardingChain,
                    format!("`{}` 只是转发调用 `{callee}`", symbol.name),
                    format!(
                        "`{}` 的函数体只包含一次调用 `{callee}`，没有添加校验、\
                         日志、错误处理或状态变更。",
                        symbol.qualified_name
                    ),
                    vec![location_for(file, &node)],
                )
                .with_severity(rule.effective_severity())
                .with_fact("forwarded_to", callee.clone())
                .with_remediation(Remediation::new(format!(
                    "确认该包装是否承担了未来的扩展职责；否则考虑让调用方直接调用 `{callee}`"
                ))),
            );
        }
        findings
    }
}

/// FR-017：不必要的工厂（返回唯一具体类型的工厂函数）。
pub struct NeedlessFactoryChecker;

impl Checker for NeedlessFactoryChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("needless-factory"),
            "对单一具体类型没有分支逻辑的工厂函数没有存在价值",
            RuleSeverity::Advisory,
            "needless_factory",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        for symbol in context.index.symbols() {
            if !symbol.kind.is_callable() {
                continue;
            }
            let name = symbol.name.to_lowercase();
            let is_factory_like = name.contains("factory")
                || name.starts_with("create")
                || name.starts_with("new_")
                || name == "build";
            if !is_factory_like {
                continue;
            }
            // 工厂的价值来自分支选择；若函数体内没有任何条件表达式，
            // 且只构造一种类型，则视为可疑。
            let Some(tree) = context.tree(&symbol.file) else {
                continue;
            };
            let Some(node) = find_node(tree, &symbol.span) else {
                continue;
            };
            let has_branching = tree.descendants().any(|candidate| {
                matches!(candidate.kind(), "if_expression" | "match_expression")
                    && is_inside(&candidate, &node)
            });
            if has_branching {
                continue;
            }
            let Some(file) = context.source(&symbol.file) else {
                continue;
            };
            findings.push(
                Finding::new(
                    rule.id.clone(),
                    FindingKind::NeedlessPattern,
                    format!("工厂函数 `{}` 没有选择逻辑", symbol.name),
                    format!(
                        "`{}` 形似工厂，但函数体内没有任何条件分支，\
                         总是返回同一种实现。若不存在预期的多实现扩展，这一层没有价值。",
                        symbol.qualified_name
                    ),
                    vec![location_for(file, &node)],
                )
                .with_severity(rule.effective_severity())
                .with_fact("has_branching", "false")
                .with_remediation(Remediation::new(
                    "确认未来是否会有多种实现；否则直接构造具体类型",
                )),
            );
        }
        findings
    }
}
