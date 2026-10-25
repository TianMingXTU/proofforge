//! 审查结果与质量分层。
//!
//! 需求文档 7.7：质量结论必须分层呈现，正确性缺陷不得被良好的代码风格
//! 抵消。因此 [`QualityLayers`] 与门禁判定被定义为领域模型的一部分，由
//! `pf-review` 填充、`pf-report` 渲染、`pf-store` 落库，三方共享同一份
//! 语义而不互相依赖。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Budget;
use crate::evidence::EvidenceArtifact;
use crate::finding::{Finding, FindingCategory};
use crate::ids::{FindingId, ProjectId, RunId};
use crate::status::{GateStatus, Severity};

/// 一次审查运行的结果。
///
/// 该结构是纯数据：不持有文件句柄或进程状态，因此可以跨 crate 传递、
/// 序列化归档，并在测试中直接构造。
#[derive(Debug, Clone)]
pub struct ReviewReport {
    /// 运行标识。
    pub run_id: RunId,
    /// 项目标识（按路径派生，同一项目保持稳定）。
    pub project_id: ProjectId,
    /// 被审查的项目根目录。
    pub project_root: PathBuf,
    /// 代码版本（Git 引用或 `workdir`）。
    pub revision: String,
    /// 分析到的文件数量。
    pub file_count: usize,
    /// 索引到的符号数量。
    pub symbol_count: usize,
    /// 全部发现。
    pub findings: Vec<Finding>,
    /// 质量分层结论（需求文档 7.7）。
    pub layers: QualityLayers,
    /// 门禁结论。
    pub gate: GateStatus,
    /// 使用的规则集版本。
    pub ruleset_version: String,
    /// 本次运行导入的项目规范（FR-058）。
    ///
    /// 与内置规则集分开记录：项目规范的版本、来源与导入时做出的判断都必须能
    /// 单独追溯，混在 `ruleset_version` 里会让「这条结论依据哪份规范」变模糊。
    pub imported_standards: Vec<ImportedStandard>,
    /// 执行耗时。
    pub duration: Duration,
    /// 证据产物（分析摘要、规则集快照等）。
    pub evidence: Vec<EvidenceArtifact>,
    /// 变更影响分析（FR-025）。
    ///
    /// 只在给出 `--baseline` 时填充：被分析的文件是「相对基线发生变化的文件」，
    /// 而影响范围是「依赖它们的文件」——改了一个文件，测试与调用方可能全在别处。
    /// 两者不同，因此分开记录；`None` 表示本次没有基线，也就无所谓影响范围。
    pub impact: Option<ImpactSummary>,
    /// 警告（解析失败、超预算跳过等）。
    pub warnings: Vec<String>,
}

/// 一次变更影响分析的结论（FR-025）。
///
/// 影响集是模块依赖图上的**反向可达集**：直接与传递依赖被改文件的全部文件。
/// 它回答「这次改动可能波及谁」，不回答「谁需要重跑测试」——后者还要看测试与
/// 实现是否在同一个文件里，那需要调用图反向可达，当前不做。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactSummary {
    /// 被分析的文件（相对基线发生变化的那批）。
    pub changed: Vec<String>,
    /// 可能受影响的文件（反向可达集，不含被分析文件自身）。
    pub impacted: Vec<String>,
}

impl ImpactSummary {
    /// 构造一份影响分析结论。
    ///
    /// 两个列表都排序：报告要能跨运行 diff，而集合的迭代顺序不稳定。
    #[must_use]
    pub fn new(mut changed: Vec<String>, mut impacted: Vec<String>) -> Self {
        changed.sort();
        changed.dedup();
        impacted.sort();
        impacted.dedup();
        Self { changed, impacted }
    }
}

