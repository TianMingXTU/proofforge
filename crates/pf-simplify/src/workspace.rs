//! 隔离补丁（FR-051）。
//!
//! # 为什么补丁必须落在独立工作区
//!
//! AGENTS.md 安全红线 3：未经用户明确授权不得修改被审查仓库。因此整个简化
//! 管线里**没有任何一步**写原始项目——候选补丁只写进 [`pf_verify::Sandbox`]
//! 的临时副本，回归与行为比较也在副本上跑。原始仓库的第一次写入发生在
//! [`crate::apply`]，而那需要调用方显式给出授权（FR-055 / FR-056）。
//!
//! # 为什么补丁要同时保留 diff 与 revert diff
//!
//! diff 是给人审查的（FR-055「允许开发者审查补丁」），revert diff 是给机器
//! 回滚的。两者都在生成时算好并随报告落库，事后再从编辑记录反推容易和实际
//! 落盘内容不一致。

use std::path::Path;

use serde::{Deserialize, Serialize};

use pf_core::config::Budget;
use pf_verify::sandbox::Sandbox;

use crate::candidate::SimplificationCandidate;
use crate::edit::{apply_edits, reverse_diff, unified_diff, Edit};
use crate::error::SimplifyError;

/// 一份候选补丁。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    /// 对应候选的稳定 ID。
    pub candidate_id: String,
    /// 主文件（定义所在文件）。
    pub path: String,
    /// 被修改的文件列表（含主文件）。
    pub touched_files: Vec<String>,
    /// 统一差异，人类可审查。
    pub diff: String,
    /// 反向差异，用于回滚。
    pub revert_diff: String,
    /// 产生该补丁的编辑记录。
    pub edits: Vec<Edit>,
}

impl Patch {
    /// 补丁是否为空（没有任何文本变化）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.diff.is_empty()
    }

    /// 净删除行数减去净新增行数，用于工程指标比较（FR-054）。
    #[must_use]
    pub fn net_line_delta(&self) -> i64 {
        let removed = self
            .diff
            .lines()
            .filter(|line| line.starts_with('-') && !line.starts_with("---"))
            .count();
        let added = self
            .diff
            .lines()
            .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
            .count();
        i64::try_from(added).unwrap_or(i64::MAX) - i64::try_from(removed).unwrap_or(i64::MAX)
    }
}

/// 带着补丁的隔离工作区。
///
/// 持有 [`Sandbox`]，因此析构时临时目录随之删除：不存在「忘了清理的副本」。
#[derive(Debug)]
pub struct PatchedWorkspace {
    /// 隔离工作区（已应用补丁）。
    sandbox: Sandbox,
    /// 应用的补丁。
    patch: Patch,
}

impl PatchedWorkspace {
    /// 隔离目标项目并把候选编辑应用到副本上。
    ///
    /// # Errors
    /// 目标无法隔离、或候选编辑无法应用（范围越界、范围重叠、文件没被复制进
    /// 沙箱）时返回 [`SimplifyError`]。任何一步失败都不会留下半应用的补丁：
    /// 全部文件先算完新内容，再逐个写入。
    pub fn create(
        target: &Path,
        candidate: &SimplificationCandidate,
        budget: &Budget,
    ) -> Result<Self, SimplifyError> {
        let sandbox = Sandbox::isolate(target, budget)?;
        let mut diff = String::new();
        let mut revert = String::new();
        let mut touched: Vec<String> = Vec::new();

        for relative in candidate.touched_files() {
            let original_path = sandbox.root().join(relative);
            let original = std::fs::read_to_string(&original_path).map_err(|reason| {
                SimplifyError::FileNotCopied {
                    path: relative.to_owned(),
                    reason: reason.to_string(),
                }
            })?;
            let patched = apply_edits(&original, relative, &candidate.edits).map_err(|reason| {
                SimplifyError::EditRejected {
                    candidate: candidate.id.clone(),
                    reason: reason.to_string(),
                }
            })?;
            if patched == original {
                continue;
            }
            sandbox.write_file(relative, &patched).map_err(|reason| {
                SimplifyError::EditRejected {
                    candidate: candidate.id.clone(),
                    reason: reason.to_string(),
                }
            })?;
            diff.push_str(&unified_diff(relative, &original, &patched));
            revert.push_str(&reverse_diff(relative, &original, &patched));
            touched.push(relative.to_owned());
        }

        Ok(Self {
            patch: Patch {
                candidate_id: candidate.id.clone(),
                path: candidate.path.clone(),
                touched_files: touched,
                diff,
                revert_diff: revert,
                edits: candidate.edits.clone(),
            },
            sandbox,
        })
    }

