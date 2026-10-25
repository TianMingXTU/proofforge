//! 具体工具实现（需求文档 8.4「可调用工具集」）。
//!
//! # 为什么工具由类型而不是字符串标识
//!
//! 每个工具是一个实现了 [`crate::tool::Tool`] 的具体类型（如 [`RunExistingTestsTool`]），
//! 而不是一个字符串名：这样「选哪个工具」在类型系统里就是显式的，不需要运行时
//! 字符串解析。工具名（`name()`）只是审计标识，不是调度依据。
//!
//! # 为什么没有「任意命令」工具
//!
//! 红线 1：所有外部执行都经 `pf-adapters::ToolAdapter` 生成命令记录。这里的工具
//! 只能选择「哪个适配器的哪个预定义操作」，不能拼接任意命令行字符串。
//!
//! # 每个工具都真的执行
//!
//! 工具的输出会被计划器当成**事实**排进下一轮，因此它不能是编造的。这里没有
//! 「模拟结果」分支：要么真实跑一遍并报告观测到的结论，要么明确说没有得到结论
//! （`conclusive: false` + 原因）。把猜测标成 `ToolConfidence::Observed` 是
//! AGENTS.md 安全红线 5 禁止的事。

use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::tool::{
    Permission, SideEffect, Tool, ToolConfidence, ToolError, ToolOutput, ToolSchema,
};

/// 工具输入里目标路径的字段名。
///
/// 抽成常量是因为三处要用：工具实现读它、runner 写它、测试构造它。
const TARGET_PATH: &str = "target_path";

/// 从工具输入里取出目标路径。
fn target_of(input: &Value) -> Result<&str, ToolError> {
    input
        .get(TARGET_PATH)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid_input("tool", format!("missing `{TARGET_PATH}`")))
}

/// 工具：执行目标项目自带的测试套件（对应 `TaskKind::RunExistingTests`）。
///
/// 经 `pf-verify::TestRunner` 在隔离工作区里跑，命令由 `pf-adapters` 生成。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunExistingTestsTool;

impl RunExistingTestsTool {
    /// 构造实例。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Tool for RunExistingTestsTool {
    fn name(&self) -> &'static str {
        "run_existing_tests"
    }

    fn description(&self) -> &'static str {
        "在目标项目上执行自带测试套件，返回通过/失败/不确定的结论"
    }

    fn input_schema(&self) -> ToolSchema {
        ToolSchema::object(
            &[(TARGET_PATH, "string"), ("timeout_secs", "integer")],
            &[TARGET_PATH],
        )
    }

    fn permission(&self) -> Permission {
        Permission::Execute
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::ProcessSpawn
    }

    fn suggested_timeout_secs(&self) -> u64 {
        300
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let target = target_of(input)?;
        let budget = budget_of(input);
        let run = pf_verify::TestRunner::run(Path::new(target), &budget)
            .map_err(|reason| ToolError::execution_failed(self.name(), reason.to_string()))?;
        let conclusive = run.status != pf_verify::test_runner::TestStatus::Inconclusive;
        Ok(ToolOutput {
            payload: json!({
                "conclusive": conclusive,
                "tests_passed": run.status == pf_verify::test_runner::TestStatus::Passed,
                "status": run.status,
                "failed_tests": run.failed_tests,
            }),
            summary: match run.status {
                pf_verify::test_runner::TestStatus::Passed => "现有测试全部通过".to_owned(),
                pf_verify::test_runner::TestStatus::Failed => {
                    format!("现有测试失败（{} 个）", run.failed_tests.len())
                }
                pf_verify::test_runner::TestStatus::Inconclusive => {
                    format!("没能得到结论：{}", first_error_line(&run.execution.stderr))
                }
            },
            confidence: ToolConfidence::Observed,
            side_effects: vec!["process_spawn".to_owned()],
        })
    }
}

/// 工具：验证一条性质（对应 `TaskKind::VerifyProperty`）。
///
/// 在沙箱里生成驱动程序、编译、执行、解读完成标记，必要时缩减反例。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerifyPropertyTool;

