//! 多语言语法解析。
//!
//! 本 crate 只做「解析与遍历」，不自行实现任何语言的语法规则：Rust 与
//! Python 的语法树直接来自 `tree-sitter-rust` / `tree-sitter-python` 两个
//! 社区维护的 grammar crate，因此升级语言支持不需要改动本 crate 的遍历逻辑。
//!
//! 需求文档 6.2 要求「优先复用成熟生态」；同时 15.6 提醒：不要试图用
//! Tree-sitter 重建完整的 Rust 类型系统，语义信息应由编译器提供。

#![warn(missing_docs)]

use std::sync::Arc;

use pf_core::source::{Language, SourceFile};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator, Tree};

/// 解析错误。
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// 该语言没有可用的 Tree-sitter grammar。
    #[error("no tree-sitter grammar available for language {language:?}")]
    GrammarUnavailable {
        /// 语言标识。
        language: String,
    },

    /// grammar 无法加载到解析器（版本不匹配、共享库损坏等）。
    #[error("failed to load {language:?} grammar: {reason}")]
    GrammarLoad {
        /// 语言标识。
        language: String,
        /// Tree-sitter 给出的原因。
        reason: String,
    },

    /// 查询表达式不合法。
    #[error("invalid tree-sitter query: {reason}")]
    InvalidQuery {
        /// 查询文本。
        query: String,
        /// Tree-sitter 给出的原因。
        reason: String,
    },

    /// 源码不是合法 UTF-8 边界内的片段。
    #[error("node text is not valid UTF-8 at byte range {start}..{end}")]
    InvalidNodeText {
        /// 起始偏移。
        start: usize,
        /// 结束偏移。
        end: usize,
    },
}

/// 解析结果别名。
pub type Result<T> = std::result::Result<T, ParseError>;

/// 语言对应的 Tree-sitter grammar。
///
/// 每种语言只初始化一次，`Parser` 可被多个文件复用，避免重复加载
/// 共享库带来的开销。
#[derive(Clone)]
pub struct GrammarSet {
    rust: tree_sitter::Language,
    python: tree_sitter::Language,
}

impl Default for GrammarSet {
    fn default() -> Self {
        Self::new()
    }
}

impl GrammarSet {
    /// 加载内置 grammar。
    #[must_use]
    pub fn new() -> Self {
        Self {
            rust: tree_sitter::Language::new(tree_sitter_rust::LANGUAGE),
            python: tree_sitter::Language::new(tree_sitter_python::LANGUAGE),
        }
    }

    /// 获取语言对应的 grammar；不支持时返回 `None`。
    #[must_use]
    pub fn get(&self, language: Language) -> Option<tree_sitter::Language> {
        match language {
            Language::Rust => Some(self.rust.clone()),
            Language::Python => Some(self.python.clone()),
            Language::Other => None,
        }
    }
}

/// 一棵已解析的语法树及其源码。
pub struct ParseTree {
    path: String,
    language: Language,
    grammar: tree_sitter::Language,
    source: Arc<str>,
    tree: Tree,
}

impl ParseTree {
    /// 文件路径（仓库相对路径）。
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// 语言。
    #[must_use]
    pub const fn language(&self) -> Language {
        self.language
    }

    /// 源码全文。
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// 底层语法树（只读）。
    #[must_use]
    pub const fn tree(&self) -> &Tree {
        &self.tree
    }

