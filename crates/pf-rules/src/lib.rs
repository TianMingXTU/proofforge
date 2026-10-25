//! 工程质量规则引擎与内置规则集。
//!
//! 需求文档 FR-007 至 FR-021：代码风格、函数质量、错误处理、资源管理、
//! 重复代码、注释质量，以及过度工程化（无效抽象、委托链、模式滥用、
//! 样板代码）检测。
//!
//! # 设计约定
//!
//! - 每条规则都是一个实现了 [`Checker`] 的纯函数式检查器：输入符号索引与
//!   语法树，输出 [`Finding`]。规则之间不共享可变状态，便于单独测试。
//! - 规则必须给出**结构事实**而不是裁决。判定「是否过度设计」需要结合业务
//!   上下文（需求文档 7.3），因此本 crate 只输出可疑结构与事实，语义判断
//!   由 `pf-review` 的 AI 审查层完成。
//! - 所有规则都遵守需求文档 1.3：默认不得修改用户代码，只能提出建议。

#![warn(missing_docs)]

use std::collections::HashMap;

use pf_core::finding::Finding;
use pf_core::ids::RuleId;
use pf_core::location::{Location, Span};
use pf_core::rule::{Rule, RuleSet};
use pf_core::source::SourceFile;
use pf_graph::{CallGraph, ModuleGraph};
use pf_parser::{ParseEngine, ParseTree};
use pf_symbol::SymbolIndex;
use tree_sitter::Node;

mod architecture;
mod conflict;
mod dependency;
mod duplication;
mod import;
mod over_engineering;
mod resource;
mod semantic;
mod standard;
mod style;

pub use architecture::{
    ArchitectureRuleChecker, CircularDependencyChecker, UncalledPublicApiChecker,
};
pub use conflict::{detect as detect_conflicts, ConflictKind, RuleConflict};
pub use dependency::UnusedDependencyChecker;
pub use import::{import_file, import_text, ImportError, ImportedStandard, SpecFormat};
pub use standard::render_markdown as render_standard;

pub use duplication::DuplicateCodeChecker;
pub use import::{exemptions_from_toml_file, exemptions_from_yaml, exemptions_from_yaml_file};
pub use over_engineering::{
    ForwardingWrapperChecker, NeedlessFactoryChecker, SingleImplementationTraitChecker,
};
/// 豁免类型定义在 [`pf_core::exempt`]（它们要能进 `Config`），这里重新导出，
/// 让「规则管理」的使用方只需依赖 `pf-rules`。
pub use pf_core::exempt::{Exemption, ExemptionError, ExemptionScope, ExemptionSet};
pub use resource::ResourceManagementChecker;
pub use semantic::{BoilerplateCodeChecker, OverConfigurationChecker, OverModularizationChecker};
pub use style::{
    CommentQualityChecker, EmptyErrorHandlerChecker, FunctionTooLongChecker,
    MissingDocumentationChecker, NamingConventionChecker, TooManyParametersChecker,
    UnwrapInLibraryChecker,
};

/// 规则执行上下文。
///
/// 把检查器需要的全部输入集中到一个不可变结构中，避免每条规则各自打开
/// 文件或重复解析。
pub struct Context<'a> {
    /// 符号索引。
    pub index: &'a SymbolIndex,
    /// 模块依赖图。
    pub module_graph: ModuleGraph,
    /// 调用图。
    pub call_graph: CallGraph,
    /// 源文件（按路径索引）。
    sources: HashMap<&'a str, &'a SourceFile>,
    /// 语法树（按路径索引）。
    trees: HashMap<String, ParseTree>,
    /// 解析失败的文件路径。
    parse_failures: Vec<String>,
}

impl<'a> Context<'a> {
    /// 构建上下文：解析全部可分析文件。
    ///
    /// 解析失败的文件不会让整体失败（需求文档 11.2），调用方可通过
    /// [`Self::parse_failures`] 获知并在报告中标注证据不完整。
    pub fn new(index: &'a SymbolIndex, sources: &'a [SourceFile]) -> Self {
        let engine = ParseEngine::new();
        let mut source_map = HashMap::new();
        for file in sources {
            source_map.insert(file.path.as_str(), file);
        }
        let mut trees = HashMap::new();
        let mut parse_failures = Vec::new();
        for file in sources {
            match engine.parse(file) {
                Ok(Some(tree)) => {
                    trees.insert(file.path.clone(), tree);
                }
                Ok(None) => {}
                Err(reason) => parse_failures.push(format!("{}: {reason}", file.path)),
            }
        }
        let paths: Vec<String> = sources.iter().map(|file| file.path.clone()).collect();
        Self {
            index,
            module_graph: ModuleGraph::build_with_files(index, paths),
            call_graph: CallGraph::build(index),
            sources: source_map,
            trees,
            parse_failures,
        }
    }