impl ReviewReport {
    /// 达到阻断级别的发现（严重程度与证据状态同时满足，且未被豁免）。
    ///
    /// 只被证据支持的发现才计入阻断：推测性问题必须由人确认，不能直接
    /// 阻挡合并（需求文档 3.7）。已豁免的发现同样不阻断（FR-062）。
    pub fn blocking_findings(&self) -> impl Iterator<Item = &Finding> {
        let block_on = self.layers.block_on;
        self.findings.iter().filter(move |finding| {
            !finding.is_exempted()
                && finding.severity.at_least(block_on)
                && finding.status.is_evidence_backed()
        })
    }

    /// 已被豁免压制的发现数量（FR-062）。
    ///
    /// 单独给出这个数字而不是只在各层里体现：审计时需要一眼看到「这次运行压下
    /// 了多少问题」。
    #[must_use]
    pub fn exempted_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|finding| finding.is_exempted())
            .count()
    }

    /// 按严重程度分组的发现数量。
    #[must_use]
    pub fn severity_histogram(&self) -> BTreeMap<Severity, usize> {
        let mut histogram = BTreeMap::new();
        for finding in &self.findings {
            *histogram.entry(finding.severity).or_insert(0) += 1;
        }
        histogram
    }

    /// 按种类分组的发现数量，按数量降序、同数量按标签升序，保证输出稳定。
    #[must_use]
    pub fn kind_histogram(&self) -> Vec<(String, usize)> {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for finding in &self.findings {
            *counts.entry(finding.kind.label().to_owned()).or_insert(0) += 1;
        }
        let mut items: Vec<(String, usize)> = counts.into_iter().collect();
        items.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        items
    }

    /// 找出指定 ID 的发现。
    #[must_use]
    pub fn finding(&self, id: &FindingId) -> Option<&Finding> {
        self.findings.iter().find(|finding| &finding.id == id)
    }

    /// 涉及的文件列表（去重且有序）。
    #[must_use]
    pub fn affected_files(&self) -> Vec<&str> {
        let mut files: Vec<&str> = self
            .findings
            .iter()
            .flat_map(|finding| finding.locations.iter())
            .map(|location| location.path.as_str())
            .collect();
        files.sort_unstable();
        files.dedup();
        files
    }
}

/// 一次导入的项目规范在报告里的摘要（FR-058）。
///
/// 只存摘要而不是完整规则集：报告要回答「这次审查依据了哪些规范、各自什么版本、
/// 导入时做了什么判断」，完整的规则定义属于证据产物。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedStandard {
    /// 来源（文件路径或格式名）。
    pub source: String,
    /// 规则集名称。
    pub name: String,
    /// 规则集版本。
    pub version: String,
    /// 导入到的规则条数。
    pub rule_count: usize,
    /// 其中有自动化检查的条数；其余只能人工审查。
    pub automated_count: usize,
    /// 导入过程中做出的判断，逐条写明。
    ///
    /// 不可省略：从自然语言提取规范本质上是启发式的，读者必须能看到工具
    /// 「读了什么、猜成了什么」。
    pub notes: Vec<String>,
}

/// 质量分层（需求文档 7.7）。
///
/// 分层的目的：严重正确性缺陷不得被良好的代码风格评分抵消，因此各层独立
/// 统计，门禁按最严重的层得出结论。
#[derive(Debug, Clone)]
pub struct QualityLayers {
    /// 第一层：正确性与安全性。
    pub correctness: LayerSummary,
    /// 第二层：工程规范。
    pub engineering: LayerSummary,
    /// 第三层：可维护性与设计。
    pub maintainability: LayerSummary,
    /// 第四层：验证可信度。
    pub verification: LayerSummary,
    /// 阻断阈值。
    pub block_on: Severity,
}

impl Default for QualityLayers {
    /// 阻断阈值与 [`crate::config::Config`] 的默认值保持一致（`high`），
    /// 避免「默认构造的报告」与「默认配置的审查」给出不同的阻断线。
    fn default() -> Self {
        Self {
            correctness: LayerSummary::default(),
            engineering: LayerSummary::default(),
            maintainability: LayerSummary::default(),
            verification: LayerSummary::default(),
            block_on: crate::config::default_block_on(),
        }
    }
}

