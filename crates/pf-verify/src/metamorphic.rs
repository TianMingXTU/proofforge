//! 蜕变测试（需求文档 FR-039）。
//!
//! # 它解决什么问题
//!
//! 性质测试需要一条**预言机**——「输出应该是什么」。很多函数没有现成的预言机：
//! 你知道「排序结果与输入顺序无关」，但写不出「排序 `[3,1,2]` 必须得到 `[1,2,3]`」
//! 这样对每个输入都成立的断言。蜕变测试换一个问法：**不检查输出的绝对值，检查
//! 两个相关输入的输出之间的关系**。
//!
//! 一条蜕变关系由三样组成：
//!
//! 1. 一个**源输入** `x`（来自 [`crate::domain::InputDomain`]）；
//! 2. 一个**输入变换** `T`（`transform`，例如把列表反转）；
//! 3. 一个**输出关系** `R`（`relation`，例如 `output_a == output_b`）。
//!
//! 断言的是 `R(f(x), f(T(x)))`。上面的例子于是变成「排序结果与输入顺序无关」——
//! 一条不需要知道正确答案的命题。
//!
//! # 三条边界
//!
//! 1. **变换由人写**。工具不知道哪个变换对目标函数有意义；自动挑一个变换再宣称
//!    「验证了蜕变关系」，是红线 5 禁止的伪造结论。
//! 2. **关系为真不等于程序正确**。一条恒真关系（例如 `output_a == output_a`）会
//!    让每个样本都通过，而它什么也没验证。工具无法识别恒真关系，因此报告用
//!    「未观察到违反」而不是「已验证」。
//! 3. **变换必须保持实参形状**。`transform` 产出的表达式要能原样填进
//!    `call_template` 的 `{args}` 槽位；形状不符时驱动程序编译不过，结论是
//!    证据不足，不是违反。

use pf_adapters::ToolOptions;
use pf_core::evidence::ExecutionResult;

use crate::domain::InputValue;
use crate::driver::{run_metamorphic, DriverOutcome, Language};
use crate::property::repro_recipe;
use crate::reduce::{Reducer, ReductionOutcome};
use crate::sandbox::Sandbox;

/// 一条蜕变关系。
///
/// 与 [`crate::driver::PropertySpec`] 的差别只在两处：`transform`（对输入做一次
/// 变换）与 `relation`（比较两次调用的输出）。其余字段——语言、引用、调用模板、
/// 输入域、样本数、种子——含义完全一致，因此规格文件可以共用同一套默认值。
#[derive(Debug, Clone)]
pub struct MetamorphicSpec {
    /// 人类可读的名称；报告用它指认每一条关系。
    pub name: Option<String>,
    /// 目标语言。
    pub language: Language,
    /// 需要引入的引用（每行一条，原样写入驱动程序的 import 区）。
    pub imports: Vec<String>,
    /// 调用模板，`{args}` 会被替换成实参表达式。
    pub call_template: String,
    /// 源输入的域。
    pub domain: crate::domain::InputDomain,
    /// 输入变换表达式，必须包含 `{input}` 占位符。
    ///
    /// `{input}` 被替换成**整个输入值**：标量域是单个值（Rust 里是 `input.0`），
    /// 元组域是元组本身（`input`）。整个表达式填的是 `call_template` 的 `{args}`
    /// 槽位，因此形状要与源调用一致：元组域要展开成逗号分隔的实参列表
    /// （`{input}.1, {input}.0`），写成字面元组会让第二次调用少收一个参数。
    pub transform: String,
    /// 对两次调用输出的关系断言，其中 `output_a` 绑定到原始输入的输出、
    /// `output_b` 绑定到变换后输入的输出。
    pub relation: String,
    /// 样本数量。
    pub samples: usize,
    /// 随机种子。
    pub seed: u64,
}

/// 一次蜕变验证的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetamorphicResult {
    /// 全部样本都满足关系。
    ///
    /// 措辞是「未观察到违反」而不是「关系成立」：样本是有限的，且关系本身由人
    /// 给出，工具没有验证过它是否恒真（AGENTS.md 安全红线 5）。
    Satisfied {
        /// 验证过的样本数。
        samples: usize,
    },
    /// 找到反例并已完成缩减。
    Violated,
    /// 没有得到结论（编译失败、工具缺失、超时）。
    Inconclusive(String),
}

/// 一个已缩减的蜕变反例。
#[derive(Debug, Clone)]
pub struct MetamorphicCounterexample {
    /// 缩减后的**源输入**。
    ///
    /// 记录源输入而不是变换后的输入：变换是规格的一部分，交给源输入就能完整
    /// 复现这一次比较。
    pub input: InputValue,
    /// 缩减过程的度量。
    pub reduction: ReductionOutcome,
    /// 复现所需的一切。
    pub repro: crate::property::ReproRecipe,
}

