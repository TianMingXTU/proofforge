//! 依赖管理（需求文档 FR-012）。
//!
//! # 检查什么
//!
//! **声明了却没有被引用的依赖**。`ModuleGraph::external_dependencies()` 已经能
//! 列出项目里实际 import 的外部模块（FR-022 的副产品），把它与构建清单里声明的
//! 依赖对一下，差值就是候选的不必要依赖。
//!
//! # 为什么只是候选
//!
//! 一个依赖「没被 import」不等于「不必要」：
//!
//! - 它可能只被宏使用（`serde::Serialize` 的 derive 展开前，源码里看不到路径）；
//! - 它可能被重新导出（`pub use` 让下游引用，自己的源码里反而不出现）；
//! - 它可能只服务于构建脚本、基准测试或被条件编译的平台分支。
//!
//! 因此每条发现都带 `needs_human_confirmation`，证据状态停在 `Suspected`：
//! 「清单里有它、源码里看不到它」是可复算的结构事实，「它可以被删除」不是。

use pf_core::finding::{Finding, FindingKind, Remediation};
use pf_core::ids::RuleId;
use pf_core::rule::{Rule, RuleSeverity};

use super::{Checker, Context};

/// Rust 构建清单里记录依赖的段名。
///
/// `dev-dependencies` 与 `build-dependencies` 也在这里：它们同样只在特定命令下
/// 被使用，源码里看不到路径是常态，但「清单里有、任何地方都看不到」依然值得
/// 人看一眼。
const DEPENDENCY_SECTIONS: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

/// FR-012：声明了却没有被引用的依赖。
pub struct UnusedDependencyChecker;

impl Checker for UnusedDependencyChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("unused-dependency"),
            "构建清单里声明却没有任何源码引用的依赖需要确认是否必要",
            RuleSeverity::Advisory,
            "unused_dependency",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let Some(manifest) = context.source("Cargo.toml") else {
            // 没有 Cargo.toml 就无从判断：Python 项目的依赖声明在
            // requirements.txt 里，格式与语义都不同，当前不支持。没有清单时
            // 不产出任何发现，而不是猜一份。
            return Vec::new();
        };
        let declared = declared_dependencies(&manifest.content);
        if declared.is_empty() {
            return Vec::new();
        }
        let referenced = referenced_crates(context);
        declared
            .into_iter()
            .filter(|(name, _)| !referenced.contains(name))
            .map(|(name, line)| {
                let location = line_location(&manifest.path, &manifest.content, line);
                Finding::new(
                    rule.id.clone(),
                    FindingKind::Other {
                        label: "unused_dependency".to_owned(),
                    },
                    format!("依赖 `{name}` 已声明但没有任何源码引用它"),
                    format!(
                        "`Cargo.toml` 里声明了 `{name}`，但项目里没有任何 import 指向它。\
                         这可能是一个真正多余的依赖，也可能它只被宏、重新导出或\
                         条件编译分支使用——后者在源码里看不到路径。\
                         请在删除前确认它不服务于上述任何一种情况。",
                    ),
                    vec![location],
                )
                .with_severity(rule.effective_severity())
                .with_fact("dependency", name.clone())
                .with_fact("needs_human_confirmation", "true")
                .with_remediation(Remediation::new(
                    "确认该依赖是否只被宏或重新导出使用；若否，从 Cargo.toml 中移除",
                ))
            })
            .collect()
    }
}