    /// 应用过的补丁。
    #[must_use]
    pub const fn patch(&self) -> &Patch {
        &self.patch
    }

    /// 补丁应用后的隔离工作区。
    #[must_use]
    pub const fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::{Automation, SimplificationCandidate, SimplificationKind};
    use pf_core::ids::{FindingId, RuleId};

    fn candidate(edits: Vec<Edit>) -> SimplificationCandidate {
        SimplificationCandidate {
            id: "test:src/lib.rs:1-2".to_owned(),
            finding_id: FindingId::derive(&["test"]),
            rule_id: RuleId::builtin("forwarding-wrapper"),
            kind: SimplificationKind::InlineForwardingWrapper,
            automation: Automation::Automatic,
            title: "test".to_owned(),
            path: "src/lib.rs".to_owned(),
            edits,
            instructions: Vec::new(),
            risks: Vec::new(),
            requirement_refs: Vec::new(),
        }
    }

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn wrapper(x: i32) -> i32 {\n    real(x)\n}\n\npub fn real(x: i32) -> i32 {\n    x + 1\n}\n",
        )
        .expect("write");
        dir
    }

    #[test]
    fn the_patch_is_applied_only_inside_the_workspace() {
        // 红线 3：原始仓库不允许被写。补丁只出现在沙箱副本里。
        let dir = project();
        let before = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");
        let workspace = PatchedWorkspace::create(
            dir.path(),
            &candidate(vec![Edit::delete("src/lib.rs", 1, 3, "drop wrapper")]),
            &Budget::default(),
        )
        .expect("workspace");

        let after = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");
        assert_eq!(before, after, "原始仓库不得被修改");
        let patched =
            std::fs::read_to_string(workspace.sandbox().root().join("src/lib.rs")).expect("read");
        assert!(!patched.contains("wrapper"));
        assert!(patched.contains("pub fn real"));
    }

    #[test]
    fn the_patch_carries_both_a_diff_and_a_revert_diff() {
        let dir = project();
        let workspace = PatchedWorkspace::create(
            dir.path(),
            &candidate(vec![Edit::delete("src/lib.rs", 1, 3, "drop wrapper")]),
            &Budget::default(),
        )
        .expect("workspace");
        let patch = workspace.patch();
        assert!(!patch.is_empty());
        assert!(patch.diff.contains("-pub fn wrapper"));
        assert!(patch.revert_diff.contains("+pub fn wrapper"));
        assert_eq!(patch.touched_files, vec!["src/lib.rs".to_owned()]);
    }

    #[test]
    fn an_out_of_range_edit_is_rejected_instead_of_partially_applied() {
        let dir = project();
        let error = PatchedWorkspace::create(
            dir.path(),
            &candidate(vec![Edit::delete("src/lib.rs", 40, 50, "bogus")]),
            &Budget::default(),
        )
        .expect_err("range");
        assert!(matches!(error, SimplifyError::EditRejected { .. }));
    }

    #[test]
    fn net_line_delta_counts_added_minus_removed() {
        let dir = project();
        let workspace = PatchedWorkspace::create(
            dir.path(),
            &candidate(vec![Edit::delete("src/lib.rs", 1, 3, "drop wrapper")]),
            &Budget::default(),
        )
        .expect("workspace");
        assert!(workspace.patch().net_line_delta() < 0, "删除应给出负增量");
    }
}
