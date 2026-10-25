//! 代码风格与文档规则。
//!
//! 覆盖需求文档 5.2：错误处理（FR-009）、函数质量（FR-008）、命名（FR-007）、
//! 注释（FR-013）与公共 API 文档（FR-014）。

use pf_core::finding::{Finding, FindingKind, Remediation};
use pf_core::ids::RuleId;
use pf_core::rule::{Rule, RuleSeverity};
use pf_parser::ParseTree;
use pf_symbol::{Symbol, SymbolKind};
use tree_sitter::Node;

use super::{is_rust_test_code, is_test_path, location_for, Checker, Context, RUST_EXTENSION};

// ---------------------------------------------------------------------------
// 具体规则
// ---------------------------------------------------------------------------

/// FR-014：公共 API 缺少文档。
pub struct MissingDocumentationChecker;

impl Checker for MissingDocumentationChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("public-api-missing-docs"),
            "公共 API（pub 函数、结构体、枚举、trait）必须带有文档注释或 docstring",
            RuleSeverity::Mandatory,
            "public_api_missing_docs",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        context
            .index
            .public_api()
            .filter(|symbol| !symbol.documented)
            .map(|symbol| {
                // 缺少文档是可以在符号表上直接核实的事实：这个条目确实没有文档。
                super::confirmed(Finding::new(
                    rule.id.clone(),
                    FindingKind::MissingDocumentation,
                    format!(
                        "公共{} `{}` 缺少文档",
                        symbol.kind.display_name(),
                        symbol.name
                    ),
                    format!(
                        "`{}` 是对外可见的{}，但没有文档注释。调用方无法从签名判断其契约、错误行为与副作用。",
                        symbol.qualified_name,
                        symbol.kind.display_name()
                    ),
                    vec![symbol.location.clone()],
                )
                .with_severity(rule.effective_severity())
                .with_remediation(Remediation::new(format!(
                    "为 `{}` 补充文档：说明用途、参数含义、错误条件与副作用",
                    symbol.name
                ))))
            })
            .collect()
    }
}

/// FR-007：命名不符合语言惯例。
pub struct NamingConventionChecker;

impl Checker for NamingConventionChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("naming-convention"),
            "Rust 函数与变量使用 snake_case，类型使用 PascalCase；Python 函数使用 snake_case",
            RuleSeverity::Project,
            "naming_convention",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        for symbol in context.index.symbols() {
            let Some(reason) = naming_violation(symbol) else {
                continue;
            };
            findings.push(
                // 命名是否符合语言惯例只取决于标识符文本，不需要业务上下文。
                super::confirmed(
                    Finding::new(
                        rule.id.clone(),
                        FindingKind::NamingViolation,
                        format!("`{}` 的命名不符合惯例", symbol.name),
                        format!("{} 应为 {}。", symbol.name, reason),
                        vec![symbol.location.clone()],
                    )
                    .with_severity(rule.effective_severity())
                    .with_fact("expected", reason)
                    .with_remediation(Remediation::new(format!(
                        "将 `{}` 重命名为符合惯例的形式",
                        symbol.name
                    ))),
                ),
            );
        }
        findings
    }
}

/// 判断命名是否违规，返回期望风格说明。
fn naming_violation(symbol: &Symbol) -> Option<&'static str> {
    let name = symbol.name.as_str();
    if name.is_empty() {
        return None;
    }
    match symbol.kind {
        SymbolKind::Struct
        | SymbolKind::Enum
        | SymbolKind::Trait
        | SymbolKind::Class
        | SymbolKind::TypeAlias => {
            if !is_pascal_case(name) {
                return Some("PascalCase");
            }
        }
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Variable => {
            if !is_snake_case(name) {
                return Some("snake_case");
            }
        }
        SymbolKind::Constant | SymbolKind::Static => {
            if !is_screaming_snake_case(name) {
                return Some("SCREAMING_SNAKE_CASE");
            }
        }
        SymbolKind::Impl | SymbolKind::Module => {}
    }
    None
}

/// `PascalCase`：首字母大写，且不含下划线。
fn is_pascal_case(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_uppercase() => {}
        _ => return false,
    }
    !name.contains('_')
}