/// 从 `Cargo.toml` 文本里取出**本包**声明的依赖名（含行号）。
///
/// 只做**词法**解析：逐行看段头与 `name = ...` 条目。用真正的 TOML 解析器会更
/// 稳，但 `pf-rules` 的规则都只拿得到源码文本；更重要的是依赖声明只有两种形状
/// （`name = "1.0"` 与 `name = { version = "1.0" }`），词法扫描足以覆盖，且能给出
/// 行号——发现必须可定位（FR-064）。
///
/// `[workspace.dependencies]` **不算**：那是一份供成员 crate 用 `.workspace = true`
/// 继承的版本目录，根包自己往往一个都不用。把它当成本包依赖，会在每一个 Cargo
/// workspace 的根清单上产出成片假阳性——而假阳性会让使用者学会忽略整条规则。
fn declared_dependencies(manifest: &str) -> Vec<(String, usize)> {
    let mut in_section = false;
    let mut out = Vec::new();
    for (index, line) in manifest.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            let section = trimmed.trim_start_matches('[').trim_end_matches(']');
            in_section = is_dependency_section(section);
            continue;
        }
        if !in_section || trimmed.starts_with('#') {
            continue;
        }
        if let Some(name) = dependency_name(trimmed) {
            out.push((name, index + 1));
        }
    }
    out
}

/// 一个段是否是**本包**的依赖段。
///
/// `[target.'cfg(windows)'.dependencies]` 是：按最后一段判定。`[workspace.dependencies]`
/// 不是：按第一段排除。两者的区别是「给本包加依赖」与「给整个 workspace 提供版本
/// 目录」。
fn is_dependency_section(section: &str) -> bool {
    if section.split('.').next() == Some("workspace") {
        return false;
    }
    let last = section.rsplit('.').next().unwrap_or(section);
    DEPENDENCY_SECTIONS.contains(&last)
}

/// 从一条 `name = ...` 里取出依赖名。
///
/// `name.workspace = true` 也认：这是从 `[workspace.dependencies]` 继承版本的标准
/// 写法，声明的依赖名是 `name`。不认它会让 workspace 里每一个成员 crate 的依赖都
/// 从清单上消失，规则随之失去意义。
fn dependency_name(line: &str) -> Option<String> {
    let (key, _) = line.split_once('=')?;
    let key = key.trim().trim_matches('"');
    let key = key.strip_suffix(".workspace").unwrap_or(key);
    // 依赖名必须是合法的 crate 名：含点、空格或空的行不是依赖条目。
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(key.to_owned())
}

/// 项目里实际引用到的外部模块名集合。
///
/// 判据有两条，命中任意一条即算「已引用」：
///
/// 1. 它作为路径根出现在某条 import 引用里（`use serde::Serialize;`）；
/// 2. 它的名字在源码里以**路径根**的形式出现（`toml::from_str(...)`）。
///
/// 第二条不是冗余。Rust 允许完全限定调用而不写 `use`，本仓库自己的
/// `pf-core/src/config.rs` 就是这么用 `toml` 的——只看 import 引用会把它报成
/// 「声明了却没人用」，而那条结论是错的。只认 import 还会漏掉宏展开后才出现的
/// 路径，但那类用法本来就在人工确认的范围内，方向是安全的：漏报好过误报。
fn referenced_crates(context: &Context<'_>) -> std::collections::HashSet<String> {
    let mut referenced: std::collections::HashSet<String> = context
        .module_graph
        .external_dependencies()
        .into_iter()
        .filter_map(|(name, _)| path_root(name))
        .collect();
    for (path, _) in context.trees() {
        let Some(file) = context.source(path) else {
            continue;
        };
        for name in path_roots_in_text(&file.content) {
            referenced.insert(name);
        }
    }
    referenced
}

/// 取一条 import 路径的根段（`serde::Serialize` → `serde`）。
fn path_root(name: &str) -> Option<String> {
    let head = name.split("::").next().or_else(|| name.split('.').next())?;
    if head.is_empty() {
        return None;
    }
    Some(head.to_owned())
}