impl VerifyPropertyTool {
    /// 构造实例。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Tool for VerifyPropertyTool {
    fn name(&self) -> &'static str {
        "verify_property"
    }

    fn description(&self) -> &'static str {
        "在沙箱中验证一条性质：生成驱动、执行、解读标记，返回满足或违反"
    }

    fn input_schema(&self) -> ToolSchema {
        ToolSchema::object(
            &[
                (TARGET_PATH, "string"),
                ("call_template", "string"),
                ("domain", "object"),
                ("invariant", "string"),
                ("samples", "integer"),
            ],
            &[TARGET_PATH, "call_template", "domain", "invariant"],
        )
    }

    fn permission(&self) -> Permission {
        Permission::Execute
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::ProcessSpawn
    }

    fn suggested_timeout_secs(&self) -> u64 {
        120
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let target = target_of(input)?;
        let spec = spec_of(input, self.name())?;
        let sandbox = pf_verify::Sandbox::isolate(Path::new(target), &budget_of(input))
            .map_err(|reason| ToolError::execution_failed(self.name(), reason.to_string()))?;
        let outcome = pf_verify::PropertyRunner::run(
            &sandbox,
            &spec,
            &pf_adapters::ToolOptions {
                timeout_secs: Some(
                    input
                        .get("timeout_secs")
                        .and_then(Value::as_u64)
                        .unwrap_or(120),
                ),
                ..pf_adapters::ToolOptions::default()
            },
        )
        .map_err(|reason| ToolError::execution_failed(self.name(), reason.to_string()))?;
        Ok(describe_property(&outcome))
    }
}

/// 工具：对一个目标做反馈驱动的模糊测试。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FuzzPropertyTool;

impl FuzzPropertyTool {
    /// 构造实例。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Tool for FuzzPropertyTool {
    fn name(&self) -> &'static str {
        "fuzz_property"
    }

    fn description(&self) -> &'static str {
        "对一条性质做反馈驱动的模糊测试，报告观察到的行为类别与反例"
    }

    fn input_schema(&self) -> ToolSchema {
        ToolSchema::object(
            &[
                (TARGET_PATH, "string"),
                ("call_template", "string"),
                ("domain", "object"),
                ("invariant", "string"),
            ],
            &[TARGET_PATH, "call_template", "domain", "invariant"],
        )
    }

    fn permission(&self) -> Permission {
        Permission::Execute
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::ProcessSpawn
    }

    fn suggested_timeout_secs(&self) -> u64 {
        300
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let target = target_of(input)?;
        let spec = spec_of(input, self.name())?;
        let sandbox = pf_verify::Sandbox::isolate(Path::new(target), &budget_of(input))
            .map_err(|reason| ToolError::execution_failed(self.name(), reason.to_string()))?;
        let outcome = pf_verify::FuzzRunner::run(
            &sandbox,
            &pf_verify::FuzzConfig::new(spec),
            &pf_adapters::ToolOptions::default(),
        )
        .map_err(|reason| ToolError::execution_failed(self.name(), reason.to_string()))?;
        Ok(ToolOutput {
            payload: json!({
                "conclusive": outcome.is_conclusive(),
                "violated": outcome.found_violation(),
                "crashed": outcome.found_crash(),
                "explored": outcome.explored,
                "counterexamples": outcome.counterexamples.len(),
                "crashers": outcome.crashers.len(),
            }),
            summary: outcome.summary(),
            confidence: ToolConfidence::Observed,
            side_effects: vec!["sandbox_write".to_owned(), "process_spawn".to_owned()],
        })
    }
}

/// 工具：运行静态检查。
///
/// 调 `pf-review` 对目标路径做一次完整审查，返回发现列表。它只读，因此权限是
/// [`Permission::Read`]——这也是红线 3「默认只读」在工具层的落点。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunStaticCheckTool;

impl RunStaticCheckTool {
    /// 构造实例。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Tool for RunStaticCheckTool {
    fn name(&self) -> &'static str {
        "run_static_check"
    }

    fn description(&self) -> &'static str {
        "对目标路径运行静态规则引擎（pf-rules），返回发现列表"
    }

    fn input_schema(&self) -> ToolSchema {
        ToolSchema::object(
            &[(TARGET_PATH, "string"), ("rule_ids", "array")],
            &[TARGET_PATH],
        )
    }

    fn permission(&self) -> Permission {
        Permission::Read
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    fn suggested_timeout_secs(&self) -> u64 {
        60
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let target = target_of(input)?;
        let review = pf_review::ReviewEngine::review(&pf_core::config::Config::for_target(target))
            .map_err(|reason| ToolError::execution_failed(self.name(), reason.to_string()))?;
        let findings: Vec<Value> = review
            .findings
            .iter()
            .map(|finding| {
                json!({
                    "rule_id": finding.rule_id.get(),
                    "severity": finding.severity,
                    "status": finding.status,
                    "title": finding.title,
                })
            })
            .collect();
        let count = findings.len();
        Ok(ToolOutput {
            payload: json!({
                "target_path": target,
                "findings": findings,
            }),
            summary: format!("静态检查完成：{target}（{count} 条发现）"),
            confidence: ToolConfidence::Observed,
            side_effects: vec![],
        })
    }
}

/// 从工具输入里取出资源预算。
fn budget_of(input: &Value) -> pf_core::config::Budget {
    let mut budget = pf_core::config::Budget::default();
    if let Some(secs) = input.get("timeout_secs").and_then(Value::as_u64) {
        budget.command_timeout_secs = secs.max(1);
        budget.task_timeout_secs = secs.max(1);
    }
    budget
}