/// 单层的汇总。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerSummary {
    /// 该层发现数量（含已豁免的）。
    pub count: usize,
    /// 其中已被证据支持、且未被豁免的数量。
    pub confirmed: usize,
    /// 其中已被豁免压制的数量（FR-062）。
    ///
    /// 单独计数而不是从 [`Self::count`] 里减掉：「这一层有 12 个问题，其中
    /// 9 个被豁免」与「这一层有 3 个问题」是不同的质量信号。
    pub exempted: usize,
    /// 其中达到阻断阈值、有证据支持且未被豁免的数量。
    ///
    /// 门禁只看这一项：它同时满足「严重程度够高」「有证据」「没有被显式豁免」
    /// 三个条件，是 [`crate::config::Config::block_on`] 的直接体现。
    pub blocking: usize,
}

impl LayerSummary {
    /// 未被豁免的发现数量。
    #[must_use]
    pub const fn active(&self) -> usize {
        self.count - self.exempted
    }

    /// 该层是否存在未被豁免的问题。
    #[must_use]
    pub const fn has_issues(&self) -> bool {
        self.active() > 0
    }
}

/// 按质量分层汇总发现。
///
/// 架构问题归入正确性层：循环依赖这类结构缺陷会直接影响行为可预测性，
/// 与可维护性层的风格问题性质不同（需求文档 7.7：各层独立，不互相抵消）。
#[must_use]
pub fn summarize(findings: &[Finding], block_on: Severity) -> QualityLayers {
    let mut layers = QualityLayers {
        block_on,
        ..QualityLayers::default()
    };
    for finding in findings {
        let layer = match finding.kind.category() {
            // 架构问题与正确性问题共用第一层：循环依赖会直接影响行为可预测性。
            FindingCategory::Correctness | FindingCategory::Architecture => &mut layers.correctness,
            FindingCategory::Engineering => &mut layers.engineering,
            FindingCategory::Maintainability => &mut layers.maintainability,
            FindingCategory::Verification => &mut layers.verification,
        };
        layer.count += 1;
        if finding.is_exempted() {
            layer.exempted += 1;
            continue;
        }
        if finding.status.is_evidence_backed() {
            layer.confirmed += 1;
        }
        // 阻断判定只看未被豁免、有证据支持、且严重程度达到阈值的发现。
        // 三层条件缺一不可：推测性问题必须由人确认，被豁免的问题是已裁决的，
        // 低于阈值的只是提示（需求文档 3.7、FR-062、FR-063）。
        if finding.status.is_evidence_backed() && finding.severity.at_least(block_on) {
            layer.blocking += 1;
        }
    }
    layers
}

