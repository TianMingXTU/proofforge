//! 测试执行器（阶段三第一个交付项）。
//!
//! 职责单一：在隔离沙箱里运行目标项目**自带的**测试套件，把结果整理成可定位
//! 的条目。它不生成测试、不判断对错——「项目自己的测试是否通过」由测试本身
//! 回答，本模块只负责可信地转述（AGENTS.md 安全红线 4）。
//!
//! # 为什么不复用 [`crate::property`] 的驱动机制
//!
//! 两者的执行路径相同（适配器生成命令 → 沙箱执行 → 解读输出），但语义不同：
//! 性质验证关心「这个输入下不变式是否成立」，测试执行关心「项目作者写的断言
//! 是否全部通过」。混在一起会让报告分不清「谁写的测试失败了」。

use pf_adapters::{Tool, ToolOptions};
use pf_core::config::Budget;
use pf_core::evidence::{ExecutionResult, ExecutionStatus};
use pf_core::status::EvidenceStatus;
use serde::{Deserialize, Serialize};

use crate::sandbox::Sandbox;

/// 一次测试套件执行的结果。
#[derive(Debug, Clone)]
pub struct TestRun {
    /// 底层执行记录，作为可复现证据。
    pub execution: ExecutionResult,
    /// 解读出的结论。
    pub status: TestStatus,
    /// 失败测试的名字（若输出里能解析出来）。
    pub failed_tests: Vec<String>,
}

/// 测试套件的执行结论。
///
/// 派生序列化：简化管线要把两侧结论原样写进报告（FR-066），转换成字符串会
/// 丢失「这是枚举而不是自由文本」这层类型信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    /// 全部通过。
    Passed,
    /// 有测试失败。
    Failed,
    /// 没能得到结论（工具缺失、超时、输出无法解析）。
    Inconclusive,
}

impl TestRun {
    /// 结论对应的证据状态。
    ///
    /// 「工具缺失」与「有测试失败」的可信度完全不同：前者是 `Inconclusive`，
    /// 后者是 `Reproduced`——测试断言是项目自己写下的预期，失败即复现。
    #[must_use]
    pub const fn evidence_status(&self) -> EvidenceStatus {
        match self.status {
            TestStatus::Passed => EvidenceStatus::NotReproduced,
            TestStatus::Failed => EvidenceStatus::Reproduced,
            TestStatus::Inconclusive => EvidenceStatus::Inconclusive,
        }
    }
}

/// 测试执行器。
#[derive(Debug, Default)]
pub struct TestRunner;

impl TestRunner {
    /// 在目标项目上执行测试套件。
    ///
    /// # Errors
    /// 沙箱创建失败时返回 [`pf_core::Error`]。
    pub fn run(target: &std::path::Path, budget: &Budget) -> pf_core::Result<TestRun> {
        Self::run_with_options(target, budget, &ToolOptions::default())
    }

    /// 在目标项目上执行测试套件，并指定工具选项。
    ///
    /// # Errors
    /// 沙箱创建失败时返回 [`pf_core::Error`]。
    pub fn run_with_options(
        target: &std::path::Path,
        budget: &Budget,
        options: &ToolOptions,
    ) -> pf_core::Result<TestRun> {
        let sandbox = Sandbox::isolate(target, budget)?;
        Ok(Self::in_sandbox(&sandbox, options))
    }

    /// 在**已隔离**的工作区里执行测试套件。
    ///
    /// 与 [`Self::run_with_options`] 的区别只在于谁负责隔离：调用方自己创建
    /// 沙箱时（例如 `pf-simplify` 要先在副本上打补丁再跑测试）用它，避免
    /// 为同一份代码复制两遍工作区。
    ///
    /// 不返回 `Result`：执行失败（工具缺失、编译不过、断言失败）本身就是
    /// 结论，由 [`TestRun::status`] 表达，不是错误。
    #[must_use]
    pub fn in_sandbox(sandbox: &Sandbox, options: &ToolOptions) -> TestRun {
        summarize(sandbox.run(Tool::Test, options))
    }
}

/// 把一次执行整理成测试结论。
fn summarize(execution: ExecutionResult) -> TestRun {
    if execution.failed_to_start() {
        return TestRun {
            status: TestStatus::Inconclusive,
            failed_tests: Vec::new(),
            execution,
        };
    }
    if execution.status == ExecutionStatus::Signalled {
        return TestRun {
            status: TestStatus::Inconclusive,
            failed_tests: Vec::new(),
            execution,
        };
    }
    if execution.succeeded() {
        return TestRun {
            status: TestStatus::Passed,
            failed_tests: Vec::new(),
            execution,
        };
    }
    TestRun {
        failed_tests: failed_test_names(&execution.stdout),
        status: TestStatus::Failed,
        execution,
    }
}

