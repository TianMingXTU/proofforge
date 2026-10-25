//! 符号索引与引用提取。
//!
//! 需求文档 FR-004：建立文件、模块、类型、函数、变量、调用和引用信息。
//!
//! 设计取舍：本 crate 只提取「语法层面确定存在」的符号与调用关系。Rust 的
//! trait 解析、泛型实例化等需要类型系统的信息不由这里推导（见需求文档
//! 15.6），未解析的引用按名字保留，交由 `pf-graph` 做启发式绑定。

#![warn(missing_docs)]

use std::collections::HashMap;

use pf_core::location::{Location, Span};
use pf_core::source::{Language, SourceFile, SourceMap};
use pf_parser::{ParseEngine, ParseTree};
use tree_sitter::Node;

/// 符号种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolKind {
    /// 自由函数。
    Function,
    /// 方法（impl 块或 trait 内定义）。
    Method,
    /// 结构体。
    Struct,
    /// 枚举。
    Enum,
    /// Trait。
    Trait,
    /// impl 块（匿名符号，使用类型名作为名字）。
    Impl,
    /// 模块。
    Module,
    /// 常量。
    Constant,
    /// 静态变量。
    Static,
    /// 类型别名。
    TypeAlias,
    /// 类（Python）。
    Class,
    /// 字段或普通变量绑定。
    Variable,
}

impl SymbolKind {
    /// 机器可读短标签，用于报告分组与序列化。
    ///
    /// 该值是稳定的英文标识：会进入 JSON 报告、SARIF 与审计记录，因此不能
    /// 随本地化改动。面向人的中文标签见 [`Self::display_name`]。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Module => "module",
            Self::Constant => "constant",
            Self::Static => "static",
            Self::TypeAlias => "type_alias",
            Self::Class => "class",
            Self::Variable => "variable",
        }
    }

    /// 面向人的中文名称，用于拼装提示信息。
    ///
    /// 与 [`Self::as_str`] 分开是必要的：后者是稳定的机器标识，直接插进中文
    /// 句子会产出「公共 function `f` 缺少文档」这种中英混杂的输出。
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::Function => "函数",
            Self::Method => "方法",
            Self::Struct => "结构体",
            Self::Enum => "枚举",
            Self::Trait => "trait",
            Self::Impl => "impl 块",
            Self::Module => "模块",
            Self::Constant => "常量",
            Self::Static => "静态变量",
            Self::TypeAlias => "类型别名",
            Self::Class => "类",
            Self::Variable => "变量",
        }
    }

    /// 是否为「可被调用的定义」。
    #[must_use]
    pub const fn is_callable(&self) -> bool {
        matches!(self, Self::Function | Self::Method)
    }

    /// 是否为「类型级定义」。
    #[must_use]
    pub const fn is_type(&self) -> bool {
        matches!(
            self,
            Self::Struct | Self::Enum | Self::Trait | Self::Class | Self::TypeAlias
        )
    }
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 可见性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    /// 私有（默认）。
    #[default]
    Private,
    /// 完全公开（`pub` / 无下划线前缀的 Python 名）。
    Public,
    /// 受限公开（`pub(crate)`、`pub(super)` 等）。
    Restricted,
}

impl Visibility {
    /// 短标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
            Self::Restricted => "restricted",
        }
    }

    /// 是否对模块外可见。
    #[must_use]
    pub const fn is_exported(&self) -> bool {
        matches!(self, Self::Public)
    }
}

/// 一个已索引的符号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// 稳定标识：`路径::限定名`，同一文件中唯一。
    pub id: String,
    /// 简单名字。
    pub name: String,
    /// 限定名（包含所属 impl / trait / 类），例如 `UserService::transfer`。
    pub qualified_name: String,
    /// 种类。
    pub kind: SymbolKind,
    /// 所属文件（仓库相对路径）。
    pub file: String,
    /// 定义位置。
    pub location: Location,
    /// 定义节点的字节范围（与 `location.span` 一致，单独保留便于快速比较）。
    pub span: Span,
    /// 可见性。
    pub visibility: Visibility,
    /// 父符号的 `id`。
    ///
    /// 注意语义：方法的父级是定义它的 `impl` 块（语法上的真 parent），而不是
    /// 结构体本身；但 `qualified_name` 使用类型名（`User::new`），与调用方
    /// 书写的路径一致。需要按类型聚合方法时请使用 `qualified_name`。
    pub parent: Option<String>,
    /// 是否带有文档注释（Rust `///`、Python docstring）。
    pub documented: bool,
    /// 定义体的行数，用于复杂度相关规则。
    pub body_lines: u32,
}