    /// 按路径取源文件。
    #[must_use]
    pub fn source(&self, path: &str) -> Option<&'a SourceFile> {
        self.sources.get(path).copied()
    }

    /// 按路径取语法树。
    #[must_use]
    pub fn tree(&self, path: &str) -> Option<&ParseTree> {
        self.trees.get(path)
    }

    /// 遍历全部已解析的语法树。
    pub fn trees(&self) -> impl Iterator<Item = (&str, &ParseTree)> {
        self.trees.iter().map(|(path, tree)| (path.as_str(), tree))
    }

    /// 解析失败的文件（路径 + 原因）。
    #[must_use]
    pub fn parse_failures(&self) -> &[String] {
        &self.parse_failures
    }

    /// 已解析文件数量。
    #[must_use]
    pub fn parsed_file_count(&self) -> usize {
        self.trees.len()
    }
}

/// 一条规则检查器。
///
/// 实现者必须是纯函数：相同的 [`Context`] 必须得到相同的 [`Finding`] 列表，
/// 不得依赖执行顺序或外部状态。
pub trait Checker: Send + Sync {
    /// 规则元数据（ID、名称、严重级别）。
    fn rule(&self) -> Rule;

    /// 执行检查，返回发现列表。
    fn check(&self, context: &Context<'_>) -> Vec<Finding>;
}

/// 把检查器与规则元数据配对注册。
pub struct RegisteredChecker {
    checker: Box<dyn Checker>,
}

impl RegisteredChecker {
    /// 包装一个检查器。
    #[must_use]
    pub fn new<C: Checker + 'static>(checker: C) -> Self {
        Self {
            checker: Box::new(checker),
        }
    }

    /// 规则元数据。
    #[must_use]
    pub fn rule(&self) -> Rule {
        self.checker.rule()
    }

    /// 执行检查。
    pub fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        self.checker.check(context)
    }
}

// ---------------------------------------------------------------------------
// 内置规则集
// ---------------------------------------------------------------------------

/// 返回全部内置检查器。
///
/// 新增内置规则时只需在这里追加一行，并在 `pf-cli` 中无需任何改动。
#[must_use]
pub fn builtin_checkers() -> Vec<RegisteredChecker> {
    vec![
        RegisteredChecker::new(MissingDocumentationChecker),
        RegisteredChecker::new(NamingConventionChecker),
        RegisteredChecker::new(UnwrapInLibraryChecker),
        RegisteredChecker::new(EmptyErrorHandlerChecker),
        RegisteredChecker::new(FunctionTooLongChecker),
        RegisteredChecker::new(TooManyParametersChecker),
        RegisteredChecker::new(CommentQualityChecker),
        RegisteredChecker::new(SingleImplementationTraitChecker),
        RegisteredChecker::new(ForwardingWrapperChecker),
        RegisteredChecker::new(NeedlessFactoryChecker),
        RegisteredChecker::new(OverConfigurationChecker),
        RegisteredChecker::new(OverModularizationChecker),
        RegisteredChecker::new(BoilerplateCodeChecker),
        RegisteredChecker::new(DuplicateCodeChecker),
        RegisteredChecker::new(CircularDependencyChecker),
        RegisteredChecker::new(UncalledPublicApiChecker),
        RegisteredChecker::new(UnusedDependencyChecker),
        RegisteredChecker::new(ResourceManagementChecker),
    ]
}

/// 内置规则集元数据（不含检查逻辑）。
#[must_use]
pub fn builtin_ruleset() -> RuleSet {
    let mut set = RuleSet::new("builtin", env!("CARGO_PKG_VERSION"));
    set.source = Some("built-in".to_owned());
    for checker in builtin_checkers() {
        set.push(checker.rule());
    }
    set
}

