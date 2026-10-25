//! 过度配置、过度模块拆分与样板代码检测。
//!
//! 覆盖需求文档 5.3 的 FR-018（过度配置）、FR-019（过度模块拆分）与
//! FR-020（样板代码）。
//!
//! # 关于 FR-020 的判定边界
//!
//! 需求文档在 FR-020 里明确写道：「不应声称能够可靠判断某段代码是否由 AI
//! 编写。审查对象始终是代码本身的工程质量，而不是作者身份。」因此本模块的
//! [`BoilerplateCodeChecker`] **只**报告可以被独立论证的低价值结构——空的占位
//! 函数体、逐字复制的匹配分支——并在每条发现的说明里显式否认作者推断。
//! 「像 AI 写的」不是结论，「这段代码没有信息量」才是。
//!
//! 三条规则都只输出结构事实并附 `needs_human_confirmation`：判定是否需要这些
//! 层次必须结合需求变更预期与团队约定（需求文档 7.3）。

use std::collections::HashMap;

use pf_core::finding::{Finding, FindingKind, Remediation};
use pf_core::ids::RuleId;
use pf_core::rule::{Rule, RuleSeverity};
use pf_parser::ParseTree;
use tree_sitter::Node;

use super::{
    find_node, is_rust_test_code, is_test_path, location_for, Checker, Context, RUST_EXTENSION,
};

// ---------------------------------------------------------------------------
// FR-018 过度配置
// ---------------------------------------------------------------------------

/// 触发「参数被配置化」判定的最少参数个数。
///
/// 三个参数里有两个是开关是常见且合理的写法；阈值取 4 是为了排除这类正常签名。
const MIN_PARAMS_FOR_CONFIG_SMELL: usize = 4;

/// 触发判定的最少开关型参数个数。
///
/// 与 [`MIN_PARAMS_FOR_CONFIG_SMELL`] 共同构成判据：参数总数达标，且其中至少
/// 有这么多个是 `bool` 或 `Option<_>`，签名才被认为是被配置驱动的。
const MIN_FLAG_PARAMS: usize = 3;

/// 占位宏的白名单：函数体只有这些宏调用时，该函数没有实现任何行为。
const PLACEHOLDER_MACROS: &[&str] = &["unimplemented", "todo", "unreachable"];

/// FR-018：过度配置。
///
/// 识别为简单业务增加的配置层与通用参数。当前实现覆盖两个可静态论证的信号：
///
/// 1. **签名被开关参数主导**——大量 `bool` / `Option<_>` 形参。
/// 2. **假的可选参数**——`Option` 形参在函数体内被无条件 `unwrap_or_default()`
///    之类的调用兜底，调用方传什么都不会改变行为。
///
/// 只支持 Rust：两条信号都依赖「形参列表的文本形状」与「方法调用解析」，
/// 在其他语言上缺乏同等可靠的判据，宁可不报。
pub struct OverConfigurationChecker;

