//! 回归验证（FR-052）。
//!
//! # 为什么回归验证必须双向跑
//!
//! 只在补丁后的副本上跑测试是不够的：如果项目**本来就有**测试失败，补丁后的
//! 失败说明不了任何事。因此基线副本与补丁副本都要跑，结论来自两者的差异。
//!
//! # 为什么「基线本就失败」记成未验证
//!
//! 基线失败时测试套件已经不能当作行为基准。此时若报告「无回归」，等于用一份
//! 坏的基准证明补丁无害——这正是红线 4 禁止的「把没做完的检查说成通过」。
//! 因此这种情况返回 [`RegressionVerdict::Unverified`]，并说明原因。

use pf_adapters::ToolOptions;
use pf_verify::sandbox::Sandbox;
use pf_verify::test_runner::{TestRunner, TestStatus};
use serde::{Deserialize, Serialize};

/// 回归结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegressionVerdict {
    /// 基线通过、补丁后也通过：没有观察到回归。
    NoRegression,
    /// 基线通过、补丁后失败：补丁引入了回归。
    Regression,
    /// 没能得到结论（工具缺失、超时、基线本就失败）。
    Unverified,
}

impl RegressionVerdict {
    /// 稳定标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NoRegression => "no_regression",
            Self::Regression => "regression",
            Self::Unverified => "unverified",
        }
    }

    /// 该结论是否允许补丁进入人工确认环节。
    ///
    /// 只有「没有观察到回归」才允许；未验证的候选仍然可以给人看，但报告必须
    /// 把它标成不可应用。
    #[must_use]
    pub const fn allows_application(&self) -> bool {
        matches!(self, Self::NoRegression)
    }
}

/// 一次回归检查的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegressionCheck {
    /// 基线副本上的测试结论。
    pub baseline: TestStatus,
    /// 补丁副本上的测试结论。
    pub patched: TestStatus,
    /// 结论。
    pub verdict: RegressionVerdict,
    /// 补丁后失败的测试名（若有）。
    pub failed_tests: Vec<String>,
    /// 补充说明：为什么未验证、或哪里需要人注意。
    pub notes: Vec<String>,
}

impl RegressionCheck {
    /// 两侧都没跑出结论时的占位结果。
    #[must_use]
    pub fn unverified(reason: impl Into<String>) -> Self {
        Self {
            baseline: TestStatus::Inconclusive,
            patched: TestStatus::Inconclusive,
            verdict: RegressionVerdict::Unverified,
            failed_tests: Vec::new(),
            notes: vec![reason.into()],
        }
    }
}

/// 在基线副本与补丁副本上分别执行测试套件并比较。
///
/// 两个沙箱由调用方提供：它们必须来自**同一份**目标项目的两次隔离，否则比较
/// 的不是补丁的影响。
#[must_use]
pub fn check(baseline: &Sandbox, patched: &Sandbox, options: &ToolOptions) -> RegressionCheck {
    let baseline_run = TestRunner::in_sandbox(baseline, options);
    let patched_run = TestRunner::in_sandbox(patched, options);

    let mut notes = Vec::new();
    let verdict = match (baseline_run.status, patched_run.status) {
        (TestStatus::Inconclusive, _) => {
            notes.push(
                "基线测试没能执行（工具缺失、超时或无法解析输出），因此没有行为基准。".to_owned(),
            );
            RegressionVerdict::Unverified
        }
        (_, TestStatus::Inconclusive) => {
            notes.push(
                "补丁后的测试没能执行（工具缺失、超时或无法解析输出），无法判断是否回归。"
                    .to_owned(),
            );
            RegressionVerdict::Unverified
        }
        (TestStatus::Failed, TestStatus::Passed) => {
            notes.push(
                "基线测试本就失败而补丁后通过：请确认补丁没有删除或削弱测试，而不是修复了行为。"
                    .to_owned(),
            );
            RegressionVerdict::NoRegression
        }
        (TestStatus::Failed, TestStatus::Failed) => {
            notes.push("基线与补丁后都有测试失败：测试套件无法证明补丁无害。".to_owned());
            RegressionVerdict::Unverified
        }
        (TestStatus::Passed, TestStatus::Failed) => RegressionVerdict::Regression,
        (TestStatus::Passed, TestStatus::Passed) => RegressionVerdict::NoRegression,
    };

    RegressionCheck {
        baseline: baseline_run.status,
        patched: patched_run.status,
        verdict,
        failed_tests: patched_run.failed_tests,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_no_regression_allows_application() {
        // 未验证的候选可以给人看，但不能被应用（红线 4）。
        assert!(RegressionVerdict::NoRegression.allows_application());
        assert!(!RegressionVerdict::Regression.allows_application());
        assert!(!RegressionVerdict::Unverified.allows_application());
    }

    #[test]
    fn an_unverified_check_explains_itself() {
        let check = RegressionCheck::unverified("cargo not found");
        assert_eq!(check.verdict, RegressionVerdict::Unverified);
        assert_eq!(check.notes, vec!["cargo not found".to_owned()]);
    }
}