    /// 根节点。
    #[must_use]
    pub fn root(&self) -> Node<'_> {
        self.tree.root_node()
    }

    /// 取节点对应的源码文本。
    ///
    /// # Errors
    /// 当节点边界不落在 UTF-8 字符边界上时返回 [`ParseError::InvalidNodeText`]。
    pub fn node_text(&self, node: &Node<'_>) -> Result<&str> {
        self.source
            .get(node.start_byte()..node.end_byte())
            .ok_or(ParseError::InvalidNodeText {
                start: node.start_byte(),
                end: node.end_byte(),
            })
    }

    /// 按先序遍历所有节点（不含根节点自身）。
    ///
    /// 兄弟节点按源码顺序返回，父节点总在子节点之前，因此可用于构建
    /// 「进入 / 离开」式的增量遍历。
    pub fn descendants(&self) -> impl Iterator<Item = Node<'_>> {
        let mut cursor = self.tree.walk();
        // 初始入栈后反转：配合「从栈尾弹出 + 子节点反序入栈」得到严格先序。
        let mut stack: Vec<Node<'_>> = self.root().children(&mut cursor).collect();
        stack.reverse();
        std::iter::from_fn(move || {
            let node = stack.pop()?;
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
            Some(node)
        })
    }
    /// 查找所有指定类型的节点。
    pub fn nodes_of_kind(&self, kind: &str) -> Vec<Node<'_>> {
        self.descendants()
            .filter(|node| node.kind() == kind)
            .collect()
    }

    /// 树中是否存在语法错误节点（Tree-sitter 的 `ERROR` / `MISSING`）。
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.root().has_error()
    }

    /// 节点总数，用于粗略衡量分析规模。
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.descendants().count()
    }

    /// 执行一条 Tree-sitter Query，返回所有匹配及其捕获。
    ///
    /// # Errors
    /// 查询表达式不合法时返回 [`ParseError::InvalidQuery`]。
    pub fn query(&self, query_source: &str) -> Result<Vec<QueryMatch<'_>>> {
        let query =
            Query::new(&self.grammar, query_source).map_err(|reason| ParseError::InvalidQuery {
                query: query_source.to_owned(),
                reason: reason.to_string(),
            })?;

        let text_provider = self.source.as_bytes();
        let mut cursor = QueryCursor::new();
        let mut stream = cursor.matches(&query, self.root(), text_provider);
        let mut matches = Vec::new();
        while let Some(m) = stream.next() {
            let captures = m
                .captures
                .iter()
                .map(|capture| Capture {
                    name: query
                        .capture_names()
                        .get(capture.index as usize)
                        .copied()
                        .unwrap_or_default()
                        .to_owned(),
                    node: capture.node,
                })
                .collect();
            matches.push(QueryMatch {
                pattern_index: m.pattern_index,
                captures,
            });
        }
        Ok(matches)
    }
}

/// Query 匹配中的单个捕获。
#[derive(Debug, Clone)]
pub struct Capture<'tree> {
    /// 捕获名（不含 `@`）。
    pub name: String,
    /// 捕获到的节点。
    pub node: Node<'tree>,
}

/// 一条 Query 匹配。
#[derive(Debug, Clone)]
pub struct QueryMatch<'tree> {
    /// 命中的模式序号。
    pub pattern_index: usize,
    /// 该模式下的所有捕获。
    pub captures: Vec<Capture<'tree>>,
}

impl QueryMatch<'_> {
    /// 按名字取第一个捕获的节点。
    #[must_use]
    pub fn capture(&self, name: &str) -> Option<Node<'_>> {
        self.captures
            .iter()
            .find(|capture| capture.name == name)
            .map(|capture| capture.node)
    }
}

/// 可复用的解析器。
///
/// 内部持有 [`GrammarSet`]，因此可以在多个文件之间共享同一个实例。
#[derive(Clone)]
pub struct ParseEngine {
    grammars: GrammarSet,
}