impl Checker for OverConfigurationChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("over-configuration"),
            "为简单业务增加的配置层与通用参数会让调用点难以理解",
            RuleSeverity::Advisory,
            "over_configuration",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        for symbol in context.index.symbols() {
            if !symbol.kind.is_callable() || !symbol.file.ends_with(RUST_EXTENSION) {
                continue;
            }
            if is_test_path(&symbol.file) {
                continue;
            }
            let Some(tree) = context.tree(&symbol.file) else {
                continue;
            };
            let Some(node) = find_node(tree, &symbol.span) else {
                continue;
            };
            if is_rust_test_code(tree, &node) {
                continue;
            }
            let Some(file) = context.source(&symbol.file) else {
                continue;
            };
            let Some(parameters) = node.child_by_field_name("parameters") else {
                continue;
            };
            let Ok(text) = tree.node_text(&parameters) else {
                continue;
            };
            let params = split_parameters(text);
            if params.len() < MIN_PARAMS_FOR_CONFIG_SMELL {
                continue;
            }
            let flags: Vec<&str> = params
                .iter()
                .copied()
                .filter(|param| is_flag_parameter(param))
                .collect();
            if flags.len() < MIN_FLAG_PARAMS {
                continue;
            }
            findings.push(
                Finding::new(
                    rule.id.clone(),
                    FindingKind::UselessAbstraction,
                    format!(
                        "`{}` 的 {} 个参数中有 {} 个是开关型参数",
                        symbol.qualified_name,
                        params.len(),
                        flags.len()
                    ),
                    format!(
                        "`{}` 的签名里有 {} 个 `bool` 或 `Option<_>` 形参（{}）。\
                         开关型参数会让每个调用点都变成一组难以记忆的布尔选择，\
                         且新增分支时必须同时改所有调用方。\
                         如果这些参数描述的是同一件事的不同侧面，\
                         应当收敛为一个有名字的配置类型，并给常用场景提供默认值。",
                        symbol.qualified_name,
                        flags.len(),
                        flags.join("、")
                    ),
                    vec![location_for(file, &node)],
                )
                .with_severity(rule.effective_severity())
                .with_fact("parameter_count", params.len().to_string())
                .with_fact("flag_count", flags.len().to_string())
                .with_fact("flags", flags.join(","))
                .with_fact("needs_human_confirmation", "true")
                .with_remediation(Remediation::new(
                    "把相关的开关参数收敛为一个配置结构体，\
                     用 `Default` 提供常用取值；与业务真正无关的参数直接删除",
                )),
            );
        }
        findings.extend(fake_option_parameters(context, &rule));
        findings
    }
}

/// 检测「假的 `Option` 参数」：形参声明为可选，函数体却无条件兜底。
///
/// 这类参数对调用方是谎言——无论传 `Some` 还是 `None`，行为都相同。它通常来自
/// 「先留个扩展口」的模板化写法。
fn fake_option_parameters(context: &Context<'_>, rule: &Rule) -> Vec<Finding> {
    let mut findings = Vec::new();
    for symbol in context.index.symbols() {
        if !symbol.kind.is_callable() || !symbol.file.ends_with(RUST_EXTENSION) {
            continue;
        }
        if is_test_path(&symbol.file) {
            continue;
        }
        let Some(tree) = context.tree(&symbol.file) else {
            continue;
        };
        let Some(node) = find_node(tree, &symbol.span) else {
            continue;
        };
        if is_rust_test_code(tree, &node) {
            continue;
        }
        let Some(parameters) = node.child_by_field_name("parameters") else {
            continue;
        };
        let Ok(text) = tree.node_text(&parameters) else {
            continue;
        };
        let option_names = option_parameter_names(text);
        if option_names.is_empty() {
            continue;
        }
        let Some(body) = node.child_by_field_name("body") else {
            continue;
        };
        // 只在函数体内、且不在任何条件分支里兜底，才算「无条件」。
        // 分支里的兜底是真实的配置行为，不能报。
        let mut cursor = body.walk();
        let mut stack = vec![body];
        while let Some(current) = stack.pop() {
            if matches!(
                current.kind(),
                "if_expression" | "match_expression" | "while_expression" | "for_expression"
            ) {
                continue;
            }
            if current.kind() == "call_expression" {
                if let Ok(callee) = tree.node_text(&current) {
                    for name in &option_names {
                        if callee.starts_with(&format!("{name}."))
                            && (callee.contains("unwrap_or") || callee.contains("unwrap_or_else"))
                        {
                            let Some(file) = context.source(&symbol.file) else {
                                continue;
                            };
                            findings.push(
                                Finding::new(
                                    rule.id.clone(),
                                    FindingKind::UselessAbstraction,
                                    format!(
                                        "`{}` 的可选参数 `{name}` 在函数体内被无条件兜底",
                                        symbol.qualified_name
                                    ),
                                    format!(
                                        "`{}` 把 `{name}` 声明为 `Option<_>`，\
                                         却在函数体里直接 `{}` 取默认值，\
                                         中间没有任何条件判断。调用方传 `Some` 与传 `None` \
                                         得到相同结果，这个参数对行为没有影响，\
                                         却让每个调用点都要多想一次该传什么。",
                                        symbol.qualified_name, callee
                                    ),
                                    vec![location_for(file, &node)],
                                )
                                .with_severity(rule.effective_severity())
                                .with_fact("parameter", name.clone())
                                .with_fact("fallback", callee.to_owned())
                                .with_fact("needs_human_confirmation", "true")
                                .with_remediation(
                                    Remediation::new(
                                        "确认该参数是否应该有条件地生效；\
                                     若暂不支持，改为普通参数或直接删除，不要用 `Option` 占位",
                                    ),
                                ),
                            );
                        }
                    }
                }
            }
            let children: Vec<Node<'_>> = current.children(&mut cursor).collect();
            stack.extend(children);
        }
    }
    findings
}

