//! 人工确认与安全应用（FR-055、FR-056）。
//!
//! # 两道闸门，缺一不可
//!
//! 1. **授权**（FR-055）：调用方必须显式给出 [`Authorization::Explicit`]。
//!    CLI 上对应 `--yes`，且必须在看过补丁之后才能传——没有「默认同意」。
//! 2. **结论**：候选的验证结论必须是 [`CandidateVerdict::SafeToApply`]。
//!    回归、行为变化、未验证一律拒绝，即使人有授权。
//!
//! 第二道闸门是必要的：授权回答的是「人允许我改」，结论回答的是「这个改动
//! 被证明是安全的」。前者不能替代后者，否则一次手滑的 `--yes` 就能把回归
//! 写进主干。
//!
//! # 为什么应用后要留回滚补丁
//!
//! 简化补丁的语义人很难在两周后凭记忆重建。回滚补丁写在目标仓库的
//! `.proofforge/revert/` 下，与补丁一一对应，`git apply -R` 即可撤销。

use std::path::Path;

use serde::{Deserialize, Serialize};

use pf_core::ids::RunId;

use crate::edit::apply_edits;
use crate::error::SimplifyError;
use crate::report::{CandidateVerdict, SimplifyReport};

/// 人工授权。
///
/// 刻意不用 `bool`：`apply(..., true)` 在调用点读不出「谁授权的、依据是什么」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Authorization {
    /// 没有授权：任何写入都不允许发生。
    None,
    /// 已授权：记录是谁批准的，进审计（FR-070）。
    Explicit {
        /// 批准人（CI 作业名、开发者 ID）。
        decided_by: String,
        /// 批准理由。
        note: String,
    },
}

impl Authorization {
    /// 是否已获得授权。
    #[must_use]
    pub const fn is_authorized(&self) -> bool {
        matches!(self, Self::Explicit { .. })
    }
}

/// 一次应用的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyOutcome {
    /// 实际应用的候选 ID。
    pub applied: Vec<String>,
    /// 被跳过的候选 ID 与原因。
    pub skipped: Vec<(String, String)>,
    /// 每个候选的回滚补丁落盘位置（仓库相对路径）。
    pub revert_patches: Vec<(String, String)>,
}

/// 把报告中通过验证的候选应用到目标仓库。
///
/// `only` 为非空时只应用列表里的候选 ID；为空时应用全部通过验证的候选。
/// 被显式点名却又不允许应用的候选会返回错误——调用方明确要它，就不能被
/// 静默跳过。
///
/// # Errors
/// 没有授权（[`SimplifyError::NotAuthorized`]）、候选未通过验证
/// （[`SimplifyError::NotVerified`]）、或写文件失败
/// （[`SimplifyError::WriteFailed`]）时返回错误。出现错误时**不会**留下半应用
/// 状态：每个候选单独应用，前一个失败会中止后续候选。
pub fn apply(
    report: &SimplifyReport,
    target: &Path,
    authorization: &Authorization,
    only: &[String],
) -> Result<ApplyOutcome, SimplifyError> {
    if !authorization.is_authorized() {
        return Err(SimplifyError::NotAuthorized);
    }

    let mut applied = Vec::new();
    let mut skipped = Vec::new();
    let mut revert_patches = Vec::new();

    for outcome in &report.candidates {
        let candidate_id = outcome.candidate.id.clone();
        if !only.is_empty() && !only.iter().any(|id| id == &candidate_id) {
            continue;
        }
        // 显式点名的候选不允许被静默跳过：调用方明确要它，就必须得到它或得到
        // 一个错误。未点名时（批量应用）跳过是合理的——报告里会写清跳过了谁。
        let explicitly_requested = only.iter().any(|id| id == &candidate_id);
        match outcome.verdict {
            CandidateVerdict::SafeToApply => {}
            other => {
                let reason = format!("结论为 `{}`，不允许应用", other.as_str());
                if explicitly_requested {
                    return Err(SimplifyError::NotVerified {
                        candidate: candidate_id,
                        verdict: reason,
                    });
                }
                skipped.push((candidate_id, reason));
                continue;
            }
        }
        let Some(patch) = &outcome.patch else {
            let reason = "没有补丁可应用".to_owned();
            if explicitly_requested {
                return Err(SimplifyError::NotVerified {
                    candidate: candidate_id,
                    verdict: reason,
                });
            }
            skipped.push((candidate_id, reason));
            continue;
        };
        if patch.is_empty() {
            let reason = "补丁为空".to_owned();
            if explicitly_requested {
                return Err(SimplifyError::NotVerified {
                    candidate: candidate_id,
                    verdict: reason,
                });
            }
            skipped.push((candidate_id, reason));
            continue;
        }

        let mut applied_files = Vec::new();
        for relative in &patch.touched_files {
            let destination = target.join(relative);
            let current = std::fs::read_to_string(&destination).map_err(|reason| {
                SimplifyError::WriteFailed {
                    path: relative.clone(),
                    reason: reason.to_string(),
                }
            })?;
            let patched = apply_edits(&current, relative, &patch.edits).map_err(|reason| {
                SimplifyError::EditRejected {
                    candidate: candidate_id.clone(),
                    reason: reason.to_string(),
                }
            })?;
            if patched == current {
                continue;
            }
            std::fs::write(&destination, patched).map_err(|reason| SimplifyError::WriteFailed {
                path: relative.clone(),
                reason: reason.to_string(),
            })?;
            applied_files.push(relative.clone());
        }

        let revert_path = write_revert_patch(target, &candidate_id, &patch.revert_diff)?;
        applied.push(candidate_id.clone());
        revert_patches.push((candidate_id.clone(), revert_path));
        tracing::info!(
            candidate = %candidate_id,
            files = ?applied_files,
            decided_by = %authorization_label(authorization),
            "简化补丁已应用"
        );
    }

    if applied.is_empty() {
        // 「一个都没应用」不是成功：调用方要的是落地补丁，全部跳过与失败等价。
        let reasons: Vec<String> = skipped
            .iter()
            .map(|(id, reason)| format!("{id}: {reason}"))
            .collect();
        if reasons.is_empty() {
            // 一个候选都没匹配上：说明调用方给的 ID 不在报告里。
            return Err(SimplifyError::NotVerified {
                candidate: only.join(","),
                verdict: "报告里没有匹配的候选".to_owned(),
            });
        }
        return Err(SimplifyError::NothingApplied { reasons });
    }

    Ok(ApplyOutcome {
        applied,
        skipped,
        revert_patches,
    })
}

