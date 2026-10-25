//! 审查发现（Finding）模型。
//!
//! 需求文档 FR-030：所有审查结论必须具备标准化的类型、位置、依据和状态。

use serde::{Deserialize, Serialize};

use crate::ids::{FindingId, RuleId};
use crate::location::Location;
use crate::status::{EvidenceStatus, Severity};

/// 发现所属的大类，用于报告分组与门禁策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    /// 正确性与安全问题（第一章质量分层第一层）。
    Correctness,
    /// 工程规范（编码风格、错误处理、资源管理、注释质量）。
    Engineering,
    /// 架构与依赖问题。
    Architecture,
    /// 可维护性与设计问题（含过度设计）。
    Maintainability,
    /// 验证可信度问题（覆盖不足、证据缺失）。
    Verification,
}

impl FindingCategory {
    /// 类别标识。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Correctness => "correctness",
            Self::Engineering => "engineering",
            Self::Architecture => "architecture",
            Self::Maintainability => "maintainability",
            Self::Verification => "verification",
        }
    }

    /// 类别展示名。
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::Correctness => "正确性与安全",
            Self::Engineering => "工程规范",
            Self::Architecture => "架构",
            Self::Maintainability => "可维护性",
            Self::Verification => "验证可信度",
        }
    }
}

impl std::fmt::Display for FindingCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 发现的具体类型。
///
/// 新增类型时必须同步更新 [`FindingKind::default_severity`]，保证门禁行为
/// 与需求文档的分层一致。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    // --- 正确性 ---
    /// 可能的逻辑错误（由静态规则命中，尚未执行验证）。
    PossibleLogicError,
    /// 未处理的错误（裸 `unwrap`、空 `except` 等）。
    UnhandledError,
    /// 吞没错误或异常。
    SwallowedError,
    /// 潜在资源泄漏。
    ResourceLeak,
    /// 并发或锁使用风险。
    ConcurrencyRisk,

    // --- 工程规范 ---
    /// 命名不符合语言惯例。
    NamingViolation,
    /// 函数过长或职责过多。
    FunctionTooComplex,
    /// 注释冗余、过期或与实现不一致。
    CommentQuality,
    /// 公共 API 缺少文档。
    MissingDocumentation,

    // --- 架构 ---
    /// 模块间循环依赖。
    CircularDependency,
    /// 违反声明的依赖方向规则。
    ArchitectureViolation,

    // --- 可维护性 / 过度设计 ---
    /// 无效抽象（单一实现且无隔离需求的接口）。
    UselessAbstraction,
    /// 纯转发委托链。
    ForwardingChain,
    /// 不必要的工厂或设计模式。
    NeedlessPattern,
    /// 重复或结构相似的实现。
    DuplicateCode,
    /// 疑似 AI 生成的样板代码（仅描述代码质量，不判断作者）。
    BoilerplateCode,

    // --- 验证 ---
    /// 缺少对应测试覆盖。
    MissingTestCoverage,
    /// 验证过程未完成。
    VerificationIncomplete,

    /// 其他规则产生、尚未归类的发现。
    Other {
        /// 规则自定义的类型标签。
        label: String,
    },
}

impl FindingKind {
    /// 该类型的默认严重程度。
    ///
    /// 默认值只是起点，规则集可以通过 [`crate::Rule::severity`] 覆盖。
    #[must_use]
    pub const fn default_severity(&self) -> Severity {
        match self {
            Self::PossibleLogicError
            | Self::ConcurrencyRisk
            | Self::CircularDependency
            | Self::ArchitectureViolation => Severity::High,
            Self::UnhandledError
            | Self::SwallowedError
            | Self::ResourceLeak
            | Self::DuplicateCode => Severity::Medium,
            Self::NamingViolation
            | Self::FunctionTooComplex
            | Self::CommentQuality
            | Self::MissingDocumentation
            | Self::UselessAbstraction
            | Self::ForwardingChain
            | Self::NeedlessPattern
            | Self::BoilerplateCode => Severity::Low,
            Self::MissingTestCoverage | Self::VerificationIncomplete | Self::Other { .. } => {
                Severity::Info
            }
        }
    }

    /// 所属大类。
    #[must_use]
    pub const fn category(&self) -> FindingCategory {
        match self {
            Self::PossibleLogicError
            | Self::UnhandledError
            | Self::SwallowedError
            | Self::ResourceLeak
            | Self::ConcurrencyRisk => FindingCategory::Correctness,
            Self::NamingViolation
            | Self::FunctionTooComplex
            | Self::CommentQuality
            | Self::MissingDocumentation => FindingCategory::Engineering,
            Self::CircularDependency | Self::ArchitectureViolation => FindingCategory::Architecture,
            Self::UselessAbstraction
            | Self::ForwardingChain
            | Self::NeedlessPattern
            | Self::DuplicateCode
            | Self::BoilerplateCode
            | Self::Other { .. } => FindingCategory::Maintainability,
            Self::MissingTestCoverage | Self::VerificationIncomplete => {
                FindingCategory::Verification
            }
        }
    }

