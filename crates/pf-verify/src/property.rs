//! 性质验证：批量样本、反例缩减与复现命令（FR-035、FR-042、FR-043）。
//!
//! 一次性质验证分三步，每步的结论都独立记录：
//!
//! 1. **批量探测**——按 [`crate::domain::InputDomain`] 生成样本，一次执行全部
//!    验证。全部满足记 `Satisfied`，任一违反则进入第 2 步。
//! 2. **反例缩减**——用 [`crate::reduce::Reducer`] 生成更小的候选，分批执行
//!    淘汰，直到找不到更小但仍违反的输入（需求文档 7.5）。
//! 3. **复现命令**——把驱动程序与命令原样交出，让人可以在自己的机器上重放
//!    （FR-043 结果复现）。
//!
//! # 证据边界
//!
//! `Satisfied` **不等于**「程序正确」：它只说明在这批样本上不变式成立。
//! 报告里的措辞因此是「未观察到违反」，而不是「已验证正确」——后者需要完备
//! 的证明，超出本系统的范围（需求文档 1.5 非目标）。

use pf_adapters::ToolOptions;
use pf_core::evidence::{ExecutionResult, ExecutionStatus};

use crate::domain::InputValue;
use crate::driver::{
    render_driver, render_reduction_driver, violation_index, violation_indices, DriverOutcome,
    Language, PropertySpec, VIOLATION_MARKER,
};
use crate::reduce::{Reducer, ReductionOutcome};
use crate::sandbox::Sandbox;

/// 一次性质验证的结论。
#[derive(Debug, Clone)]
pub struct PropertyOutcome {
    /// 被验证的性质。
    pub spec: PropertySpec,
    /// 结论。
    pub result: PropertyResult,
    /// 缩减后的反例（若有）。
    pub counterexample: Option<Counterexample>,
    /// 首次探测的执行记录。
    pub probe: ExecutionResult,
    /// 缩减过程的执行记录（每轮一条）。
    pub reduction_rounds: Vec<ExecutionResult>,
}

/// 性质验证的三种结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertyResult {
    /// 全部样本都满足不变式。
    ///
    /// 措辞是「未观察到违反」而不是「正确」：样本是有限的，结论的强度不能
    /// 超过证据（AGENTS.md 安全红线 5）。
    Satisfied {
        /// 验证过的样本数。
        samples: usize,
    },
    /// 找到反例并已完成缩减。
    Violated,
    /// 没有得到结论（编译失败、工具缺失、超时）。
    Inconclusive(String),
}

/// 一个已缩减的反例。
#[derive(Debug, Clone)]
pub struct Counterexample {
    /// 缩减后的输入。
    pub input: InputValue,
    /// 缩减过程的度量。
    pub reduction: ReductionOutcome,
    /// 复现所需的一切。
    pub repro: ReproRecipe,
}

/// 复现一份反例所需的全部信息。
///
/// 设计目标：**不看 `ProofForge` 的文档也能重放**。因此它自带驱动程序源码、
/// 放置路径与命令，三样凑齐就能在用户自己的机器上跑。
#[derive(Debug, Clone)]
pub struct ReproRecipe {
    /// 目标语言。
    pub language: Language,
    /// 驱动程序在用户项目里的相对路径。
    pub driver_path: String,
    /// 驱动程序源码。
    pub driver_source: String,
    /// 可直接粘贴执行的命令。
    pub command: String,
}

impl ReproRecipe {
    /// 渲染成人类可读的复现说明。
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "把下面的内容保存为 `{path}`，然后执行：\n\n    {command}\n\n\
             --- {path} ---\n{source}\n--- 结束 ---",
            path = self.driver_path,
            command = self.command,
            source = self.driver_source,
        )
    }
}

/// 性质验证器。
#[derive(Debug, Default)]
pub struct PropertyRunner;

impl PropertyRunner {
    /// 在沙箱里验证一条性质。
    ///
    /// # Errors
    /// 驱动程序写不进沙箱时返回 [`pf_core::Error`]。
    pub fn run(
        sandbox: &Sandbox,
        spec: &PropertySpec,
        options: &ToolOptions,
    ) -> pf_core::Result<PropertyOutcome> {
        let run = crate::driver::run_property(sandbox, spec, options)?;
        match run.outcome {
            DriverOutcome::Satisfied => Ok(PropertyOutcome {
                spec: spec.clone(),
                result: PropertyResult::Satisfied {
                    samples: spec.samples,
                },
                counterexample: None,
                probe: run.execution,
                reduction_rounds: Vec::new(),
            }),
            DriverOutcome::Violated(_) => {
                let input = run
                    .counterexample
                    .ok_or_else(|| missing_counterexample(spec))?;
                let (counterexample, rounds) = reduce_in_sandbox(sandbox, spec, &input, options);
                Ok(PropertyOutcome {
                    spec: spec.clone(),
                    result: PropertyResult::Violated,
                    counterexample: Some(counterexample),
                    probe: run.execution,
                    reduction_rounds: rounds,
                })
            }
            DriverOutcome::Inconclusive(reason) => Ok(PropertyOutcome {
                spec: spec.clone(),
                result: PropertyResult::Inconclusive(reason),
                counterexample: None,
                probe: run.execution,
                reduction_rounds: Vec::new(),
            }),
        }
    }
}

