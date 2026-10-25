//! 工程指标比较（FR-054）。
//!
//! # 比较哪些指标
//!
//! 需求文档 FR-054 指定了四类：结构复杂度、重复程度、依赖数量、代码规模。
//! 每一类都由**可数的结构事实**给出，而不是主观评分：
//!
//! | 指标 | 来源 |
//! |---|---|
//! | 结构复杂度 | 符号数、函数长度类发现数 |
//! | 重复程度 | 重复代码类发现数 |
//! | 依赖数量 | 模块依赖边数、循环依赖数 |
//! | 代码规模 | 文件数、总行数、对外 API 数 |
//!
//! # 为什么不用「质量分」
//!
//! 一个加权总分会把四个维度压成一个数字，于是「复杂度降了但重复涨了」这种
//! 情况在总分上可能看不出来。这里逐项列出增减，由人判断这次简化是否划算——
//! 工具负责给出事实，不负责代替人权衡（1.3「尊重工程上下文」）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use pf_core::config::Budget;
use pf_core::finding::FindingCategory;
use pf_core::source::{collect_source_files, is_ignored_path, read_source_file, SourceFile};
use pf_graph::ModuleGraph;
use pf_rules::builtin_checkers;
use pf_symbol::SymbolIndex;

/// 一次工程指标快照。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineeringMetrics {
    /// 参与分析的文件数。
    pub file_count: usize,
    /// 符号总数。
    pub symbol_count: usize,
    /// 总行数（含空行与注释）。
    pub total_lines: usize,
    /// 对外可见 API 数量。
    pub public_api_count: usize,
    /// 模块依赖边数量。
    pub module_edge_count: usize,
    /// 循环依赖数量。
    pub cycle_count: usize,
    /// 按大类统计的发现数。
    pub findings_by_category: BTreeMap<String, usize>,
    /// 需要阻断合并的发现数。
    pub blocking_finding_count: usize,
    /// 重复代码类发现数。
    pub duplicate_hits: usize,
    /// 复杂度类发现数（函数过长、参数过多）。
    pub complexity_hits: usize,
    /// 被度量的文件（相对 root 的 POSIX 路径）。
    ///
    /// 补丁前后的比较必须在**同一组文件**上进行。沙箱里除了项目副本，还会有
    /// 驱动程序、回归用的测试文件等生成物；不固定文件集的话，「简化后多了 40 行」
    /// 可能只是数到了自己刚生成的驱动。
    pub measured_files: Vec<String>,
}

/// 补丁前后的指标差。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsDelta {
    /// 补丁前。
    pub before: EngineeringMetrics,
    /// 补丁后。
    pub after: EngineeringMetrics,
    /// 改善项（人类可读）。
    pub improvements: Vec<String>,
    /// 恶化项（人类可读）。
    pub regressions: Vec<String>,
}

impl MetricsDelta {
    /// 是否存在任何改善。
    #[must_use]
    pub fn has_improvement(&self) -> bool {
        !self.improvements.is_empty()
    }

    /// 是否存在任何恶化。
    #[must_use]
    pub fn has_regression(&self) -> bool {
        !self.regressions.is_empty()
    }
}

/// 度量一个项目目录。
///
/// # Errors
/// 目录不可读时返回 [`pf_core::Error`]。单个文件读失败不会让整体失败——它被
/// 跳过，与 `pf-review` 的行为一致：少一个文件与完全没有度量是两件事。
pub fn measure(root: &Path, budget: &Budget) -> pf_core::Result<EngineeringMetrics> {
    let mut files: Vec<String> = Vec::new();
    for path in collect_source_files(root, budget.max_file_bytes)? {
        if files.len() >= budget.max_files {
            break;
        }
        let relative = pf_core::source::to_relative_posix(root, &path);
        if is_ignored_path(&relative) {
            continue;
        }
        files.push(relative);
    }
    measure_files(root, &files, budget)
}