/// 从源码文本里找出全部「作为路径根出现」的标识符。
///
/// 判据是标识符后面紧跟 `::`（Rust）或 `.`（Python）：`toml::from_str` 命中，
/// 而注释里单独提到的 `toml` 不命中。宁可漏认——把注释里的一句话当成一次引用只会
/// 让该报的漏掉，而反过来会把用着的依赖报成没人用。
fn path_roots_in_text(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for character in content.chars() {
        if character.is_alphanumeric() || character == '_' {
            current.push(character);
            continue;
        }
        if !current.is_empty() {
            if character == ':' || character == '.' {
                out.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    out
}

/// 为清单里的某一行构造定位。
///
/// 清单文件没有语法树，行号来自文本扫描，因此这里按行号反查字节区间，并补上行列号
/// ——发现必须能以 `path:line:column` 的形式被人找到（FR-064）。不复用
/// [`super::location_for`]：后者要求一个树节点，而清单文件不参与解析。
fn line_location(path: &str, content: &str, line: usize) -> pf_core::location::Location {
    let mut offset = 0usize;
    let mut start = 0usize;
    let mut end = content.len();
    for (index, text) in content.split_inclusive('\n').enumerate() {
        if index + 1 == line {
            start = offset;
            end = offset + text.trim_end_matches('\n').len();
            break;
        }
        offset += text.len();
    }
    let start_line = u32::try_from(line).unwrap_or(u32::MAX);
    pf_core::location::Location::new(path.to_owned(), pf_core::location::Span::new(start, end))
        .with_lines(start_line, 1, start_line, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::with_context;

    fn manifest(deps: &str) -> String {
        format!("[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n\n[dependencies]\n{deps}\n")
    }

    #[test]
    fn a_declared_but_unreferenced_dependency_is_reported() {
        with_context(
            &[
                ("Cargo.toml", &manifest("serde = \"1.0\"\n")),
                ("src/lib.rs", "pub fn one() -> i32 { 1 }\n"),
            ],
            |context| {
                let findings = UnusedDependencyChecker.check(context);
                assert_eq!(findings.len(), 1, "{findings:?}");
                assert!(findings[0].title.contains("serde"));
                assert_eq!(
                    findings[0]
                        .facts
                        .iter()
                        .find(|(k, _)| k == "dependency")
                        .map(|(_, v)| v.as_str()),
                    Some("serde")
                );
                assert_eq!(
                    findings[0]
                        .facts
                        .iter()
                        .find(|(k, _)| k == "needs_human_confirmation")
                        .map(|(_, v)| v.as_str()),
                    Some("true"),
                    "「没被 import」不等于「不必要」，必须留给人确认"
                );
                assert_eq!(
                    findings[0].status,
                    pf_core::status::EvidenceStatus::Suspected
                );
            },
        );
    }

    #[test]
    fn a_referenced_dependency_is_not_reported() {
        with_context(
            &[
                (
                    "Cargo.toml",
                    &manifest("serde = \"1.0\"\nunused_one = \"0.1\"\n"),
                ),
                (
                    "src/lib.rs",
                    "use serde::Serialize;\npub fn one() -> i32 { 1 }\n",
                ),
            ],
            |context| {
                let findings = UnusedDependencyChecker.check(context);
                assert_eq!(findings.len(), 1, "{findings:?}");
                assert!(findings[0].title.contains("unused_one"));
            },
        );
    }

    #[test]
    fn dev_and_build_dependencies_are_checked_too() {
        with_context(
            &[
                (
                    "Cargo.toml",
                    "[dependencies]\nserde = \"1.0\"\n\n[dev-dependencies]\ntempfile = \"3\"\n",
                ),
                (
                    "src/lib.rs",
                    "use serde::Serialize;\npub fn one() -> i32 { 1 }\n",
                ),
            ],
            |context| {
                let findings = UnusedDependencyChecker.check(context);
                assert_eq!(findings.len(), 1, "{findings:?}");
                assert!(findings[0].title.contains("tempfile"));
            },
        );
    }

    #[test]
    fn a_project_without_a_manifest_yields_nothing() {
        // 没有清单就无从判断：Python 项目的依赖声明格式与语义都不同，当前不支持。
        // 没有清单时不产出发现，而不是猜一份。
        with_context(
            &[("src/lib.rs", "pub fn one() -> i32 { 1 }\n")],
            |context| {
                assert_eq!(UnusedDependencyChecker.check(context), []);
            },
        );
    }

    #[test]
    fn the_finding_points_at_the_declaration_line() {
        with_context(
            &[
                ("Cargo.toml", &manifest("serde = \"1.0\"\n")),
                ("src/lib.rs", "pub fn one() -> i32 { 1 }\n"),
            ],
            |context| {
                let findings = UnusedDependencyChecker.check(context);
                let location = findings[0].primary_location().expect("location");
                assert_eq!(location.path, "Cargo.toml");
                assert_eq!(location.start_line, Some(6), "serde 声明在第 6 行");
            },
        );
    }

    #[test]
    fn dependency_names_with_dashes_are_accepted() {
        assert_eq!(
            dependency_name("tree-sitter = \"0.20\""),
            Some("tree-sitter".to_owned())
        );
        assert_eq!(
            dependency_name("serde = { version = \"1\", features = [\"derive\"] }"),
            Some("serde".to_owned())
        );
    }

    #[test]
    fn non_dependency_entries_are_ignored() {
        // 段内的注释与非法键名不是依赖条目。
        let text = "[dependencies]\n# serde = \"1.0\"\n\"quoted.key\" = \"1\"\n";
        assert_eq!(declared_dependencies(text), []);
    }

    #[test]
    fn target_specific_dependency_sections_are_recognised() {
        let text = "[target.'cfg(windows)'.dependencies]\nwinapi = \"0.3\"\n";
        assert_eq!(declared_dependencies(text), vec![("winapi".to_owned(), 2)]);
    }

    #[test]
    fn the_workspace_dependency_catalog_is_not_the_packages_own_dependencies() {
        // `[workspace.dependencies]` 是给成员 crate 继承的版本目录，不是本包的依赖。
        // 把它算进来会在每个 workspace 的根清单上产出成片假阳性。
        let text = "[workspace.dependencies]\nserde = \"1\"\ntokio = \"1\"\n\n\
                    [dependencies]\nserde.workspace = true\n";
        assert_eq!(
            declared_dependencies(text),
            vec![("serde".to_owned(), 6)],
            "只应看到本包 [dependencies] 里的那一条"
        );
    }

    #[test]
    fn a_fully_qualified_use_counts_as_a_reference() {
        // `toml::from_str(...)` 不写 `use`，只看 import 引用会把它报成「没人用」。
        // 本仓库的 pf-core 就是这么用 toml 的——这条判据守的是一个真实的假阳性。
        with_context(
            &[
                ("Cargo.toml", &manifest("toml = \"0.8\"\n")),
                (
                    "src/config.rs",
                    "pub fn load(text: &str) -> Result<i32, String> {\n\
                     \n    toml::from_str(text).map_err(|e| e.to_string())\n}\n",
                ),
            ],
            |context| {
                assert_eq!(UnusedDependencyChecker.check(context), Vec::new());
            },
        );
    }

    #[test]
    fn a_bare_mention_in_prose_is_not_a_reference() {
        // 注释里提到依赖名不算引用，但这个方向的错误是安全的：最多漏报。
        with_context(
            &[
                ("Cargo.toml", &manifest("serde = \"1.0\"\n")),
                ("src/lib.rs", "//! 这里曾经用过 serde，现在不用了。\n"),
            ],
            |context| {
                let findings = UnusedDependencyChecker.check(context);
                assert_eq!(findings.len(), 1, "{findings:?}");
            },
        );
    }

    #[test]
    fn the_referenced_crate_name_is_the_first_path_segment() {
        // `serde::Serialize` 的 crate 名是 `serde`；少了这一步，`serde` 会被当成
        // 未引用依赖报出来。
        with_context(
            &[
                ("Cargo.toml", &manifest("serde = \"1.0\"\n")),
                (
                    "src/lib.rs",
                    "use serde::Serialize;\nuse std::collections::HashMap;\npub fn one() -> i32 { 1 }\n",
                ),
            ],
            |context| {
                let referenced = referenced_crates(context);
                assert!(referenced.contains("serde"), "{referenced:?}");
                assert!(referenced.contains("std"), "标准库也走同一条外部引用路径");
                assert_eq!(UnusedDependencyChecker.check(context), []);
            },
        );
    }
}