/// 按 ID 列表筛选启用的检查器。
#[must_use]
pub fn select_checkers(all: Vec<RegisteredChecker>, enabled: &[RuleId]) -> Vec<RegisteredChecker> {
    if enabled.is_empty() {
        return all;
    }
    all.into_iter()
        .filter(|checker| enabled.contains(&checker.rule().id))
        .collect()
}

// ---------------------------------------------------------------------------
// 共享辅助函数
// ---------------------------------------------------------------------------

/// Rust 源文件的扩展名（含点号）。
///
/// 提成常量而不是散落的字面量：大小写不敏感的匹配只在这一处决定，避免不同
/// 检查器对 `foo.RS`、`foo.rs` 给出不同结论。
pub(crate) const RUST_EXTENSION: &str = ".rs";

/// 把一条发现标记为「确定性静态命中」（`EvidenceStatus::StaticConfirmed`）。
///
/// 判据见 `docs/evidence-model.md`：规则能在语法树上直接指出违反位置——那一行
/// 确实是 `unwrap()`、那个标识符确实不符合惯例、那个函数确实有 120 行。这类
/// 事实可复算，因此是有证据的；但它不代表缺陷已被复现，所以不是 `Reproduced`。
///
/// 与之相对，需要结合业务上下文才能结论的规则必须留在 `Suspected`：单一实现的
/// trait 是否多余、两个函数是否算重复、公共 API 是否真的没人用，都不取决于
/// 代码长什么样。把猜测标成已验证正是 AGENTS.md 安全红线 5 禁止的事。
pub(crate) fn confirmed(finding: Finding) -> Finding {
    finding.with_status(pf_core::status::EvidenceStatus::StaticConfirmed)
}

/// 判断路径是否属于测试代码。
pub(crate) fn is_test_path(path: &str) -> bool {
    path.contains("/tests/")
        || path.starts_with("tests/")
        || path.contains("/benches/")
        || path.ends_with("_test.rs")
        || path.ends_with("test_utils.rs")
}

/// 判断属性节点是否把紧随的定义标记为测试代码。
///
/// 覆盖 `#[test]`、`#[tokio::test]` 这类属性测试宏，以及
/// `#[cfg(test)]` / `#[cfg(all(test, feature = "x"))]` 这类条件编译属性。
/// 逐 token 比较而不是整段 `contains("test")`：后者会把
/// `#[cfg(feature = "latest")]` 误判成测试代码。
pub(crate) fn is_test_attribute(text: &str) -> bool {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix('#')
        .unwrap_or(trimmed)
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    // 属性路径取到第一个 `(` 或 `=` 之前：`cfg(test)` 的路径是 `cfg`。
    let path = inner
        .split(['(', '='])
        .next()
        .unwrap_or(inner)
        .trim()
        .rsplit("::")
        .next()
        .unwrap_or("")
        .trim();
    if path == "test" {
        return true;
    }
    if path != "cfg" {
        return false;
    }
    inner
        .split([',', '(', ')', ' ', '='])
        .any(|token| token == "test")
}

/// 判断节点是否位于 Rust 测试代码中。
///
/// 与 [`is_test_path`] 互补：后者处理 `tests/` 下的独立文件，这里处理源码内
/// 的 `#[cfg(test)] mod tests` 与被 `#[test]` 标注的函数。测试代码的规范与库
/// 代码不同——`expect()`、超长函数、一次性包装在测试里都是可接受的写法——
/// 不区分会把大量噪音混进报告，让真正的缺陷淹没其中。
pub(crate) fn is_rust_test_code(tree: &ParseTree, node: &Node<'_>) -> bool {
    let mut current = Some(*node);
    while let Some(candidate) = current {
        if matches!(candidate.kind(), "mod_item" | "function_item") {
            let mut sibling = candidate.prev_sibling();
            while let Some(attribute) = sibling {
                if attribute.kind() == "attribute_item" {
                    if let Ok(text) = tree.node_text(&attribute) {
                        if is_test_attribute(text) {
                            return true;
                        }
                    }
                }
                sibling = attribute.prev_sibling();
            }
        }
        current = candidate.parent();
    }
    false
}

/// 判断函数定义节点是否位于 trait 契约之内。
///
/// 包含两种情况：定义在 `trait_item` 声明体内的默认方法，以及 `impl_item`
/// 为某个 trait 所做的实现方法。两者的函数体都必须遵守 trait 签名，转发是
/// 契约要求而非可消除的多余层次。
pub(crate) fn is_trait_contract_node(tree: &ParseTree, node: &Node<'_>) -> bool {
    let _ = tree;
    let mut current = node.parent();
    while let Some(candidate) = current {
        match candidate.kind() {
            "trait_item" => return true,
            "impl_item" => return candidate.child_by_field_name("trait").is_some(),
            _ => {}
        }
        current = candidate.parent();
    }
    false
}