impl Default for ParseEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ParseEngine {
    /// 创建解析引擎（加载全部内置 grammar）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            grammars: GrammarSet::new(),
        }
    }

    /// 解析单个源文件。
    ///
    /// 不支持的语言返回 `Ok(None)`，由调用方决定是否忽略。
    ///
    /// # Errors
    /// grammar 加载失败时返回 [`ParseError::GrammarLoad`]。
    pub fn parse(&self, file: &SourceFile) -> Result<Option<ParseTree>> {
        let Some(grammar) = self.grammars.get(file.language) else {
            return Ok(None);
        };
        let mut parser = Parser::new();
        parser
            .set_language(&grammar)
            .map_err(|reason| ParseError::GrammarLoad {
                language: file.language.as_str().to_owned(),
                reason: reason.to_string(),
            })?;
        let tree = parser.parse(file.content.as_bytes(), None).ok_or_else(|| {
            ParseError::GrammarUnavailable {
                language: file.language.as_str().to_owned(),
            }
        })?;
        Ok(Some(ParseTree {
            path: file.path.clone(),
            language: file.language,
            grammar,
            source: Arc::from(file.content.as_str()),
            tree,
        }))
    }

    /// 批量解析，跳过不支持的文件。
    ///
    /// # Errors
    /// 与 [`Self::parse`] 相同。
    pub fn parse_all<'a>(
        &self,
        files: impl IntoIterator<Item = &'a SourceFile>,
    ) -> Result<Vec<ParseTree>> {
        files
            .into_iter()
            .filter_map(|file| self.parse(file).transpose())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::source::SourceFile;

    fn rust_file(content: &str) -> SourceFile {
        SourceFile::new("src/lib.rs", content)
    }

    #[test]
    fn parses_rust_function() {
        let engine = ParseEngine::new();
        let file = rust_file("pub fn add(a: i32, b: i32) -> i32 { a + b }\n");
        let tree = engine.parse(&file).expect("parse").expect("grammar");
        assert_eq!(tree.language(), Language::Rust);
        assert!(!tree.has_errors());

        let functions = tree.nodes_of_kind("function_item");
        assert_eq!(functions.len(), 1);
        let name = functions[0]
            .child_by_field_name("name")
            .expect("function has a name");
        assert_eq!(tree.node_text(&name).expect("text"), "add");
    }

    #[test]
    fn parses_python_function() {
        let engine = ParseEngine::new();
        let file = SourceFile::new("app.py", "def add(a, b):\n    return a + b\n");
        let tree = engine.parse(&file).expect("parse").expect("grammar");
        assert_eq!(tree.language(), Language::Python);
        let defs = tree.nodes_of_kind("function_definition");
        assert_eq!(defs.len(), 1);
    }

    #[test]
    fn unsupported_language_returns_none() {
        let engine = ParseEngine::new();
        let file = SourceFile::new("README.md", "# hello\n");
        assert!(engine.parse(&file).expect("parse").is_none());
    }

    #[test]
    fn detects_syntax_errors() {
        let engine = ParseEngine::new();
        let file = rust_file("fn broken( {\n");
        let tree = engine.parse(&file).expect("parse").expect("grammar");
        assert!(tree.has_errors());
    }

    #[test]
    fn query_finds_functions_with_doc_comments() {
        let engine = ParseEngine::new();
        let file = rust_file(
            "/// Adds two numbers.\npub fn add(a: i32, b: i32) -> i32 { a + b }\n\nfn helper() {}\n",
        );
        let tree = engine.parse(&file).expect("parse").expect("grammar");
        let matches = tree
            .query("(function_item name: (identifier) @name)")
            .expect("query");
        let names: Vec<String> = matches
            .iter()
            .filter_map(|m| m.capture("name"))
            .map(|node| tree.node_text(&node).expect("text").to_owned())
            .collect();
        assert_eq!(names, vec!["add".to_owned(), "helper".to_owned()]);
    }

    #[test]
    fn invalid_query_is_reported() {
        let engine = ParseEngine::new();
        let file = rust_file("fn a() {}\n");
        let tree = engine.parse(&file).expect("parse").expect("grammar");
        let err = tree.query("(function_item").expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidQuery { .. }));
    }

    #[test]
    fn node_text_handles_unicode() {
        let engine = ParseEngine::new();
        let file = rust_file("fn 名前() -> &'static str { \"こんにちは\" }\n");
        let tree = engine.parse(&file).expect("parse").expect("grammar");
        let name = tree.nodes_of_kind("function_item")[0]
            .child_by_field_name("name")
            .expect("name");
        assert_eq!(tree.node_text(&name).expect("text"), "名前");
    }

    #[test]
    fn parse_all_skips_unsupported_files() {
        let engine = ParseEngine::new();
        let rust = rust_file("fn a() {}\n");
        let markdown = SourceFile::new("docs.md", "# x\n");
        let trees = engine.parse_all([&rust, &markdown]).expect("parse_all");
        assert_eq!(trees.len(), 1);
    }

    #[test]
    fn descendants_visits_in_source_order() {
        let engine = ParseEngine::new();
        let file = rust_file("fn first() {}\nfn second() {}\nfn third() {}\n");
        let tree = engine.parse(&file).expect("parse").expect("grammar");
        let names: Vec<String> = tree
            .descendants()
            .filter(|node| node.kind() == "function_item")
            .filter_map(|node| node.child_by_field_name("name"))
            .filter_map(|node| tree.node_text(&node).ok())
            .map(str::to_owned)
            .collect();
        assert_eq!(
            names,
            vec!["first".to_owned(), "second".to_owned(), "third".to_owned()]
        );
    }

    #[test]
    fn descendants_visits_parent_before_children() {
        let engine = ParseEngine::new();
        let file = rust_file("mod outer {\n    fn inner() {}\n}\n");
        let tree = engine.parse(&file).expect("parse").expect("grammar");
        let kinds: Vec<&str> = tree.descendants().map(|node| node.kind()).collect();
        let mod_position = kinds
            .iter()
            .position(|kind| *kind == "mod_item")
            .expect("mod_item");
        let fn_position = kinds
            .iter()
            .position(|kind| *kind == "function_item")
            .expect("function_item");
        assert!(mod_position < fn_position, "parent must be visited first");
    }
}