/// 按顶层逗号切分形参文本。
///
/// 不能简单按 `,` 切：泛型参数、闭包与元组类型里都含逗号
/// （`HashMap<String, Vec<u8>>`、`impl Fn(u32, u32) -> u32`）。
/// 因此按尖括号、圆括号、方括号的深度切分。
fn split_parameters(text: &str) -> Vec<&str> {
    let inner = text.trim();
    let inner = inner
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(inner);
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, character) in inner.char_indices() {
        match character {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let piece = inner[start..index].trim();
                if !piece.is_empty() {
                    out.push(piece);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    let tail = inner[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// 判断形参是否是开关型参数。
///
/// `bool` 与 `Option<_>` 都算：前者让调用点变成布尔选择，后者在没有条件判断时
/// 是纯粹的占位。
fn is_flag_parameter(parameter: &str) -> bool {
    let (_, declared) = split_name_and_type(parameter);
    let declared = declared.trim();
    declared == "bool"
        || declared.starts_with("Option <")
        || declared.starts_with("Option<")
        || declared == "Option"
}

/// 把形参拆成「名字 + 类型」。
///
/// 形参可能写作 `name: Type`、`mut name: Type` 或只有 `Type`（trait 方法签名里
/// 允许省略名字，Python 的类型注解同理）。按最后一个顶层 `:` 切分。
fn split_name_and_type(parameter: &str) -> (String, &str) {
    let parameter = parameter.trim().trim_start_matches("mut ").trim();
    let mut depth = 0usize;
    let mut split_at = None;
    for (index, character) in parameter.char_indices() {
        match character {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => split_at = Some(index),
            _ => {}
        }
    }
    match split_at {
        Some(index) => (
            parameter[..index].trim().to_owned(),
            parameter[index + 1..].trim(),
        ),
        None => (String::new(), parameter),
    }
}

/// 取出全部 `Option` 形参的名字。
fn option_parameter_names(text: &str) -> Vec<String> {
    split_parameters(text)
        .into_iter()
        .filter(|parameter| {
            let (_, declared) = split_name_and_type(parameter);
            declared.trim().starts_with("Option")
        })
        .map(|parameter| split_name_and_type(parameter).0)
        .filter(|name| !name.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// FR-019 过度模块拆分
// ---------------------------------------------------------------------------

/// 触发「拆分过细」判定的最少项目内依赖数。
///
/// 一个文件要依赖这么多其他文件才能说清自己的职责时，读者必须同时打开所有这些
/// 文件——理解成本已经超过了拆分带来的边界收益。
const MIN_DEPENDENCIES_FOR_SPLIT_SMELL: usize = 3;

/// 触发判定的最多符号数。
///
/// 与 [`MIN_DEPENDENCIES_FOR_SPLIT_SMELL`] 配对：内容这么少，却要跨这么多文件。
const MAX_SYMBOLS_FOR_SPLIT_SMELL: usize = 2;

/// 不参与判定的文件名。
///
/// 入口文件（`lib.rs`、`main.rs`、`mod.rs`）按惯例承担重新导出与子模块声明，
/// 符号少而依赖多是它们的本职，不是拆分过细。
const ENTRY_FILE_NAMES: &[&str] = &["lib.rs", "main.rs", "mod.rs"];

/// FR-019：过度模块拆分。
///
/// 报告「内容很少却横跨多个文件」的模块：理解它所需的依赖数已经超过它自身承载
/// 的信息量。判据是纯结构的——依赖数来自模块图，符号数来自符号索引——因此
/// 不会因为命名风格而误判。
pub struct OverModularizationChecker;

impl Checker for OverModularizationChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("over-modularization"),
            "内容很少却依赖多个文件的模块增加了不必要的理解成本",
            RuleSeverity::Advisory,
            "over_modularization",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        let mut paths: Vec<&str> = context.module_graph.files().collect();
        paths.sort_unstable();
        for path in paths {
            if is_test_path(path) {
                continue;
            }
            if ENTRY_FILE_NAMES
                .iter()
                .any(|name| path.ends_with(&format!("/{name}")) || path == *name)
            {
                continue;
            }
            let dependencies = context.module_graph.dependencies_of(path);
            if dependencies.len() < MIN_DEPENDENCIES_FOR_SPLIT_SMELL {
                continue;
            }
            // 只数本文件定义的顶层符号：方法归属于 impl 块，不能算作文件内容。
            let symbol_count = context
                .index
                .in_file(path)
                .filter(|symbol| symbol.parent.is_none())
                .count();
            if symbol_count > MAX_SYMBOLS_FOR_SPLIT_SMELL {
                continue;
            }
            let Some(tree) = context.tree(path) else {
                continue;
            };
            let Some(file) = context.source(path) else {
                continue;
            };
            let mut sorted_dependencies: Vec<&str> = dependencies;
            sorted_dependencies.sort_unstable();
            findings.push(
                Finding::new(
                    rule.id.clone(),
                    FindingKind::NeedlessPattern,
                    format!(
                        "`{path}` 只有 {symbol_count} 个定义，却依赖 {} 个文件",
                        sorted_dependencies.len()
                    ),
                    format!(
                        "`{path}` 自身只定义 {symbol_count} 个顶层项，\
                         却引用了 {} 个项目内文件（{}）。\
                         读者必须同时打开这些文件才能理解这个模块，\
                         而它承载的信息量不足以补偿这份成本。\
                         通常的成因是把一个内聚的实现按「将来可能独立」预先拆开。",
                        sorted_dependencies.len(),
                        sorted_dependencies.join("、")
                    ),
                    vec![location_for(file, &tree.root())],
                )
                .with_severity(rule.effective_severity())
                .with_fact("symbol_count", symbol_count.to_string())
                .with_fact("dependency_count", sorted_dependencies.len().to_string())
                .with_fact("dependencies", sorted_dependencies.join(","))
                .with_fact("needs_human_confirmation", "true")
                .with_remediation(Remediation::new(
                    "确认这些依赖是否服务于同一个职责；若是，考虑合并回同一个文件，\
                     或把被反复引用的部分下沉为更稳定的底层模块",
                )),
            );
        }
        findings
    }
}

// ---------------------------------------------------------------------------
// FR-020 样板代码
// ---------------------------------------------------------------------------

/// 判定「匹配分支逐字重复」的最少分支数。
const MIN_IDENTICAL_MATCH_ARMS: usize = 3;

/// FR-020：样板代码。
///
/// 识别可以被独立论证的低价值结构，**不**推断代码作者。当前实现覆盖两个信号：
///
/// 1. **空占位函数**——函数体只有 `todo!()` / `unimplemented!()` / `unreachable!()`。
/// 2. **逐字重复的匹配分支**——同一个 `match` 里有多个分支体完全相同。
///
/// 这两个信号都与「谁写的」无关：它们只说明这段代码当前不承载信息。
pub struct BoilerplateCodeChecker;

impl Checker for BoilerplateCodeChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("boilerplate-code"),
            "空的占位实现与逐字复制的分支不承载信息",
            RuleSeverity::Advisory,
            "boilerplate_code",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        for symbol in context.index.symbols() {
            if !symbol.kind.is_callable() || !symbol.file.ends_with(RUST_EXTENSION) {
                continue;
            }
            if is_test_path(&symbol.file) {
                continue;
            }
            let Some(tree) = context.tree(&symbol.file) else {
                continue;
            };
            let Some(node) = find_node(tree, &symbol.span) else {
                continue;
            };
            if is_rust_test_code(tree, &node) {
                continue;
            }
            let Some(file) = context.source(&symbol.file) else {
                continue;
            };
            let Some(body) = node.child_by_field_name("body") else {
                continue;
            };
            let Some(macro_name) = placeholder_macro(tree, &body) else {
                continue;
            };
            findings.push(
                Finding::new(
                    rule.id.clone(),
                    FindingKind::BoilerplateCode,
                    format!("`{}` 的函数体只有 `{macro_name}!()`", symbol.qualified_name),
                    format!(
                        "`{}` 的函数体只有 `{macro_name}!()`，没有实现任何行为。\
                         如果这是有意留下的扩展点，请补充注释说明它保护的是哪个不变式；\
                         如果只是尚未填写，它现在只会让调用方在运行到一半时才失败。",
                        symbol.qualified_name
                    ),
                    vec![location_for(file, &node)],
                )
                .with_severity(rule.effective_severity())
                .with_fact("placeholder", macro_name)
                .with_fact("needs_human_confirmation", "true")
                .with_remediation(Remediation::new(
                    "补充实现，或把该函数改为返回错误并在文档中说明其契约",
                )),
            );
        }
        findings.extend(identical_match_arms(context, &rule));
        findings
    }
}

/// 取出函数体中的占位宏名。
///
/// 只在函数体**只有**这一条宏调用时返回：混合了其他逻辑的函数即便包含 `todo!()`
/// 也不是空壳。这里描述的是代码质量，与代码由谁编写无关。
fn placeholder_macro<'tree>(tree: &'tree ParseTree, body: &Node<'tree>) -> Option<&'static str> {
    let mut cursor = body.walk();
    let statements: Vec<Node<'tree>> = body.named_children(&mut cursor).collect();
    let [only] = statements.as_slice() else {
        return None;
    };
    if only.kind() != "expression_statement" && only.kind() != "macro_invocation" {
        return None;
    }
    let target = if only.kind() == "expression_statement" {
        only.named_child(0)?
    } else {
        *only
    };
    if target.kind() != "macro_invocation" {
        return None;
    }
    let macro_node = target.child_by_field_name("macro")?;
    let name = tree.node_text(&macro_node).ok()?;
    PLACEHOLDER_MACROS
        .iter()
        .find(|candidate| **candidate == name)
        .copied()
}

/// 检测同一个 `match` 里逐字重复的分支。
///
/// 归一化只去掉注释与首尾空白：分支体连字面量都相同才是复制粘贴的强信号。
/// 反过来，仅形状相同而字面量不同的分支留给 `duplicate-code` 规则，
/// 在那里阈值更严格、上下文更完整。
fn identical_match_arms(context: &Context<'_>, rule: &Rule) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (path, tree) in context.trees() {
        if is_test_path(path) {
            continue;
        }
        let Some(file) = context.source(path) else {
            continue;
        };
        for node in tree.descendants() {
            if node.kind() != "match_expression" {
                continue;
            }
            if is_rust_test_code(tree, &node) {
                continue;
            }
            let mut groups: HashMap<String, Vec<String>> = HashMap::new();
            // 分支挂在 `body` 字段的 `match_block` 下，不是 `match_expression`
            // 的直接子节点：tree-sitter-rust 把 `match scrutinee { ... }` 拆成
            // `value` 与 `body` 两个字段，直接遍历子节点只会看到被匹配的表达式。
            let Some(body) = node.child_by_field_name("body") else {
                continue;
            };
            let mut cursor = body.walk();
            for arm in body.named_children(&mut cursor) {
                if arm.kind() != "match_arm" {
                    continue;
                }
                let Some(value) = arm.child_by_field_name("value") else {
                    continue;
                };
                let Ok(text) = tree.node_text(&value) else {
                    continue;
                };
                let key = text
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.starts_with("//"))
                    .collect::<Vec<_>>()
                    .join(" ");
                let pattern = arm
                    .child_by_field_name("pattern")
                    .and_then(|pattern| tree.node_text(&pattern).ok())
                    .unwrap_or_default()
                    .to_owned();
                groups.entry(key).or_default().push(pattern);
            }
            let mut duplicated: Vec<(String, Vec<String>)> = groups
                .into_iter()
                .filter(|(_, patterns)| patterns.len() >= MIN_IDENTICAL_MATCH_ARMS)
                .collect();
            duplicated.sort_by(|a, b| a.0.cmp(&b.0));
            for (body_text, patterns) in duplicated {
                findings.push(
                    Finding::new(
                        rule.id.clone(),
                        FindingKind::BoilerplateCode,
                        format!("{} 个匹配分支的返回值完全相同", patterns.len()),
                        format!(
                            "分支 {} 的函数体逐字相同（`{}`）。\
                             复制出来的分支通常意味着这个 `match` 没有真正区分情况，\
                             要么收敛为通配分支，要么为各分支补充差异化的处理。",
                            patterns
                                .iter()
                                .map(|pattern| format!("`{pattern}`"))
                                .collect::<Vec<_>>()
                                .join("、"),
                            body_text
                        ),
                        vec![location_for(file, &node)],
                    )
                    .with_severity(rule.effective_severity())
                    .with_fact("arm_count", patterns.len().to_string())
                    .with_fact("patterns", patterns.join(","))
                    .with_fact("needs_human_confirmation", "true")
                    .with_remediation(Remediation::new(
                        "确认这些分支是否应当区别处理；\
                         若返回值本就相同，用一个通配分支替代",
                    )),
                );
            }
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::with_context;

    #[test]
    fn detects_flag_heavy_signature() {
        with_context(
            &[(
                "src/lib.rs",
                "pub fn render(\n    path: &str,\n    verbose: bool,\n    dry_run: bool,\n    retries: Option<u32>,\n    strict: bool,\n) -> String {\n    String::new()\n}\n",
            )],
            |context| {
                let findings = OverConfigurationChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].kind, FindingKind::UselessAbstraction);
                assert!(
                    findings[0]
                        .facts
                        .iter()
                        .any(|(key, value)| key == "flag_count" && value == "4"),
                    "{:?}",
                    findings[0].facts
                );
            },
        );
    }

    #[test]
    fn ordinary_signature_is_not_reported() {
        // 三个参数里一个是开关是常见写法；阈值必须排除这类正常签名。
        with_context(
            &[(
                "src/lib.rs",
                "pub fn render(path: &str, verbose: bool, out: &mut String) -> bool {\n    true\n}\n",
            )],
            |context| {
                assert_eq!(
                    OverConfigurationChecker.check(context),
                    Vec::new(),
                    "普通签名不应产生发现"
                );
            },
        );
    }

    #[test]
    fn detects_option_parameter_that_is_unconditionally_defaulted() {
        with_context(
            &[(
                "src/lib.rs",
                "pub fn load(path: &str, limit: Option<usize>) -> Vec<u8> {\n    let limit = limit.unwrap_or_default();\n    Vec::new()\n}\n",
            )],
            |context| {
                let findings = OverConfigurationChecker.check(context);
                assert_eq!(findings.len(), 1, "无条件兜底的可选参数是假配置");
                assert!(
                    findings[0]
                        .facts
                        .iter()
                        .any(|(key, value)| key == "parameter" && value == "limit"),
                    "{:?}",
                    findings[0].facts
                );
            },
        );
    }

    #[test]
    fn conditional_fallback_is_not_reported() {
        // 分支里的兜底是真实的配置行为：调用方的选择确实改变了结果。
        with_context(
            &[(
                "src/lib.rs",
                "pub fn load(path: &str, limit: Option<usize>) -> Vec<u8> {\n    let limit = if path.is_empty() { 0 } else { limit.unwrap_or(64) };\n    Vec::new()\n}\n",
            )],
            |context| {
                assert_eq!(
                    OverConfigurationChecker.check(context),
                    Vec::new(),
                    "分支兜底不应产生发现"
                );
            },
        );
    }

    #[test]
    fn detects_thin_module_with_many_dependencies() {
        with_context(
            &[
                (
                    "src/lib.rs",
                    "mod alpha;\nmod beta;\nmod gamma;\nmod thin;\n",
                ),
                ("src/alpha.rs", "use crate::beta;\npub fn a() {}\n"),
                ("src/beta.rs", "use crate::gamma;\npub fn b() {}\n"),
                ("src/gamma.rs", "pub fn c() {}\n"),
                (
                    "src/thin.rs",
                    "use crate::alpha;\nuse crate::beta;\nuse crate::gamma;\npub fn only() {}\n",
                ),
            ],
            |context| {
                let findings = OverModularizationChecker.check(context);
                let thin: Vec<_> = findings
                    .iter()
                    .filter(|finding| finding.locations.iter().any(|l| l.path == "src/thin.rs"))
                    .collect();
                assert_eq!(thin.len(), 1, "薄模块跨多文件应被发现：{findings:?}");
            },
        );
    }

    #[test]
    fn module_with_enough_content_is_not_reported() {
        // 依赖数相同，但自身承载了足够的信息，理解成本由内容抵消。
        with_context(
            &[
                ("src/lib.rs", "mod alpha;\nmod beta;\nmod gamma;\nmod fat;\n"),
                ("src/alpha.rs", "use crate::beta;\npub fn a() {}\n"),
                ("src/beta.rs", "use crate::gamma;\npub fn b() {}\n"),
                ("src/gamma.rs", "pub fn c() {}\n"),
                (
                    "src/fat.rs",
                    "use crate::alpha;\nuse crate::beta;\nuse crate::gamma;\n\npub struct One;\npub struct Two;\npub struct Three;\npub fn go() {}\n",
                ),
            ],
            |context| {
                let findings = OverModularizationChecker.check(context);
                assert!(
                    findings.iter().all(|finding| finding
                        .locations
                        .iter()
                        .all(|location| location.path != "src/fat.rs")),
                    "内容充足的模块不应产生发现：{findings:?}"
                );
            },
        );
    }

    #[test]
    fn entry_file_is_not_reported_as_over_split() {
        // `lib.rs` 的职责就是声明子模块，符号少而依赖多是本职。
        with_context(
            &[
                (
                    "src/lib.rs",
                    "use crate::alpha;\nuse crate::beta;\nuse crate::gamma;\n",
                ),
                ("src/alpha.rs", "use crate::beta;\npub fn a() {}\n"),
                ("src/beta.rs", "use crate::gamma;\npub fn b() {}\n"),
                ("src/gamma.rs", "pub fn c() {}\n"),
            ],
            |context| {
                let findings = OverModularizationChecker.check(context);
                assert!(
                    findings
                        .iter()
                        .all(|finding| finding.locations.iter().all(|l| l.path != "src/lib.rs")),
                    "入口文件不应产生发现：{findings:?}"
                );
            },
        );
    }

    #[test]
    fn detects_placeholder_function_body() {
        with_context(
            &[(
                "src/lib.rs",
                "pub trait Store {\n    fn save(&self, key: &str, value: &str);\n}\n\npub struct Memory;\nimpl Store for Memory {\n    fn save(&self, key: &str, value: &str) {\n        unimplemented!()\n    }\n}\n",
            )],
            |context| {
                let findings = BoilerplateCodeChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].kind, FindingKind::BoilerplateCode);
                assert!(
                    findings[0].title.contains("unimplemented"),
                    "标题要点出占位宏"
                );
                // 判定对象是代码质量，不得暗示作者身份。
                assert!(
                    !findings[0].detail.contains("AI"),
                    "不得推断代码作者：{}",
                    findings[0].detail
                );
            },
        );
    }

    #[test]
    fn implemented_function_with_macro_call_is_not_reported() {
        with_context(
            &[(
                "src/lib.rs",
                "pub fn parse(text: &str) -> u32 {\n    let value = text.parse().unwrap();\n    value + 1\n}\n",
            )],
            |context| {
                assert_eq!(
                    BoilerplateCodeChecker.check(context),
                    Vec::new(),
                    "有实现的函数不应产生发现"
                );
            },
        );
    }

    #[test]
    fn detects_identical_match_arms() {
        with_context(
            &[(
                "src/lib.rs",
                "pub enum Kind {\n    A,\n    B,\n    C,\n}\n\npub fn label(kind: Kind) -> &'static str {\n    match kind {\n        Kind::A => \"unknown\",\n        Kind::B => \"unknown\",\n        Kind::C => \"unknown\",\n    }\n}\n",
            )],
            |context| {
                let findings = BoilerplateCodeChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert!(
                    findings[0]
                        .facts
                        .iter()
                        .any(|(key, value)| key == "arm_count" && value == "3"),
                    "{:?}",
                    findings[0].facts
                );
            },
        );
    }

    #[test]
    fn distinct_match_arms_are_not_reported() {
        with_context(
            &[(
                "src/lib.rs",
                "pub enum Kind {\n    A,\n    B,\n    C,\n}\n\npub fn label(kind: Kind) -> &'static str {\n    match kind {\n        Kind::A => \"a\",\n        Kind::B => \"b\",\n        Kind::C => \"c\",\n    }\n}\n",
            )],
            |context| {
                assert_eq!(
                    BoilerplateCodeChecker.check(context),
                    Vec::new(),
                    "分支有区分时不应产生发现"
                );
            },
        );
    }

    #[test]
    fn split_parameters_respects_nesting() {
        assert_eq!(split_parameters("(a: u32, b: u32)").len(), 2);
        assert_eq!(
            split_parameters("(map: HashMap<String, Vec<u8>>, f: impl Fn(u32, u32) -> u32)").len(),
            2,
            "泛型与闭包签名里的逗号不能当作参数分隔符"
        );
        // 接收者占据参数列表中的一项；是否转发它由 `is_flag_parameter` 判定。
        assert_eq!(split_parameters("(&self)"), vec!["&self"]);
    }

    #[test]
    fn is_flag_parameter_recognises_bool_and_option() {
        assert!(is_flag_parameter("verbose: bool"));
        assert!(is_flag_parameter("retries: Option<u32>"));
        assert!(!is_flag_parameter("path: &str"));
        assert!(!is_flag_parameter("items: Vec<u8>"));
    }

    #[test]
    fn option_parameter_names_skips_unnamed_parameters() {
        assert_eq!(
            option_parameter_names("(limit: Option<usize>)"),
            vec!["limit"]
        );
        assert_eq!(
            option_parameter_names("(&self, limit: Option<usize>)"),
            vec!["limit"]
        );
        // trait 方法签名允许省略参数名，此时无法定位兜底调用，直接跳过。
        assert_eq!(
            option_parameter_names("(Option<usize>)"),
            Vec::<String>::new()
        );
    }
}