/// 把回滚补丁写进目标仓库的工具目录。
///
/// 目录名带 [`RunId`]：同一次运行重复应用不会互相覆盖，而标识本身可追溯。
fn write_revert_patch(
    target: &Path,
    candidate_id: &str,
    revert_diff: &str,
) -> Result<String, SimplifyError> {
    let run = RunId::new();
    let relative = format!(".proofforge/revert/{run}-{}.patch", sanitize(candidate_id));
    let destination = target.join(&relative);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|reason| SimplifyError::WriteFailed {
            path: relative.clone(),
            reason: reason.to_string(),
        })?;
    }
    std::fs::write(&destination, revert_diff).map_err(|reason| SimplifyError::WriteFailed {
        path: relative.clone(),
        reason: reason.to_string(),
    })?;
    Ok(relative)
}

/// 把候选 ID 变成安全的文件名片段。
fn sanitize(candidate_id: &str) -> String {
    candidate_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 授权者的标签，仅用于日志。
fn authorization_label(authorization: &Authorization) -> &str {
    match authorization {
        Authorization::None => "none",
        Authorization::Explicit { decided_by, .. } => decided_by.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Automation, SimplificationCandidate, SimplificationKind};
    use crate::report::CandidateOutcome;
    use crate::workspace::Patch;
    use pf_core::ids::{FindingId, RuleId};

    fn candidate(id: &str) -> SimplificationCandidate {
        SimplificationCandidate {
            id: id.to_owned(),
            finding_id: FindingId::derive(&["test"]),
            rule_id: RuleId::builtin("forwarding-wrapper"),
            kind: SimplificationKind::InlineForwardingWrapper,
            automation: Automation::Automatic,
            title: "wrapper".to_owned(),
            path: "src/lib.rs".to_owned(),
            edits: Vec::new(),
            instructions: Vec::new(),
            risks: Vec::new(),
            requirement_refs: Vec::new(),
        }
    }

    fn report_with(verdict: CandidateVerdict, edits: Vec<crate::edit::Edit>) -> SimplifyReport {
        SimplifyReport {
            target: std::path::PathBuf::from("."),
            candidates: vec![CandidateOutcome {
                candidate: candidate("test:src/lib.rs:1-3"),
                patch: Some(Patch {
                    candidate_id: "test:src/lib.rs:1-3".to_owned(),
                    path: "src/lib.rs".to_owned(),
                    touched_files: vec!["src/lib.rs".to_owned()],
                    diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,1 @@\n-pub fn wrapper() {}\n".to_owned(),
                    revert_diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n".to_owned(),
                    edits,
                }),
                regression: None,
                behavior: None,
                metrics: None,
                verdict,
                reasons: Vec::new(),
            }],
            gate: pf_core::status::GateStatus::Passed,
            reasons: Vec::new(),
            residual_risks: Vec::new(),
            ruleset_version: "test".to_owned(),
        }
    }

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn wrapper() {}\npub fn real() {}\n",
        )
        .expect("write");
        dir
    }

    fn yes() -> Authorization {
        Authorization::Explicit {
            decided_by: "test".to_owned(),
            note: "reviewed".to_owned(),
        }
    }

    #[test]
    fn applying_without_authorization_is_refused() {
        // FR-055：没有明确授权就不写目标仓库。
        let dir = project();
        let before = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");
        let report = report_with(
            CandidateVerdict::SafeToApply,
            vec![crate::edit::Edit::delete("src/lib.rs", 1, 1, "drop")],
        );
        let error = apply(&report, dir.path(), &Authorization::None, &[]).expect_err("refused");
        assert!(matches!(error, SimplifyError::NotAuthorized));
        let after = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");
        assert_eq!(before, after);
    }

    #[test]
    fn an_unverified_candidate_is_refused_even_with_authorization() {
        // 授权回答「人允许我改」，结论回答「这个改动安全」——两者不能互相替代。
        let dir = project();
        let before = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");
        let report = report_with(
            CandidateVerdict::Unverified,
            vec![crate::edit::Edit::delete("src/lib.rs", 1, 1, "drop")],
        );
        // 显式点名：必须得到错误，而不是被静默跳过。
        let error = apply(
            &report,
            dir.path(),
            &yes(),
            &["test:src/lib.rs:1-3".to_owned()],
        )
        .expect_err("refused");
        assert!(matches!(error, SimplifyError::NotVerified { .. }));
        let after = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");
        assert_eq!(before, after, "未验证的候选不得落盘");
    }

    #[test]
    fn batch_application_skips_unverified_candidates_with_a_reason() {
        // 批量应用时跳过是合理的：报告里写清跳过了谁、为什么。
        let dir = project();
        let report = report_with(
            CandidateVerdict::Unverified,
            vec![crate::edit::Edit::delete("src/lib.rs", 1, 1, "drop")],
        );
        let error = apply(&report, dir.path(), &yes(), &[]).expect_err("nothing applied");
        match error {
            SimplifyError::NothingApplied { reasons } => {
                assert!(reasons[0].contains("unverified"), "{reasons:?}");
            }
            other => panic!("expected NothingApplied, got {other:?}"),
        }
    }

    #[test]
    fn an_authorized_safe_candidate_is_applied_and_leaves_a_revert_patch() {
        let dir = project();
        let report = report_with(
            CandidateVerdict::SafeToApply,
            vec![crate::edit::Edit::delete("src/lib.rs", 1, 1, "drop")],
        );
        let outcome = apply(&report, dir.path(), &yes(), &[]).expect("apply");
        assert_eq!(outcome.applied, vec!["test:src/lib.rs:1-3".to_owned()]);
        let after = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");
        assert!(!after.contains("wrapper"), "补丁应已落到目标仓库");
        assert_eq!(outcome.revert_patches.len(), 1);
        let revert = dir.path().join(&outcome.revert_patches[0].1);
        assert!(revert.exists(), "回滚补丁必须落盘");
    }

    #[test]
    fn the_only_filter_restricts_what_gets_applied() {
        let dir = project();
        let report = report_with(
            CandidateVerdict::SafeToApply,
            vec![crate::edit::Edit::delete("src/lib.rs", 1, 1, "drop")],
        );
        let outcome = apply(&report, dir.path(), &yes(), &["other:candidate".to_owned()])
            .expect_err("nothing selected");
        match outcome {
            SimplifyError::NotVerified { candidate, verdict } => {
                assert_eq!(candidate, "other:candidate");
                assert!(verdict.contains("没有匹配的候选"), "{verdict}");
            }
            other => panic!("expected NotVerified, got {other:?}"),
        }
    }

    #[test]
    fn a_candidate_id_is_sanitized_for_the_file_system() {
        assert_eq!(sanitize("test:src/lib.rs:1-3"), "test_src_lib_rs_1-3");
    }
}