/// 从工具输入里组装性质规格。
///
/// 缺字段或域不合法时返回错误而不是猜一个：一个编出来的域会生成一个跑不对的
/// 驱动程序，那时看到的「违反」与目标程序无关。
fn spec_of(input: &Value, tool: &str) -> Result<pf_verify::PropertySpec, ToolError> {
    let missing = |field: &str| ToolError::invalid_input(tool, format!("missing `{field}`"));
    let call_template = input
        .get("call_template")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("call_template"))?
        .to_owned();
    let invariant = input
        .get("invariant")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("invariant"))?
        .to_owned();
    let domain: pf_verify::InputDomain = input
        .get("domain")
        .cloned()
        .ok_or_else(|| missing("domain"))
        .and_then(|value| {
            serde_json::from_value(value).map_err(|reason| {
                ToolError::invalid_input(tool, format!("`domain` 不是合法的输入域：{reason}"))
            })
        })?;
    if !domain.is_well_formed() {
        return Err(ToolError::invalid_input(
            tool,
            "`domain` 不是良定义的输入域",
        ));
    }
    Ok(pf_verify::PropertySpec {
        name: input.get("name").and_then(Value::as_str).map(str::to_owned),
        language: pf_verify::Language::Rust,
        imports: Vec::new(),
        call_template,
        domain,
        invariant,
        samples: input
            .get("samples")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(16),
        seed: 1,
    })
}

/// 把性质验证结果翻译成工具输出。
fn describe_property(outcome: &pf_verify::PropertyOutcome) -> ToolOutput {
    let (conclusive, violated, summary) = match &outcome.result {
        pf_verify::PropertyResult::Satisfied { samples } => {
            (true, false, format!("{samples} 个样本均未观察到违反"))
        }
        pf_verify::PropertyResult::Violated => {
            let example = outcome
                .counterexample
                .as_ref()
                .map_or_else(|| "未知输入".to_owned(), |example| example.input.describe());
            let (from, to) = outcome.counterexample.as_ref().map_or((0, 0), |example| {
                (
                    example.reduction.original_size,
                    example.reduction.final_size,
                )
            });
            (
                true,
                true,
                format!("已复现违反，反例 {example}（规模 {from} → {to}）"),
            )
        }
        pf_verify::PropertyResult::Inconclusive(reason) => {
            (false, false, format!("证据不足：{reason}"))
        }
    };
    ToolOutput {
        payload: json!({
            "conclusive": conclusive,
            "violated": violated,
            "counterexample": outcome
                .counterexample
                .as_ref()
                .map(|example| example.input.describe()),
        }),
        summary,
        confidence: ToolConfidence::Observed,
        side_effects: vec!["sandbox_write".to_owned(), "process_spawn".to_owned()],
    }
}

/// 取编译器输出的第一行 `error`，用于说明「为什么没跑起来」。
fn first_error_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with("error"))
        .unwrap_or("未给出原因")
        .chars()
        .take(200)
        .collect()
}

/// 工具注册表：把所有可用工具绑定到一个命名空间。
///
/// 注册表不持有外部状态（网络连接、文件句柄），因此可以安全地在多个线程间共享
/// （`Tool` trait 要求 `Send + Sync`）。
#[derive(Debug, Clone, Default)]
pub struct ToolRegistry;