/// 按给定文件清单度量一个目录。
///
/// 与 [`measure`] 分开是为了让补丁前后的比较落在同一组文件上：调用方把补丁前
/// 的 [`EngineeringMetrics::measured_files`] 原样传进来，沙箱里后来生成的驱动
/// 程序与测试文件就不会被算进「简化后的规模」。
///
/// 清单里的文件不存在时跳过（补丁可能删掉了整个文件），读失败同样跳过。
///
/// # Errors
/// 只有目录不可读才会失败。
pub fn measure_files(
    root: &Path,
    relative_paths: &[String],
    budget: &Budget,
) -> pf_core::Result<EngineeringMetrics> {
    let mut sources: Vec<SourceFile> = Vec::new();
    for relative in relative_paths {
        if sources.len() >= budget.max_files {
            break;
        }
        if is_ignored_path(relative) {
            continue;
        }
        if let Some(file) = read_source_file(&root.join(relative), root)? {
            sources.push(file);
        }
    }

    let total_lines = sources.iter().map(SourceFile::line_count).sum();
    let map: pf_core::source::SourceMap = sources.iter().cloned().collect();
    let index = SymbolIndex::build(&map);
    let module_graph = ModuleGraph::build(&index);

    let context = pf_rules::Context::new(&index, &sources);
    let mut findings = Vec::new();
    for checker in builtin_checkers() {
        findings.extend(checker.check(&context));
    }

    let mut findings_by_category: BTreeMap<String, usize> = BTreeMap::new();
    for finding in &findings {
        *findings_by_category
            .entry(finding.kind.category().as_str().to_owned())
            .or_insert(0) += 1;
    }
    let duplicate_hits = findings
        .iter()
        .filter(|finding| finding.kind.category() == FindingCategory::Maintainability)
        .count();
    let complexity_hits = findings
        .iter()
        .filter(|finding| {
            finding.rule_id.local_name() == "function-too-long"
                || finding.rule_id.local_name() == "too-many-parameters"
        })
        .count();

    Ok(EngineeringMetrics {
        file_count: sources.len(),
        symbol_count: index.symbol_count(),
        total_lines,
        public_api_count: index.public_api().count(),
        module_edge_count: module_graph.edge_count(),
        cycle_count: module_graph.cycles().len(),
        findings_by_category,
        blocking_finding_count: findings.iter().filter(|f| f.is_blocking()).count(),
        duplicate_hits,
        complexity_hits,
        measured_files: sources.iter().map(|file| file.path.clone()).collect(),
    })
}