/// 一次蜕变验证的完整记录。
#[derive(Debug, Clone)]
pub struct MetamorphicOutcome {
    /// 被验证的关系。
    pub spec: MetamorphicSpec,
    /// 结论。
    pub result: MetamorphicResult,
    /// 缩减后的反例（若有）。
    pub counterexample: Option<MetamorphicCounterexample>,
    /// 首次探测的执行记录。
    pub probe: ExecutionResult,
    /// 缩减过程的执行记录（每轮一条）。
    pub reduction_rounds: Vec<ExecutionResult>,
}

/// 蜕变验证器。
#[derive(Debug, Default)]
pub struct MetamorphicRunner;

impl MetamorphicRunner {
    /// 在沙箱里验证一条蜕变关系。
    ///
    /// 步骤与 [`crate::property::PropertyRunner`] 一致：批量探测 → 反例缩减 →
    /// 复现配方。缩减的判定函数是「这个候选仍然让关系不成立」，因此每一轮都要
    /// 真实执行一次目标程序。
    ///
    /// # Errors
    /// 驱动程序写不进沙箱时返回 [`pf_core::Error`]。
    pub fn run(
        sandbox: &Sandbox,
        spec: &MetamorphicSpec,
        options: &ToolOptions,
    ) -> pf_core::Result<MetamorphicOutcome> {
        let run = run_metamorphic(sandbox, spec, options)?;
        match run.outcome {
            DriverOutcome::Satisfied => Ok(MetamorphicOutcome {
                spec: spec.clone(),
                result: MetamorphicResult::Satisfied {
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
                Ok(MetamorphicOutcome {
                    spec: spec.clone(),
                    result: MetamorphicResult::Violated,
                    counterexample: Some(counterexample),
                    probe: run.execution,
                    reduction_rounds: rounds,
                })
            }
            DriverOutcome::Inconclusive(reason) => Ok(MetamorphicOutcome {
                spec: spec.clone(),
                result: MetamorphicResult::Inconclusive(reason),
                counterexample: None,
                probe: run.execution,
                reduction_rounds: Vec::new(),
            }),
        }
    }
}

/// 在沙箱里缩减一个蜕变反例。
///
/// 与 [`crate::property`] 的缩减共用 [`Reducer`]，因此「更小」的定义一致：规模
/// 更小、规模相同时按稳定描述排序。两套验证给出同一套缩减语义，报告才对得上。
fn reduce_in_sandbox(
    sandbox: &Sandbox,
    spec: &MetamorphicSpec,
    input: &InputValue,
    options: &ToolOptions,
) -> (MetamorphicCounterexample, Vec<ExecutionResult>) {
    let mut rounds = Vec::new();
    let mut current = input.clone();
    let mut applied = 0usize;
    // 只有在每一轮都真实地问出结论时才叫收敛：驱动程序编译不过时候选列表也是
    // 空的，但那是问不出来，不是没有更小的反例。
    let mut converged = true;

    loop {
        let candidates = Reducer::candidates(&current);
        if candidates.is_empty() {
            break;
        }
        let mut still_failing = Vec::new();
        let mut ran = false;
        for (index, candidate) in candidates.iter().enumerate() {
            let source =
                crate::driver::render_metamorphic_driver(spec, std::slice::from_ref(candidate));
            if sandbox
                .write_file(spec.language.driver_path(), &source)
                .is_err()
            {
                break;
            }
            let execution = crate::driver::run_driver(sandbox, spec.language, options);
            rounds.push(execution.clone());
            if execution.failed_to_start()
                || execution.status == pf_core::evidence::ExecutionStatus::Signalled
            {
                break;
            }
            if !crate::driver::driver_ran(&execution.stdout) {
                break;
            }
            ran = true;
            if crate::driver::reports_violation_marker(&execution.stdout) {
                still_failing.push(index);
            }
        }
        if !ran {
            converged = false;
            break;
        }
        let next = still_failing
            .into_iter()
            .filter_map(|index| candidates.get(index).cloned())
            .filter(|candidate| candidate.size() < current.size())
            .min_by(|a, b| {
                a.size()
                    .cmp(&b.size())
                    .then_with(|| (a.size(), a.describe()).cmp(&(b.size(), b.describe())))
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
    let driver_source =
        crate::driver::render_metamorphic_driver(spec, std::slice::from_ref(&current));
    (
        MetamorphicCounterexample {
            input: current,
            reduction,
            repro: repro_recipe_for_metamorphic(spec, driver_source),
        },
        rounds,
    )
}

/// 构造蜕变反例的复现配方。
///
/// 复用 [`crate::property::repro_recipe`]：驱动程序落在同一个路径、用同一条命令
/// 执行。两份配方不一致会让「照着一份跑出来、照着另一份跑不出来」。
fn repro_recipe_for_metamorphic(
    spec: &MetamorphicSpec,
    driver_source: String,
) -> crate::property::ReproRecipe {
    let property_spec = crate::driver::PropertySpec {
        name: spec.name.clone(),
        language: spec.language,
        imports: spec.imports.clone(),
        call_template: spec.call_template.clone(),
        domain: spec.domain.clone(),
        invariant: spec.relation.clone(),
        samples: spec.samples,
        seed: spec.seed,
    };
    repro_recipe(&property_spec, driver_source)
}

/// 驱动程序报告了违反却给不出输入：这是 `ProofForge` 内部的缺陷。
///
/// 用错误而不是猜一个输入——编一个输入充当反例就是伪造证据（红线 5）。
fn missing_counterexample(spec: &MetamorphicSpec) -> pf_core::Error {
    pf_core::Error::Serialization {
        reason: format!(
            "driver reported a violation for `{}` but no sample matched",
            spec.call_template
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::InputDomain;
    use pf_core::evidence::{Command, ExecutionStatus};

    fn spec() -> MetamorphicSpec {
        MetamorphicSpec {
            name: Some("排序与输入顺序无关".to_owned()),
            language: Language::Rust,
            imports: vec!["fixture".to_owned()],
            call_template: "fixture::sort({args})".to_owned(),
            domain: InputDomain::List {
                item: Box::new(InputDomain::Int { min: -20, max: 20 }),
                max_len: 6,
            },
            transform: "{input}.iter().rev().cloned().collect::<Vec<_>>()".to_owned(),
            relation: "output_a == output_b".to_owned(),
            samples: 8,
            seed: 5,
        }
    }

    #[test]
    fn transform_placeholder_is_substituted_with_the_whole_input_value() {
        let source = crate::driver::render_metamorphic_driver(
            &spec(),
            &[InputValue::List(vec![
                InputValue::Int(1),
                InputValue::Int(2),
            ])],
        );
        assert!(
            source.contains("input.0.iter().rev().cloned().collect::<Vec<_>>()"),
            "标量域拿到的是 input.0：{source}"
        );
        assert!(
            source.contains("let output_a = fixture::sort(input.0);"),
            "{source}"
        );
        assert!(source.contains("output_a == output_b"), "{source}");
    }

    #[test]
    fn a_tuple_transform_expands_into_the_same_argument_list_as_the_source_call() {
        // 回归：变换填的是 `{args}` 槽位，形状必须与源调用一致。曾经把这条契约
        // 写成「元组域的变换要产出字面元组」，于是第二次调用只收到一个参数，
        // 每一个元组域的蜕变关系都编译不过——而编译失败只报证据不足，看不出是
        // 规格写错了。
        let tuple_spec = MetamorphicSpec {
            domain: InputDomain::Tuple {
                fields: vec![
                    InputDomain::Int { min: 0, max: 5 },
                    InputDomain::Int { min: 0, max: 5 },
                ],
            },
            transform: "{input}.1, {input}.0".to_owned(),
            relation: "output_b == output_a".to_owned(),
            ..spec()
        };
        let source = crate::driver::render_metamorphic_driver(
            &tuple_spec,
            &[InputValue::Tuple(vec![
                InputValue::Int(1),
                InputValue::Int(2),
            ])],
        );
        assert!(
            source.contains("let output_a = fixture::sort(input.0, input.1);"),
            "{source}"
        );
        assert!(
            source.contains("let output_b = fixture::sort(input.1, input.0);"),
            "两次调用的实参个数必须一致：{source}"
        );
    }

    #[test]
    fn the_driver_prints_the_same_markers_as_the_property_driver() {
        // 两套驱动必须共用标记与判定规则，否则「性质说跑过了、蜕变说没跑」。
        let source = crate::driver::render_metamorphic_driver(
            &spec(),
            &[InputValue::List(vec![InputValue::Int(1)])],
        );
        assert!(source.contains(crate::driver::VIOLATION_MARKER));
        assert!(source.contains(crate::driver::DONE_MARKER));
    }

    #[test]
    fn a_satisfied_relation_reports_the_sample_count() {
        let outcome = MetamorphicOutcome {
            spec: spec(),
            result: MetamorphicResult::Satisfied { samples: 8 },
            counterexample: None,
            probe: ExecutionResult {
                command: Command::new("cargo"),
                status: ExecutionStatus::Exited,
                exit_code: Some(0),
                signal: None,
                duration_ms: 1,
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
            },
            reduction_rounds: Vec::new(),
        };
        assert!(matches!(
            outcome.result,
            MetamorphicResult::Satisfied { samples: 8 }
        ));
    }

    #[test]
    fn missing_counterexample_is_an_error_not_a_guess() {
        let err = missing_counterexample(&spec());
        assert!(err.to_string().contains("no sample matched"), "{err}");
    }
}
