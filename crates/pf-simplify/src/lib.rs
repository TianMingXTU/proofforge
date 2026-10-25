//! `ProofForge` 代码简化：候选生成、隔离补丁、回归与行为验证、人工确认后应用。
//!
//! 对应需求文档 5.8（FR-050 … FR-056）与 3.5「保留行为的代码简化」。
//!
//! # 管线
//!
//! ```text
//! 审查发现 ──► 候选生成 ──► 隔离补丁 ──► 回归验证 ──► 行为比较 ──► 指标比较
//! (FR-050)      (FR-050)      (FR-051)      (FR-052)      (FR-053)     (FR-054)
//!                                                                          │
//!                                                   人工确认 (FR-055) ◄────┘
//!                                                                          │
//!                                                   安全应用 (FR-056) ◄────┘
//! ```
//!
//! # 三条不可谈判的约束
//!
//! 1. **原仓库只读**（红线 3）。整条管线上只有 [`apply`] 写目标仓库，而它
//!    要求显式授权，且候选必须先通过回归与行为验证。
//! 2. **没验证完不是通过**（红线 4）。工具缺失、基线本就失败、没有性质规格，
//!    一律让门禁返回 [`pf_core::status::GateStatus::InsufficientEvidence`]。
//! 3. **不追求机械减少代码**（需求文档 1.3）。需要语义判断的变换只产出建议，
//!    不产出补丁；候选报告必须列出残余风险。
//!
//! # 依赖方向
//!
//! `pf-cli → pf-simplify → {pf-review, pf-rules, pf-verify, pf-symbol, pf-graph}`，
//! 全部只依赖 `pf-core` 作为领域模型。没有任何 crate 依赖 `pf-simplify`——
//! 简化是会改用户代码的能力，必须是管线的终点。

#![warn(missing_docs)]

pub mod apply;
pub mod behavior;
pub mod candidate;
pub mod edit;
pub mod error;
pub mod metrics;
pub mod regression;
pub mod report;
pub mod workspace;

pub use apply::{apply, ApplyOutcome, Authorization};
pub use behavior::{BehaviorComparison, BehaviorVerdict, ProbeComparison};
pub use candidate::{Automation, CandidateGenerator, SimplificationCandidate, SimplificationKind};
pub use edit::{apply_edits, edits_for, unified_diff, Edit, EditError};
pub use error::SimplifyError;
pub use metrics::{
    compare as compare_metrics, measure, measure_files, EngineeringMetrics, MetricsDelta,
};
pub use regression::{check as check_regression, RegressionCheck, RegressionVerdict};
pub use report::{decide_gate, CandidateOutcome, CandidateVerdict, SimplifyReport};
pub use workspace::{Patch, PatchedWorkspace};

use std::path::Path;

use pf_core::config::{Budget, Config};
use pf_core::Result;
use pf_verify::PropertySpec;
use pf_verify::Sandbox;

/// 简化引擎。
///
/// 无状态、无时钟、无随机源：同一份目标项目与同一份配置必然产出同一份报告。
/// 这让简化结论可以进审计记录，也可以跨版本 diff（FR-061、FR-070）。
#[derive(Debug, Default)]
pub struct SimplifyEngine;

/// 一次简化分析的配置。
///
/// 不派生 `PartialEq`：`properties` 里的 [`PropertySpec`] 含输入域与调用模板，
/// 比较两份配置是否相等没有意义——配置是输入，不是需要去重的值。
#[derive(Debug, Clone)]
pub struct SimplifyConfig {
    /// 审查配置：目标路径、预算、阻断级别。
    pub review: Config,
    /// 最多验证多少个候选。
    ///
    /// 每个候选都要隔离一份工作区并跑一遍测试，因此这个数字直接决定耗时。
    /// 默认 3：足以覆盖最常见的几处冗余，又不至于让一次分析跑几分钟。
    pub max_candidates: usize,
    /// 用于行为比较的性质规格（FR-053）。
    ///
    /// 为空时行为比较只能依赖测试套件，报告会明确写出覆盖范围的缺口。
    pub properties: Vec<PropertySpec>,
}

impl Default for SimplifyConfig {
    fn default() -> Self {
        Self {
            review: Config::default(),
            max_candidates: 3,
            properties: Vec::new(),
        }
    }
}