/// `snake_case`：全小写，可含数字与下划线，不以数字开头。
fn is_snake_case(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `SCREAMING_SNAKE_CASE`。
fn is_screaming_snake_case(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// FR-009：库代码中不合理的错误处理（裸 `unwrap` / `expect`）。
pub struct UnwrapInLibraryChecker;

impl Checker for UnwrapInLibraryChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("no-unwrap-in-library"),
            "库代码（非测试）不得使用 unwrap/expect，应向上传播错误",
            RuleSeverity::Project,
            "no_unwrap_in_library",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        for (path, tree) in context.trees() {
            if !path.ends_with(RUST_EXTENSION) || is_test_path(path) {
                continue;
            }
            for node in tree.descendants() {
                if node.kind() != "call_expression" {
                    continue;
                }
                if is_rust_test_code(tree, &node) {
                    continue;
                }
                let Some(callee) = node.child_by_field_name("function") else {
                    continue;
                };
                let Ok(name) = tree.node_text(&callee) else {
                    continue;
                };
                // 兼容 `foo()`、`foo::unwrap()` 与 `value.unwrap()` 三种写法。
                let short = name
                    .rsplit("::")
                    .next()
                    .unwrap_or(name)
                    .rsplit('.')
                    .next()
                    .unwrap_or(name);
                if short != "unwrap" && short != "expect" {
                    continue;
                }
                let Some(file) = context.source(path) else {
                    continue;
                };
                let location = location_for(file, &callee);
                findings.push(
                    // 语法树上确实有一个 unwrap/expect 调用：这是可复算的事实。
                    super::confirmed(
                        Finding::new(
                            rule.id.clone(),
                            FindingKind::UnhandledError,
                            format!("库代码中使用了 `{short}()`"),
                            format!(
                                "`{path}` 中的 `{short}()` 会在错误发生时直接 panic。\
                             库代码应返回错误让调用方决定如何处理。"
                            ),
                            vec![location],
                        )
                        .with_severity(rule.effective_severity())
                        .with_fact("callee", short)
                        .with_remediation(Remediation::new(
                            "改用 `?` 传播错误，或在确实不可能失败时用类型与不变量说明原因",
                        )),
                    ),
                );
            }
        }
        findings
    }
}

/// Rust 源文件的扩展名（含点号）。
///
/// 提成常量而不是散落的字面量：大小写不敏感的匹配只在这一处决定，避免不同
/// 检查器对 `foo.RS`、`foo.rs` 给出不同结论。
pub struct EmptyErrorHandlerChecker;

impl Checker for EmptyErrorHandlerChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("no-empty-error-handler"),
            "不得使用空的异常处理块吞没错误",
            RuleSeverity::Mandatory,
            "no_empty_error_handler",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        for (path, tree) in context.trees() {
            for node in tree.descendants() {
                let is_empty_handler = match path.rsplit('.').next().unwrap_or_default() {
                    "py" => node.kind() == "except_clause" && empty_block(tree, &node),
                    _ => false,
                };
                if !is_empty_handler {
                    continue;
                }
                let Some(file) = context.source(path) else {
                    continue;
                };
                findings.push(
                    // except 块体为空可以直接从语法树确认。
                    super::confirmed(
                        Finding::new(
                            rule.id.clone(),
                            FindingKind::SwallowedError,
                            "空的异常处理块吞没了错误",
                            format!(
                                "`{path}` 中的 except 块没有任何处理逻辑，失败会被静默忽略，\
                             使问题难以定位。"
                            ),
                            vec![location_for(file, &node)],
                        )
                        .with_severity(rule.effective_severity())
                        .with_remediation(Remediation::new(
                            "至少记录日志，或显式说明为什么可以安全忽略该异常",
                        )),
                    ),
                );
            }
        }
        findings
    }
}

/// 判断语法节点的块体是否为空（或只有 `pass` / `...`）。
fn empty_block(tree: &ParseTree, node: &Node<'_>) -> bool {
    let mut cursor = node.walk();
    let body = node.child_by_field_name("body").or_else(|| {
        node.children(&mut cursor)
            .find(|child| child.kind() == "block")
    });
    let Some(body) = body else {
        return false;
    };
    let mut body_cursor = body.walk();
    let children: Vec<_> = body.children(&mut body_cursor).collect();
    match children.len() {
        0 => true,
        1 => {
            let text = tree.node_text(&children[0]).unwrap_or_default().trim();
            text == "pass" || text == "..."
        }
        _ => false,
    }
}

/// FR-008：函数过长。
pub struct FunctionTooLongChecker;

impl FunctionTooLongChecker {
    /// 触发告警的行数阈值。
    const THRESHOLD_LINES: u32 = 80;
}

