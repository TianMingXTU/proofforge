//! 审查编排。
//!
//! 需求文档第五章：把项目导入、工程规范审查、过度工程化审查、架构分析与
//! 结果报告串成一次完整运行，并按第七章 7.7 的质量分层给出门禁结论。
//!
//! # 职责边界
//!
//! 本 crate 负责「编排」：收集输入、调用 [`pf_rules`] 的检查器、汇总结论。
//! 它不自己实现任何检查逻辑，也不调用 LLM（AI 语义审查由后续的
//! `pf-agent` 承担）。这样静态审查部分可以完全离线、确定性地复现。
//!
//! 报告数据结构本身定义在 [`pf_core::review`]，与渲染（`pf-report`）和存储
//! （`pf-store`）共享，避免下游 crate 反向依赖本 crate。

#![warn(missing_docs)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use pf_core::config::Config;
use pf_core::evidence::EvidenceArtifact;
use pf_core::location::Location;
use pf_core::review::{decide_gate, summarize, ImpactSummary, QualityLayers, ReviewReport};
use pf_core::rule::Rule;
use pf_core::source::{
    collect_source_files, is_ignored_path, read_source_file, to_relative_posix, SourceFile,
    SourceMap,
};
use pf_git::GitRepo;
use pf_graph::ModuleGraph;
use pf_rules::{
    builtin_checkers, builtin_ruleset, ArchitectureRuleChecker, Context, RegisteredChecker,
};
use pf_symbol::SymbolIndex;

mod exemptions;

pub use pf_core::review::LayerSummary;

/// 审查引擎。
#[derive(Debug, Default)]
pub struct ReviewEngine;

impl ReviewEngine {
    /// 执行一次工程质量审查。
    ///
    /// 等价于 [`Self::review_at`] 并以系统当前日期作为豁免过期判断的参考日期。
    ///
    /// # Errors
    /// 目标路径不存在、不可读，或规则集无法序列化时返回 [`pf_core::Error`]。
    pub fn review(config: &Config) -> pf_core::Result<ReviewReport> {
        Self::review_at(config, &pf_core::exempt::today_iso())
    }

