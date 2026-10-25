//! 重复代码检测（FR-011）。
//!
//! 识别「完全重复」与「结构相似」的实现。判定完全基于语法树形状，不做语义
//! 等价判断：两个函数即便算出同一个结果，只要语法形状不同就不算重复——语义
//! 等价需要数据流分析，超出当前静态层的范围，误报的代价比漏报更高。
//!
//! 归一化策略分两级：
//!
//! - **形状签名**：只保留节点种类序列，标识符与字面量统一替换为占位符。
//!   用于分组，因此改个变量名不会躲过检测。
//! - **词法签名**：保留标识符名字，字面量替换为占位符。组内成员的词法签名
//!   全部相同时判定为「完全重复」，否则判定为「结构相似」。

use std::collections::HashMap;

use pf_core::finding::{Finding, FindingKind, Remediation};
use pf_core::ids::RuleId;
use pf_core::rule::{Rule, RuleSeverity};
use pf_symbol::Symbol;
use tree_sitter::Node;

use super::{is_rust_test_code, Checker, Context};

/// 形状签名的节点数下限。
///
/// 低于该值的函数体（例如只有一条 `return` 的访问器）在任何代码库里都会大量
/// 撞形状，报告出来只会淹没真正值得合并的实现。
const MIN_SIGNATURE_NODES: usize = 14;

/// 参与判定的函数体最少语句数。
///
/// 与 [`MIN_SIGNATURE_NODES`] 共同构成阈值：前者排除琐碎的 getter，后者排除
/// 「只有一行但语法节点很多」的表达式。
const MIN_BODY_STATEMENTS: usize = 3;

/// FR-011：重复代码（完全重复与结构相似）。
pub struct DuplicateCodeChecker;

impl Checker for DuplicateCodeChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("duplicate-code"),
            "存在形状完全一致或高度相似的实现，应提取共享逻辑",
            RuleSeverity::Advisory,
            "duplicate_code",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        for members in duplicate_groups(context) {
            findings.push(describe_group(context, &rule, &members));
        }
        findings
    }
}

/// 按形状签名把候选函数分组，只返回成员数 ≥ 2 的组。
///
/// 组按首个成员的位置排序：符号表的遍历顺序取决于哈希表，不排序会让同一份代码
/// 在不同运行中产生不同顺序的发现，报告因此无法 diff（FR-061 可审计性）。
fn duplicate_groups<'a>(context: &'a Context<'_>) -> Vec<Vec<&'a Symbol>> {
    let mut groups: HashMap<String, Vec<&Symbol>> = HashMap::new();
    for symbol in context.index.symbols() {
        if !symbol.kind.is_callable() {
            continue;
        }
        let Some(tree) = context.tree(&symbol.file) else {
            continue;
        };
        let Some(node) = super::find_node(tree, &symbol.span) else {
            continue;
        };
        if is_rust_test_code(tree, &node) {
            continue;
        }
        let Some(body) = node.child_by_field_name("body") else {
            continue;
        };
        let mut cursor = body.walk();
        if body.named_children(&mut cursor).count() < MIN_BODY_STATEMENTS {
            continue;
        }
        let signature = shape_signature(&body);
        if signature.split(' ').count() < MIN_SIGNATURE_NODES {
            continue;
        }
        groups.entry(signature).or_default().push(symbol);
    }
    let mut duplicate_groups: Vec<Vec<&Symbol>> = groups
        .into_values()
        .filter(|members| members.len() >= 2)
        .collect();
    duplicate_groups.sort_by(|a, b| location_key(a[0]).cmp(&location_key(b[0])));
    duplicate_groups
}

