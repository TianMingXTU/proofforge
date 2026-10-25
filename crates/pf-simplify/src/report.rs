//! 简化报告（FR-066）与门禁结论（FR-067）。
//!
//! # 报告必须分开写三件事
//!
//! 1. **候选是什么**：哪个发现、哪种变换、自动还是手动；
//! 2. **验证结论是什么**：测试有没有回归、行为有没有变化、指标怎么动；
//! 3. **残余风险是什么**：即使全部通过也仍然成立的不确定因素。
//!
//! 第三项不可省略。一个「测试通过 + 行为一致」的简化补丁依然可能删掉下游
//! 在用的 API——把这类风险写进报告，人工确认才是有依据的确认（FR-055）。

use std::path::PathBuf;

use pf_core::status::GateStatus;
use serde::{Deserialize, Serialize};

use crate::behavior::BehaviorComparison;
use crate::candidate::SimplificationCandidate;
use crate::metrics::MetricsDelta;
use crate::regression::RegressionCheck;
use crate::workspace::Patch;

/// 单个候选的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateVerdict {
    /// 回归与行为都通过：可以进入人工确认。
    SafeToApply,
    /// 至少一条性质探针的结论变了：行为被改变。
    BehaviorChanged,
    /// 测试出现回归。
    RegressionDetected,
    /// 没能得到结论（工具缺失、基线失败、没有性质规格）。
    Unverified,
    /// 只给建议，工具没有生成补丁。
    ManualOnly,
}

impl CandidateVerdict {
    /// 稳定标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SafeToApply => "safe_to_apply",
            Self::BehaviorChanged => "behavior_changed",
            Self::RegressionDetected => "regression_detected",
            Self::Unverified => "unverified",
            Self::ManualOnly => "manual_only",
        }
    }

    /// 该结论是否允许应用补丁。
    ///
    /// 只有 [`Self::SafeToApply`] 允许。其余四种一律不允许——包括
    /// [`Self::ManualOnly`]：没有补丁就没有可应用的东西。
    #[must_use]
    pub const fn allows_application(&self) -> bool {
        matches!(self, Self::SafeToApply)
    }
}

/// 一个候选及其验证结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateOutcome {
    /// 候选本身。
    pub candidate: SimplificationCandidate,
    /// 生成的补丁（手动候选为 `None`）。
    pub patch: Option<Patch>,
    /// 回归检查结果。
    pub regression: Option<RegressionCheck>,
    /// 行为比较结果。
    pub behavior: Option<BehaviorComparison>,
    /// 工程指标差。
    pub metrics: Option<MetricsDelta>,
    /// 结论。
    pub verdict: CandidateVerdict,
    /// 结论依据（人类可读，逐条）。
    pub reasons: Vec<String>,
}

/// 一次简化分析的完整报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimplifyReport {
    /// 被分析的项目根目录。
    pub target: PathBuf,
    /// 每个候选的结论。
    pub candidates: Vec<CandidateOutcome>,
    /// 门禁结论（FR-067）。
    pub gate: GateStatus,
    /// 门禁依据。
    pub reasons: Vec<String>,
    /// 跨候选的残余风险。
    pub residual_risks: Vec<String>,
    /// 本次分析使用的规则集版本（FR-061）。
    pub ruleset_version: String,
}

impl SimplifyReport {
    /// 可以进入人工确认的候选。
    pub fn applicable(&self) -> impl Iterator<Item = &CandidateOutcome> {
        self.candidates
            .iter()
            .filter(|outcome| outcome.verdict.allows_application())
    }

    /// 按 ID 找候选结论。
    #[must_use]
    pub fn outcome(&self, candidate_id: &str) -> Option<&CandidateOutcome> {
        self.candidates
            .iter()
            .find(|outcome| outcome.candidate.id == candidate_id)
    }