impl Checker for FunctionTooLongChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("function-too-long"),
            "函数体超过 80 行时应拆分为更小的职责单元",
            RuleSeverity::Advisory,
            "function_too_long",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        context
            .index
            .symbols()
            .iter()
            .filter(|symbol| symbol.kind.is_callable())
            .filter(|symbol| symbol.body_lines > Self::THRESHOLD_LINES)
            .map(|symbol| {
                // 行数是多少可以直接数出来：超过阈值是一个可复算的事实。
                super::confirmed(
                    Finding::new(
                        rule.id.clone(),
                        FindingKind::FunctionTooComplex,
                        format!("函数 `{}` 过长（{} 行）", symbol.name, symbol.body_lines),
                        format!(
                            "`{}` 占 {} 行，超过 {} 行阈值。长函数通常意味着承担了多项职责，\
                         测试与修改成本都会上升。",
                            symbol.qualified_name,
                            symbol.body_lines,
                            Self::THRESHOLD_LINES
                        ),
                        vec![symbol.location.clone()],
                    )
                    .with_severity(rule.effective_severity())
                    .with_fact("lines", symbol.body_lines.to_string())
                    .with_fact("threshold", Self::THRESHOLD_LINES.to_string())
                    .with_remediation(Remediation::new(
                        "按职责拆分为多个私有函数；若拆分后需要共享状态，考虑引入数据结构",
                    )),
                )
            })
            .collect()
    }
}

/// FR-008：参数过多。
pub struct TooManyParametersChecker;

impl TooManyParametersChecker {
    /// 触发告警的参数个数阈值。
    const THRESHOLD_PARAMS: usize = 5;
}

impl Checker for TooManyParametersChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("too-many-parameters"),
            "函数参数超过 5 个时应改用结构体承载",
            RuleSeverity::Advisory,
            "too_many_parameters",
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
            let Some(node) = tree.descendants().find(|node| {
                node.start_byte() == symbol.span.start && node.end_byte() == symbol.span.end
            }) else {
                continue;
            };
            let Some(params) = node.child_by_field_name("parameters") else {
                continue;
            };
            let mut cursor = params.walk();
            let count = params.children(&mut cursor).filter(Node::is_named).count();
            if count <= Self::THRESHOLD_PARAMS {
                continue;
            }
            findings.push(
                // 形参个数同样只取决于签名文本。
                super::confirmed(
                    Finding::new(
                        rule.id.clone(),
                        FindingKind::FunctionTooComplex,
                        format!("函数 `{}` 有 {count} 个参数", symbol.name),
                        "参数过多会让调用点难以读对，也容易把不相关的字段耦合在一起。".to_owned(),
                        vec![symbol.location.clone()],
                    )
                    .with_severity(rule.effective_severity())
                    .with_fact("parameters", count.to_string())
                    .with_remediation(Remediation::new(
                        "把相关参数收敛为一个具名结构体，让调用点自解释",
                    )),
                ),
            );
        }
        findings
    }
}

/// FR-013：注释质量（冗余注释：注释内容与代码文本重复）。
pub struct CommentQualityChecker;

impl Checker for CommentQualityChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("redundant-comment"),
            "注释不应重复描述代码文本本身，而应解释意图与约束",
            RuleSeverity::Advisory,
            "redundant_comment",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        for (path, tree) in context.trees() {
            let Some(file) = context.source(path) else {
                continue;
            };
            for node in tree.descendants() {
                if node.kind() != "line_comment" && node.kind() != "comment" {
                    continue;
                }
                let Ok(text) = tree.node_text(&node) else {
                    continue;
                };
                let body = text.trim_start_matches("//").trim_start_matches('#').trim();
                // 取紧随其后的第一个兄弟节点作为「被注释的代码」。
                let Some(next) = node.next_sibling() else {
                    continue;
                };
                let Ok(code) = tree.node_text(&next) else {
                    continue;
                };
                // 被声明符号的名字（`fn count` 的 `count`）：注释如果只是在念这个
                // 名字，就没有说明意图或约束。
                let declared = next
                    .child_by_field_name("name")
                    .and_then(|name_node| tree.node_text(&name_node).ok());
                if is_comment_redundant(body, code, declared) {
                    findings.push(
                        Finding::new(
                            rule.id.clone(),
                            FindingKind::CommentQuality,
                            "注释只是重复了代码文本",
                            format!(
                                "`{path}` 中的注释「{body}」与紧随的代码文本几乎相同，\
                                 没有提供额外信息。"
                            ),
                            vec![location_for(file, &node)],
                        )
                        .with_severity(rule.effective_severity())
                        .with_remediation(Remediation::new(
                            "删除该注释，或改写为说明「为什么这样写」以及存在哪些约束",
                        )),
                    );
                }
            }
        }
        findings
    }
}