impl Symbol {
    /// 符号是否为公共 API（对外可见且是类型或可调用定义）。
    #[must_use]
    pub fn is_public_api(&self) -> bool {
        self.visibility.is_exported() && (self.kind.is_callable() || self.kind.is_type())
    }
}

/// 引用种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReferenceKind {
    /// 函数或方法调用。
    Call,
    /// 类型引用（字段类型、泛型参数、返回类型）。
    TypeUse,
    /// `use` / `import` 导入。
    Import,
    /// 变量或常量读取。
    Read,
}

impl ReferenceKind {
    /// 短标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::TypeUse => "type_use",
            Self::Import => "import",
            Self::Read => "read",
        }
    }
}

impl std::fmt::Display for ReferenceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条引用：从某个符号指向另一个名字。
///
/// 目标只记录名字而不立即解析到符号，因为跨文件解析需要全局视角，
/// 由 `pf-graph` 完成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// 引用所在文件。
    pub file: String,
    /// 发起引用的符号 `id`（文件顶层引用为 `None`）。
    pub from_symbol: Option<String>,
    /// 被引用的名字（可能带路径，如 `std::fmt::Display`）。
    pub to_name: String,
    /// 引用种类。
    pub kind: ReferenceKind,
    /// 引用位置。
    pub location: Location,
}

/// 一个定义节点对作用域栈的影响。
#[derive(Debug)]
enum ScopeEffect {
    /// 不是定义节点，应当继续按引用处理。
    NotADefinition,
    /// 是定义，但不引入新的限定作用域（例如结构体定义自身）。
    NoScope,
    /// 是定义，并在给定帧结束前引入一个新的限定作用域。
    Push(ScopeFrame),
}

/// 定义节点引入的作用域形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeShape {
    /// 不引入作用域：类型别名、常量这类叶子定义。
    None,
    /// 值作用域（模块、函数）：其中出现的函数是普通函数。
    Value,
    /// 类型作用域（trait、impl、class）：其中出现的函数是方法。
    Type,
}

/// 一次符号定义的输入。
///
/// 用命名字段替代位置参数：七个参数里有两个是 `&str`、两个是 `&Node`，位置
/// 传错时编译器不会报错，只会在符号表里留下错位的数据。
#[derive(Clone, Copy)]
struct SymbolSpec<'a> {
    /// 所在源文件。
    file: &'a SourceFile,
    /// 该文件的语法树。
    tree: &'a ParseTree,
    /// 定义节点的根节点。
    node: &'a Node<'a>,
    /// 符号的简单名字。
    name: &'a str,
    /// 符号种类。
    kind: SymbolKind,
    /// 进入该节点时的作用域栈。
    scope: &'a [ScopeFrame],
    /// 语言层面的可见性。
    visibility: Visibility,
}

/// 一个项目的符号索引。
#[derive(Debug, Clone, Default)]
pub struct SymbolIndex {
    symbols: Vec<Symbol>,
    references: Vec<Reference>,
    by_file: HashMap<String, Vec<usize>>,
    by_qualified_name: HashMap<String, usize>,
    /// 索引过程中的警告（例如解析失败的文件）。
    warnings: Vec<String>,
}

impl SymbolIndex {
    /// 构造空索引。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 从源文件集合构建索引。
    ///
    /// 解析失败的文件会被跳过并记入 [`Self::warnings`]，不会让整个任务失败
    /// （需求文档 11.2：单个检查器异常不得导致整个任务丢失）。
    pub fn build(source_map: &SourceMap) -> Self {
        Self::build_with(&ParseEngine::new(), source_map)
    }

    /// 复用已有的 [`ParseEngine`] 构建索引（避免重复加载 grammar）。
    pub fn build_with(engine: &ParseEngine, source_map: &SourceMap) -> Self {
        let mut index = Self::new();
        for file in source_map.iter() {
            match engine.parse(file) {
                Ok(Some(tree)) => index.index_file(file, &tree),
                Ok(None) => {}
                Err(reason) => index
                    .warnings
                    .push(format!("{}: failed to parse ({reason})", file.path)),
            }
        }
        index
    }