    /// 稳定的机器可读标签，用于报告分组与统计。
    ///
    /// 新增 [`FindingKind`] 变体时必须同步更新该方法，否则新类型会落到
    /// `Other` 标签上而丢失区分度。
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::PossibleLogicError => "possible_logic_error",
            Self::UnhandledError => "unhandled_error",
            Self::SwallowedError => "swallowed_error",
            Self::ResourceLeak => "resource_leak",
            Self::ConcurrencyRisk => "concurrency_risk",
            Self::NamingViolation => "naming_violation",
            Self::FunctionTooComplex => "function_too_complex",
            Self::CommentQuality => "comment_quality",
            Self::MissingDocumentation => "missing_documentation",
            Self::CircularDependency => "circular_dependency",
            Self::ArchitectureViolation => "architecture_violation",
            Self::UselessAbstraction => "useless_abstraction",
            Self::ForwardingChain => "forwarding_chain",
            Self::NeedlessPattern => "needless_pattern",
            Self::DuplicateCode => "duplicate_code",
            Self::BoilerplateCode => "boilerplate_code",
            Self::MissingTestCoverage => "missing_test_coverage",
            Self::VerificationIncomplete => "verification_incomplete",
            Self::Other { .. } => "other",
        }
    }
}

/// 修复建议。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remediation {
    /// 人类可读的建议说明。
    pub summary: String,
    /// 可选的替代代码片段或补丁文本。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_code: Option<String>,
    /// 是否需要人工确认（自动修改建议一律为 `true`）。
    pub requires_human_review: bool,
}

impl Remediation {
    /// 构造一条需要人工确认的建议。
    #[must_use]
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            suggested_code: None,
            requires_human_review: true,
        }
    }

    /// 附带建议代码。
    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.suggested_code = Some(code.into());
        self
    }
}

/// 一条审查发现。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// 唯一标识。
    pub id: FindingId,
    /// 触发发现的规则。
    pub rule_id: RuleId,
    /// 问题类型。
    pub kind: FindingKind,
    /// 人类可读的标题（一句话说明问题）。
    pub title: String,
    /// 详细说明：结构事实、判断依据、尚未验证的部分。
    pub detail: String,
    /// 代码位置（可能多处，例如委托链的每一跳）。
    pub locations: Vec<Location>,
    /// 严重程度。
    pub severity: Severity,
    /// 证据状态。
    pub status: EvidenceStatus,
    /// 修复建议。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<Remediation>,
    /// 关联的需求条目或验收标准编号。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requirement_refs: Vec<String>,
    /// 结构事实（键值对），用于可解释性输出。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<(String, String)>,
    /// 已生效的豁免（FR-062）。
    ///
    /// 豁免**不删除发现**：它只把发现的呈现改成「已豁免」并附上理由与批准人。
    /// 静默删除会让「我们到底压掉了多少问题」无从审计，因此豁免是发现的一部
    /// 分而不是过滤器。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exemption: Option<crate::exempt::Exemption>,
}

impl Finding {
    /// 使用最小必需信息构造一条发现（默认为 `Suspected`）。
    ///
    /// 标识由内容派生而不是随机生成：同一个问题在重复运行中得到同一个
    /// [`FindingId`]，报告因此可以跨版本 diff，落库也不会为同一个问题
    /// 累积多行（FR-061 可审计性）。
    #[must_use]
    pub fn new(
        rule_id: RuleId,
        kind: FindingKind,
        title: impl Into<String>,
        detail: impl Into<String>,
        locations: Vec<Location>,
    ) -> Self {
        let severity = kind.default_severity();
        let title = title.into();
        let detail = detail.into();
        let id = FindingId::derive(&[
            rule_id.get(),
            kind.label(),
            &title,
            &locations
                .iter()
                .map(Location::display)
                .collect::<Vec<_>>()
                .join(","),
        ]);
        Self {
            id,
            rule_id,
            kind,
            title,
            detail,
            locations,
            severity,
            status: EvidenceStatus::Suspected,
            remediation: None,
            requirement_refs: Vec::new(),
            facts: Vec::new(),
            exemption: None,
        }
    }

    /// 覆盖严重程度。
    #[must_use]
    pub const fn with_severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    /// 覆盖证据状态。
    #[must_use]
    pub const fn with_status(mut self, status: EvidenceStatus) -> Self {
        self.status = status;
        self
    }

    /// 附带修复建议。
    #[must_use]
    pub fn with_remediation(mut self, remediation: Remediation) -> Self {
        self.remediation = Some(remediation);
        self
    }

    /// 关联需求条目。
    #[must_use]
    pub fn with_requirements(mut self, refs: Vec<String>) -> Self {
        self.requirement_refs = refs;
        self
    }

    /// 追加一条结构事实。
    #[must_use]
    pub fn with_fact(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.facts.push((key.into(), value.into()));
        self
    }