/// 把一组重复实现翻译成一条发现。
///
/// 「完全重复」与「结构相似」的区分只看词法签名：函数体的标识符全部相同才判为
/// 完全复制，否则只报告形状一致并要求人工确认——形状相同很可能只是巧合。
fn describe_group(context: &Context<'_>, rule: &Rule, members: &[&Symbol]) -> Finding {
    let is_exact = members
        .iter()
        .all(|symbol| lexical_signature(context, symbol) == lexical_signature(context, members[0]));
    let (kind_label, detail) = if is_exact {
        (
            "完全重复",
            format!(
                "以下 {} 个实现的语法形状与标识符完全一致，只有字面量不同：{}。\
                 完全重复通常是复制粘贴的产物，应当提取为一个带参数的共享函数。",
                members.len(),
                join_names(members, "、")
            ),
        )
    } else {
        (
            "结构相似",
            format!(
                "以下 {} 个实现的语法形状一致，但标识符不同：{}。\
                 形状相同而命名不同，可能是同一逻辑被反复重写；\
                 也可能只是巧合（例如多个同形状的解析函数），需要人工确认。",
                members.len(),
                join_names(members, "、")
            ),
        )
    };
    Finding::new(
        rule.id.clone(),
        FindingKind::DuplicateCode,
        format!(
            "{} 个实现{}（{} 个语法节点）",
            members.len(),
            kind_label,
            shape_node_count(members[0], context)
        ),
        detail,
        members
            .iter()
            .map(|symbol| symbol.location.clone())
            .collect(),
    )
    .with_severity(rule.effective_severity())
    .with_fact("occurrences", members.len().to_string())
    .with_fact("duplication", if is_exact { "exact" } else { "structural" })
    .with_fact(
        "members",
        members
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>()
            .join(","),
    )
    .with_remediation(Remediation::new(
        "提取共享逻辑为一个函数，用参数替代重复部分；\
         若各实现后续会独立演化，请在评审中说明保留理由",
    ))
}

/// 把成员名格式化为 `` `a`、`b` `` 形式。
fn join_names(members: &[&Symbol], separator: &str) -> String {
    members
        .iter()
        .map(|symbol| format!("`{}`", symbol.qualified_name))
        .collect::<Vec<_>>()
        .join(separator)
}

/// 位置的稳定排序键：先文件后行列，保证报告的组顺序可复现。
fn location_key(symbol: &Symbol) -> (String, u32, u32) {
    (
        symbol.file.clone(),
        symbol.location.start_line.unwrap_or(0),
        symbol.location.start_column.unwrap_or(0),
    )
}

/// 形状签名的节点数（用于在标题里给出量级）。
fn shape_node_count(symbol: &Symbol, context: &Context<'_>) -> usize {
    context
        .tree(&symbol.file)
        .and_then(|tree| super::find_node(tree, &symbol.span))
        .and_then(|node| node.child_by_field_name("body"))
        .map_or_else(|| 0, |body| shape_signature(&body).split(' ').count())
}