    /// 执行一次工程质量审查，并显式指定豁免过期判断的参考日期。
    ///
    /// # 为什么参考日期是参数
    ///
    /// 豁免是否过期取决于「今天是谁」。把它做成参数而不是在库里读时钟，审查
    /// 结果因此可以被精确复现：同一份代码、同一个日期，必然得到同一份报告
    /// （FR-061 可审计性）。CLI 层负责提供当前日期。
    ///
    /// # Errors
    /// 与 [`Self::review`] 相同。
    pub fn review_at(config: &Config, reference_date: &str) -> pf_core::Result<ReviewReport> {
        let started = Instant::now();
        let run_id = pf_core::ids::RunId::new();
        let mut warnings = Vec::new();

        let root = canonical_target(&config.target)?;

        // --- 1. 收集输入 ---
        let sources = collect_sources(&root, config, &mut warnings)?;
        let file_count = sources.len();

        // --- 2. 构建索引与图 ---
        let map: SourceMap = sources.iter().cloned().collect();
        let index = SymbolIndex::build(&map);
        warnings.extend(index.warnings().iter().cloned());

        let context = Context::new(&index, &sources);
        warnings.extend(context.parse_failures().iter().cloned());

        // --- 2b. 变更影响分析（FR-025）---
        //
        // 只在给出基线时计算：没有基线就无所谓「相对谁变更」，影响范围这个词
        // 本身不成立。影响集是模块依赖图上的反向可达集——被分析的文件只是
        // 「变了的那批」，依赖它们的文件全在别处。
        //
        // 图必须建在**全部**文件上，而不是本轮分析的那批：只用变更文件建图时，
        // 依赖它们的文件根本不在图里，反向可达集恒为空，「可能波及 0 个文件」
        // 于是成了一句永远为真的废话。多出来的索引成本是这项分析的真实代价。
        let impact = config.baseline.as_ref().map(|_| {
            let changed: Vec<String> = sources.iter().map(|file| file.path.clone()).collect();
            match full_module_graph(&root, config, &mut warnings) {
                Ok(graph) => impact_summary(&graph, changed),
                // 建不出全量图就如实说没有影响结论，而不是给一个恒为空的影响集。
                Err(_) => ImpactSummary::new(changed, Vec::new()),
            }
        });

        // --- 3. 执行规则 ---
        //
        // 架构规则（FR-024）来自配置而不是内置集：工具不预设分层，因此规则为空时
        // 这个检查器不参与执行，也不进入规则集元数据——一份写着「检查了架构规则」
        // 却没有规则的规则集是误导。
        let mut checkers = builtin_checkers();
        let architecture_checkers = architecture_checkers(&config.architecture_rules);
        checkers.extend(
            architecture_checkers
                .iter()
                .map(|(checker, _)| RegisteredChecker::new((*checker).clone())),
        );
        let mut findings = Vec::new();
        for checker in &checkers {
            findings.extend(checker.check(&context));
        }

        // --- 3b. 应用豁免（FR-062）---
        //
        // 发生在排序与分层之前：豁免改变的是门禁权重，被豁免的发现仍留在列表里，
        // 因此排序与汇总看到的都必须是最终状态。
        let exemption_set = exemptions::load(&root, &config.exemption_files, &mut warnings);
        let suppressed =
            exemptions::apply(&exemption_set, &mut findings, reference_date, &mut warnings);

        findings.sort_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then_with(|| {
                    let left = a
                        .primary_location()
                        .map_or(String::new(), Location::display);
                    let right = b
                        .primary_location()
                        .map_or(String::new(), Location::display);
                    left.cmp(&right)
                })
                .then_with(|| a.id.cmp(&b.id))
        });

        // --- 4. 导入项目规范（FR-058）与分层门禁 ---
        //
        // 导入发生在规则执行之后：内置检查器是 Rust 代码，导入的项目规范只补充
        // 元数据与人工审查项，不改变本次已经跑完的检查。合并后的规则集进入证据
        // 产物，因此「这次审查依据了哪些规范」可复算。
        let mut ruleset = builtin_ruleset();
        for (_, rule) in &architecture_checkers {
            ruleset.push(rule.clone());
        }
        let imported_standards = import_standards(&root, config, &mut ruleset, &mut warnings);

        let layers: QualityLayers = summarize(&findings, config.block_on);
        let gate = decide_gate(&layers, file_count, started.elapsed(), &config.budget);

        let evidence = vec![
            EvidenceArtifact::from_text(
                "analysis-summary",
                format!(
                    "files={file_count} symbols={} findings={} gate={gate}",
                    index.symbol_count(),
                    findings.len()
                ),
            )
            .with_source(run_id.to_string()),
            EvidenceArtifact::from_text("ruleset", serde_json::to_string_pretty(&ruleset)?)
                .with_source("builtin"),
            EvidenceArtifact::from_text(
                "exemptions",
                format!(
                    "{}\nreference_date={reference_date}",
                    exemptions::describe(&exemption_set, suppressed)
                ),
            )
            .with_source(run_id.to_string()),
        ];

        Ok(ReviewReport {
            run_id,
            project_id: pf_core::ids::ProjectId::new(),
            project_root: root,
            revision: config
                .revision
                .clone()
                .unwrap_or_else(|| match &config.baseline {
                    Some(baseline) => format!("workdir..{baseline}"),
                    None => "workdir".to_owned(),
                }),
            file_count,
            symbol_count: index.symbol_count(),
            findings,
            layers,
            gate,
            ruleset_version: ruleset.version.clone(),
            impact,
            imported_standards,
            duration: started.elapsed(),
            evidence,
            warnings,
        })
    }
}

