//! 依赖图与调用图分析。
//!
//! 需求文档 FR-022 至 FR-026、第七章 7.1：构建模块依赖图、检测循环依赖、
//! 支持架构规则与变更影响分析。
//!
//! 图算法全部复用 [`petgraph`]（Tarjan 强连通分量、拓扑排序、反向可达集），
//! 本 crate 只负责把 [`pf_symbol::SymbolIndex`] 中的引用解析成边。

#![warn(missing_docs)]

use std::collections::{HashMap, HashSet};

use petgraph::algo::{tarjan_scc, toposort};
use petgraph::stable_graph::{NodeIndex, StableDiGraph};
use petgraph::visit::EdgeRef;
use pf_symbol::{ReferenceKind, SymbolIndex};

/// 模块依赖图中节点的种类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleNode {
    /// 项目内的一个源文件。
    File {
        /// 仓库相对路径。
        path: String,
    },
    /// 项目外的依赖（第三方 crate / 包）。
    External {
        /// 依赖名。
        name: String,
    },
}

/// 模块依赖边的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleEdge {
    /// `mod` / `from . import` 形成的文件间依赖。
    ModuleDeclaration,
    /// `use crate::...` / `from .mod import ...` 形成的引用依赖。
    Import,
}

/// 模块依赖图。
#[derive(Debug, Clone, Default)]
pub struct ModuleGraph {
    graph: StableDiGraph<ModuleNode, ModuleEdge>,
    nodes: HashMap<String, NodeIndex>,
    /// 未能解析到项目内文件的引用目标（通常是外部依赖）。
    external_refs: HashMap<String, usize>,
}

impl ModuleGraph {
    /// 从符号索引构建模块依赖图。
    ///
    /// 只出现在引用中而没有符号定义的文件（例如只有 `use` 声明的文件）也会
    /// 被登记为节点，否则这类文件之间的环会漏检。
    #[must_use]
    pub fn build(index: &SymbolIndex) -> Self {
        let mut files: Vec<String> = index
            .symbols()
            .iter()
            .map(|symbol| symbol.file.clone())
            .collect();
        for reference in index.references() {
            if !files.contains(&reference.file) {
                files.push(reference.file.clone());
            }
        }
        Self::build_with_files(index, files)
    }

    /// 显式指定参与分析的文件集合来构建模块依赖图。
    ///
    /// 调用方通常传入 [`pf_core::source::SourceMap`] 中的全部文件，以确保
    /// 空文件与纯配置文件也能参与依赖分析。
    #[must_use]
    pub fn build_with_files<I, S>(index: &SymbolIndex, files: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut graph = Self::default();
        for file in files {
            graph.ensure_file(file.as_ref());
        }
        for reference in index.references() {
            if reference.kind != ReferenceKind::Import {
                continue;
            }
            let Some(source) = graph.nodes.get(&reference.file).copied() else {
                continue;
            };
            match graph.resolve_target(&reference.to_name) {
                Some(target) => {
                    let target = graph.ensure_file(&target);
                    graph.graph.add_edge(source, target, ModuleEdge::Import);
                }
                None => {
                    *graph
                        .external_refs
                        .entry(reference.to_name.clone())
                        .or_insert(0) += 1;
                }
            }
        }
        graph
    }

    /// 登记一个文件节点（已存在时返回既有节点）。
    fn ensure_file(&mut self, path: &str) -> NodeIndex {
        if let Some(node) = self.nodes.get(path) {
            return *node;
        }
        let node = self.graph.add_node(ModuleNode::File {
            path: path.to_owned(),
        });
        self.nodes.insert(path.to_owned(), node);
        node
    }

    /// 把导入目标解析为项目内文件路径。
    ///
    /// 只处理可以静态确定的相对导入与 crate 内绝对路径；无法解析时返回
    /// `None`（由调用方记为外部依赖），不做猜测。
    fn resolve_target(&self, target: &str) -> Option<String> {
        if let Some(rest) = target.strip_prefix("crate::") {
            let first = rest.split("::").next()?;
            return self.find_rust_module(first);
        }
        if let Some(rest) = target.strip_prefix('.') {
            let module = rest.trim_start_matches('.');
            if module.is_empty() {
                return None;
            }
            return self.find_python_module(module);
        }
        None
    }