/// 计算函数体的形状签名。
///
/// 先序遍历节点种类；标识符、字面量与字段名一律归一化为 `_`，这样重命名变量
/// 不会改变签名。节点之间用空格分隔，便于计数与比较。
fn shape_signature(root: &Node<'_>) -> String {
    let mut cursor = root.walk();
    let mut stack = vec![*root];
    let mut out = Vec::new();
    while let Some(node) = stack.pop() {
        out.push(normalized_kind(node.kind()));
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    out.join(" ")
}

/// 计算函数体的词法签名：保留标识符名字，字面量归一化。
///
/// 与 [`shape_signature`] 的差别只在标识符：这里保留真实名字，因此可以区分
/// 「复制粘贴」与「照着形状重写」。
fn lexical_signature(context: &Context<'_>, symbol: &Symbol) -> String {
    let Some(tree) = context.tree(&symbol.file) else {
        return String::new();
    };
    let Some(node) = super::find_node(tree, &symbol.span) else {
        return String::new();
    };
    let Some(body) = node.child_by_field_name("body") else {
        return String::new();
    };
    let mut cursor = body.walk();
    let mut stack = vec![body];
    let mut out = Vec::new();
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if kind == "identifier" || kind == "field_identifier" || kind == "type_identifier" {
            // 标识符保留真实文本。
            out.push(tree.node_text(&node).unwrap_or_default().to_owned());
        } else {
            out.push(normalized_kind(kind).to_owned());
        }
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    out.join(" ")
}

/// 把「因重命名或换字面量而必然变化」的节点种类归一化。
fn normalized_kind(kind: &str) -> &str {
    match kind {
        "identifier" | "field_identifier" | "type_identifier" | "scoped_identifier"
        | "integer_literal" | "float_literal" | "string_literal" | "raw_string_literal"
        | "char_literal" | "boolean_literal" | "line_comment" | "comment" => "_",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::with_context;

    /// 构造一个达到形状阈值、语句数达标的函数体。
    ///
    /// 阈值来自 [`MIN_SIGNATURE_NODES`] 与 [`MIN_BODY_STATEMENTS`]，测试代码必须
    /// 同时跨过两者，否则规则会因为「样本太小」而不触发，测试就失去了意义。
    fn duplicated_body(tag: &str) -> String {
        format!(
            "fn {tag}(input: &str) -> String {{
    let trimmed = input.trim();
    let lowered = trimmed.to_lowercase();
    let parts: Vec<&str> = lowered.split(',').collect();
    let mut out = Vec::new();
    for part in parts {{
        if part.is_empty() {{
            continue;
        }}
        out.push(part.to_owned());
    }}
    out.join(\"-\")
}}
"
        )
    }

    #[test]
    fn detects_identical_duplicate_implementations() {
        with_context(
            &[
                ("src/a.rs", &duplicated_body("first")),
                ("src/b.rs", &duplicated_body("second")),
            ],
            |context| {
                let findings = DuplicateCodeChecker.check(context);
                assert_eq!(findings.len(), 1, "两处逐字重复应合并为一条发现");
                assert_eq!(findings[0].kind, FindingKind::DuplicateCode);
                assert_eq!(findings[0].locations.len(), 2, "发现要列出全部重复位置");
                assert!(
                    findings[0]
                        .facts
                        .iter()
                        .any(|(key, value)| key == "duplication" && value == "exact"),
                    "标识符相同应判定为完全重复"
                );
            },
        );
    }

    #[test]
    fn renamed_locals_are_reported_as_structural_not_exact() {
        // 只改函数名不改变函数体，因此仍是逐字复制（判为 exact）。
        // 真正区分「照着形状重写」的是函数体里的标识符：改名不改变形状签名，
        // 组仍会成立，但词法签名不同，只能判为「结构相似」并要求人工确认。
        let rewritten = duplicated_body("alpha").replace("trimmed", "stripped");
        with_context(
            &[
                ("src/a.rs", &duplicated_body("alpha")),
                ("src/b.rs", &rewritten),
            ],
            |context| {
                let findings = DuplicateCodeChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert!(
                    findings[0]
                        .facts
                        .iter()
                        .any(|(key, value)| key == "duplication" && value == "structural"),
                    "{:?}",
                    findings[0].facts
                );
            },
        );
    }

    #[test]
    fn renaming_only_the_function_keeps_it_exact() {
        // 函数名不参与函数体的词法签名：复制一个函数时最自然会改的就是名字，
        // 把改名判成「只是相似」会让真正的复制粘贴逃过 exact 归类。
        let renamed = duplicated_body("alpha").replace("alpha", "beta");
        with_context(
            &[
                ("src/a.rs", &duplicated_body("alpha")),
                ("src/b.rs", &renamed),
            ],
            |context| {
                let findings = DuplicateCodeChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert!(
                    findings[0]
                        .facts
                        .iter()
                        .any(|(key, value)| key == "duplication" && value == "exact"),
                    "{:?}",
                    findings[0].facts
                );
            },
        );
    }

    #[test]
    fn trivial_bodies_are_not_reported() {
        // 访问器级别的函数体在任何代码库里都会大量撞形状，必须被阈值排除。
        with_context(
            &[
                (
                    "src/a.rs",
                    "fn one(items: &[u8]) -> usize {\n    items.len()\n}\n",
                ),
                (
                    "src/b.rs",
                    "fn two(items: &[u8]) -> usize {\n    items.len()\n}\n",
                ),
            ],
            |context| {
                assert_eq!(
                    DuplicateCodeChecker.check(context),
                    Vec::new(),
                    "过短的函数体不应产生发现"
                );
            },
        );
    }

    #[test]
    fn different_shapes_are_not_grouped() {
        with_context(
            &[
                ("src/a.rs", &duplicated_body("alpha")),
                (
                    "src/b.rs",
                    "fn beta(input: &str) -> String {\n    let a = input.trim();\n    let b = a.to_uppercase();\n    let c = b.trim();\n    c.to_owned()\n}\n",
                ),
            ],
            |context| {
                assert_eq!(
                    DuplicateCodeChecker.check(context),
                    Vec::new(),
                    "形状不同不应归为一组"
                );
            },
        );
    }

    #[test]
    fn test_code_is_exempt() {
        // `#[cfg(test)]` 里的重复断言是表驱动测试的常态，不是可消除的冗余。
        let body = duplicated_body("case");
        let source = format!("#[cfg(test)]\nmod tests {{\n{body}\n}}\n");
        with_context(
            &[
                ("src/lib.rs", &duplicated_body("real")),
                ("src/lib.rs", &source),
            ],
            |context| {
                assert_eq!(
                    DuplicateCodeChecker.check(context),
                    Vec::new(),
                    "测试代码不应参与重复判定"
                );
            },
        );
    }
}