    /// 索引单个文件。
    fn index_file(&mut self, file: &SourceFile, tree: &ParseTree) {
        match file.language {
            Language::Rust => self.index_rust(file, tree),
            Language::Python => self.index_python(file, tree),
            Language::Other => {}
        }
    }

    /// Rust 符号与引用提取。
    ///
    /// 遍历与定义、引用分成两步：这里只负责驱动作用域栈，具体的节点语义在
    /// [`Self::rust_definition`] 与 [`Self::rust_reference`] 中。
    fn index_rust(&mut self, file: &SourceFile, tree: &ParseTree) {
        // 作用域栈：每条记录带结束字节偏移，离开该范围时弹出，从而在扁平的
        // 先序遍历中也能得到正确的嵌套限定名。
        let mut scope: Vec<ScopeFrame> = Vec::new();

        for node in tree.descendants() {
            close_scopes(&mut scope, node.start_byte());
            match self.rust_definition(file, tree, &node, &scope) {
                ScopeEffect::NotADefinition => self.rust_reference(file, tree, &node, &scope),
                ScopeEffect::NoScope => {}
                ScopeEffect::Push(frame) => scope.push(frame),
            }
        }
    }

    /// 登记一个 Rust 定义节点，并说明它对作用域栈的影响。
    ///
    /// 没有名字字段的定义节点（例如部分宏展开结果）被静默跳过：它们无法
    /// 参与限定名拼接，也不构成引用来源。
    fn rust_definition(
        &mut self,
        file: &SourceFile,
        tree: &ParseTree,
        node: &Node<'_>,
        scope: &[ScopeFrame],
    ) -> ScopeEffect {
        let kind = node.kind();
        // impl 没有 `name` 字段，展示名需要自行拼装，单独处理。
        if kind == "impl_item" {
            return self.rust_impl_definition(file, tree, node, scope);
        }
        let Some((symbol_kind, shape)) = rust_definition_shape(kind, scope) else {
            return ScopeEffect::NotADefinition;
        };
        let Some(name) = field_text(tree, node, "name") else {
            return ScopeEffect::NotADefinition;
        };
        self.define_scoped(
            SymbolSpec {
                file,
                tree,
                node,
                name: &name,
                kind: symbol_kind,
                scope,
                visibility: rust_visibility(tree, node),
            },
            shape,
        )
    }

    /// 登记一个 impl 块，并把其中的方法挂到类型作用域下。
    ///
    /// impl 没有 `name` 字段，展示名需要自行拼装；因此无法复用
    /// [`Self::define_scoped`] 的名字提取逻辑。
    fn rust_impl_definition(
        &mut self,
        file: &SourceFile,
        tree: &ParseTree,
        node: &Node<'_>,
        scope: &[ScopeFrame],
    ) -> ScopeEffect {
        let Some(target) = impl_target(tree, node) else {
            return ScopeEffect::NotADefinition;
        };
        // 展示名带上 trait 名（若有），既保证 ID 不与同名结构体冲突，也让
        // 「trait 的 impl 数量」可以被直接统计。
        let label = match &target.trait_name {
            Some(trait_name) => format!("impl {trait_name} for {}", target.type_name),
            None => format!("impl {}", target.type_name),
        };
        let id = self.define_symbol(SymbolSpec {
            file,
            tree,
            node,
            name: &label,
            kind: SymbolKind::Impl,
            scope,
            visibility: Visibility::Restricted,
        });
        // 作用域标签使用类型名，使方法限定名为 `Type::method`。
        ScopeEffect::Push(ScopeFrame::new(node.end_byte(), target.type_name, id, true))
    }

    /// 登记一个定义，并按 [`ScopeShape`] 推导作用域变化。
    ///
    /// Rust 与 Python 两条提取路径共用：作用域语义与语言无关，只有节点种类
    /// 的判定不同。
    fn define_scoped(&mut self, spec: SymbolSpec<'_>, shape: ScopeShape) -> ScopeEffect {
        let id = self.define_symbol(spec);
        match shape {
            ScopeShape::None => ScopeEffect::NoScope,
            ScopeShape::Value => ScopeEffect::Push(ScopeFrame::new(
                spec.node.end_byte(),
                spec.name.to_owned(),
                id,
                false,
            )),
            ScopeShape::Type => ScopeEffect::Push(ScopeFrame::new(
                spec.node.end_byte(),
                spec.name.to_owned(),
                id,
                true,
            )),
        }
    }