/// 比较两份指标快照，逐项给出增减说明。
#[must_use]
pub fn compare(before: EngineeringMetrics, after: EngineeringMetrics) -> MetricsDelta {
    let mut improvements = Vec::new();
    let mut regressions = Vec::new();

    if after.total_lines < before.total_lines {
        improvements.push(format!(
            "代码规模：{} → {} 行（-{}）",
            before.total_lines,
            after.total_lines,
            before.total_lines - after.total_lines
        ));
    } else if after.total_lines > before.total_lines {
        regressions.push(format!(
            "代码规模：{} → {} 行（+{}）",
            before.total_lines,
            after.total_lines,
            after.total_lines - before.total_lines
        ));
    }

    if after.symbol_count < before.symbol_count {
        improvements.push(format!(
            "结构复杂度：符号 {} → {}（-{}）",
            before.symbol_count,
            after.symbol_count,
            before.symbol_count - after.symbol_count
        ));
    } else if after.symbol_count > before.symbol_count {
        regressions.push(format!(
            "结构复杂度：符号 {} → {}（+{}）",
            before.symbol_count,
            after.symbol_count,
            after.symbol_count - before.symbol_count
        ));
    }

    if after.duplicate_hits < before.duplicate_hits {
        improvements.push(format!(
            "重复程度：{} → {} 处",
            before.duplicate_hits, after.duplicate_hits
        ));
    } else if after.duplicate_hits > before.duplicate_hits {
        regressions.push(format!(
            "重复程度：{} → {} 处",
            before.duplicate_hits, after.duplicate_hits
        ));
    }

    if after.module_edge_count < before.module_edge_count {
        improvements.push(format!(
            "依赖数量：{} → {} 条模块依赖边",
            before.module_edge_count, after.module_edge_count
        ));
    } else if after.module_edge_count > before.module_edge_count {
        regressions.push(format!(
            "依赖数量：{} → {} 条模块依赖边",
            before.module_edge_count, after.module_edge_count
        ));
    }

    if after.cycle_count > before.cycle_count {
        regressions.push(format!(
            "循环依赖：{} → {} 个",
            before.cycle_count, after.cycle_count
        ));
    }

    if after.public_api_count < before.public_api_count {
        improvements.push(format!(
            "对外 API：{} → {} 个",
            before.public_api_count, after.public_api_count
        ));
    }

    MetricsDelta {
        before,
        after,
        improvements,
        regressions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(total_lines: usize, symbols: usize) -> EngineeringMetrics {
        EngineeringMetrics {
            total_lines,
            symbol_count: symbols,
            ..EngineeringMetrics::default()
        }
    }

    #[test]
    fn a_smaller_code_base_is_reported_as_an_improvement() {
        let delta = compare(snapshot(100, 20), snapshot(80, 18));
        assert!(delta.has_improvement());
        assert!(!delta.has_regression());
    }

    #[test]
    fn a_larger_code_base_is_reported_as_a_regression() {
        let delta = compare(snapshot(80, 18), snapshot(100, 20));
        assert!(delta.has_regression());
        assert!(!delta.has_improvement());
    }

    #[test]
    fn an_unchanged_code_base_reports_neither() {
        let delta = compare(snapshot(100, 20), snapshot(100, 20));
        assert!(!delta.has_improvement());
        assert!(!delta.has_regression());
    }

    #[test]
    fn a_new_cycle_is_a_regression_even_when_lines_shrink() {
        let mut after = snapshot(50, 10);
        after.cycle_count = 1;
        let delta = compare(snapshot(100, 20), after);
        assert!(delta.has_improvement(), "行数减少应记为改善");
        assert!(delta.has_regression(), "新增循环依赖必须记为恶化");
    }

    #[test]
    fn measuring_a_real_directory_counts_files_and_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "/// Doc.\npub fn one() -> i32 { 1 }\n",
        )
        .expect("write");
        let metrics = measure(dir.path(), &Budget::default()).expect("measure");
        assert_eq!(metrics.file_count, 1);
        assert_eq!(metrics.total_lines, 2);
        assert!(metrics.symbol_count >= 1);
    }

    #[test]
    fn a_fixed_file_set_ignores_files_generated_after_the_baseline() {
        // 沙箱里除了项目副本，还有回归与行为比较生成的驱动。补丁前后的比较
        // 必须是同一组文件，否则「简化后多了 40 行」数到的可能是自己刚生成的
        // 驱动——那会让一次删除被报告成恶化（FR-054）。
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "/// Doc.\npub fn one() -> i32 { 1 }\n",
        )
        .expect("write");
        let budget = Budget::default();
        let baseline = measure(dir.path(), &budget).expect("measure");

        std::fs::create_dir_all(dir.path().join("tests")).expect("mkdir");
        std::fs::write(
            dir.path().join("tests/proofforge_driver.rs"),
            "// 生成物\nfn driver() {}\n",
        )
        .expect("write");

        let same_set =
            measure_files(dir.path(), &baseline.measured_files, &budget).expect("measure");
        assert_eq!(same_set.total_lines, baseline.total_lines);
        assert_eq!(same_set.file_count, baseline.file_count);
        // 按目录重新收集确实会把生成物算进来——这正是上面的 API 要避开的行为。
        let by_directory = measure(dir.path(), &budget).expect("measure");
        assert!(by_directory.total_lines > baseline.total_lines);
    }
}