    /// 退出码：与 `inspect` / `verify` 的门禁语义一致。
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        self.gate.exit_code()
    }

    /// 渲染成人能读的终端报告。
    #[must_use]
    pub fn render_terminal(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("目标：{}\n", self.target.display()));
        out.push_str(&format!("规则集版本：{}\n", self.ruleset_version));
        out.push_str(&format!("候选数：{}\n", self.candidates.len()));
        out.push_str(&format!("门禁：{}\n", self.gate.as_str()));
        for reason in &self.reasons {
            out.push_str(&format!("  - {reason}\n"));
        }
        for outcome in &self.candidates {
            let candidate = &outcome.candidate;
            out.push_str(&format!(
                "\n[{}] {}（{}）\n",
                outcome.verdict.as_str(),
                candidate.title,
                candidate.automation_label()
            ));
            out.push_str(&format!("  文件：{}\n", candidate.path));
            for reason in &outcome.reasons {
                out.push_str(&format!("  · {reason}\n"));
            }
            if let Some(patch) = &outcome.patch {
                if !patch.diff.is_empty() {
                    out.push_str("  补丁：\n");
                    for line in patch.diff.lines() {
                        out.push_str(&format!("    {line}\n"));
                    }
                }
            }
            if let Some(metrics) = &outcome.metrics {
                for item in &metrics.improvements {
                    out.push_str(&format!("  ↑ {item}\n"));
                }
                for item in &metrics.regressions {
                    out.push_str(&format!("  ↓ {item}\n"));
                }
            }
            for risk in &candidate.risks {
                out.push_str(&format!("  风险：{risk}\n"));
            }
        }
        if !self.residual_risks.is_empty() {
            out.push_str("\n残余风险：\n");
            for risk in &self.residual_risks {
                out.push_str(&format!("  - {risk}\n"));
            }
        }
        out
    }

    /// 序列化为 JSON。
    ///
    /// # Errors
    /// 序列化失败时返回 [`crate::SimplifyError::Serialize`]。领域类型全部派生
    /// `Serialize`，因此这实际上不会发生；保留错误路径是为了调用方不必
    /// `unwrap`。
    pub fn to_json(&self) -> Result<String, crate::SimplifyError> {
        serde_json::to_string_pretty(self).map_err(|reason| crate::SimplifyError::Serialize {
            reason: reason.to_string(),
        })
    }
}