/// 注释被视为冗余的最短长度（归一化之后的字符数）。
///
/// 更短的注释（`// ok`、`// id`）在任何上下文里都可能是巧合，不判定。
const MIN_REDUNDANT_COMMENT_CHARS: usize = 4;

/// 符号名参与判定所需的最短长度。
///
/// `id`、`len`、`get` 这类短名字会作为普通单词出现在大量英文注释里，
/// 拿它们做子串匹配会把「说明意图的注释」误判成复述。
const MIN_DECLARED_NAME_CHARS: usize = 4;

/// 判断注释是否只是复述代码。
///
/// 两级判定，都可解释且不依赖语义理解：
///
/// 1. 注释文本与紧随的代码文本几乎相同（归一化后相等，或注释是代码的前缀）。
/// 2. 注释文本就是被声明符号的名字本身，允许名字里的 `_` / `-` 被写成空格——
///    例如 `// increment the counter` 挂在 `fn increment_the_counter()` 上。
///    这类注释只是在念名字，没有说明「为什么这样写」与「有哪些约束」。
///
/// 第二级刻意要求**相等**而不是「包含」：说明意图的注释几乎总会以同源动词开头
/// （`validate` → `// validates the id format before caching`），用包含关系
/// 会把最有价值的注释全部误判。之所以需要第二级：Rust 声明的代码文本总以
/// `pub` / `fn` 开头，第一级几乎不可能命中，规则会退化成永不触发。
fn is_comment_redundant(comment: &str, code: &str, declared_name: Option<&str>) -> bool {
    let normalize = |text: &str| -> String {
        text.chars()
            .filter(|c| !c.is_whitespace() && !matches!(c, '.' | ':' | ';' | ',' | '(' | ')'))
            .flat_map(char::to_lowercase)
            .collect()
    };
    let comment = normalize(comment);
    let code = normalize(code);
    if comment.len() < MIN_REDUNDANT_COMMENT_CHARS || code.is_empty() {
        return false;
    }
    if comment == code || code.starts_with(&comment) {
        return true;
    }
    match declared_name {
        Some(name) => {
            // 名字里的分隔符一律去掉：`increment_the_counter` 与
            // `increment the counter` 应当视为同一条文本。
            let name = normalize(&name.replace(['_', '-'], " "));
            comment.len() >= MIN_DECLARED_NAME_CHARS && comment == name
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redundant_comment_detection() {
        // 第一级：注释与代码文本几乎相同。
        assert!(is_comment_redundant(
            "increment the counter",
            "increment the counter",
            None
        ));
        assert!(is_comment_redundant(
            "returns the sum",
            "returns the sum",
            None
        ));
        // 第二级：注释就是被声明符号的名字（下划线可写作空格）。
        assert!(is_comment_redundant(
            "count",
            "fn count() {}",
            Some("count")
        ));
        assert!(is_comment_redundant(
            "increment the counter",
            "fn increment_the_counter() {}",
            Some("increment_the_counter")
        ));
        // 说明「为什么」的注释不算冗余，即使与名字同源。
        assert!(!is_comment_redundant(
            "validates the id format before caching",
            "fn validate() {}",
            Some("validate")
        ));
        assert!(!is_comment_redundant(
            "必须持有锁，否则会与后台刷新竞争",
            "refresh()",
            Some("refresh")
        ));
        // 过短的注释与过短的名字都不判定。
        assert!(!is_comment_redundant("add", "x", None));
        assert!(!is_comment_redundant("id", "fn validate() {}", Some("id")));
    }

    #[test]
    fn naming_helpers_classify_correctly() {
        assert!(is_snake_case("good_name"));
        assert!(is_snake_case("a1"));
        assert!(!is_snake_case("BadName"));
        assert!(!is_snake_case("1bad"));
        assert!(is_pascal_case("GoodName"));
        assert!(!is_pascal_case("bad_name"));
        assert!(is_screaming_snake_case("MAX_SIZE"));
        assert!(!is_screaming_snake_case("max_size"));
    }
}