    /// 在已索引文件中查找 Rust 模块对应的文件。
    fn find_rust_module(&self, module: &str) -> Option<String> {
        let direct = format!("src/{module}.rs");
        if self.nodes.contains_key(&direct) {
            return Some(direct);
        }
        let nested = format!("src/{module}/mod.rs");
        if self.nodes.contains_key(&nested) {
            return Some(nested);
        }
        None
    }

    /// 在已索引文件中查找 Python 相对导入对应的文件。
    fn find_python_module(&self, module: &str) -> Option<String> {
        let with_slash = module.replace('.', "/");
        [
            format!("{with_slash}.py"),
            format!("{with_slash}/__init__.py"),
        ]
        .into_iter()
        .find(|candidate| self.nodes.contains_key(candidate))
    }

    /// 文件节点数量。
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.nodes.len()
    }

    /// 依赖边数量。
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    /// 全部文件路径。
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.nodes.keys().map(String::as_str)
    }

    /// 某文件的直接依赖（出边）。
    pub fn dependencies_of(&self, path: &str) -> Vec<&str> {
        let Some(node) = self.nodes.get(path) else {
            return Vec::new();
        };
        self.graph
            .edges(*node)
            .filter_map(|edge| match self.graph.node_weight(edge.target()) {
                Some(ModuleNode::File { path }) => Some(path.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 某文件的直接被依赖者（入边）。
    pub fn dependents_of(&self, path: &str) -> Vec<&str> {
        let Some(node) = self.nodes.get(path) else {
            return Vec::new();
        };
        self.graph
            .edges_directed(*node, petgraph::Direction::Incoming)
            .filter_map(|edge| match self.graph.node_weight(edge.source()) {
                Some(ModuleNode::File { path }) => Some(path.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 检测循环依赖，返回所有长度大于 1 的强连通分量。
    ///
    /// 使用 Tarjan 算法，时间线性于图规模。
    #[must_use]
    pub fn cycles(&self) -> Vec<Vec<String>> {
        tarjan_scc(&self.graph)
            .into_iter()
            .filter(|component| component.len() > 1)
            .map(|component| {
                component
                    .iter()
                    .filter_map(|node| match self.graph.node_weight(*node) {
                        Some(ModuleNode::File { path }) => Some(path.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .filter(|component: &Vec<String>| !component.is_empty())
            .collect()
    }

    /// 文件是否参与循环依赖。
    #[must_use]
    pub fn is_in_cycle(&self, path: &str) -> bool {
        self.cycles()
            .iter()
            .any(|cycle| cycle.iter().any(|member| member == path))
    }

    /// 拓扑排序；存在环时返回 `Err(环中成员)`。
    ///
    /// 返回值可直接作为构建顺序使用；失败时给出的成员列表就是环的证据。
    #[must_use = "returns the build order, or the members of the cycle that blocks it"]
    pub fn topological_order(&self) -> Result<Vec<String>, Vec<String>> {
        match toposort(&self.graph, None) {
            Ok(order) => Ok(order
                .into_iter()
                .filter_map(|node| match self.graph.node_weight(node) {
                    Some(ModuleNode::File { path }) => Some(path.clone()),
                    _ => None,
                })
                .collect()),
            Err(cycle_node) => {
                // 返回该节点所在的强连通分量，作为环的具体证据。
                let cycle = self
                    .cycles()
                    .into_iter()
                    .find(|component| {
                        component
                            .iter()
                            .any(|path| self.nodes.get(path) == Some(&cycle_node.node_id()))
                    })
                    .unwrap_or_default();
                Err(cycle)
            }
        }
    }

    /// 变更影响分析：修改 `path` 可能波及的所有文件（反向可达集）。
    #[must_use]
    pub fn impact_set(&self, path: &str) -> HashSet<String> {
        let mut impacted = HashSet::new();
        let Some(start) = self.nodes.get(path).copied() else {
            return impacted;
        };
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            for edge in self
                .graph
                .edges_directed(node, petgraph::Direction::Incoming)
            {
                let source = edge.source();
                if let Some(ModuleNode::File { path }) = self.graph.node_weight(source) {
                    if impacted.insert(path.clone()) {
                        stack.push(source);
                    }
                }
            }
        }
        impacted.remove(path);
        impacted
    }

    /// 外部依赖引用统计（名字 → 引用次数）。
    #[must_use]
    pub fn external_dependencies(&self) -> Vec<(&str, usize)> {
        let mut items: Vec<(&str, usize)> = self
            .external_refs
            .iter()
            .map(|(name, count)| (name.as_str(), *count))
            .collect();
        items.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        items
    }

    /// 没有任何入边也没有出边的孤立文件。
    #[must_use]
    pub fn isolated_files(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|(_, node)| {
                self.graph.edges(**node).count() == 0
                    && self
                        .graph
                        .edges_directed(**node, petgraph::Direction::Incoming)
                        .count()
                        == 0
            })
            .map(|(path, _)| path.as_str())
            .collect()
    }
}

/// 调用关系的解析置信度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// 完整限定名精确匹配。
    Exact,
    /// 仅按最后一段名字匹配（`obj.method()`、`self.foo()` 等）。
    Heuristic,
}

/// 调用图。
#[derive(Debug, Clone, Default)]
pub struct CallGraph {
    graph: StableDiGraph<String, Resolution>,
    nodes: HashMap<String, NodeIndex>,
    /// 限定名 / ID → 符号 ID，用于 O(1) 精确解析。
    by_qualified_name: HashMap<String, String>,
    /// 无法解析到项目内符号的调用目标。
    unresolved: HashSet<String>,
}

impl CallGraph {
    /// 从符号索引构建调用图。
    ///
    /// 解析策略分两级：先尝试完整限定名精确匹配，再退化为按名字最后一段
    /// 匹配。未解析的调用被记录下来，用于识别「调用外部代码」而不是当作
    /// 缺陷。
    #[must_use]
    pub fn build(index: &SymbolIndex) -> Self {
        let mut graph = Self::default();

        // 建立两级索引：限定名用于精确解析，名字最后一段用于启发式解析。
        let mut by_short_name: HashMap<&str, Vec<String>> = HashMap::new();
        for symbol in index.symbols() {
            graph.ensure(&symbol.id);
            graph
                .by_qualified_name
                .insert(symbol.qualified_name.clone(), symbol.id.clone());
            graph
                .by_qualified_name
                .insert(symbol.id.clone(), symbol.id.clone());
            by_short_name
                .entry(symbol.name.as_str())
                .or_default()
                .push(symbol.id.clone());
        }

        for reference in index.references() {
            if reference.kind != ReferenceKind::Call {
                continue;
            }
            let Some(caller) = reference.from_symbol.as_ref() else {
                continue;
            };
            let target = reference.to_name.as_str();

            // 精确匹配：完整限定名或以 `::` 结尾的路径。
            if let Some(callee) = graph.resolve_exact(target) {
                graph.add_call(caller, &callee, Resolution::Exact);
                continue;
            }
            // 启发式匹配：取调用表达式的最后一段。
            //
            // 必须同时按 `::` 与 `.` 切分：Rust 的方法调用在源码里写作
            // `receiver.method()`，引用记录保存的是整段文本。只按 `::` 切分会
            // 让所有方法调用都无法解析，调用图随之退化成「几乎为空」，
            // 依赖它的规则（例如未调用的公共 API）会全部误报。
            let short = target.rsplit([':', '.']).next().unwrap_or(target);
            if let Some(candidates) = by_short_name.get(short) {
                if candidates.len() == 1 {
                    graph.add_call(caller, &candidates[0], Resolution::Heuristic);
                    continue;
                }
            }
            graph.unresolved.insert(target.to_owned());
        }
        graph
    }

    /// 尝试把调用目标解析为项目内符号 ID。
    fn resolve_exact(&self, target: &str) -> Option<String> {
        let candidates = [target, target.trim_end_matches("()")];
        candidates
            .into_iter()
            .find_map(|candidate| self.by_qualified_name.get(candidate).cloned())
    }

    /// 登记一个符号节点。
    fn ensure(&mut self, id: &str) -> NodeIndex {
        if let Some(node) = self.nodes.get(id) {
            return *node;
        }
        let node = self.graph.add_node(id.to_owned());
        self.nodes.insert(id.to_owned(), node);
        node
    }

    /// 添加一条调用边。
    ///
    /// 已存在的同向边不重复添加：调用图只表达「是否调用」，多重调用次数对
    /// 规则没有意义，重复边会让环检测的结果出现歧义。
    fn add_call(&mut self, from: &str, to: &str, resolution: Resolution) {
        let source = self.ensure(from);
        let target = self.ensure(to);
        if !self.graph.contains_edge(source, target) {
            self.graph.add_edge(source, target, resolution);
        }
    }

    /// 某符号调用的全部项目内符号。
    pub fn callees_of(&self, symbol_id: &str) -> Vec<&str> {
        let Some(node) = self.nodes.get(symbol_id) else {
            return Vec::new();
        };
        self.graph
            .edges(*node)
            .filter_map(|edge| self.graph.node_weight(edge.target()).map(String::as_str))
            .collect()
    }

    /// 调用某符号的全部项目内符号。
    pub fn callers_of(&self, symbol_id: &str) -> Vec<&str> {
        let Some(node) = self.nodes.get(symbol_id) else {
            return Vec::new();
        };
        self.graph
            .edges_directed(*node, petgraph::Direction::Incoming)
            .filter_map(|edge| self.graph.node_weight(edge.source()).map(String::as_str))
            .collect()
    }

    /// 解析到项目内的调用边数量。
    #[must_use]
    pub fn resolved_edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    /// 未能解析的调用目标（调用外部代码或动态分发）。
    #[must_use]
    pub fn unresolved_calls(&self) -> &HashSet<String> {
        &self.unresolved
    }

    /// 直接或间接递归的符号（自环或位于调用环中）。
    #[must_use]
    pub fn recursive_symbols(&self) -> Vec<String> {
        tarjan_scc(&self.graph)
            .into_iter()
            .filter(|component| {
                component.len() > 1
                    || component
                        .first()
                        .is_some_and(|node| self.graph.contains_edge(*node, *node))
            })
            .flatten()
            .filter_map(|node| self.graph.node_weight(node).cloned())
            .collect()
    }

    /// 从未被任何项目内符号调用的符号（可能是死代码或纯入口点）。
    #[must_use]
    pub fn uncalled_symbols(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|(_, node)| {
                self.graph
                    .edges_directed(**node, petgraph::Direction::Incoming)
                    .count()
                    == 0
            })
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::source::{SourceFile, SourceMap};
    use pf_symbol::SymbolIndex;

    fn index_of(entries: &[(&str, &str)]) -> SymbolIndex {
        let map: SourceMap = entries
            .iter()
            .map(|(path, content)| SourceFile::new(*path, *content))
            .collect();
        SymbolIndex::build(&map)
    }

    #[test]
    fn module_graph_detects_cycles() {
        let index = index_of(&[
            ("src/a.rs", "use crate::b;\npub fn a() {}\n"),
            ("src/b.rs", "use crate::a;\npub fn b() {}\n"),
            ("src/c.rs", "pub fn c() {}\n"),
        ]);
        let graph = ModuleGraph::build(&index);
        assert_eq!(graph.file_count(), 3);

        let cycles = graph.cycles();
        assert_eq!(cycles.len(), 1);
        let cycle = &cycles[0];
        assert!(cycle.contains(&"src/a.rs".to_owned()));
        assert!(cycle.contains(&"src/b.rs".to_owned()));
        assert!(graph.is_in_cycle("src/a.rs"));
        assert!(!graph.is_in_cycle("src/c.rs"));
    }

    #[test]
    fn module_graph_computes_impact_set() {
        let index = index_of(&[
            ("src/base.rs", "pub fn base() {}\n"),
            ("src/mid.rs", "use crate::base;\npub fn mid() {}\n"),
            ("src/top.rs", "use crate::mid;\npub fn top() {}\n"),
        ]);
        let graph = ModuleGraph::build(&index);
        let impacted = graph.impact_set("src/base.rs");
        assert!(impacted.contains("src/mid.rs"));
        assert!(impacted.contains("src/top.rs"));
        assert!(!impacted.contains("src/base.rs"));
    }

    #[test]
    fn module_graph_tracks_external_dependencies() {
        let index = index_of(&[("src/lib.rs", "use serde::Serialize;\npub fn f() {}\n")]);
        let graph = ModuleGraph::build(&index);
        assert_eq!(graph.file_count(), 1);
        let external = graph.external_dependencies();
        assert!(external.iter().any(|(name, _)| *name == "serde::Serialize"));
    }

    #[test]
    fn topological_order_reports_cycles() {
        let index = index_of(&[
            ("src/a.rs", "use crate::b;\n"),
            ("src/b.rs", "use crate::a;\n"),
        ]);
        let graph = ModuleGraph::build(&index);
        assert!(graph.topological_order().is_err());
    }

    #[test]
    fn topological_order_succeeds_for_dag() {
        let index = index_of(&[
            ("src/a.rs", "use crate::b;\n"),
            ("src/b.rs", "pub fn b() {}\n"),
        ]);
        let graph = ModuleGraph::build(&index);
        let order = graph.topological_order().expect("no cycles");
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn call_graph_resolves_exact_and_heuristic() {
        let index = index_of(&[(
            "src/lib.rs",
            "fn helper() {}\n\nfn main() {\n    helper();\n    let s = Service::new();\n}\n\nstruct Service;\nimpl Service {\n    fn new() -> Self { Self }\n}\n",
        )]);
        let graph = CallGraph::build(&index);

        let main_callees = graph.callees_of("src/lib.rs::main");
        assert!(main_callees.contains(&"src/lib.rs::helper"));
        assert!(main_callees.contains(&"src/lib.rs::Service::new"));

        let callers = graph.callers_of("src/lib.rs::helper");
        assert!(callers.contains(&"src/lib.rs::main"));
        assert_eq!(graph.resolved_edge_count(), 2);
    }

    #[test]
    fn call_graph_resolves_method_calls_on_receivers() {
        // 方法调用在源码里是 `receiver.method()`，引用记录保存整段文本。回归
        // 测试：启发式匹配曾经只按 `::` 切分，导致所有方法调用都无法解析，
        // 调用图退化成近乎为空，依赖它的规则（未调用的公共 API）全部误报。
        let index = index_of(&[(
            "src/lib.rs",
            "pub fn label(kind: Kind) -> String {\n    kind.name()\n}\n\npub struct Kind;\nimpl Kind {\n    pub fn name(&self) -> String {\n        String::new()\n    }\n}\n",
        )]);
        let graph = CallGraph::build(&index);

        assert!(graph
            .callees_of("src/lib.rs::label")
            .contains(&"src/lib.rs::Kind::name"));
        assert!(graph
            .callers_of("src/lib.rs::Kind::name")
            .contains(&"src/lib.rs::label"));
        assert_eq!(graph.resolved_edge_count(), 1);
    }

    #[test]
    fn call_graph_detects_recursion() {
        let index = index_of(&[(
            "src/lib.rs",
            "fn loop_fn(n: u32) -> u32 {\n    if n == 0 { 0 } else { loop_fn(n - 1) }\n}\n",
        )]);
        let graph = CallGraph::build(&index);
        assert!(graph
            .recursive_symbols()
            .contains(&"src/lib.rs::loop_fn".to_owned()));
    }

    #[test]
    fn call_graph_records_unresolved_calls() {
        // 调用项目内未定义的函数：既不能解析，也不是宏调用。
        let index = index_of(&[("src/lib.rs", "fn main() {\n    external_helper();\n}\n")]);
        let graph = CallGraph::build(&index);
        assert!(graph.unresolved_calls().contains("external_helper"));
        assert_eq!(graph.resolved_edge_count(), 0);
    }

    #[test]
    fn uncalled_symbols_lists_entry_points() {
        let index = index_of(&[("src/lib.rs", "fn only_caller() {}\n")]);
        let graph = CallGraph::build(&index);
        assert!(graph
            .uncalled_symbols()
            .contains(&"src/lib.rs::only_caller"));
    }

    #[test]
    fn isolated_files_are_reported() {
        let index = index_of(&[("src/alone.rs", "pub fn alone() {}\n")]);
        let graph = ModuleGraph::build(&index);
        assert_eq!(graph.isolated_files(), vec!["src/alone.rs"]);
    }
}