/// 为整个项目建一张模块依赖图（FR-025 用）。
///
/// 与规则执行用的图分开：后者只覆盖本轮分析的文件（配了基线时就是变更文件），
/// 而影响分析要问的是「依赖它们的文件有哪些」——那批文件恰恰不在分析范围里。
///
/// 因此这里是全量索引，代价与一次不带基线的审查相当。这个代价无法避免：不建全量
/// 图，反向可达集恒为空，报告会写「可能波及 0 个文件」，一句永远为真的废话。
///
/// # Errors
/// 与 [`collect_sources`] 相同。
fn full_module_graph(
    root: &Path,
    config: &Config,
    warnings: &mut Vec<String>,
) -> pf_core::Result<ModuleGraph> {
    let mut sources = Vec::new();
    for path in collect_source_files(root, config.budget.max_file_bytes)? {
        if sources.len() >= config.budget.max_files {
            warnings.push(format!(
                "file budget exhausted ({} files); the impact set may be incomplete",
                config.budget.max_files
            ));
            break;
        }
        let relative = to_relative_posix(root, &path);
        if is_ignored_path(&relative) {
            continue;
        }
        if let Some(file) = read_source_file(&path, root)? {
            sources.push(file);
        }
    }
    let map: SourceMap = sources.iter().cloned().collect();
    Ok(ModuleGraph::build(&SymbolIndex::build(&map)))
}

/// 计算变更影响分析（FR-025）。
///
/// 影响集是模块依赖图上的**反向可达集**：依赖了变更文件的文件，以及依赖了那些
/// 文件、再往上的文件。被分析的文件本身不算「受影响」——它们已经在这一轮的审查
/// 范围里，再列一遍只会让读者以为影响面比实际大。
///
/// 只按文件路径计算，不区分变更类型：文件级的影响分析回答「要看哪些地方」，而
/// 「这次改动具体动摇了哪个符号」需要符号级依赖边，当前依赖图没有。
///
/// # 为什么要单独提成函数
///
/// 调用它的路径以 Git 为前提（`collect_sources` 要读基线树），而这里是纯计算。
/// 分开之后影响分析本身可以被直接测试，不必依赖一个真的 Git 仓库。
fn impact_summary(graph: &ModuleGraph, changed: Vec<String>) -> ImpactSummary {
    let mut impacted: BTreeSet<String> = changed
        .iter()
        .flat_map(|path| graph.impact_set(path))
        .collect();
    impacted.retain(|path| !changed.contains(path));
    ImpactSummary::new(changed, impacted.into_iter().collect())
}

/// 为配置里的架构规则构造检查器（FR-024）。
///
/// 返回「检查器 + 规则元数据」的配对：执行时用前者，规则集版本记录时用后者。
/// 一条架构规则对应一个检查器实例，因此每条规则的名称与理由都能单独出现在证据里；
/// 规则为空时返回空列表，调用方据此跳过整件事——一份写着「检查了架构规则」却没有任何
/// 规则的规则集是误导。
fn architecture_checkers(
    rules: &[pf_core::config::ArchitectureRule],
) -> Vec<(ArchitectureRuleChecker, Rule)> {
    rules
        .iter()
        .map(|entry| {
            let checker = ArchitectureRuleChecker::new(vec![entry.clone()]);
            let rule = Rule::builtin(
                pf_core::ids::RuleId::builtin("architecture-rule"),
                entry.reason.as_deref().map_or_else(
                    || "项目定义的依赖方向约束（未写理由）".to_owned(),
                    ToOwned::to_owned,
                ),
                pf_core::rule::RuleSeverity::Mandatory,
                "architecture_rule",
            );
            (checker, rule)
        })
        .collect()
}

/// 导入配置里列出的项目规范，并合并进 `ruleset`（FR-058）。
///
/// # 诚实的边界
///
/// 导入失败不会让审查失败——一份写错的 `AGENTS.md` 不应该让整个仓库失去审查
/// 结果。失败以警告形式进入报告，调用方因此知道「本该生效的规范没有生效」。
///
/// Markdown 导入的规则全部是 [`pf_core::rule::RuleCheck::Manual`]：把一句意图
/// 猜成一条可执行查询，再报告「该规则已检查并通过」，是在伪造证据（红线 4）。
fn import_standards(
    root: &Path,
    config: &Config,
    ruleset: &mut pf_core::rule::RuleSet,
    warnings: &mut Vec<String>,
) -> Vec<pf_core::review::ImportedStandard> {
    let mut imported = Vec::new();
    for file in &config.rule_files {
        let path = if file.is_absolute() {
            file.clone()
        } else {
            root.join(file)
        };
        match pf_rules::import_file(&path) {
            Ok(standard) => {
                let automated = standard
                    .ruleset
                    .rules
                    .iter()
                    .filter(|rule| rule.is_automated())
                    .count();
                warnings.extend(standard.notes.iter().cloned());
                imported.push(pf_core::review::ImportedStandard {
                    source: path.display().to_string(),
                    name: standard.ruleset.name.clone(),
                    version: standard.ruleset.version.clone(),
                    rule_count: standard.ruleset.rules.len(),
                    automated_count: automated,
                    notes: standard.notes.clone(),
                });
                ruleset.merge(&standard.ruleset);
            }
            Err(reason) => warnings.push(format!(
                "项目规范 {} 导入失败（{reason}），该规范没有参与本次审查。",
                path.display()
            )),
        }
    }
    imported
}