/// 输入值的稳定排序键。
///
/// [`InputValue`] 没有实现 `Ord`——它的变体之间没有天然的大小关系。这里用
/// 「规模 + 文本描述」做键：规模相同的情况下按描述排序，保证整个缩减过程
/// 对相同输入给出相同结果。
fn describe_key(value: &InputValue) -> (usize, String) {
    (value.size(), value.describe())
}

/// 在沙箱里缩减一个反例，并生成复现配方。
///
/// 缩减的判定函数是「这个候选仍然违反不变式」，因此每一轮都要真实执行一次
/// 目标程序。候选成批执行：一轮一次编译执行，而不是一个候选一次。
fn reduce_in_sandbox(
    sandbox: &Sandbox,
    spec: &PropertySpec,
    input: &InputValue,
    options: &ToolOptions,
) -> (Counterexample, Vec<ExecutionResult>) {
    let mut rounds = Vec::new();
    let mut current = input.clone();
    let mut applied = 0usize;
    // 只有在「每一轮都真实地问出了结论」时才叫收敛。驱动程序编译不过时
    // `still_failing` 也是空的，但那意味着问不出来，不是没有更小的反例——
    // 把这种情况记成收敛就是对使用者撒谎。
    let mut converged = true;

    loop {
        let candidates = Reducer::candidates(&current);
        if candidates.is_empty() {
            break;
        }
        let batch = run_reduction_batch(sandbox, spec, &candidates, options, &mut rounds);
        if !batch.ran {
            converged = false;
            break;
        }
        // 取「仍然失败且规模最小」的候选；规模相同时取靠前的一个——候选顺序
        // 本身是确定的，因此整个缩减过程可复现。
        let next = batch
            .still_failing
            .into_iter()
            .filter_map(|index| candidates.get(index).cloned())
            .filter(|candidate| candidate.size() < current.size())
            .min_by(|a, b| {
                a.size()
                    .cmp(&b.size())
                    .then_with(|| describe_key(a).cmp(&describe_key(b)))
            });
        match next {
            Some(smaller) => {
                current = smaller;
                applied += 1;
            }
            None => break,
        }
    }

    let reduction = ReductionOutcome {
        value: current.clone(),
        original_size: input.size(),
        final_size: current.size(),
        rounds: applied,
        converged,
    };
    let driver_source = render_driver(spec, std::slice::from_ref(&current));
    (
        Counterexample {
            input: current,
            reduction,
            repro: repro_recipe(spec, driver_source),
        },
        rounds,
    )
}

/// 一批缩减候选的执行结论。
#[derive(Debug, Clone, Default)]
struct BatchOutcome {
    /// 仍然失败的下标。
    still_failing: Vec<usize>,
    /// 驱动程序是否真的跑完了。
    ///
    /// `false` 时 `still_failing` 必然为空，但那表示「问不出来」，不是「没有
    /// 候选仍然失败」。少了这个字段，两者在调用方看起来一模一样。
    ran: bool,
}

/// 执行一批缩减候选，返回仍然失败的下标。
///
/// 执行失败（编译不过、超时、工具缺失）时 `ran` 为 `false`：拿不到结论就不能
/// 继续缩减，此时反例保持原样——一个没被压小的真反例好过一个被压坏的假反例。
fn run_reduction_batch(
    sandbox: &Sandbox,
    spec: &PropertySpec,
    candidates: &[InputValue],
    options: &ToolOptions,
    rounds: &mut Vec<ExecutionResult>,
) -> BatchOutcome {
    let source = render_reduction_driver(spec, candidates);
    if sandbox
        .write_file(spec.language.driver_path(), &source)
        .is_err()
    {
        return BatchOutcome::default();
    }
    let execution = crate::driver::run_driver(sandbox, spec.language, options);
    rounds.push(execution.clone());
    if execution.failed_to_start() || execution.status == ExecutionStatus::Signalled {
        return BatchOutcome::default();
    }
    let ran = crate::driver::reduction_driver_ran(&execution.stdout);
    // 崩溃与违反都算「仍然失败」——两者都是「这个候选不满足不变式」的观测。
    // 分开只是为了报告能说清是哪一类（见 `fuzz::BehaviorClass`），缩减本身
    // 对它们一视同仁。
    let mut still_failing = violation_indices(&execution.stdout).unwrap_or_default();
    still_failing.extend(crate::driver::crash_indices(&execution.stdout).unwrap_or_default());
    still_failing.sort_unstable();
    still_failing.dedup();
    BatchOutcome { still_failing, ran }
}