/// 判断函数定义节点是否转发了调用方传入的参数。
///
/// `self` 接收者不算：`Bag::len(&self)` 没有可转发的参数，它返回的是派生值。
/// `parameters` 字段在 tree-sitter-rust 里始终存在（哪怕为空），因此这里数的
/// 是「除 self 之外的具名参数」，而不是判断字段本身是否存在。
pub(crate) fn forwards_arguments(tree: &ParseTree, node: &Node<'_>) -> bool {
    node.child_by_field_name("parameters")
        .and_then(|parameters| tree.node_text(&parameters).ok())
        .is_some_and(|text| {
            text.trim_start_matches('(')
                .trim_end_matches(')')
                .split(',')
                .map(str::trim)
                .any(|parameter| !parameter.is_empty() && !is_self_receiver(parameter))
        })
}

/// 判断形参文本是否是 `self` 接收者。
///
/// 覆盖 `self`、`&self`、`&mut self` 与带生命周期的 `&'a self`：这些都是接收者
/// 而不是被转发的参数。
pub(crate) fn is_self_receiver(parameter: &str) -> bool {
    let mut rest = parameter
        .trim()
        .strip_prefix('&')
        .unwrap_or(parameter)
        .trim();
    if let Some(tail) = rest.strip_prefix('\'') {
        // `&'a mut self`：去掉生命周期之后才可能看到 `mut`。
        if let Some((_, after)) = tail.split_once(' ') {
            rest = after.trim();
        }
    }
    rest.strip_prefix("mut ").unwrap_or(rest).trim() == "self"
}

/// 为语法节点构造带行列号的位置。
///
/// 行列号由源文件的行首表换算得到。带行列号的位置才能被人和 IDE 直接跳转，
/// 因此这里不做「只有字节偏移」的降级 —— 万一换算出错，宁可退回调用方
/// 已有的符号位置，也不输出一个无法定位的发现（FR-064）。
fn location_for(file: &SourceFile, node: &Node<'_>) -> Location {
    let span = Span::new(node.start_byte(), node.end_byte());
    match (file.line_column(span.start), file.line_column(span.end)) {
        (Some((start_line, start_column)), Some((end_line, end_column))) => Location::new(
            file.path.clone(),
            span,
        )
        .with_lines(start_line, start_column, end_line, end_column),
        _ => Location::new(file.path.clone(), span),
    }
}

/// 按字节范围在树中精确定位节点。
fn find_node<'tree>(tree: &'tree ParseTree, span: &Span) -> Option<Node<'tree>> {
    tree.descendants()
        .find(|node| node.start_byte() == span.start && node.end_byte() == span.end)
}

/// 判断节点是否完全包含在另一节点内。
fn is_inside(node: &Node<'_>, container: &Node<'_>) -> bool {
    node.start_byte() >= container.start_byte() && node.end_byte() <= container.end_byte()
}

/// 测试辅助设施。
///
/// 各规则模块的测试都需要「从若干源码片段构造 [`Context`]」的能力，因此提到这里
/// 共享。用回调而不是返回上下文：`Context` 借用符号索引与源文件，直接返回会让
/// 调用方为了绕过借用检查而泄漏内存。
#[cfg(test)]
pub(crate) mod testing {
    use super::Context;
    use pf_core::source::{SourceFile, SourceMap};
    use pf_symbol::SymbolIndex;