/// 把目标路径规范化为绝对路径并验证其存在。
fn canonical_target(target: &Path) -> pf_core::Result<PathBuf> {
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| pf_core::Error::TargetNotFound {
                path: target.to_path_buf(),
            })?
            .join(target)
    };
    let canonical = absolute
        .canonicalize()
        .map_err(|_| pf_core::Error::TargetNotFound {
            path: target.to_path_buf(),
        })?;
    if !canonical.is_dir() {
        return Err(pf_core::Error::TargetNotDirectory {
            path: target.to_path_buf(),
        });
    }
    Ok(canonical)
}

/// 收集参与分析的源文件。
///
/// 超过预算上限的文件会被跳过并记入警告，而不是让整体失败。
///
/// 配置了 `baseline` 时只收集相对该引用发生变化的文件（FR-005）。基线对比是
/// 「基线树 ↔ 工作区磁盘内容」，因为工作区本身没有 Git 引用；删除的文件不会
/// 出现在磁盘上，自然不被分析。
fn collect_sources(
    root: &Path,
    config: &Config,
    warnings: &mut Vec<String>,
) -> pf_core::Result<Vec<SourceFile>> {
    let changed = match &config.baseline {
        Some(baseline) => Some(baseline_changed_files(root, baseline)?),
        None => None,
    };
    let paths = collect_source_files(root, config.budget.max_file_bytes)?;
    let mut sources = Vec::new();
    for path in paths {
        if sources.len() >= config.budget.max_files {
            warnings.push(format!(
                "file budget exhausted ({} files); remaining files were not analyzed",
                config.budget.max_files
            ));
            break;
        }
        let relative = to_relative_posix(root, &path);
        if is_ignored_path(&relative) {
            continue;
        }
        if let Some(changed) = &changed {
            if !changed.contains(&relative) {
                continue;
            }
        }
        match read_source_file(&path, root)? {
            Some(file) => sources.push(file),
            None => warnings.push(format!("skipped non-UTF-8 file: {relative}")),
        }
    }
    Ok(sources)
}

/// 计算相对基线引用发生变化（新增或修改）的文件集合。
///
/// 路径使用仓库相对 POSIX 形式，与 [`to_relative_posix`] 的输出一致。
///
/// # Errors
/// 目标不是 Git 仓库（[`pf_core::Error::RepositoryOpen`]）或基线引用无法解析
/// （[`pf_core::Error::RevisionNotFound`]）时返回错误。基线分析要求明确的 Git
/// 上下文：无法确定基线时返回错误，而不是静默退化为全量分析——后者会让调用方
/// 以为只看过变更，实际上看过全部（AGENTS.md 安全红线 4）。
fn baseline_changed_files(root: &Path, baseline: &str) -> pf_core::Result<BTreeSet<String>> {
    let repo = GitRepo::open(root).map_err(|reason| pf_core::Error::RepositoryOpen {
        path: root.to_path_buf(),
        reason: reason.to_string(),
    })?;
    let baseline_id =
        repo.resolve(baseline)
            .map_err(|reason| pf_core::Error::RevisionNotFound {
                revision: baseline.to_owned(),
                reason: reason.to_string(),
            })?;
    let baseline_tree = repo
        .tree_files(&repo.tree_of(&baseline_id).map_err(|reason| {
            pf_core::Error::RevisionNotFound {
                revision: baseline.to_owned(),
                reason: reason.to_string(),
            }
        })?)
        .map_err(|reason| pf_core::Error::RepositoryOpen {
            path: root.to_path_buf(),
            reason: format!("cannot read tree of {baseline:?}: {reason}"),
        })?;

    let mut changed = BTreeSet::new();
    for path in collect_source_files(root, usize::MAX)? {
        let relative = to_relative_posix(root, &path);
        let Some(blob_id) = baseline_tree.get(&relative).copied() else {
            // 基线中不存在：新增文件。
            changed.insert(relative);
            continue;
        };
        let current = std::fs::read(&path).unwrap_or_default();
        // 读不到基线内容时按「已变更」处理：宁可多审，不可漏审。
        let unchanged = repo
            .read_blob(&blob_id)
            .is_ok_and(|baseline_text| baseline_text.as_bytes() == current.as_slice());
        if !unchanged {
            changed.insert(relative);
        }
    }
    Ok(changed)
}