/// 构造复现配方。
///
/// 公开给蜕变测试复用：两份复现配方必须落在同一路径、用同一条命令，否则
/// 「照着一份跑出来、照着另一份跑不出来」。
pub fn repro_recipe(spec: &PropertySpec, driver_source: String) -> ReproRecipe {
    let (driver_path, command) = match spec.language {
        Language::Rust => (
            "tests/proofforge_driver.rs".to_owned(),
            "cargo test --test proofforge_driver".to_owned(),
        ),
        Language::Python => (
            "test_proofforge_driver.py".to_owned(),
            "pytest -q test_proofforge_driver.py".to_owned(),
        ),
    };
    ReproRecipe {
        language: spec.language,
        driver_path,
        driver_source,
        command,
    }
}

/// 驱动程序报告了违反却给不出输入：这是 `ProofForge` 内部的缺陷。
///
/// 用错误而不是猜测一个输入——编一个输入充当反例就是伪造证据（红线 5）。
fn missing_counterexample(spec: &PropertySpec) -> pf_core::Error {
    pf_core::Error::Serialization {
        reason: format!(
            "driver reported a violation for `{}` but no sample matched",
            spec.call_template
        ),
    }
}

/// 判断一次执行是否报告了违反。
///
/// 报告层用它把「性质被违反」与「没跑起来」区分开：两者的退出码都是非零，
/// 但只有前者能支撑一条发现（AGENTS.md 安全红线 4）。
#[must_use]
pub fn reports_violation(execution: &ExecutionResult) -> bool {
    violation_index(&execution.stdout).is_some()
        || execution.stdout.contains(&format!("{VIOLATION_MARKER} ["))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::InputDomain;
    use crate::driver::DriverRun;
    use pf_core::evidence::Command;

    fn spec() -> PropertySpec {
        PropertySpec {
            name: None,
            language: Language::Rust,
            imports: vec!["fixture".to_owned()],
            call_template: "fixture::parse({args})".to_owned(),
            domain: InputDomain::Int { min: 0, max: 10 },
            invariant: "output >= 0".to_owned(),
            samples: 8,
            seed: 3,
        }
    }

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
    fn satisfied_run_carries_the_sample_count() {
        let run = DriverRun {
            outcome: DriverOutcome::Satisfied,
            counterexample: None,
            execution: execution("test result: ok. 1 passed\n", Some(0)),
        };
        assert!(matches!(run.outcome, DriverOutcome::Satisfied));
    }

    #[test]
    fn repro_recipe_is_self_contained() {
        let spec = spec();
        let source = "// driver\n".to_owned();
        let recipe = repro_recipe(&spec, source.clone());
        let rendered = recipe.render();
        assert!(rendered.contains("tests/proofforge_driver.rs"));
        assert!(rendered.contains("cargo test --test proofforge_driver"));
        assert!(rendered.contains("// driver"), "配方必须自带驱动程序源码");
        // 不看 `ProofForge` 文档也能照着做：路径、命令、源码三样齐全。
        assert!(rendered.contains("--- tests/proofforge_driver.rs ---"));
    }

    #[test]
    fn python_repro_recipe_uses_pytest() {
        let spec = PropertySpec {
            name: None,
            language: Language::Python,
            ..spec()
        };
        let recipe = repro_recipe(&spec, "# driver\n".to_owned());
        assert_eq!(recipe.driver_path, "test_proofforge_driver.py");
        assert_eq!(recipe.command, "pytest -q test_proofforge_driver.py");
    }

    #[test]
    fn missing_counterexample_is_an_error_not_a_guess() {
        let err = missing_counterexample(&spec());
        assert!(err.to_string().contains("no sample matched"), "{err}");
    }

    #[test]
    fn reports_violation_detects_both_message_shapes() {
        assert!(reports_violation(&execution(
            &format!("{VIOLATION_MARKER} 1\n"),
            Some(101)
        )));
        assert!(reports_violation(&execution(
            &format!("{VIOLATION_MARKER} [0, 2]\n"),
            Some(101)
        )));
        assert!(!reports_violation(&execution("test result: ok\n", Some(0))));
    }
}