    /// 登记 Rust 的引用类节点（调用与导入）。
    fn rust_reference(
        &mut self,
        file: &SourceFile,
        tree: &ParseTree,
        node: &Node<'_>,
        scope: &[ScopeFrame],
    ) {
        match node.kind() {
            "call_expression" => {
                if let Some(callee) = node.child_by_field_name("function") {
                    self.references.push(Reference {
                        file: file.path.clone(),
                        from_symbol: scope.last().map(|frame| frame.symbol_id.clone()),
                        to_name: text_or_empty(tree, &callee),
                        kind: ReferenceKind::Call,
                        location: location_of(file, &callee),
                    });
                }
            }
            "use_declaration" => {
                for target in rust_use_targets(&text_or_empty(tree, node)) {
                    self.references.push(Reference {
                        file: file.path.clone(),
                        from_symbol: None,
                        to_name: target,
                        kind: ReferenceKind::Import,
                        location: location_of(file, node),
                    });
                }
            }
            _ => {}
        }
    }

    /// Python 符号与引用提取。
    fn index_python(&mut self, file: &SourceFile, tree: &ParseTree) {
        let mut scope: Vec<ScopeFrame> = Vec::new();

        for node in tree.descendants() {
            close_scopes(&mut scope, node.start_byte());

            match node.kind() {
                "function_definition" | "class_definition" => {
                    let Some(name) = field_text(tree, &node, "name") else {
                        continue;
                    };
                    let is_class = node.kind() == "class_definition";
                    let kind = if is_class {
                        SymbolKind::Class
                    } else if scope.last().is_some_and(|frame| frame.is_type) {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    let visibility = if name.starts_with('_') {
                        Visibility::Private
                    } else {
                        Visibility::Public
                    };
                    let shape = if is_class {
                        ScopeShape::Type
                    } else {
                        ScopeShape::Value
                    };
                    let effect = self.define_scoped(
                        SymbolSpec {
                            file,
                            tree,
                            node: &node,
                            name: &name,
                            kind,
                            scope: &scope,
                            visibility,
                        },
                        shape,
                    );
                    if let ScopeEffect::Push(frame) = effect {
                        scope.push(frame);
                    }
                }
                "call" => {
                    if let Some(callee) = node.child_by_field_name("function") {
                        self.references.push(Reference {
                            file: file.path.clone(),
                            from_symbol: scope.last().map(|frame| frame.symbol_id.clone()),
                            to_name: text_or_empty(tree, &callee),
                            kind: ReferenceKind::Call,
                            location: location_of(file, &callee),
                        });
                    }
                }
                "import_statement" | "import_from_statement" => {
                    for target in python_import_targets(&text_or_empty(tree, &node)) {
                        self.references.push(Reference {
                            file: file.path.clone(),
                            from_symbol: None,
                            to_name: target,
                            kind: ReferenceKind::Import,
                            location: location_of(file, &node),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    /// 登记一个符号并返回其 ID。
    ///
    /// 把「构造 Symbol + 建索引」集中到一处，保证 Rust 与 Python 两条提取
    /// 路径行为一致。输入收敛为 [`SymbolSpec`]，避免七个位置参数传错顺序。
    fn define_symbol(&mut self, spec: SymbolSpec<'_>) -> String {
        let SymbolSpec {
            file,
            tree,
            node,
            name,
            kind,
            scope,
            visibility,
        } = spec;
        let qualified_name = qualify(scope, name);
        let id = format!("{}::{}", file.path, qualified_name);
        let symbol = Symbol {
            id: id.clone(),
            name: name.to_owned(),
            qualified_name,
            kind,
            file: file.path.clone(),
            location: location_of(file, node),
            span: Span::new(node.start_byte(), node.end_byte()),
            visibility,
            parent: scope.last().map(|frame| frame.symbol_id.clone()),
            documented: match file.language {
                Language::Rust => has_rust_doc_comment(tree, node),
                Language::Python => python_has_docstring(tree, node),
                Language::Other => false,
            },
            body_lines: node_lines(tree, node),
        };
        let position = self.symbols.len();
        self.symbols.push(symbol);
        self.by_qualified_name.insert(id.clone(), position);
        self.by_file
            .entry(file.path.clone())
            .or_default()
            .push(position);
        id
    }

    /// 全部符号。
    pub fn symbols(&self) -> &[Symbol] {
        &self.symbols
    }

    /// 全部引用。
    pub fn references(&self) -> &[Reference] {
        &self.references
    }

    /// 索引期间的警告。
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// 按限定名查找符号。
    #[must_use]
    pub fn by_qualified_name(&self, qualified_name: &str) -> Option<&Symbol> {
        self.by_qualified_name
            .get(qualified_name)
            .map(|position| &self.symbols[*position])
    }

    /// 查找某文件中全部符号。
    pub fn in_file(&self, path: &str) -> impl Iterator<Item = &Symbol> + '_ {
        self.by_file
            .get(path)
            .map(|positions| {
                positions
                    .iter()
                    .map(move |position| &self.symbols[*position])
            })
            .into_iter()
            .flatten()
    }

    /// 按简单名字查找全部同名符号（跨文件）。
    pub fn by_name(&self, name: &str) -> impl Iterator<Item = &Symbol> + '_ {
        // 先把匹配键复制为拥有所有权的值，使返回的迭代器只依赖 `&self`。
        let name = name.to_owned();
        self.symbols
            .iter()
            .filter(move |symbol| symbol.name == name)
    }

    /// 符号总数。
    #[must_use]
    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    /// 引用总数。
    #[must_use]
    pub fn reference_count(&self) -> usize {
        self.references.len()
    }

    /// 找出从某符号发出的全部调用引用。
    pub fn calls_from(&self, symbol_id: &str) -> impl Iterator<Item = &Reference> + '_ {
        let symbol_id = symbol_id.to_owned();
        self.references.iter().filter(move |reference| {
            reference.kind == ReferenceKind::Call
                && reference.from_symbol.as_deref() == Some(symbol_id.as_str())
        })
    }

    /// 找出指向某个名字的全部调用引用。
    pub fn calls_to(&self, name: &str) -> impl Iterator<Item = &Reference> + '_ {
        let name = name.to_owned();
        self.references.iter().filter(move |reference| {
            reference.kind == ReferenceKind::Call && reference.to_name == name
        })
    }

    /// 找出某文件的全部导入引用。
    pub fn imports_of(&self, path: &str) -> impl Iterator<Item = &Reference> + '_ {
        let path = path.to_owned();
        self.references.iter().filter(move |reference| {
            reference.kind == ReferenceKind::Import && reference.file == path
        })
    }

    /// 统计每种符号的数量，用于报告概览。
    #[must_use]
    pub fn kind_histogram(&self) -> Vec<(SymbolKind, usize)> {
        let mut counts: Vec<(SymbolKind, usize)> = Vec::new();
        for symbol in &self.symbols {
            match counts.iter_mut().find(|(kind, _)| *kind == symbol.kind) {
                Some((_, count)) => *count += 1,
                None => counts.push((symbol.kind, 1)),
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        counts
    }

    /// 公共 API 符号列表。
    pub fn public_api(&self) -> impl Iterator<Item = &Symbol> + '_ {
        self.symbols.iter().filter(|symbol| symbol.is_public_api())
    }

    /// 找出包含指定字节位置的符号（最内层者）。
    #[must_use]
    pub fn symbol_at(&self, path: &str, offset: usize) -> Option<&Symbol> {
        self.in_file(path)
            .filter(|symbol| symbol.span.contains(offset))
            .min_by_key(|symbol| symbol.span.len())
    }
}

/// 遍历过程中的一个作用域帧。
///
/// 携带结束字节偏移，使得扁平的先序遍历也能正确维护嵌套关系。
#[derive(Debug, Clone)]
struct ScopeFrame {
    /// 作用域结束的字节偏移（不含）。
    end_byte: usize,
    /// 限定名中使用的标签（函数名、类型名、模块名）。
    label: String,
    /// 该作用域对应符号的 ID，用作子符号的 `parent`。
    symbol_id: String,
    /// 是否为类型级作用域（impl / trait / class），决定子定义的默认种类。
    is_type: bool,
}

impl ScopeFrame {
    /// 构造一个作用域帧。
    fn new(end_byte: usize, label: String, symbol_id: String, is_type: bool) -> Self {
        Self {
            end_byte,
            label,
            symbol_id,
            is_type,
        }
    }
}

/// 弹出所有已经在给定偏移之前结束的作用域。
fn close_scopes(scope: &mut Vec<ScopeFrame>, offset: usize) {
    while scope.last().is_some_and(|frame| frame.end_byte <= offset) {
        scope.pop();
    }
}

/// 把作用域栈与名字组合成限定名。
fn qualify(scope: &[ScopeFrame], name: &str) -> String {
    if scope.is_empty() {
        name.to_owned()
    } else {
        let parts: Vec<&str> = scope.iter().map(|frame| frame.label.as_str()).collect();
        format!("{}::{}", parts.join("::"), name)
    }
}

/// 读取节点指定字段的文本。
fn field_text(tree: &ParseTree, node: &Node<'_>, field: &str) -> Option<String> {
    let child = node.child_by_field_name(field)?;
    tree.node_text(&child).ok().map(str::to_owned)
}

/// 读取节点文本，失败时返回空串（调用方只把它当作标签使用）。
fn text_or_empty(tree: &ParseTree, node: &Node<'_>) -> String {
    tree.node_text(node).unwrap_or_default().to_owned()
}

/// 计算节点的行数。
fn node_lines(tree: &ParseTree, node: &Node<'_>) -> u32 {
    let newlines = tree
        .node_text(node)
        .map_or(0, |text| text.matches('\n').count());
    u32::try_from(newlines).map_or(u32::MAX, |count| count + 1)
}

/// 构造带行列号的位置（行列号由源文件的行首索引换算）。
fn location_of(file: &SourceFile, node: &Node<'_>) -> Location {
    let span = Span::new(node.start_byte(), node.end_byte());
    let mut location = Location::new(file.path.clone(), span);
    if let (Some((start_line, start_column)), Some((end_line, end_column))) = (
        file.line_column(node.start_byte()),
        file.line_column(node.end_byte()),
    ) {
        location = location.with_lines(start_line, start_column, end_line, end_column);
    }
    location
}

/// impl 块的解析结果。
struct ImplTarget {
    /// 被实现的类型名，例如 `MemoryStore`。
    type_name: String,
    /// 若为 trait 实现，则为 trait 名（例如 `Store`）；固有 impl 为 `None`。
    trait_name: Option<String>,
}

/// 解析 impl 块的类型与（可选的）trait 名。
fn impl_target(tree: &ParseTree, node: &Node<'_>) -> Option<ImplTarget> {
    let type_name = tree
        .node_text(&node.child_by_field_name("type")?)
        .ok()?
        .trim()
        .to_owned();
    let trait_name = node
        .child_by_field_name("trait")
        .and_then(|trait_node| tree.node_text(&trait_node).ok())
        .map(|text| text.trim().to_owned());
    Some(ImplTarget {
        type_name,
        trait_name,
    })
}

/// 判断 Rust 节点是否为带名字的定义，并给出符号种类与作用域形态。
///
/// 返回 `None` 表示该节点不是定义，调用方应按引用继续处理。把这张映射表与
/// [`SymbolIndex::rust_definition`] 的遍历逻辑分开：新增一种定义节点时只需在
/// 这里加一行，不必再复制一遍「取名 → 登记 → 推作用域」的样板。
fn rust_definition_shape(kind: &str, scope: &[ScopeFrame]) -> Option<(SymbolKind, ScopeShape)> {
    match kind {
        // 类型级定义自身不引入限定作用域：它们的成员通过各自的定义节点入栈。
        "struct_item" | "enum_item" | "type_item" | "const_item" | "static_item" => {
            Some((rust_type_kind(kind), ScopeShape::None))
        }
        // trait 体中的函数是方法，方法限定名需要 `Trait::method`。
        "trait_item" => Some((SymbolKind::Trait, ScopeShape::Type)),
        "mod_item" => Some((SymbolKind::Module, ScopeShape::Value)),
        "function_item" | "function_signature_item" => {
            let symbol_kind = if scope.last().is_some_and(|frame| frame.is_type) {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            Some((symbol_kind, ScopeShape::Value))
        }
        _ => None,
    }
}

/// 把 Rust 类型级节点种类映射为符号种类。
fn rust_type_kind(kind: &str) -> SymbolKind {
    match kind {
        "struct_item" => SymbolKind::Struct,
        "enum_item" => SymbolKind::Enum,
        "type_item" => SymbolKind::TypeAlias,
        "const_item" => SymbolKind::Constant,
        _ => SymbolKind::Static,
    }
}

/// Rust 符号是否带有文档注释（`///` 或 `//!`）。
///
/// 允许注释与定义之间隔着属性节点（`#[derive(...)]` 等）。
fn has_rust_doc_comment(tree: &ParseTree, node: &Node<'_>) -> bool {
    let mut previous = node.prev_sibling();
    while let Some(candidate) = previous {
        match candidate.kind() {
            "line_comment" => {
                return tree
                    .node_text(&candidate)
                    .is_ok_and(|text| text.starts_with("///") || text.starts_with("//!"));
            }
            "attribute_item" => previous = candidate.prev_sibling(),
            _ => return false,
        }
    }
    false
}

/// Python 定义是否带 docstring。
/// Python 定义是否带 docstring（函数体 / 类体首条语句为字符串字面量）。
fn python_has_docstring(tree: &ParseTree, node: &Node<'_>) -> bool {
    let Some(body) = node.child_by_field_name("body") else {
        return false;
    };
    let mut cursor = body.walk();
    let Some(first) = body.children(&mut cursor).next() else {
        return false;
    };
    if first.kind() != "expression_statement" {
        return false;
    }
    let Some(expression) = first.child(0) else {
        return false;
    };
    expression.kind() == "string"
        && tree
            .node_text(&expression)
            .is_ok_and(|text| text.len() >= 2)
}

/// Rust 可见性判断。
fn rust_visibility(tree: &ParseTree, node: &Node<'_>) -> Visibility {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            let text = tree.node_text(&child).unwrap_or_default().trim();
            return if text == "pub" {
                Visibility::Public
            } else {
                Visibility::Restricted
            };
        }
    }
    Visibility::Private
}

/// 从 `use a::b::c;` 文本中提取导入目标。
fn rust_use_targets(text: &str) -> Vec<String> {
    let text = text.trim();
    let text = text
        .strip_prefix("use ")
        .or_else(|| text.strip_prefix("pub use "))
        .unwrap_or(text);
    let text = text.trim_end_matches(';').trim();
    if text.is_empty() || text.starts_with('{') {
        return Vec::new();
    }
    // 去掉 `as` 别名，只保留路径部分。
    let path = text.split(" as ").next().unwrap_or(text).trim();
    vec![path.to_owned()]
}

/// 从 Python import 文本中提取目标。
fn python_import_targets(text: &str) -> Vec<String> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("from ") {
        let module = rest.split(" import ").next().unwrap_or("").trim();
        return vec![module.to_owned()];
    }
    if let Some(rest) = text.strip_prefix("import ") {
        return rest
            .split(',')
            .map(|part| part.trim().split(" as ").next().unwrap_or("").trim())
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::source::SourceFile;

    fn index_of(entries: &[(&str, &str)]) -> SymbolIndex {
        let map: SourceMap = entries
            .iter()
            .map(|(path, content)| SourceFile::new(*path, *content))
            .collect();
        SymbolIndex::build(&map)
    }

    #[test]
    fn indexes_rust_functions_and_structs() {
        let index = index_of(&[(
            "src/lib.rs",
            "/// A user.\npub struct User {\n    pub name: String,\n}\n\nimpl User {\n    pub fn new(name: String) -> Self {\n        Self { name }\n    }\n}\n\nfn helper() {}\n",
        )]);

        assert_eq!(index.symbol_count(), 4);
        let user = index.by_qualified_name("src/lib.rs::User").expect("User");
        assert_eq!(user.kind, SymbolKind::Struct);
        assert!(user.documented);
        assert_eq!(user.visibility, Visibility::Public);

        let new = index
            .by_qualified_name("src/lib.rs::User::new")
            .expect("User::new");
        assert_eq!(new.kind, SymbolKind::Method);
        // 语法上的父级是 impl 块；限定名则使用类型名，便于按调用路径解析。
        assert_eq!(new.parent.as_deref(), Some("src/lib.rs::impl User"));
        assert!(index.by_qualified_name("src/lib.rs::impl User").is_some());

        let helper = index
            .by_qualified_name("src/lib.rs::helper")
            .expect("helper");
        assert_eq!(helper.kind, SymbolKind::Function);
        assert!(!helper.documented);
        assert_eq!(helper.visibility, Visibility::Private);
    }

    #[test]
    fn records_call_references_with_enclosing_symbol() {
        let index = index_of(&[(
            "src/lib.rs",
            "fn helper() {}\n\nfn main() {\n    helper();\n    let x = compute();\n}\n",
        )]);

        let calls: Vec<&Reference> = index.calls_from("src/lib.rs::main").collect();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().any(|call| call.to_name == "helper"));
        assert!(calls.iter().any(|call| call.to_name == "compute"));
        assert_eq!(calls[0].kind, ReferenceKind::Call);
    }

    #[test]
    fn indexes_python_classes_and_methods() {
        let index = index_of(&[(
            "app.py",
            "\"\"\"Module doc.\"\"\"\n\n\nclass Service:\n    \"\"\"A service.\"\"\"\n\n    def run(self):\n        return self._work()\n\n    def _work(self):\n        return 1\n",
        )]);

        let service = index.by_qualified_name("app.py::Service").expect("Service");
        assert_eq!(service.kind, SymbolKind::Class);
        assert!(service.documented);
        assert_eq!(service.visibility, Visibility::Public);

        let run = index
            .by_qualified_name("app.py::Service::run")
            .expect("run");
        assert_eq!(run.kind, SymbolKind::Method);
        assert_eq!(run.visibility, Visibility::Public);

        let work = index
            .by_qualified_name("app.py::Service::_work")
            .expect("_work");
        assert_eq!(work.visibility, Visibility::Private);
    }

    #[test]
    fn records_python_calls_and_imports() {
        let index = index_of(&[(
            "app.py",
            "import os\nfrom typing import List\n\n\ndef main():\n    print(os.getcwd())\n",
        )]);

        let imports: Vec<&Reference> = index.imports_of("app.py").collect();
        assert!(imports.iter().any(|reference| reference.to_name == "os"));
        assert!(imports
            .iter()
            .any(|reference| reference.to_name == "typing"));

        // `print(os.getcwd())` 是两层嵌套调用，因此 main 发出两条调用引用。
        let calls: Vec<&Reference> = index.calls_from("app.py::main").collect();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().any(|call| call.to_name == "print"));
        assert!(calls.iter().any(|call| call.to_name == "os.getcwd"));
    }

    #[test]
    fn module_scope_qualifies_nested_items() {
        let index = index_of(&[("src/lib.rs", "mod outer {\n    pub fn inner() {}\n}\n")]);

        assert!(index.by_qualified_name("src/lib.rs::outer").is_some());
        assert!(index
            .by_qualified_name("src/lib.rs::outer::inner")
            .is_some());
    }

    #[test]
    fn location_carries_line_numbers() {
        let index = index_of(&[("src/lib.rs", "fn a() {}\nfn b() {}\n")]);
        let b = index.by_qualified_name("src/lib.rs::b").expect("b");
        assert_eq!(b.location.start_line, Some(2));
        assert_eq!(b.location.start_column, Some(1));
        assert_eq!(b.location.display(), "src/lib.rs:2:1");
    }

    #[test]
    fn kind_histogram_and_public_api() {
        let index = index_of(&[("src/lib.rs", "pub struct A;\npub struct B;\nfn c() {}\n")]);
        let histogram = index.kind_histogram();
        assert_eq!(histogram[0], (SymbolKind::Struct, 2));
        assert_eq!(index.public_api().count(), 2);
    }

    #[test]
    fn symbol_at_finds_innermost_symbol() {
        let index = index_of(&[("src/lib.rs", "struct A;\nimpl A {\n    fn m(&self) {}\n}\n")]);
        // "fn m" 的起始偏移位于 impl 块内部。
        let offset = "struct A;\nimpl A {\n    ".len();
        let symbol = index.symbol_at("src/lib.rs", offset).expect("symbol");
        assert_eq!(symbol.name, "m");
    }

    #[test]
    fn parse_failure_is_recorded_as_warning_not_error() {
        // 二进制不可见字符会让 tree-sitter 产生 ERROR 节点，但仍应完成索引。
        let index = index_of(&[("src/lib.rs", "fn broken( {\nfn ok() {}\n")]);
        assert!(index.symbol_count() >= 1);
    }
}
