//! 行为验证：测试执行、沙箱隔离、属性测试、模糊测试、反例缩减与复现。
//!
//! 对应需求文档阶段三（自主验证原型）：
//!
//! | 模块 | 交付能力 |
//! |---|---|
//! | [`sandbox`] | 沙箱隔离：独立工作区、凭据剔除、超时（FR-028） |
//! | [`domain`] | 输入域与确定性样本生成（FR-034） |
//! | [`driver`] | 驱动程序渲染、执行与结果解读（FR-034、FR-043） |
//! | [`property`] | 性质验证与反例缩减（FR-035、FR-042、FR-043） |
//! | [`fuzz`] | 反馈驱动的输入空间探索（FR-036） |
//! | [`metamorphic`] | 蜕变测试：输入变换与输出关系（FR-039） |
//! | [`mutation`] | 变异测试：度量测试套件能否发现行为变化（FR-037） |
//! | [`report`] | 验证报告与门禁（FR-064、FR-065） |
//! | [`test_runner`] | 测试执行器：跑目标项目自带的测试套件 |
//! | [`spec`] | 性质规格文件：把「验证什么」变成可执行输入 |
//!
//! # 设计约束
//!
//! - **原仓库只读**。一切写入都落在 [`Sandbox`] 的临时副本上，因为 AGENTS.md
//!   安全红线 3 禁止未经授权修改被审查仓库。
//! - **只运行适配器生成的命令**。沙箱不提供「执行任意字符串」的入口（红线 1）。
//! - **凭据不进子进程**。沙箱内的每次执行都剔除环境里的密钥（红线 2）。
//! - **没跑出来就不是通过**。编译失败、工具缺失、超时一律记为证据不足（红线 4）。
//! - **措辞不超过证据**。样本通过只说「未观察到违反」，不说「正确」（红线 5）。
//!
//! # 依赖方向
//!
//! `pf-verify` 只依赖 `pf-core` 与 `pf-adapters`，不被任何静态分析 crate 依赖。
//! 这样「验证」与「静态审查」可以分别运行、分别测试，也可以只在需要验证时
//! 才付出编译目标项目的代价。

#![warn(missing_docs)]

pub mod domain;
pub mod driver;
pub mod fuzz;
pub mod metamorphic;
pub mod mutation;
pub mod property;
pub mod reduce;
pub mod report;
pub mod sandbox;
pub mod spec;
pub mod test_runner;

pub use domain::{InputDomain, InputValue};
pub use driver::{Language, PropertySpec};
pub use fuzz::{FuzzConfig, FuzzOutcome, FuzzRunner};
pub use metamorphic::{MetamorphicOutcome, MetamorphicResult, MetamorphicRunner, MetamorphicSpec};
pub use mutation::{
    plan as plan_mutants, Mutant, MutantResult, MutantStatus, MutationConfig, MutationOperator,
    MutationOutcome, MutationRunner,
};
pub use property::{Counterexample, PropertyOutcome, PropertyResult, PropertyRunner, ReproRecipe};
pub use reduce::{Reducer, ReductionOutcome};
pub use report::{CounterexampleSummary, Verdict, VerificationReport, VerifiedProperty};
pub use sandbox::Sandbox;
pub use spec::{MetamorphicEntry, MetamorphicFile, PropertyEntry, PropertyFile, SpecError};
pub use test_runner::{TestRun, TestRunner};

/// 验证引擎：把「隔离 → 验证 → 报告」串成一次完整运行。
///
/// 与 [`pf_review::ReviewEngine`] 的分工：后者回答「代码长什么样」，本引擎回答
/// 「程序在指定输入下做了什么」。两者都只产出可审计的证据，不修改用户代码。
#[derive(Debug, Default)]
pub struct VerificationEngine;

impl VerificationEngine {
    /// 在隔离沙箱里验证一组性质。
    ///
    /// # 步骤
    ///
    /// 1. 把目标项目复制进独立工作区。
    /// 2. 逐条验证性质，反例自动缩减并生成复现配方。
    /// 3. 汇总成报告与门禁结论。
    ///
    /// # Errors
    /// 目标不存在、不是目录，或沙箱创建失败时返回 [`pf_core::Error`]。
    /// 单条性质「没跑出结论」不是错误——它进入报告的 `Inconclusive` 条目。
    pub fn verify(
        target: &std::path::Path,
        specs: &[PropertySpec],
        relations: &[MetamorphicSpec],
        config: &pf_core::config::Config,
    ) -> pf_core::Result<VerificationReport> {
        let sandbox = Sandbox::isolate(target, &config.budget)?;
        let mut report = VerificationReport::default();
        for skipped in sandbox.skipped() {
            report.warn(format!("sandbox skipped: {skipped}"));
        }
        if sandbox.file_count() == 0 {
            report.warn("sandbox copied no files; verification cannot run");
        }
        let options = pf_adapters::ToolOptions {
            extra_args: Vec::new(),
            timeout_secs: Some(config.budget.command_timeout_secs),
            only_changed: false,
        };
        for spec in specs {
            let outcome = PropertyRunner::run(&sandbox, spec, &options)?;
            report.push(&outcome);
        }
        for spec in relations {
            let outcome = MetamorphicRunner::run(&sandbox, spec, &options)?;
            report.push_metamorphic(&outcome);
        }
        Ok(report)
    }

    /// 在目标项目上做一次变异测试（FR-037）。
    ///
    /// 与 [`Self::verify`] 的分工：后者验证「程序在这个输入下做了什么」，本方法
    /// 验证「测试套件能不能发现行为变化」。两者的结论指向不同的对象——一条被违反
    /// 的性质说明程序有问题，一个幸存变异体说明测试有缺口。因此它们的门禁语义也
    /// 不同，见 [`mutation::MutationOutcome`]。
    ///
    /// # Errors
    /// 目标不存在、不可读，或沙箱创建失败时返回 [`pf_core::Error`]。
    pub fn mutate(
        target: &std::path::Path,
        config: &MutationConfig,
        budget: &pf_core::config::Budget,
    ) -> pf_core::Result<MutationOutcome> {
        let options = pf_adapters::ToolOptions {
            extra_args: Vec::new(),
            timeout_secs: Some(budget.command_timeout_secs),
            only_changed: false,
        };
        MutationRunner::run(target, config, budget, &options)
    }
}
