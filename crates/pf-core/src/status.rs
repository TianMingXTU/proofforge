//! 严重程度、证据状态与门禁结论。
//!
//! 这三个枚举是需求文档第七章「质量判定模型」与第十章「证据状态」的直接
//! 体现：严重程度决定报告的优先级，证据状态决定结论的可信度，门禁状态
//! 决定 CI 的通过与否。

use serde::{Deserialize, Serialize};

/// 问题的严重程度。
///
/// 顺序即优先级：`Blocker > High > Medium > Low > Info`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// 已确认会导致错误行为、数据损坏或安全漏洞。
    Blocker,
    /// 很可能造成缺陷或明显违反工程约束。
    High,
    /// 需要关注但影响有限的问题。
    Medium,
    /// 轻微的规范或可读性问题。
    Low,
    /// 仅作提示，不作为质量门禁依据。
    Info,
}

impl Severity {
    /// 是否达到或超过给定阈值。
    #[must_use]
    pub const fn at_least(&self, other: Self) -> bool {
        (*self as u8) <= (other as u8)
    }

    /// 用于报告与 CLI 输出的短标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Blocker => "blocker",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Info => "info",
        }
    }

    /// 是否属于「必须阻断合并」级别。
    #[must_use]
    pub const fn is_blocking(&self) -> bool {
        matches!(self, Self::Blocker | Self::High)
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一项发现的证据状态。
///
/// 需求文档 10.2：`NOT_REPRODUCED` 不代表问题不存在，只能说明在指定条件下
/// 未能复现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    /// 模型或启发式提出的怀疑，尚未验证。
    Suspected,
    /// 确定性静态规则命中（可复算）。
    StaticConfirmed,
    /// 已通过真实执行复现。
    Reproduced,
    /// 在指定条件下未能复现。
    NotReproduced,
    /// 证据不足，无法得出结论。
    Inconclusive,
    /// 人工或可靠验证确认是误报。
    FalsePositive,
}

impl EvidenceStatus {
    /// 是否属于「已被证据支持」的状态。
    #[must_use]
    pub const fn is_evidence_backed(&self) -> bool {
        matches!(self, Self::StaticConfirmed | Self::Reproduced)
    }

    /// 是否属于「应当阻断合并」的状态。
    #[must_use]
    pub const fn is_confirmed_problem(&self) -> bool {
        matches!(self, Self::Reproduced)
    }

    /// 报告与 CLI 输出的短标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Suspected => "suspected",
            Self::StaticConfirmed => "static_confirmed",
            Self::Reproduced => "reproduced",
            Self::NotReproduced => "not_reproduced",
            Self::Inconclusive => "inconclusive",
            Self::FalsePositive => "false_positive",
        }
    }
}

impl std::fmt::Display for EvidenceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 质量门禁的最终结论。
///
/// 需求文档 14.3 明确要求：检查失败、检查未完成和代码通过必须表现为不同
/// 状态，因此 `Failed`、`InsufficientEvidence` 与 `Passed` 是三种独立结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    /// 全部检查成功执行且未发现阻断级问题。
    Passed,
    /// 检查成功执行，但发现阻断级问题。
    Failed,
    /// 检查未能完整执行（工具缺失、预算耗尽、崩溃），证据不足以判定。
    InsufficientEvidence,
    /// 被用户或系统取消。
    Cancelled,
}

impl GateStatus {
    /// 对应的进程退出码约定：通过为 0，未通过为 1，证据不足为 2，取消为 3。
    ///
    /// 返回 `u8` 而不是 `i32`：这个值只会被送进 [`std::process::ExitCode`]，
    /// 提前收窄类型可以杜绝调用点写出会静默截断的强制转换。
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Passed => 0,
            Self::Failed => 1,
            Self::InsufficientEvidence => 2,
            Self::Cancelled => 3,
        }
    }

    /// 报告与 CLI 输出的短标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::InsufficientEvidence => "insufficient_evidence",
            Self::Cancelled => "cancelled",
        }
    }

    /// 是否允许合并。
    #[must_use]
    pub const fn allows_merge(&self) -> bool {
        matches!(self, Self::Passed)
    }
}

impl std::fmt::Display for GateStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_orders_by_priority() {
        assert!(Severity::Blocker < Severity::High);
        assert!(Severity::High.at_least(Severity::High));
        assert!(!Severity::Low.at_least(Severity::High));
        assert!(Severity::Blocker.is_blocking());
        assert!(!Severity::Medium.is_blocking());
    }

    #[test]
    fn evidence_status_classification() {
        assert!(EvidenceStatus::Reproduced.is_confirmed_problem());
        assert!(EvidenceStatus::Reproduced.is_evidence_backed());
        assert!(EvidenceStatus::StaticConfirmed.is_evidence_backed());
        assert!(!EvidenceStatus::StaticConfirmed.is_confirmed_problem());
        assert!(!EvidenceStatus::Suspected.is_evidence_backed());
    }

    #[test]
    fn gate_status_exit_codes_are_distinct() {
        let codes = [
            GateStatus::Passed,
            GateStatus::Failed,
            GateStatus::InsufficientEvidence,
            GateStatus::Cancelled,
        ]
        .map(|status| status.exit_code());
        assert_eq!(codes, [0, 1, 2, 3]);
        assert!(GateStatus::Passed.allows_merge());
        assert!(!GateStatus::InsufficientEvidence.allows_merge());
    }
}