impl ToolRegistry {
    /// 构造空注册表。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// 注册全部内置工具，返回 `(工具名 -> 工具实例)` 映射。
    #[must_use]
    pub fn register_all(&self) -> HashMap<&'static str, Box<dyn Tool>> {
        let mut map: HashMap<&'static str, Box<dyn Tool>> = HashMap::new();
        map.insert(
            "run_existing_tests",
            Box::new(RunExistingTestsTool::new()) as Box<dyn Tool>,
        );
        map.insert(
            "verify_property",
            Box::new(VerifyPropertyTool::new()) as Box<dyn Tool>,
        );
        map.insert(
            "fuzz_property",
            Box::new(FuzzPropertyTool::new()) as Box<dyn Tool>,
        );
        map.insert(
            "run_static_check",
            Box::new(RunStaticCheckTool::new()) as Box<dyn Tool>,
        );
        map
    }

    /// 按名字分发一次工具调用，**先检查权限**。
    ///
    /// 需求文档 9.3 要求权限由 Runtime 强制，而不是写在提示词里让模型自律。
    /// 因此检查发生在这里：模型（或计划器）只能选「哪个工具的哪组参数」，
    /// 不能因为输出里写着「忽略系统规则」就获得更高权限——仓库内容是不可信输入。
    ///
    /// # Errors
    ///
    /// 工具不存在、权限不足、输入不合法或执行失败时返回 [`ToolError`]。
    pub fn dispatch(
        &self,
        name: &str,
        input: &Value,
        granted: Permission,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self
            .register_all()
            .remove(name)
            .ok_or_else(|| ToolError::unknown_tool(name))?;
        if tool.permission() > granted {
            return Err(ToolError::permission_denied(
                name,
                granted,
                tool.permission(),
            ));
        }
        tool.call(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_tool_has_execute_permission() {
        let tool = RunExistingTestsTool::new();
        assert_eq!(tool.name(), "run_existing_tests");
        assert!(matches!(tool.permission(), Permission::Execute));
        assert!(matches!(tool.side_effect(), SideEffect::ProcessSpawn));
    }

    #[test]
    fn the_verify_tool_has_execute_permission() {
        let tool = VerifyPropertyTool::new();
        assert_eq!(tool.name(), "verify_property");
        assert!(matches!(tool.permission(), Permission::Execute));
    }

    #[test]
    fn the_static_check_tool_has_read_permission_only() {
        // 静态检查不修改任何内容，因此只需要读权限（红线 3：默认只读）。
        let tool = RunStaticCheckTool::new();
        assert_eq!(tool.name(), "run_static_check");
        assert!(matches!(tool.permission(), Permission::Read));
        assert!(matches!(tool.side_effect(), SideEffect::None));
    }

    #[test]
    fn the_registry_registers_all_four_tools() {
        let registry = ToolRegistry::new();
        let map = registry.register_all();
        assert!(map.contains_key("run_existing_tests"));
        assert!(map.contains_key("verify_property"));
        assert!(map.contains_key("fuzz_property"));
        assert!(map.contains_key("run_static_check"));
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn the_verify_tool_rejects_invalid_input() {
        let tool = VerifyPropertyTool::new();
        let bad_input = json!({"timeout_secs": 30});
        let result = tool.call(&bad_input);
        assert!(result.is_err(), "缺少 target_path 应被拒绝");
        assert!(
            format!("{:?}", result.unwrap_err()).contains("target_path"),
            "错误信息应指出缺失字段"
        );
    }

    #[test]
    fn a_malformed_domain_is_rejected_before_any_execution() {
        // 编一个域会生成跑不对的驱动程序，那时看到的「违反」与目标程序无关。
        let tool = VerifyPropertyTool::new();
        let input = json!({
            "target_path": ".",
            "call_template": "demo::parse({args})",
            "domain": {"kind": "int", "min": 10, "max": 1},
            "invariant": "output >= 0",
        });
        let error = tool.call(&input).expect_err("矛盾域必须被拒绝");
        assert!(format!("{error}").contains("domain"), "{error}");
    }

    #[test]
    fn dispatch_refuses_a_tool_above_the_granted_permission() {
        // 9.3：权限由 Runtime 强制。只授予 Read 时，跑测试的请求必须被拒。
        let registry = ToolRegistry::new();
        let input = json!({"target_path": "."});
        let error = registry
            .dispatch("run_existing_tests", &input, Permission::Read)
            .expect_err("Read 权限不足以跑测试");
        assert!(format!("{error}").contains("permission"), "{error}");
    }

    #[test]
    fn dispatch_allows_a_tool_within_the_granted_permission() {
        let registry = ToolRegistry::new();
        let input = json!({"target_path": "."});
        let result = registry.dispatch("run_static_check", &input, Permission::Read);
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn tool_outputs_never_claim_shell_access() {
        // 红线 1：没有工具执行任意命令。检查所有注册工具的描述和副作用，
        // 确保没有提到「执行任意字符串」或「shell」。
        let registry = ToolRegistry::new();
        let map = registry.register_all();
        for (name, tool) in &map {
            let desc = tool.description().to_lowercase();
            assert!(
                !desc.contains("shell"),
                "工具 `{name}` 的描述不应包含 shell 执行"
            );
            // 副作用必须在预定义的枚举内（没有「任意命令执行」这种开放类型）。
            let allowed = [
                SideEffect::None,
                SideEffect::SandboxWrite,
                SideEffect::ProcessSpawn,
                SideEffect::EvidenceAppend,
                SideEffect::RepositoryMutate,
            ];
            assert!(
                allowed.contains(&tool.side_effect()),
                "工具 `{name}` 的副作用必须在预定义集合内（没有任意命令执行）"
            );
        }
    }

    #[test]
    fn every_registered_tool_has_stable_name() {
        // 工具名进入轨迹与审计记录，不得随机生成。
        let registry = ToolRegistry::new();
        let map = registry.register_all();
        for name in map.keys() {
            assert!(!name.is_empty(), "工具名不能为空");
            assert!(!name.contains(' '), "工具名 `{name}` 不应含空格");
        }
    }
}