    /// 标记该发现已被某条豁免压制（FR-062）。
    ///
    /// 只应传入 [`crate::exempt::ExemptionSet::matching`] 命中的豁免：豁免的
    /// 理由与批准人会随发现一起进入报告与审计记录。
    #[must_use]
    pub fn with_exemption(mut self, exemption: crate::exempt::Exemption) -> Self {
        self.exemption = Some(exemption);
        self
    }

    /// 该发现是否已被豁免压制。
    #[must_use]
    pub const fn is_exempted(&self) -> bool {
        self.exemption.is_some()
    }

    /// 主要位置（第一个），用于排序与去重提示。
    #[must_use]
    pub fn primary_location(&self) -> Option<&Location> {
        self.locations.first()
    }

    /// 是否需要阻断合并。
    ///
    /// 已豁免的发现不阻断：豁免是一次显式、有理由、可归属的裁决，与「没有这个
    /// 问题」在报告里仍然可区分（[`Self::is_exempted`]）。
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        !self.is_exempted() && self.severity.is_blocking() && self.status.is_evidence_backed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> RuleId {
        RuleId::builtin("test-rule")
    }

    fn loc() -> Location {
        Location::new("src/a.rs".to_owned(), crate::location::Span::new(0, 3))
    }

    #[test]
    fn finding_defaults_to_suspected_with_kind_severity() {
        let finding = Finding::new(
            rule(),
            FindingKind::UnhandledError,
            "unwrap on user input",
            "detail",
            vec![loc()],
        );
        assert_eq!(finding.severity, Severity::Medium);
        assert_eq!(finding.status, EvidenceStatus::Suspected);
        assert_eq!(finding.kind.category(), FindingCategory::Correctness);
    }

    #[test]
    fn blocking_requires_evidence() {
        let mut finding = Finding::new(
            rule(),
            FindingKind::PossibleLogicError,
            "t",
            "d",
            vec![loc()],
        );
        // 仅怀疑中的高危问题不阻断。
        assert!(!finding.is_blocking());
        finding.status = EvidenceStatus::Reproduced;
        assert!(finding.is_blocking());
    }

    #[test]
    fn remediation_always_requires_human_review() {
        let remediation = Remediation::new("do x").with_code("let x = 1;");
        assert!(remediation.requires_human_review);
        assert_eq!(remediation.suggested_code.as_deref(), Some("let x = 1;"));
    }

    #[test]
    fn an_exempted_finding_stops_blocking_but_stays_visible() {
        // FR-062：豁免压制的是门禁结论，不是发现本身——报告里仍看得到它。
        let mut finding = Finding::new(
            rule(),
            FindingKind::PossibleLogicError,
            "t",
            "d",
            vec![loc()],
        )
        .with_status(EvidenceStatus::Reproduced);
        assert!(finding.is_blocking());
        finding.exemption = Some(crate::exempt::Exemption {
            rule_id: rule(),
            scope: crate::exempt::ExemptionScope::Rule,
            reason: "已知问题，排期在 #1234".to_owned(),
            granted_by: "alice".to_owned(),
            expires_on: None,
        });
        assert!(finding.is_exempted());
        assert!(!finding.is_blocking(), "已豁免的发现不应阻断合并");
        assert_eq!(finding.title, "t", "豁免不得改写发现内容");
    }

    #[test]
    fn facts_are_ordered_and_accessible() {
        let finding = Finding::new(
            rule(),
            FindingKind::Other { label: "x".into() },
            "t",
            "d",
            vec![],
        )
        .with_fact("impls", "1")
        .with_fact("calls", "3");
        assert_eq!(finding.facts.len(), 2);
        assert_eq!(finding.facts[0], ("impls".to_owned(), "1".to_owned()));
    }

    #[test]
    fn every_kind_has_a_distinct_stable_label() {
        // 新增 FindingKind 变体时若忘记更新 label()，这里会失败。
        let kinds = [
            FindingKind::PossibleLogicError,
            FindingKind::UnhandledError,
            FindingKind::SwallowedError,
            FindingKind::ResourceLeak,
            FindingKind::ConcurrencyRisk,
            FindingKind::NamingViolation,
            FindingKind::FunctionTooComplex,
            FindingKind::CommentQuality,
            FindingKind::MissingDocumentation,
            FindingKind::CircularDependency,
            FindingKind::ArchitectureViolation,
            FindingKind::UselessAbstraction,
            FindingKind::ForwardingChain,
            FindingKind::NeedlessPattern,
            FindingKind::DuplicateCode,
            FindingKind::BoilerplateCode,
            FindingKind::MissingTestCoverage,
            FindingKind::VerificationIncomplete,
        ];
        let labels: Vec<&str> = kinds.iter().map(FindingKind::label).collect();
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(labels.len(), unique.len(), "labels must be unique");
        assert!(labels
            .iter()
            .all(|label| !label.is_empty() && !label.contains(' ')));
    }
}