/// 从 `cargo test` / `pytest` 的输出里提取失败测试的名字。
///
/// 解析是**尽力而为**：不同工具链的输出格式会变。解析不到名字不影响结论——
/// 失败这件事由退出码给出，名字只是给人看的线索。
fn failed_test_names(stdout: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in stdout.lines() {
        // cargo: "test foo::bar ... FAILED" / "failures:\n    foo::bar"
        if let Some(rest) = line.trim().strip_prefix("test ") {
            if let Some(name) = rest.strip_suffix(" ... FAILED") {
                names.push(name.trim().to_owned());
            }
        }
        // pytest: "FAILED tests/test_x.py::test_case - assert ..."
        if let Some(rest) = line.trim().strip_prefix("FAILED ") {
            if let Some((name, _)) = rest.split_once(" - ") {
                names.push(name.trim().to_owned());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::evidence::Command;

    fn execution(stdout: &str, exit_code: Option<i32>) -> ExecutionResult {
        ExecutionResult {
            command: Command::new("cargo"),
            status: ExecutionStatus::Exited,
            exit_code,
            signal: None,
            duration_ms: 1,
            stdout: stdout.to_owned(),
            stderr: String::new(),
            truncated: false,
        }
    }

    #[test]
    fn passing_suite_is_not_reproduced() {
        let run = summarize(execution("test result: ok. 3 passed\n", Some(0)));
        assert_eq!(run.status, TestStatus::Passed);
        assert_eq!(run.evidence_status(), EvidenceStatus::NotReproduced);
        assert_eq!(run.failed_tests, Vec::<String>::new());
    }

    #[test]
    fn failing_suite_is_reproduced_with_names() {
        let run = summarize(execution(
            "test alpha ... FAILED\ntest beta ... ok\nfailures:\n    alpha\n",
            Some(101),
        ));
        assert_eq!(run.status, TestStatus::Failed);
        assert_eq!(run.evidence_status(), EvidenceStatus::Reproduced);
        assert_eq!(run.failed_tests, vec!["alpha".to_owned()]);
    }

    #[test]
    fn pytest_failures_are_parsed_too() {
        let run = summarize(execution(
            "FAILED tests/test_api.py::test_case - assert 1 == 2\n",
            Some(1),
        ));
        assert_eq!(run.status, TestStatus::Failed);
        assert_eq!(
            run.failed_tests,
            vec!["tests/test_api.py::test_case".to_owned()]
        );
    }

    #[test]
    fn missing_toolchain_is_inconclusive() {
        let mut result = execution("", None);
        result.status = ExecutionStatus::FailedToStart;
        result.stderr = "cargo: not found".to_owned();
        let run = summarize(result);
        assert_eq!(run.status, TestStatus::Inconclusive);
        assert_eq!(run.evidence_status(), EvidenceStatus::Inconclusive);
    }

    #[test]
    fn timeout_is_inconclusive() {
        let mut result = execution("", None);
        result.status = ExecutionStatus::Signalled;
        result.stderr = "timed out after 300s".to_owned();
        let run = summarize(result);
        assert_eq!(run.status, TestStatus::Inconclusive);
        assert_eq!(run.evidence_status(), EvidenceStatus::Inconclusive);
    }

    #[test]
    fn unparseable_failure_still_counts_as_failed() {
        // 解析不到测试名不影响结论：退出码已经说明有测试失败。
        let run = summarize(execution("something unexpected happened\n", Some(101)));
        assert_eq!(run.status, TestStatus::Failed);
        assert_eq!(run.failed_tests, Vec::<String>::new());
    }

    #[test]
    fn duplicate_failure_lines_are_deduplicated() {
        let run = summarize(execution(
            "test alpha ... FAILED\nfailures:\n    alpha\n",
            Some(101),
        ));
        assert_eq!(run.failed_tests, vec!["alpha".to_owned()]);
    }

    #[test]
    fn missing_target_is_an_error() {
        let err = TestRunner::run(
            std::path::Path::new("/definitely/not/real"),
            &Budget::default(),
        )
        .expect_err("should fail");
        assert!(matches!(err, pf_core::Error::TargetNotFound { .. }));
    }
}