/// 根据各候选的结论计算门禁（FR-067）。
///
/// 规则只有三条，且都朝「严格」方向倒：
///
/// 1. 任何候选回归或行为改变 → `Failed`。调用方常常整体应用，一个坏候选会
///    毁了整批。
/// 2. 否则任何候选没能验证 → `InsufficientEvidence`。没跑完的检查不是通过
///    （红线 4）。
/// 3. 否则 → `Passed`。没有候选也算通过：分析正常完成，只是没有发现可自动
///    简化的结构，这不是失败。
#[must_use]
pub fn decide_gate(candidates: &[CandidateOutcome]) -> (GateStatus, Vec<String>) {
    let mut reasons = Vec::new();
    if candidates.is_empty() {
        return (
            GateStatus::Passed,
            vec![
                "没有发现可自动简化的候选：分析正常完成，但不存在工具敢自动改写的结构。".to_owned(),
            ],
        );
    }
    let blocking: Vec<&str> = candidates
        .iter()
        .filter(|outcome| {
            matches!(
                outcome.verdict,
                CandidateVerdict::RegressionDetected | CandidateVerdict::BehaviorChanged
            )
        })
        .map(|outcome| outcome.candidate.title.as_str())
        .collect();
    if !blocking.is_empty() {
        reasons.push(format!(
            "{} 个候选出现回归或行为变化，不得应用：{}",
            blocking.len(),
            blocking.join("；")
        ));
        return (GateStatus::Failed, reasons);
    }
    let unverified: Vec<&str> = candidates
        .iter()
        .filter(|outcome| outcome.verdict == CandidateVerdict::Unverified)
        .map(|outcome| outcome.candidate.title.as_str())
        .collect();
    if !unverified.is_empty() {
        reasons.push(format!(
            "{} 个候选没能验证（工具缺失、基线失败或没有性质规格），证据不足：{}",
            unverified.len(),
            unverified.join("；")
        ));
        return (GateStatus::InsufficientEvidence, reasons);
    }
    let safe = candidates
        .iter()
        .filter(|outcome| outcome.verdict.allows_application())
        .count();
    reasons.push(format!("{safe} 个候选通过回归与行为验证，可进入人工确认。"));
    (GateStatus::Passed, reasons)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Automation, SimplificationKind};
    use pf_core::ids::{FindingId, RuleId};

    fn outcome(verdict: CandidateVerdict) -> CandidateOutcome {
        CandidateOutcome {
            candidate: SimplificationCandidate {
                id: "test:src/lib.rs:1-2".to_owned(),
                finding_id: FindingId::derive(&["test"]),
                rule_id: RuleId::builtin("forwarding-wrapper"),
                kind: SimplificationKind::InlineForwardingWrapper,
                automation: Automation::Automatic,
                title: "`wrapper` 只是转发调用 `real`".to_owned(),
                path: "src/lib.rs".to_owned(),
                edits: Vec::new(),
                instructions: Vec::new(),
                risks: vec!["对外调用方不在索引里".to_owned()],
                requirement_refs: Vec::new(),
            },
            patch: None,
            regression: None,
            behavior: None,
            metrics: None,
            verdict,
            reasons: vec!["测试通过".to_owned()],
        }
    }

    fn report(verdicts: Vec<CandidateVerdict>) -> SimplifyReport {
        SimplifyReport {
            target: PathBuf::from("."),
            candidates: verdicts.into_iter().map(outcome).collect(),
            gate: GateStatus::Passed,
            reasons: Vec::new(),
            residual_risks: Vec::new(),
            ruleset_version: "test".to_owned(),
        }
    }

    #[test]
    fn only_a_safe_candidate_is_applicable() {
        assert!(CandidateVerdict::SafeToApply.allows_application());
        for verdict in [
            CandidateVerdict::BehaviorChanged,
            CandidateVerdict::RegressionDetected,
            CandidateVerdict::Unverified,
            CandidateVerdict::ManualOnly,
        ] {
            assert!(
                !verdict.allows_application(),
                "{} 不得被应用",
                verdict.as_str()
            );
        }
    }

    #[test]
    fn a_report_with_only_manual_proposals_passes_with_a_note() {
        // 没有可自动简化的候选不是失败：分析正常完成了。
        let report = report(vec![CandidateVerdict::ManualOnly]);
        assert_eq!(report.applicable().count(), 0);
        assert!(report.outcome("test:src/lib.rs:1-2").is_some());
    }

    #[test]
    fn a_regression_blocks_the_whole_report() {
        // 只要有一个候选回归，门禁就不能是通过——否则调用方会整体应用。
        let (gate, reasons) = decide_gate(&[
            outcome(CandidateVerdict::SafeToApply),
            outcome(CandidateVerdict::RegressionDetected),
        ]);
        assert_eq!(gate, GateStatus::Failed);
        assert!(reasons.iter().any(|reason| reason.contains("不得应用")));
    }

    #[test]
    fn an_unverified_candidate_yields_insufficient_evidence() {
        // 红线 4：没验证完不能算通过。
        let (gate, _) = decide_gate(&[outcome(CandidateVerdict::Unverified)]);
        assert_eq!(gate, GateStatus::InsufficientEvidence);
        assert_eq!(gate.exit_code(), 2);
    }

    #[test]
    fn an_empty_candidate_list_passes_with_an_explanation() {
        let (gate, reasons) = decide_gate(&[]);
        assert_eq!(gate, GateStatus::Passed);
        assert!(reasons[0].contains("没有发现可自动简化的候选"));
    }

    #[test]
    fn the_terminal_rendering_shows_patch_risks_and_verdicts() {
        let mut single = report(vec![CandidateVerdict::SafeToApply]);
        single.candidates[0].patch = Some(Patch {
            candidate_id: "test:src/lib.rs:1-2".to_owned(),
            path: "src/lib.rs".to_owned(),
            touched_files: vec!["src/lib.rs".to_owned()],
            diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,1 @@\n-pub fn wrapper() {}\n"
                .to_owned(),
            revert_diff: String::new(),
            edits: Vec::new(),
        });
        let rendered = single.render_terminal();
        assert!(rendered.contains("safe_to_apply"));
        assert!(rendered.contains("-pub fn wrapper"));
        assert!(rendered.contains("对外调用方不在索引里"));
    }

    #[test]
    fn the_json_rendering_round_trips() {
        let single = report(vec![CandidateVerdict::Unverified]);
        let json = single.to_json().expect("json");
        let back: SimplifyReport = serde_json::from_str(&json).expect("parse");
        assert_eq!(single, back);
    }
}