    /// 用给定的源码片段构造上下文并执行断言。
    pub(crate) fn with_context<R>(
        entries: &[(&str, &str)],
        assertions: impl FnOnce(&Context<'_>) -> R,
    ) -> R {
        let files: Vec<SourceFile> = entries
            .iter()
            .map(|(path, content)| SourceFile::new(*path, *content))
            .collect();
        let map: SourceMap = files.iter().cloned().collect();
        let index = SymbolIndex::build(&map);
        let context = Context::new(&index, &files);
        assertions(&context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::finding::FindingKind;
    use testing::with_context;

    #[test]
    fn builtin_ruleset_is_well_formed() {
        let set = builtin_ruleset();
        assert_ne!(set.rules.len(), 0, "内置规则集不能为空");
        assert!(set.find_duplicate_id().is_none());
        for rule in &set.rules {
            assert_ne!(rule.name, "", "规则必须有可读名称");
            assert!(matches!(
                rule.check,
                pf_core::rule::RuleCheck::Builtin { .. }
            ));
        }
    }

    #[test]
    fn detects_missing_docs_on_public_api() {
        with_context(
            &[(
                "src/lib.rs",
                "pub fn undocumented(a: i32) -> i32 { a }\n\n/// Documented.\npub fn documented() {}\n",
            )],
            |context| {
                let findings = MissingDocumentationChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].kind, FindingKind::MissingDocumentation);
                assert!(findings[0].title.contains("undocumented"));
            },
        );
    }

    #[test]
    fn private_items_do_not_require_docs() {
        with_context(&[("src/lib.rs", "fn helper() {}\n")], |context| {
            assert_eq!(
                MissingDocumentationChecker.check(context),
                Vec::new(),
                "不应产生发现"
            );
        });
    }

    #[test]
    fn detects_naming_violations() {
        with_context(
            &[(
                "src/lib.rs",
                "pub struct myStruct;\npub fn BadName() {}\nfn good_name() {}\n",
            )],
            |context| {
                let findings = NamingConventionChecker.check(context);
                let names: Vec<&str> = findings.iter().map(|f| f.title.as_str()).collect();
                assert!(names.iter().any(|t| t.contains("myStruct")), "{names:?}");
                assert!(names.iter().any(|t| t.contains("BadName")), "{names:?}");
                assert!(!names.iter().any(|t| t.contains("good_name")));
            },
        );
    }

    #[test]
    fn detects_unwrap_in_library_but_not_in_tests() {
        with_context(
            &[
                (
                    "src/lib.rs",
                    "pub fn parse(s: &str) -> i32 { s.parse().unwrap() }\n",
                ),
                (
                    "tests/it.rs",
                    "pub fn t() { let _ = \"1\".parse::<i32>().unwrap(); }\n",
                ),
            ],
            |context| {
                let findings = UnwrapInLibraryChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert!(findings[0].locations[0].path.contains("src/lib.rs"));
            },
        );
    }

    #[test]
    fn detects_empty_except_in_python() {
        with_context(
            &[(
                "app.py",
                "def load(path):\n    try:\n        return open(path).read()\n    except OSError:\n        pass\n",
            )],
            |context| {
                let findings = EmptyErrorHandlerChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].kind, FindingKind::SwallowedError);
            },
        );
    }

    #[test]
    fn detects_long_function() {
        let body = format!("fn big() {{\n{}\n}}\n", "    let x = 1;\n".repeat(100));
        with_context(&[("src/lib.rs", &body)], |context| {
            let findings = FunctionTooLongChecker.check(context);
            assert_eq!(findings.len(), 1);
            assert!(findings[0].facts.iter().any(|(key, _)| key == "lines"));
        });
    }

    #[test]
    fn detects_comment_that_only_restates_the_name() {
        // 注释只是在念被声明符号的名字，没有说明意图或约束。
        // 回归测试：启发式曾经只跟「整段代码文本」比前缀，而 Rust 声明的代码
        // 总以 pub/fn 开头，导致这条规则在实际代码上永不触发。
        with_context(
            &[(
                "src/lib.rs",
                "// count\nfn count(items: &[i32]) -> usize {\n    items.len()\n}\n",
            )],
            |context| {
                let findings = CommentQualityChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].kind, FindingKind::CommentQuality);
            },
        );
    }

    #[test]
    fn detects_comment_identical_to_the_statement() {
        // 第一级：归一化之后注释与语句文本相同。
        with_context(
            &[(
                "src/lib.rs",
                "fn f(total: i32) {\n    // total + 1\n    total + 1;\n}\n",
            )],
            |context| {
                assert_eq!(CommentQualityChecker.check(context).len(), 1);
            },
        );
    }

    #[test]
    fn detects_comment_that_spells_out_a_snake_case_name() {
        // 名字里的下划线写成空格仍然是复述：注释没有增加任何信息。
        with_context(
            &[(
                "src/lib.rs",
                "// increment the counter\npub fn increment_the_counter(counter: &mut i32) {\n    *counter += 1;\n}\n",
            )],
            |context| {
                assert_eq!(CommentQualityChecker.check(context).len(), 1);
            },
        );
    }

    #[test]
    fn explanatory_comment_is_not_flagged() {
        // 说明了「为什么」与约束的注释是有价值的，即使提到了函数名。
        with_context(
            &[(
                "src/lib.rs",
                "// 必须按字典序返回：调用方依赖这个顺序做二分查找。\nfn sorted_names() -> Vec<String> {\n    Vec::new()\n}\n",
            )],
            |context| {
                assert_eq!(CommentQualityChecker.check(context), Vec::new(), "不应产生发现");
            },
        );
    }

    #[test]
    fn comment_explaining_behaviour_is_not_flagged() {
        // 说明「做什么、为什么」的注释即使以同源动词开头也不算复述。回归测试：
        // 判据曾经是「注释包含函数名」，会把这类注释全部误判。
        with_context(
            &[(
                "src/lib.rs",
                "// validates the id format before caching\nfn validate(id: &str) -> bool {\n    !id.is_empty()\n}\n",
            )],
            |context| {
                assert_eq!(CommentQualityChecker.check(context), Vec::new(), "不应产生发现");
            },
        );
    }

    #[test]
    fn detects_too_many_parameters() {
        with_context(
            &[(
                "src/lib.rs",
                "pub fn many(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32, g: i32) {}\n",
            )],
            |context| {
                assert_eq!(TooManyParametersChecker.check(context).len(), 1);
            },
        );
    }

    #[test]
    fn detects_single_implementation_trait() {
        with_context(
            &[(
                "src/lib.rs",
                "pub trait Store {\n    fn get(&self, k: &str) -> Option<String>;\n}\n\nstruct MemoryStore;\nimpl Store for MemoryStore {\n    fn get(&self, _k: &str) -> Option<String> { None }\n}\n",
            )],
            |context| {
                let findings = SingleImplementationTraitChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].kind, FindingKind::UselessAbstraction);
                // 必须显式标记需要人工确认（需求文档 7.3）。
                assert!(findings[0]
                    .facts
                    .iter()
                    .any(|(key, value)| key == "needs_human_confirmation" && value == "true"));
            },
        );
    }

    #[test]
    fn multi_implementation_trait_is_not_flagged() {
        with_context(
            &[(
                "src/lib.rs",
                "pub trait Store {\n    fn get(&self, k: &str) -> Option<String>;\n}\n\nstruct A;\nstruct B;\nimpl Store for A {\n    fn get(&self, _k: &str) -> Option<String> { None }\n}\nimpl Store for B {\n    fn get(&self, _k: &str) -> Option<String> { None }\n}\n",
            )],
            |context| {
                assert_eq!(SingleImplementationTraitChecker.check(context), Vec::new(), "不应产生发现");
            },
        );
    }

    #[test]
    fn detects_forwarding_wrapper() {
        with_context(
            &[(
                "src/lib.rs",
                "fn real_work(x: i32) -> i32 { x + 1 }\n\npub fn wrapper(x: i32) -> i32 {\n    real_work(x)\n}\n",
            )],
            |context| {
                let findings = ForwardingWrapperChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].kind, FindingKind::ForwardingChain);
                assert!(findings[0]
                    .facts
                    .iter()
                    .any(|(key, value)| key == "forwarded_to" && value == "real_work"));
            },
        );
    }

    #[test]
    fn wrapper_with_extra_logic_is_not_flagged() {
        with_context(
            &[(
                "src/lib.rs",
                "fn real_work(x: i32) -> i32 { x + 1 }\n\npub fn wrapper(x: i32) -> i32 {\n    if x < 0 { return 0; }\n    real_work(x)\n}\n",
            )],
            |context| {
                assert_eq!(ForwardingWrapperChecker.check(context), Vec::new(), "不应产生发现");
            },
        );
    }

    #[test]
    fn trait_implementations_are_not_forwarding_wrappers() {
        // `Display::fmt` 与 `From::from` 必须转发：转发是 trait 契约，不是
        // 可以消除的多余层次。回归测试：曾经把这两个实现报成纯转发包装。
        with_context(
            &[(
                "src/lib.rs",
                "use std::fmt;\n\npub struct Wrapper(pub i32);\n\nimpl fmt::Display for Wrapper {\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {\n        f.write_str(\"w\")\n    }\n}\n\nimpl From<i32> for Wrapper {\n    fn from(value: i32) -> Self {\n        Self(value)\n    }\n}\n",
            )],
            |context| {
                assert_eq!(ForwardingWrapperChecker.check(context), Vec::new(), "不应产生发现");
            },
        );
    }

    #[test]
    fn zero_argument_accessors_are_not_forwarding_wrappers() {
        // `len` / `is_empty` 返回派生值，是 Rust 暴露不变量的惯用方式；本规则
        // 只针对把参数原样转手的代理层。
        with_context(
            &[(
                "src/lib.rs",
                "pub struct Bag {\n    items: Vec<i32>,\n}\n\nimpl Bag {\n    pub fn len(&self) -> usize {\n        self.items.len()\n    }\n\n    pub fn is_empty(&self) -> bool {\n        self.items.is_empty()\n    }\n}\n",
            )],
            |context| {
                assert_eq!(ForwardingWrapperChecker.check(context), Vec::new(), "不应产生发现");
            },
        );
    }

    #[test]
    fn unwrap_in_test_code_is_not_flagged() {
        // `expect()` 在 `#[cfg(test)] mod tests` 与 `#[test]` 函数里是允许的
        // 写法（AGENTS.md 4.3 只约束库代码）。回归测试：曾经把测试代码里的
        // expect 全部报成库代码缺陷。
        with_context(
            &[(
                "src/lib.rs",
                "pub fn parse(text: &str) -> i32 {\n    text.parse().unwrap()\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn parses() {\n        assert_eq!(super::parse(\"1\"), 1);\n        let value = \"2\".parse::<i32>().expect(\"literal\");\n        assert_eq!(value, 2);\n    }\n}\n",
            )],
            |context| {
                let findings = UnwrapInLibraryChecker.check(context);
                assert_eq!(findings.len(), 1, "只应报告库代码里的 unwrap");
                assert_eq!(findings[0].locations[0].start_line, Some(2));
            },
        );
    }

    #[test]
    fn cfg_feature_named_like_test_is_not_test_code() {
        // `latest` 含有子串 `test`，但它是特性名而不是测试条件编译。回归测试：
        // 曾经用整段 contains(\"test\") 判断，把这类模块整体判成测试代码而漏报。
        with_context(
            &[(
                "src/lib.rs",
                "#[cfg(feature = \"latest\")]\npub fn parse(text: &str) -> i32 {\n    text.parse().unwrap()\n}\n",
            )],
            |context| {
                assert_eq!(UnwrapInLibraryChecker.check(context).len(), 1);
            },
        );
    }

    #[test]
    fn detects_circular_dependency() {
        with_context(
            &[
                ("src/a.rs", "use crate::b;\npub fn a() {}\n"),
                ("src/b.rs", "use crate::a;\npub fn b() {}\n"),
            ],
            |context| {
                let findings = CircularDependencyChecker.check(context);
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].kind, FindingKind::CircularDependency);
            },
        );
    }

    #[test]
    fn detects_uncalled_public_api() {
        with_context(
            &[(
                "src/lib.rs",
                "/// Public but unused.\npub fn never_called() {}\n",
            )],
            |context| {
                assert_eq!(UncalledPublicApiChecker.check(context).len(), 1);
            },
        );
    }

    #[test]
    fn called_public_api_is_not_flagged() {
        with_context(
            &[(
                "src/lib.rs",
                "/// Used.\npub fn used() {}\n\nfn caller() { used(); }\n",
            )],
            |context| {
                assert_eq!(
                    UncalledPublicApiChecker.check(context),
                    Vec::new(),
                    "不应产生发现"
                );
            },
        );
    }

    #[test]
    fn select_checkers_filters_by_id() {
        let all = builtin_checkers();
        let target = all[0].rule().id.clone();
        let selected = select_checkers(all, std::slice::from_ref(&target));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].rule().id, target);
    }

    #[test]
    fn select_checkers_with_empty_list_keeps_all() {
        let all = builtin_checkers();
        assert_eq!(select_checkers(all, &[]).len(), builtin_checkers().len());
    }

    #[test]
    fn context_reports_parse_state_without_failing() {
        with_context(&[("src/lib.rs", "fn broken( {\nfn ok() {}\n")], |context| {
            // Tree-sitter 具备错误恢复能力，语法错误的文件仍应被索引。
            assert!(context.parsed_file_count() >= 1);
        });
    }
}