/// 门禁判定。
///
/// 优先级：执行失败 > 发现阻断问题 > 通过。证据不足必须与「通过」区分开
/// （需求文档 14.3）：检查未能完整执行时绝不能返回 [`GateStatus::Passed`]。
///
/// 「阻断问题」按各层的 [`LayerSummary::blocking`] 判定，因此阈值来自
/// [`crate::config::Config::block_on`] 而不是硬编码的严重程度。
#[must_use]
pub fn decide_gate(
    layers: &QualityLayers,
    file_count: usize,
    elapsed: Duration,
    budget: &Budget,
) -> GateStatus {
    // 没有任何可分析文件时，无法得出任何结论。
    if file_count == 0 {
        return GateStatus::InsufficientEvidence;
    }
    // 超过任务预算时结果不完整。
    if elapsed > Duration::from_secs(budget.task_timeout_secs) {
        return GateStatus::InsufficientEvidence;
    }
    let blocking = layers.correctness.blocking
        + layers.engineering.blocking
        + layers.maintainability.blocking
        + layers.verification.blocking;
    if blocking > 0 {
        return GateStatus::Failed;
    }
    GateStatus::Passed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::FindingKind;
    use crate::ids::RuleId;
    use crate::location::{Location, Span};
    use crate::status::EvidenceStatus;
    use std::path::PathBuf;

    fn finding(severity: Severity, status: EvidenceStatus, category: FindingCategory) -> Finding {
        Finding::new(
            RuleId::builtin("test"),
            match category {
                FindingCategory::Correctness => FindingKind::UnhandledError,
                FindingCategory::Engineering => FindingKind::NamingViolation,
                FindingCategory::Architecture => FindingKind::CircularDependency,
                FindingCategory::Maintainability => FindingKind::UselessAbstraction,
                FindingCategory::Verification => FindingKind::MissingTestCoverage,
            },
            "t",
            "d",
            vec![Location::new("a.rs".to_owned(), Span::new(0, 1))],
        )
        .with_severity(severity)
        .with_status(status)
    }

    #[test]
    fn summarize_counts_by_layer() {
        let findings = vec![
            finding(
                Severity::High,
                EvidenceStatus::Reproduced,
                FindingCategory::Correctness,
            ),
            finding(
                Severity::Low,
                EvidenceStatus::Suspected,
                FindingCategory::Maintainability,
            ),
            finding(
                Severity::Info,
                EvidenceStatus::Suspected,
                FindingCategory::Verification,
            ),
        ];
        let layers = summarize(&findings, Severity::High);
        assert_eq!(layers.correctness.count, 1);
        assert_eq!(layers.correctness.confirmed, 1);
        assert_eq!(layers.maintainability.count, 1);
        assert_eq!(layers.maintainability.confirmed, 0);
        assert_eq!(layers.verification.count, 1);
    }

    #[test]
    fn architecture_findings_count_as_correctness() {
        // 循环依赖会直接影响行为可预测性，必须计入正确性层而不是可维护性层。
        let layers = summarize(
            &[finding(
                Severity::High,
                EvidenceStatus::StaticConfirmed,
                FindingCategory::Architecture,
            )],
            Severity::High,
        );
        assert_eq!(layers.correctness.count, 1);
        assert_eq!(layers.correctness.confirmed, 1);
        assert_eq!(layers.maintainability.count, 0);
    }

    #[test]
    fn gate_fails_when_correctness_confirmed() {
        let layers = summarize(
            &[finding(
                Severity::Blocker,
                EvidenceStatus::Reproduced,
                FindingCategory::Correctness,
            )],
            Severity::High,
        );
        assert_eq!(
            decide_gate(&layers, 10, Duration::from_secs(1), &Budget::default()),
            GateStatus::Failed
        );
    }

    #[test]
    fn gate_passes_when_only_suspected_maintainability() {
        let layers = summarize(
            &[finding(
                Severity::Low,
                EvidenceStatus::Suspected,
                FindingCategory::Maintainability,
            )],
            Severity::High,
        );
        assert_eq!(
            decide_gate(&layers, 10, Duration::from_secs(1), &Budget::default()),
            GateStatus::Passed
        );
    }

    #[test]
    fn gate_insufficient_evidence_without_files() {
        let layers = QualityLayers::default();
        assert_eq!(
            decide_gate(&layers, 0, Duration::from_secs(1), &Budget::default()),
            GateStatus::InsufficientEvidence
        );
    }

    #[test]
    fn gate_ignores_findings_below_the_blocking_threshold() {
        // FR-063：只有严重程度达到 block_on 的发现才阻断。把阈值降到 low 之后，
        // 同一条 Low 发现才开始阻断——阈值必须真的被用上，而不是写死的 high。
        let findings = [finding(
            Severity::Low,
            EvidenceStatus::StaticConfirmed,
            FindingCategory::Maintainability,
        )];
        let lenient = summarize(&findings, Severity::Low);
        assert_eq!(lenient.maintainability.blocking, 1);
        assert_eq!(
            decide_gate(&lenient, 10, Duration::from_secs(1), &Budget::default()),
            GateStatus::Failed
        );
        let strict = summarize(&findings, Severity::High);
        assert_eq!(strict.maintainability.blocking, 0);
        assert_eq!(
            decide_gate(&strict, 10, Duration::from_secs(1), &Budget::default()),
            GateStatus::Passed
        );
    }

    #[test]
    fn an_exempted_finding_never_reaches_the_blocking_count() {
        // FR-062：豁免是显式裁决，被豁免的发现即使达到阻断阈值也不再阻断。
        let mut exempted = finding(
            Severity::Blocker,
            EvidenceStatus::Reproduced,
            FindingCategory::Correctness,
        );
        exempted.exemption = Some(crate::exempt::Exemption {
            rule_id: RuleId::builtin("test"),
            scope: crate::exempt::ExemptionScope::Rule,
            reason: "排期在 #42".to_owned(),
            granted_by: "alice".to_owned(),
            expires_on: None,
        });
        let layers = summarize(&[exempted], Severity::High);
        assert_eq!(layers.correctness.count, 1);
        assert_eq!(layers.correctness.exempted, 1);
        assert_eq!(layers.correctness.confirmed, 0);
        assert_eq!(layers.correctness.blocking, 0);
        assert_eq!(layers.correctness.active(), 0);
        assert!(!layers.correctness.has_issues());
        assert_eq!(
            decide_gate(&layers, 10, Duration::from_secs(1), &Budget::default()),
            GateStatus::Passed
        );
    }

    #[test]
    fn a_suspected_blocker_does_not_reach_the_blocking_count() {
        // 没有证据支持的阻断级发现必须由人确认，不能直接挡合并（需求文档 3.7）。
        let layers = summarize(
            &[finding(
                Severity::Blocker,
                EvidenceStatus::Suspected,
                FindingCategory::Correctness,
            )],
            Severity::High,
        );
        assert_eq!(layers.correctness.blocking, 0);
        assert_eq!(
            decide_gate(&layers, 10, Duration::from_secs(1), &Budget::default()),
            GateStatus::Passed
        );
    }

    #[test]
    fn gate_insufficient_evidence_when_budget_exceeded() {
        let layers = QualityLayers::default();
        assert_eq!(
            decide_gate(&layers, 5, Duration::from_secs(9999), &Budget::default()),
            GateStatus::InsufficientEvidence
        );
    }

    #[test]
    fn blocking_findings_require_evidence() {
        let report = ReviewReport {
            run_id: RunId::new(),
            project_id: ProjectId::new(),
            project_root: PathBuf::from("/tmp"),
            revision: "workdir".to_owned(),
            file_count: 1,
            symbol_count: 0,
            findings: vec![
                finding(
                    Severity::Blocker,
                    EvidenceStatus::Suspected,
                    FindingCategory::Correctness,
                ),
                finding(
                    Severity::Blocker,
                    EvidenceStatus::Reproduced,
                    FindingCategory::Correctness,
                ),
            ],
            layers: QualityLayers {
                correctness: LayerSummary {
                    count: 2,
                    confirmed: 1,
                    exempted: 0,
                    blocking: 1,
                },
                engineering: LayerSummary::default(),
                maintainability: LayerSummary::default(),
                verification: LayerSummary::default(),
                block_on: Severity::High,
            },
            gate: GateStatus::Failed,
            ruleset_version: "test".to_owned(),
            impact: None,
            imported_standards: Vec::new(),
            duration: Duration::ZERO,
            evidence: vec![],
            warnings: vec![],
        };
        // 只有被证据支持的那一条才阻挡合并。
        assert_eq!(report.blocking_findings().count(), 1);
        assert_eq!(report.affected_files(), vec!["a.rs"]);
    }
}