/// 便捷函数：对单个路径执行审查。
///
/// # Errors
/// 与 [`ReviewEngine::review`] 相同。
pub fn review_path(path: impl AsRef<Path>) -> pf_core::Result<ReviewReport> {
    let config = Config::for_target(path.as_ref());
    ReviewEngine::review(&config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::status::{GateStatus, Severity};

    #[test]
    fn review_reports_missing_target() {
        let err = review_path("/definitely/not/a/real/path").expect_err("should fail");
        assert!(matches!(err, pf_core::Error::TargetNotFound { .. }));
    }

    #[test]
    fn review_reports_file_target_as_error() {
        // 目标是文件而不是目录时必须报错，而不是静默返回空结果。
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let err = review_path(manifest_dir.join("Cargo.toml")).expect_err("should fail");
        assert!(matches!(err, pf_core::Error::TargetNotDirectory { .. }));
    }

    #[test]
    fn review_of_real_crate_produces_findings() {
        // 对本仓库自身的 pf-core 执行一次真实审查。
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let pf_core = manifest_dir.join("../pf-core");
        let report = review_path(pf_core).expect("review");
        assert!(report.file_count > 0);
        assert_eq!(report.gate, GateStatus::Passed);
        assert_ne!(report.ruleset_version, "", "规则集版本必须出现在报告里");
        assert!(report.evidence.iter().all(EvidenceArtifact::verify));
    }

    #[test]
    fn review_is_deterministic_apart_from_identifiers() {
        // 相同输入必须得到相同的发现集合与门禁结论；只有运行标识不同。
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let pf_core = manifest_dir.join("../pf-core");
        let first = review_path(&pf_core).expect("first");
        let second = review_path(&pf_core).expect("second");
        assert_eq!(first.file_count, second.file_count);
        assert_eq!(first.symbol_count, second.symbol_count);
        assert_eq!(first.gate, second.gate);
        let left: Vec<String> = first
            .findings
            .iter()
            .map(|finding| format!("{}", finding.rule_id))
            .collect();
        let right: Vec<String> = second
            .findings
            .iter()
            .map(|finding| format!("{}", finding.rule_id))
            .collect();
        assert_eq!(left, right);
    }

    /// 构造一个带 Git 历史的临时项目：先提交一份干净的 lib.rs，再追加一个有
    /// 命名问题的函数。
    fn repo_with_baseline() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(
            src.join("lib.rs"),
            "//! fixture\n\n/// Documented.\npub fn documented() {}\n",
        )
        .expect("write clean lib.rs");

        // pf-git 只读，建仓与提交需要借助 git CLI。参数是固定字面量，不来自
        // 被审查仓库，因此不违反 AGENTS.md 安全红线 1。
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "fixture@example.invalid"],
            vec!["config", "user.name", "fixture"],
            vec!["add", "-A"],
            vec!["commit", "-qm", "clean baseline"],
        ] {
            let status = std::process::Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        }

        std::fs::write(
            src.join("lib.rs"),
            "//! fixture\n\n/// Documented.\npub fn documented() {}\n\npub fn badName() {}\n",
        )
        .expect("write dirty lib.rs");

        let baseline = GitRepo::open(dir.path())
            .expect("open repo")
            .head_id()
            .expect("head")
            .to_string();
        (dir, baseline)
    }

    #[test]
    fn baseline_restricts_analysis_to_changed_files() {
        // FR-005：增量分析。变更文件里有命名问题，未变更的干净文件不应被分析。
        // 回归测试：`--baseline` 曾经被解析后丢弃，输出与全量分析完全一致。
        let (dir, baseline) = repo_with_baseline();
        let mut config = Config::for_target(dir.path());
        config.baseline = Some(baseline.clone());

        let incremental = ReviewEngine::review(&config).expect("incremental review");
        let full = ReviewEngine::review(&Config::for_target(dir.path())).expect("full review");

        assert_eq!(incremental.file_count, 1, "只有变更文件应参与分析");
        assert_eq!(full.file_count, 1, "夹具本身只有一个文件");
        // 报告必须标注这是增量分析，否则读者无法判断证据覆盖范围。
        assert!(
            incremental.revision.ends_with(&format!("..{baseline}")),
            "revision should record the baseline: {}",
            incremental.revision
        );
        assert_eq!(full.revision, "workdir", "全量分析不应带基线标记");
    }

    #[test]
    fn baseline_reports_missing_repository_as_error() {
        // 不是 Git 仓库却要求基线分析：必须报错，不能静默退化为全量分析
        // （AGENTS.md 安全红线 4）。
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::for_target(dir.path());
        config.baseline = Some("HEAD".to_owned());
        assert!(ReviewEngine::review(&config).is_err());
    }

    #[test]
    fn baseline_rejects_unresolvable_revision() {
        let (dir, _) = repo_with_baseline();
        let mut config = Config::for_target(dir.path());
        config.baseline = Some("no-such-ref".to_owned());
        let err = ReviewEngine::review(&config).expect_err("should fail");
        assert!(
            matches!(err, pf_core::Error::RevisionNotFound { .. }),
            "unexpected error: {err:?}"
        );
    }

    /// 构造一个只有命名问题的临时项目，并写一份只豁免该问题的 TOML。
    ///
    /// `badName` 也带文档注释：否则它会同时触发 `public-api-missing-docs`，
    /// 那条是强制性规则，豁免了命名也照样阻断，测试就看不出来豁免是否生效。
    fn project_with_exemption() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(
            src.join("lib.rs"),
            "//! fixture\n\n/// Documented.\npub fn documented() {}\n\n/// Documented.\npub fn badName() {}\n",
        )
        .expect("write lib.rs");
        let exemption = dir.path().join("exemptions.toml");
        std::fs::write(
            &exemption,
            "[[exemptions]]\nrule_id = \"builtin.naming-convention\"\n\
             scope = { kind = \"rule\" }\nreason = \"历史 API 名，改名排期在 #42\"\n\
             granted_by = \"alice\"\n",
        )
        .expect("write exemptions");
        (dir, exemption)
    }

    /// 命名问题的阻断阈值设为 `medium`：该规则的默认严重程度就是 medium，
    /// 用默认的 `high` 做阈值时它本来就不阻断，测不出豁免的效果。
    fn config_for(dir: &tempfile::TempDir) -> Config {
        let mut config = Config::for_target(dir.path());
        config.block_on = Severity::Medium;
        config
    }

    #[test]
    fn an_exemption_suppresses_the_gate_but_keeps_the_finding() {
        // FR-062：豁免让门禁不再被这条发现阻断，但发现本身必须仍出现在报告里，
        // 并带上理由与批准人——否则「压掉了多少问题」无从审计。
        let (dir, exemption) = project_with_exemption();
        let mut config = config_for(&dir);
        config.exemption_files = vec![exemption];

        let report = ReviewEngine::review_at(&config, "2026-10-25").expect("review");

        assert_eq!(report.exempted_count(), 1, "应恰好压制一条发现");
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.is_exempted())
            .expect("豁免后的发现仍在报告里");
        let exemption = finding.exemption.as_ref().expect("exemption");
        assert_eq!(exemption.granted_by, "alice");
        assert!(exemption.reason.contains("#42"));
        // 发现内容没有被豁免改写。
        assert!(finding.title.contains("badName"), "{}", finding.title);
        assert_eq!(report.layers.engineering.count, 1);
        assert_eq!(report.layers.engineering.exempted, 1);
        assert_eq!(report.layers.engineering.confirmed, 0);
        assert_eq!(report.layers.engineering.blocking, 0);
        assert_eq!(report.gate, GateStatus::Passed);
    }

    #[test]
    fn the_same_project_without_the_exemption_still_fails_the_gate() {
        // 对照实验：去掉豁免文件后，同一条发现必须重新阻断合并。否则上面的
        // 「通过」可能只是这条规则本来就不阻断。
        let (dir, _) = project_with_exemption();
        let report = ReviewEngine::review_at(&config_for(&dir), "2026-10-25").expect("review");
        assert_eq!(report.exempted_count(), 0);
        assert_eq!(report.layers.engineering.confirmed, 1);
        assert_eq!(report.layers.engineering.blocking, 1);
        assert_eq!(report.gate, GateStatus::Failed);
    }

    #[test]
    fn an_expired_exemption_is_reported_and_stops_suppressing() {
        let (dir, exemption) = project_with_exemption();
        std::fs::write(
            &exemption,
            "[[exemptions]]\nrule_id = \"builtin.naming-convention\"\n\
             scope = { kind = \"rule\" }\nreason = \"临时豁免\"\ngranted_by = \"alice\"\n\
             expires_on = \"2026-01-01\"\n",
        )
        .expect("rewrite exemptions");
        let mut config = config_for(&dir);
        config.exemption_files = vec![exemption];

        let report = ReviewEngine::review_at(&config, "2026-10-25").expect("review");

        assert_eq!(report.exempted_count(), 0, "过期豁免不得压制发现");
        assert_eq!(report.gate, GateStatus::Failed);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("过期")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn impact_analysis_reports_the_reverse_reachable_set() {
        // FR-025。变更影响回答「改了它要看哪些地方」，而本轮只分析了变更文件本身，
        // 依赖它们的文件在别处——影响集正是把它们算出来。
        let map: SourceMap = [
            ("src/base.rs", "pub fn base() {}\n"),
            ("src/mid.rs", "use crate::base;\npub fn mid() {}\n"),
            ("src/top.rs", "use crate::mid;\npub fn top() {}\n"),
            ("src/unrelated.rs", "pub fn other() {}\n"),
        ]
        .iter()
        .map(|(path, content)| SourceFile::new(*path, *content))
        .collect();
        let graph = ModuleGraph::build(&SymbolIndex::build(&map));

        let impact = impact_summary(&graph, vec!["src/base.rs".to_owned()]);

        assert_eq!(impact.changed, vec!["src/base.rs".to_owned()]);
        assert_eq!(
            impact.impacted,
            vec!["src/mid.rs".to_owned(), "src/top.rs".to_owned()],
            "影响集是反向可达集，且不含无关文件"
        );
    }

    #[test]
    fn a_changed_file_is_not_listed_as_impacted() {
        // 变更文件已经在本轮审查范围里，再列一遍会让读者以为影响面比实际大。
        let map: SourceMap = [
            ("src/base.rs", "pub fn base() {}\n"),
            ("src/mid.rs", "use crate::base;\npub fn mid() {}\n"),
        ]
        .iter()
        .map(|(path, content)| SourceFile::new(*path, *content))
        .collect();
        let graph = ModuleGraph::build(&SymbolIndex::build(&map));

        let impact = impact_summary(
            &graph,
            vec!["src/base.rs".to_owned(), "src/mid.rs".to_owned()],
        );

        assert_eq!(impact.changed.len(), 2);
        assert!(
            impact.impacted.is_empty(),
            "两个文件都变了，没有第三个文件受影响：{:?}",
            impact.impacted
        );
    }

    #[test]
    fn an_unreadable_exemption_file_is_reported_not_ignored() {
        // 写错路径/格式的豁免文件必须让人知道它没生效（红线 4）。
        let (dir, _) = project_with_exemption();
        let mut config = config_for(&dir);
        config.exemption_files = vec![PathBuf::from("does-not-exist.toml")];

        let report = ReviewEngine::review_at(&config, "2026-10-25").expect("review");

        assert_eq!(report.exempted_count(), 0);
        assert_eq!(report.gate, GateStatus::Failed);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("豁免文件")),
            "{:?}",
            report.warnings
        );
    }
}