impl SimplifyEngine {
    /// 构造引擎。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// 分析目标项目，产出候选与验证结论。
    ///
    /// # 步骤
    ///
    /// 1. 运行工程质量审查，拿到发现（复用 [`pf_review::ReviewEngine`]）。
    /// 2. 从发现里生成简化候选（FR-050）。
    /// 3. 对每个候选：隔离工作区并打补丁（FR-051）→ 跑回归（FR-052）→
    ///    比较行为（FR-053）→ 比较工程指标（FR-054）。
    /// 4. 汇总门禁结论（FR-067）。
    ///
    /// # Errors
    /// 目标不可读、或审查失败时返回 [`pf_core::Error`]。单条性质「没跑出
    /// 结论」不是错误——它让该候选的结论变成未验证。
    pub fn analyze(target: &Path, config: &SimplifyConfig) -> Result<SimplifyReport> {
        let review_config = Config {
            target: target.to_path_buf(),
            ..config.review.clone()
        };
        let review = pf_review::ReviewEngine::review(&review_config)?;
        let ruleset_version = review.ruleset_version.clone();

        let mut sources = Vec::new();
        for path in
            pf_core::source::collect_source_files(target, config.review.budget.max_file_bytes)?
        {
            let relative = pf_core::source::to_relative_posix(target, &path);
            if pf_core::source::is_ignored_path(&relative) {
                continue;
            }
            if sources.len() >= config.review.budget.max_files {
                break;
            }
            if let Some(file) = pf_core::source::read_source_file(&path, target)? {
                sources.push(file);
            }
        }
        let map: pf_core::source::SourceMap = sources.iter().cloned().collect();
        let index = pf_symbol::SymbolIndex::build(&map);
        let context = pf_rules::Context::new(&index, &sources);

        let candidates = CandidateGenerator::generate(&context, &review.findings);
        let selected: Vec<_> = candidates
            .into_iter()
            .filter(|candidate| candidate.automation == Automation::Automatic)
            .take(config.max_candidates)
            .collect();

        let options = pf_adapters::ToolOptions {
            extra_args: Vec::new(),
            timeout_secs: Some(config.review.budget.command_timeout_secs),
            only_changed: false,
        };

        let mut outcomes = Vec::new();
        let baseline_metrics = measure(target, &config.review.budget)?;
        for candidate in selected {
            outcomes.push(verify_candidate(
                target,
                candidate,
                &config.review.budget,
                &config.properties,
                &options,
                &baseline_metrics,
            )?);
        }

        // 手动候选也进报告：它们是人能立刻动手的建议，只是工具不敢自动改。
        for candidate in CandidateGenerator::generate(&context, &review.findings)
            .into_iter()
            .filter(|candidate| candidate.automation == Automation::Manual)
        {
            outcomes.push(CandidateOutcome {
                candidate,
                patch: None,
                regression: None,
                behavior: None,
                metrics: None,
                verdict: CandidateVerdict::ManualOnly,
                reasons: vec!["该变换需要语义判断，工具不生成补丁".to_owned()],
            });
        }

        let (gate, reasons) = decide_gate(&outcomes);
        let mut residual_risks = Vec::new();
        for outcome in &outcomes {
            residual_risks.extend(outcome.candidate.risks.iter().cloned());
        }
        residual_risks.sort();
        residual_risks.dedup();
        residual_risks.push(
            "测试通过只说明「在这些样本下没观察到行为变化」；没有覆盖的输入空间仍是未知的。"
                .to_owned(),
        );

        Ok(SimplifyReport {
            target: target.to_path_buf(),
            candidates: outcomes,
            gate,
            reasons,
            residual_risks,
            ruleset_version,
        })
    }
}

/// 验证单个候选：隔离、打补丁、跑回归、比较行为与指标。
fn verify_candidate(
    target: &Path,
    candidate: SimplificationCandidate,
    budget: &Budget,
    properties: &[PropertySpec],
    options: &pf_adapters::ToolOptions,
    baseline_metrics: &EngineeringMetrics,
) -> Result<CandidateOutcome> {
    let mut reasons = Vec::new();
    let workspace = PatchedWorkspace::create(target, &candidate, budget).map_err(|reason| {
        pf_core::Error::SourceRead {
            path: target.to_path_buf(),
            reason: reason.to_string(),
        }
    })?;
    if workspace.patch().is_empty() {
        return Ok(CandidateOutcome {
            candidate,
            patch: Some(workspace.patch().clone()),
            regression: None,
            behavior: None,
            metrics: None,
            verdict: CandidateVerdict::Unverified,
            reasons: vec!["补丁没有产生任何文本变化，候选无效".to_owned()],
        });
    }

    let baseline_sandbox = Sandbox::isolate(target, budget)?;
    let patched_sandbox = workspace.sandbox();
    let regression = check_regression(&baseline_sandbox, patched_sandbox, options);
    let behavior = behavior::compare(&baseline_sandbox, patched_sandbox, properties, options);
    // 与基线度量**同一组文件**：此刻沙箱里已经有了回归与行为比较生成的驱动
    // 程序，按目录重新收集会把它们算成「简化后新增的代码」。
    let patched_metrics = measure_files(
        patched_sandbox.root(),
        &baseline_metrics.measured_files,
        budget,
    )?;
    let metrics = compare_metrics(baseline_metrics.clone(), patched_metrics);

    reasons.push(format!("回归：{}", regression.verdict.as_str()));
    reasons.push(format!("行为：{}", behavior.verdict.as_str()));
    reasons.extend(regression.notes.iter().cloned());
    reasons.extend(behavior.notes.iter().cloned());

    // 结论映射必须区分「测出了问题」与「没测出来」：把后者写成前者会让报告
    // 声称一个并不存在的回归，把前者写成后者会让回归悄悄通过（红线 4）。
    let verdict = match regression.verdict {
        RegressionVerdict::Regression => CandidateVerdict::RegressionDetected,
        RegressionVerdict::Unverified => CandidateVerdict::Unverified,
        RegressionVerdict::NoRegression => match behavior.verdict {
            BehaviorVerdict::Changed => CandidateVerdict::BehaviorChanged,
            BehaviorVerdict::Unverified => CandidateVerdict::Unverified,
            BehaviorVerdict::Preserved => CandidateVerdict::SafeToApply,
        },
    };

    Ok(CandidateOutcome {
        candidate,
        patch: Some(workspace.patch().clone()),
        regression: Some(regression),
        behavior: Some(behavior),
        metrics: Some(metrics),
        verdict,
        reasons,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::status::GateStatus;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "/// Wrapper.\npub fn wrapper(x: i32) -> i32 {\n    real(x)\n}\n\npub fn real(x: i32) -> i32 {\n    x + 1\n}\n",
        )
        .expect("write");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write");
        dir
    }

    #[test]
    fn analyzing_a_fixture_produces_a_report_with_a_gate() {
        // 端到端：fixture 里有一个纯转发包装，候选必须被生成并验证。
        let dir = project();
        let config = SimplifyConfig {
            review: Config {
                target: dir.path().to_path_buf(),
                ..Config::default()
            },
            ..SimplifyConfig::default()
        };
        let report = SimplifyEngine::analyze(dir.path(), &config).expect("analyze");
        assert!(
            report.candidates.iter().any(
                |outcome| outcome.candidate.kind == SimplificationKind::InlineForwardingWrapper
            ),
            "应生成内联转发包装的候选：{:?}",
            report
                .candidates
                .iter()
                .map(|o| o.candidate.kind.as_str())
                .collect::<Vec<_>>()
        );
        // 门禁必须是一个明确状态，而不是默认值。
        assert!(matches!(
            report.gate,
            GateStatus::Passed | GateStatus::Failed | GateStatus::InsufficientEvidence
        ));
        // 无论门禁是什么，残余风险都必须写出来（FR-066）。
        assert!(
            !report.residual_risks.is_empty(),
            "残余风险必须写入报告（FR-066）"
        );
    }

    #[test]
    fn the_original_project_is_untouched_by_analysis() {
        // 红线 3：分析阶段绝不写目标仓库。
        let dir = project();
        let before = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");
        let config = SimplifyConfig {
            review: Config {
                target: dir.path().to_path_buf(),
                ..Config::default()
            },
            ..SimplifyConfig::default()
        };
        let _ = SimplifyEngine::analyze(dir.path(), &config).expect("analyze");
        let after = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");
        assert_eq!(before, after);
    }

    #[test]
    fn a_report_serializes_to_json_for_the_cli() {
        let dir = project();
        let config = SimplifyConfig {
            review: Config {
                target: dir.path().to_path_buf(),
                ..Config::default()
            },
            ..SimplifyConfig::default()
        };
        let report = SimplifyEngine::analyze(dir.path(), &config).expect("analyze");
        let json = report.to_json().expect("json");
        assert!(json.contains("residual_risks"));
    }
}
