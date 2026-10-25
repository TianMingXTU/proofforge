//! Agent 执行循环（FR-049）。
//!
//! # 为什么只有一个循环函数
//!
//! 整个阶段四要求「根据程序结构和测试反馈动态调整验证任务」。调整发生在
//! `plan`（结构生成计划）与 `feedback`（结果调整计划）两层，而实际执行只是一层
//! 确定的循环：取任务 → 执行 → 记录反馈 → 重复。这意味着整个 Agent 只需要一个
//! `run()` 函数，不需要多层抽象。
//!
//! # 为什么不暴露「中途停止」的公共方法
//!
//! 停止是状态机转移到终态的结果，而不是调用方主动调用的方法。这样「为什么
//! 停」只能从状态机和轨迹中读出，无法被外部代码「悄悄取消」。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::budget::{BudgetTracker, ExecutionBudget, StopReason};
use crate::feedback::FeedbackEngine;
use crate::plan::{StructuralPlanner, VerificationPlan, VerificationTask};
use crate::state::{AgentState, StateMachine};
use crate::strategy::StrategyEngine;
use crate::tool::{Permission, ToolConfidence, ToolOutput};
use crate::tools::ToolRegistry;
use crate::trace::{ExecutionTrace, TraceEntry, TraceKind};

/// 一次 Agent 会话的完整执行记录。
///
/// 包含状态转移历史、任务执行结果、预算用量与最终结论。设计目标是可序列化、
/// 可重放、可审计：同一份输入配置必然产出同一份执行记录（无随机、无时钟依赖）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSession {
    /// 会话标识（由调用方提供）。
    pub session_id: String,
    /// 最终状态。
    pub final_state: AgentState,
    /// 停止原因（若已到终态）。
    pub stop_reason: Option<StopReason>,
    /// 执行轨迹（逐条记录）。
    pub trace: ExecutionTrace,
    /// 执行过的任务结果（任务标识 → 工具输出）。
    pub completed_tasks: BTreeMap<String, ToolOutput>,
    /// 最终预算用量快照。
    pub budget_usage: crate::budget::BudgetUsage,
}

impl AgentSession {
    /// 会话的门禁结论。
    ///
    /// 判定顺序与 `pf-verify` 的 [`pf_verify::VerificationReport::gate`] 一致：
    /// **已经复现的缺陷优先**。一个真实观测到的违反不是「不可靠的证据」，它是
    /// 直接观察；此时再宣称「证据不足」会把已知的缺陷藏起来。
    ///
    /// 会话本身没跑完（预算耗尽、取消）时返回 `InsufficientEvidence`，即使前面
    /// 已完成的任务全都通过——那是 AGENTS.md 安全红线 4：没做完不是通过。
    #[must_use]
    pub fn gate(&self) -> pf_core::status::GateStatus {
        if !self.final_state.is_success() {
            return pf_core::status::GateStatus::InsufficientEvidence;
        }
        if self.found_violation() {
            return pf_core::status::GateStatus::Failed;
        }
        if self.has_inconclusive() {
            return pf_core::status::GateStatus::InsufficientEvidence;
        }
        pf_core::status::GateStatus::Passed
    }

    /// 是否有任务复现了不变式违反。
    #[must_use]
    pub fn found_violation(&self) -> bool {
        self.completed_tasks
            .values()
            .any(|output| output.payload.get("violated").and_then(Value::as_bool) == Some(true))
    }

    /// 是否有任务没有得到结论。
    #[must_use]
    pub fn has_inconclusive(&self) -> bool {
        self.completed_tasks
            .values()
            .any(|output| output.payload.get("conclusive").and_then(Value::as_bool) == Some(false))
    }

    /// 逐条任务的结论摘要，供报告层直接打印。
    #[must_use]
    pub fn task_summaries(&self) -> Vec<(String, String)> {
        self.completed_tasks
            .iter()
            .map(|(key, output)| (key.clone(), output.summary.clone()))
            .collect()
    }
}

/// Agent 运行时配置。
///
/// 只包含可序列化、可重放的数据：不含时钟、不含随机源、不含外部连接状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    /// 执行预算（墙钟、工具次数、子进程秒数、模型费用）。
    pub budget: ExecutionBudget,
    /// 是否包含「跑现有测试」任务。
    pub include_existing_tests: bool,
    /// 每目标最多生成几条性质。
    pub max_properties_per_target: usize,
    /// 是否对高优先级目标追加模糊测试。
    pub fuzz_high_priority_targets: bool,
    /// 会话 ID（用于轨迹与审计）。
    pub session_id: String,
    /// 被验证的项目根目录。
    ///
    /// `None` 表示没有任何可执行的目标。此时每个任务都记「证据不足」而不是
    /// 「通过」：一份没有跑过的验证不能给出绿灯（AGENTS.md 安全红线 4）。
    pub target: Option<std::path::PathBuf>,
}

/// 执行一次 Agent 会话。
///
/// # 步骤
///
/// 1. 初始化状态机（`Created` → `ContextReady` → `Analyzing` → `Planning`）。
/// 2. 生成验证计划（结构化计划器）。
/// 3. 进入执行循环：取任务 → 执行 → 反馈更新 → 重复，直到计划完成或预算耗尽。
/// 4. 根据最终状态生成结论（`StopReason` 与 `AgentState`）。
///
/// # 为什么不在这里处理人工批准
///
/// `WaitingApproval` 状态的处理由外部协调：本函数在遇到需要批准的任务时把
/// 状态切到 `WaitingApproval` 并立即停止，外部流程在获得批准后重新调用
/// `run()`（传入已调整的计划与状态）。这样「人工批准」与「自动执行」在
/// 同一套状态机里统一表达，不需要两套逻辑。
///
/// # Errors
///
/// 计划生成或状态转移失败时返回错误（这些是编程错误，不是执行失败——执行
/// 失败由状态机的终态表达）。
pub fn run(
    candidates: &[crate::plan::PlanCandidate],
    config: &AgentConfig,
) -> Result<AgentSession, AgentRunError> {
    let mut machine = StateMachine::new();
    let mut trace = ExecutionTrace::new();
    let mut plan = prepare(&mut machine, &mut trace, candidates, config)?;

    let mut budget_tracker = BudgetTracker::new(config.budget.clone());
    let mut completed_tasks: BTreeMap<String, ToolOutput> = BTreeMap::new();
    execute_loop(
        &mut machine,
        &mut trace,
        &mut plan,
        config,
        &mut budget_tracker,
        &mut completed_tasks,
    );

    let final_state = machine.current();
    let stop_reason = conclude(&mut trace, final_state, &completed_tasks, &budget_tracker);

    Ok(AgentSession {
        session_id: config.session_id.clone(),
        final_state,
        stop_reason: Some(stop_reason),
        trace,
        completed_tasks,
        budget_usage: budget_tracker.usage(),
    })
}

/// 初始化阶段：把状态机推进到 `Planning` 并生成验证计划。
///
/// 单独拆出来是因为这一步的失败模式与执行阶段完全不同：这里是**编程错误**
/// （状态机转移写错），执行阶段是**运行结果**（预算耗尽、测试失败）。
fn prepare(
    machine: &mut StateMachine,
    trace: &mut ExecutionTrace,
    candidates: &[crate::plan::PlanCandidate],
    config: &AgentConfig,
) -> Result<VerificationPlan, AgentRunError> {
    machine.context_ready()?;
    trace.record(TraceEntry {
        kind: TraceKind::StateChange,
        message: "会话开始，状态转为 ContextReady".to_owned(),
        detail: None,
        usage: crate::budget::BudgetUsage::default(),
        state: machine.current(),
    });

    machine.analyzing()?;
    trace.record(TraceEntry {
        kind: TraceKind::StateChange,
        message: "进入分析阶段".to_owned(),
        detail: None,
        usage: crate::budget::BudgetUsage::default(),
        state: machine.current(),
    });

    machine.planning()?;
    trace.record(TraceEntry {
        kind: TraceKind::StateChange,
        message: "进入计划阶段".to_owned(),
        detail: None,
        usage: crate::budget::BudgetUsage::default(),
        state: machine.current(),
    });

    let planner = StructuralPlanner {
        include_existing_tests: config.include_existing_tests,
        max_properties_per_target: config.max_properties_per_target,
        fuzz_high_priority_targets: config.fuzz_high_priority_targets,
    };
    let plan = planner.plan(candidates);
    trace.record(TraceEntry {
        kind: TraceKind::StrategyChoice,
        message: format!("计划已生成，包含 {} 条任务", plan.len()),
        detail: Some(serde_json::to_value(&plan).unwrap_or(Value::Null)),
        usage: crate::budget::BudgetUsage::default(),
        state: machine.current(),
    });

    machine.verifying()?;
    trace.record(TraceEntry {
        kind: TraceKind::StateChange,
        message: "进入验证执行阶段".to_owned(),
        detail: None,
        usage: crate::budget::BudgetUsage::default(),
        state: machine.current(),
    });
    Ok(plan)
}

/// 执行循环：取任务 → 执行 → 记录反馈 → 重复，直到计划完成或预算耗尽。
///
/// 循环体里唯一「不直观」的地方是反馈应用后不切回 `Evaluating`：状态机不
/// 允许 评估 → 验证 的转移，而计划已在原地调整，下一轮直接取下一个任务即可。
/// 这条约束记录在 [`crate::state`] 的文档里。
fn execute_loop(
    machine: &mut StateMachine,
    trace: &mut ExecutionTrace,
    plan: &mut VerificationPlan,
    config: &AgentConfig,
    budget_tracker: &mut BudgetTracker,
    completed_tasks: &mut BTreeMap<String, ToolOutput>,
) {
    let strategy_engine = StrategyEngine::new();
    let feedback_engine = FeedbackEngine::new();

    loop {
        if budget_tracker.is_exhausted() {
            trace.record(TraceEntry {
                kind: TraceKind::BudgetCheck,
                message: format!(
                    "预算耗尽（维度：{}）",
                    budget_tracker
                        .exhaustion()
                        .map_or("unknown", |dimension| dimension.as_str())
                ),
                detail: Some(serde_json::to_value(budget_tracker.usage()).unwrap_or_default()),
                usage: budget_tracker.usage(),
                state: machine.current(),
            });
            // 预算耗尽不是「失败」：它是预定的停止条件，因此转移到专门的终态。
            let _ = machine.abort(AgentState::BudgetExhausted);
            break;
        }

        let choice = strategy_engine.choose(
            machine.current(),
            plan,
            budget_tracker.usage(),
            &config.budget,
        );
        trace.record(TraceEntry {
            kind: TraceKind::StrategyChoice,
            message: choice.rationale.clone(),
            detail: Some(serde_json::json!({
                "stopped": choice.stopped,
                "task_key": choice.task.as_ref().map(|task| task.kind.key()),
            })),
            usage: budget_tracker.usage(),
            state: machine.current(),
        });

        if choice.stopped {
            if choice.task.is_none() {
                // 计划完成：进入评估、报告，然后正常结束。
                let _ = machine.evaluating();
                let _ = machine.reporting();
                let _ = machine.complete();
                for message in ["计划任务全部完成，进入评估", "进入报告阶段", "会话正常完成"]
                {
                    trace.record(TraceEntry {
                        kind: TraceKind::StateChange,
                        message: message.to_owned(),
                        detail: None,
                        usage: budget_tracker.usage(),
                        state: machine.current(),
                    });
                }
            }
            break;
        }

        let Some(task) = choice.task else {
            break;
        };
        let output = execute_task(&task, config);
        completed_tasks.insert(task.kind.key(), output.clone());
        budget_tracker.record_call(0);

        trace.record(TraceEntry {
            kind: TraceKind::ToolCall,
            // 结论写进 message 而不只留在 detail：轨迹是给人看的审计记录，
            // 「跑了哪个任务」和「跑出来什么」必须出现在同一处。
            message: format!("执行任务 `{}`：{}", task.kind.key(), output.summary),
            detail: Some(serde_json::to_value(&output).unwrap_or_default()),
            usage: budget_tracker.usage(),
            state: machine.current(),
        });

        let updates = feedback_engine.apply(completed_tasks, plan);
        for update in &updates {
            trace.record(TraceEntry {
                kind: TraceKind::FeedbackUpdate,
                message: update.rationale.clone(),
                detail: Some(serde_json::to_value(update).unwrap_or_default()),
                usage: budget_tracker.usage(),
                state: machine.current(),
            });
        }
        trace.record(TraceEntry {
            kind: TraceKind::FeedbackUpdate,
            message: format!("反馈应用完成，调整 {} 条任务", updates.len()),
            detail: Some(serde_json::json!({
                "updates_applied": updates.len(),
                "completed_so_far": completed_tasks.len(),
            })),
            usage: budget_tracker.usage(),
            state: machine.current(),
        });
    }
}

/// 收尾：根据最终状态算出停止原因，并写入轨迹。
fn conclude(
    trace: &mut ExecutionTrace,
    final_state: AgentState,
    completed_tasks: &BTreeMap<String, ToolOutput>,
    budget_tracker: &BudgetTracker,
) -> StopReason {
    let stop_reason = if final_state.is_success() {
        StopReason::PlanCompleted
    } else if matches!(final_state, AgentState::BudgetExhausted) {
        StopReason::BudgetExhausted(
            budget_tracker
                .exhaustion()
                .unwrap_or(crate::budget::BudgetDimension::WallTime),
        )
    } else if final_state.is_terminal() {
        StopReason::Failed {
            reason: format!("会话在非成功终态 `{}` 结束", final_state.as_str()),
        }
    } else {
        StopReason::Failed {
            reason: format!(
                "会话在非终态 `{}` 停止（可能被外部取消）",
                final_state.as_str()
            ),
        }
    };

    trace.record(TraceEntry {
        kind: TraceKind::SessionEnd,
        message: format!(
            "会话结束，状态：{}，已完成任务：{}",
            final_state.as_str(),
            completed_tasks.len()
        ),
        detail: Some(serde_json::json!({
            "final_state": final_state.as_str(),
            "completed_tasks": completed_tasks.len(),
            "stop_reason": match &stop_reason {
                StopReason::PlanCompleted => "plan_completed",
                StopReason::BudgetExhausted(_) => "budget_exhausted",
                StopReason::Cancelled => "cancelled",
                StopReason::GoalReached { goal } => goal.as_str(),
                StopReason::Failed { reason } => reason.as_str(),
            },
        })),
        usage: budget_tracker.usage(),
        state: final_state,
    });
    stop_reason
}

/// 真实执行一个任务。
///
/// 每个任务都翻译成一次**带权限检查**的工具调用（见
/// [`crate::tools::ToolRegistry::dispatch`]），由具体工具落到 `pf-verify` 的真实
/// 执行上：跑项目自带测试套件、在沙箱里编译并运行性质驱动程序、或做反馈驱动的
/// 模糊测试。全部外部执行都经 `pf-adapters` 生成命令，原仓库只读。
///
/// # 为什么没有「模拟」分支
///
/// 曾经的实现按任务类型编造结果（`tests_passed: true`、`violated: false`），
/// 并以 `ToolConfidence::Observed` 标记。那是把猜测说成观测——红线 5 禁止的事，
/// 而且它让「测试反馈调整计划」这条核心能力退化成按字符串内容猜答案。因此这里
/// 只有两条路：**真实跑一遍**，或者**明确说没跑**（`conclusive: false` + 原因）。
fn execute_task(task: &VerificationTask, config: &AgentConfig) -> ToolOutput {
    let Some(target) = config.target.as_deref() else {
        return inconclusive(
            task,
            "没有配置目标项目路径，因此这个任务没有执行；未执行的验证不能算通过",
        );
    };
    let Some((tool, input)) = tool_call(task, target, config) else {
        // 候选来自静态发现而不是显式性质：没有调用模板与输入域就生成不了驱动
        // 程序。这是「这个候选不可执行」，不是「它通过了」。
        return inconclusive(
            task,
            "候选没有可执行的调用模板或输入域（它来自静态发现，不是显式性质），\
             因此无法生成驱动程序；请用 --properties 提供显式规格",
        );
    };
    match ToolRegistry.dispatch(&tool, &input, Permission::Execute) {
        Ok(mut output) => {
            // 任务标识进 payload：报告层与轨迹都按任务键索引，输出自身不带键的
            // 话，两个同名任务的结果就分不开了。
            if let Some(map) = output.payload.as_object_mut() {
                map.insert("task".to_owned(), Value::from(task.kind.key()));
            }
            output
        }
        Err(reason) => inconclusive(task, &format!("工具 `{tool}` 没能给出结论：{reason}")),
    }
}

/// 把一个任务翻译成 `(工具名, 工具输入)`。
///
/// 返回 `None` 表示这个任务**不可执行**：调用模板或输入域缺失。此时绝不能挑一个
/// 工具去跑——那只会得到一份与目标无关的报告。
fn tool_call(
    task: &VerificationTask,
    target: &Path,
    config: &AgentConfig,
) -> Option<(String, Value)> {
    let target_path = target.to_string_lossy().to_string();
    let timeout = Some(config.budget.into_verify_budget().command_timeout_secs);
    match &task.kind {
        crate::plan::TaskKind::RunExistingTests => Some((
            "run_existing_tests".to_owned(),
            json!({
                "target_path": target_path,
                "timeout_secs": timeout,
            }),
        )),
        crate::plan::TaskKind::VerifyProperty {
            call_template,
            domain,
            invariant,
            ..
        }
        | crate::plan::TaskKind::Fuzz {
            call_template,
            domain,
            invariant,
            ..
        } => {
            if call_template.is_empty() || invariant.is_empty() || !domain.is_object() {
                return None;
            }
            let name = match &task.kind {
                crate::plan::TaskKind::Fuzz { .. } => "fuzz_property",
                _ => "verify_property",
            };
            Some((
                name.to_owned(),
                json!({
                    "target_path": target_path,
                    "name": task.kind.key(),
                    "call_template": call_template,
                    "domain": domain,
                    "invariant": invariant,
                    "samples": config.max_properties_per_target.max(1) * 8,
                    "timeout_secs": timeout,
                }),
            ))
        }
    }
}

/// 构造一条「没有结论」的工具输出。
///
/// 与 [`ToolConfidence::Observed` 的区别必须显式：观测是「我们看见了」，
/// 推断是「我们猜的」。没有执行时两者都不是，因此用 `Inferred` 并说清原因。
fn inconclusive(task: &VerificationTask, reason: &str) -> ToolOutput {
    ToolOutput {
        payload: json!({
            "task": task.kind.key(),
            "conclusive": false,
            "reason": reason,
        }),
        summary: format!("任务 `{}` 没有得到结论：{}", task.kind.key(), reason),
        confidence: ToolConfidence::Inferred,
        side_effects: Vec::new(),
    }
}

/// Agent 执行失败的原因。
#[derive(Debug, thiserror::Error, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AgentRunError {
    /// 状态机非法转移（编程错误，不是执行失败）。
    #[error("illegal state transition: {from} -> {to}")]
    IllegalTransition {
        /// 迁移前的状态。
        from: String,
        /// 试图迁往的状态。
        to: String,
    },
    /// 计划生成失败。
    #[error("plan generation failed: {reason}")]
    PlanFailed {
        /// 失败原因。
        reason: String,
    },
}

impl From<crate::state::IllegalTransition> for AgentRunError {
    fn from(err: crate::state::IllegalTransition) -> Self {
        Self::IllegalTransition {
            from: err.from.as_str().to_owned(),
            to: err.to.as_str().to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{CandidateEvidence, PlanCandidate};

    fn dummy_candidates() -> Vec<PlanCandidate> {
        vec![PlanCandidate {
            target: "mod::parse".to_owned(),
            call_template: "mod::parse({args})".to_owned(),
            domain: serde_json::json!({"kind": "int", "min": -5, "max": 5}),
            invariant: "output >= 0".to_owned(),
            evidence: CandidateEvidence {
                rules: vec!["test-rule".to_owned()],
                severity: Some(pf_core::rule::RuleSeverity::Advisory),
                ..CandidateEvidence::default()
            },
        }]
    }

    #[test]
    fn the_agent_completes_a_simple_session() {
        let config = AgentConfig {
            budget: ExecutionBudget::default(),
            include_existing_tests: false,
            max_properties_per_target: 1,
            fuzz_high_priority_targets: false,
            session_id: "test-session-001".to_owned(),
            // 不设目标：每个任务都必须报「没有结论」而不是编一个通过。
            target: None,
        };
        let result = run(&dummy_candidates(), &config);
        assert!(result.is_ok(), "{result:?}");
        let session = result.expect("session");
        assert_eq!(session.session_id, "test-session-001");
        assert!(session.final_state.is_terminal());
        assert!(!session.trace.is_empty());
    }

    #[test]
    fn the_agent_records_a_budget_check_in_trace() {
        let config = AgentConfig {
            budget: ExecutionBudget {
                wall_time_secs: Some(0),
                ..ExecutionBudget::default()
            },
            ..AgentConfig::default()
        };
        let result = run(&dummy_candidates(), &config);
        assert!(result.is_ok());
        let session = result.expect("session");
        // 预算为 0 时第一轮循环就必须记账：没有这一条，说明预算检查被跳过了。
        assert!(
            session.trace.count(TraceKind::BudgetCheck) >= 1,
            "预算为 0 时应记录预算检查：{:?}",
            session.trace.entries()
        );
    }

    #[test]
    fn the_agent_never_allows_shell_access() {
        // 红线 1：没有任何工具调用生成任意命令。这条测试只检查模拟执行的
        // 结果不包含「执行任意字符串」的副作用类型。
        let config = AgentConfig::default();
        let session = run(&dummy_candidates(), &config).expect("session");
        for entry in session.trace.entries() {
            let detail_str = format!("{:?}", entry.detail);
            assert!(
                !detail_str.contains("shell"),
                "轨迹中不应出现 shell 执行：{}",
                entry.message
            );
        }
    }

    #[test]
    fn incomplete_execution_is_not_reported_as_complete() {
        // 红线 4：预算耗尽时状态应为非成功终态。
        let config = AgentConfig {
            budget: ExecutionBudget {
                wall_time_secs: Some(0),
                ..ExecutionBudget::default()
            },
            ..AgentConfig::default()
        };
        let session = run(&dummy_candidates(), &config).expect("session");
        assert!(!session.final_state.is_success());
        assert!(
            session
                .stop_reason
                .as_ref()
                .is_some_and(|r| !r.is_complete()),
            "预算耗尽时不应报告完整结论"
        );
    }

    #[test]
    fn feedback_updates_are_recorded_in_trace() {
        let config = AgentConfig {
            budget: ExecutionBudget {
                wall_time_secs: Some(3600),
                tool_calls: Some(50),
                subprocess_secs: Some(600),
                model_cost_micros: Some(1000),
            },
            session_id: "feedback-test".to_owned(),
            ..AgentConfig::default()
        };
        let session = run(&dummy_candidates(), &config).expect("session");
        // 反馈记录应存在（无论是否有实际调整，轨迹应记录反馈阶段活动）。
        assert!(
            session
                .trace
                .entries()
                .iter()
                .any(|e| e.kind == TraceKind::FeedbackUpdate),
            "应记录反馈更新阶段活动（调整数：{})",
            session
                .trace
                .entries()
                .iter()
                .filter(|e| e.kind == TraceKind::FeedbackUpdate)
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    #[test]
    fn the_agent_session_is_serializable() {
        let config = AgentConfig::default();
        let session = run(&dummy_candidates(), &config).expect("session");
        let json = serde_json::to_string(&session).expect("serialize session");
        assert!(!json.is_empty(), "序列化结果不应为空");
        assert!(json.contains("final_state"));
    }

    /// 造一个带已知缺陷的最小 Rust 项目，返回其临时目录。
    ///
    /// `parse` 在非正数上 panic，因此「output < 100usize」这条性质必然被复现。
    fn fixture_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"agent_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .expect("write Cargo.toml");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir src");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "//! fixture\n\npub fn parse(value: i64) -> usize {\n    value.try_into().unwrap()\n}\n",
        )
        .expect("write lib.rs");
        dir
    }

    #[test]
    fn the_agent_really_runs_the_property_and_reports_the_violation() {
        // 阶段四的验收要求是「根据程序结构和测试反馈动态调整验证任务」。
        // 反馈必须来自真实执行：这一条测试把一条必然违反的性质交给 Agent，
        // 断言它观测到违反、门禁返回未通过、退出码对应 1。
        let dir = fixture_project();
        let config = AgentConfig {
            budget: ExecutionBudget {
                wall_time_secs: Some(600),
                ..ExecutionBudget::default()
            },
            include_existing_tests: true,
            max_properties_per_target: 1,
            fuzz_high_priority_targets: false,
            session_id: "real-run".to_owned(),
            target: Some(dir.path().to_path_buf()),
        };
        let candidates = vec![PlanCandidate {
            target: "agent_fixture::parse".to_owned(),
            call_template: "agent_fixture::parse({args})".to_owned(),
            domain: serde_json::json!({"kind": "int", "min": -20, "max": 20}),
            invariant: "output < 100usize".to_owned(),
            evidence: CandidateEvidence {
                rules: vec!["property-spec".to_owned()],
                ..CandidateEvidence::default()
            },
        }];

        let session = run(&candidates, &config).expect("session");

        assert!(session.found_violation(), "必须真的观测到违反：{session:?}");
        assert_eq!(session.gate(), pf_core::status::GateStatus::Failed);
        assert_eq!(session.gate().exit_code(), 1);
        // 轨迹里必须留下「跑出来什么」，否则这次会话不可审计（FR-049）。
        assert!(
            session
                .trace
                .entries()
                .iter()
                .any(|entry| entry.message.contains("已复现违反")),
            "{:?}",
            session.trace.entries()
        );
    }

    #[test]
    fn a_candidate_without_an_executable_domain_is_inconclusive_not_passing() {
        // 静态发现给出的候选没有调用模板与输入域。它不能被验证，因此必须报
        // 「没有结论」——报 `violated: false` 等于伪造一条通过（红线 4 / 5）。
        let dir = fixture_project();
        let config = AgentConfig {
            include_existing_tests: false,
            session_id: "static-only".to_owned(),
            target: Some(dir.path().to_path_buf()),
            ..AgentConfig::default()
        };
        let candidates = vec![PlanCandidate {
            target: "src/lib.rs".to_owned(),
            call_template: String::new(),
            domain: Value::Null,
            invariant: "库代码中使用了 unwrap()".to_owned(),
            evidence: CandidateEvidence::default(),
        }];

        let session = run(&candidates, &config).expect("session");

        assert!(session.has_inconclusive(), "{session:?}");
        assert!(!session.found_violation());
        assert_eq!(
            session.gate(),
            pf_core::status::GateStatus::InsufficientEvidence
        );
        assert_eq!(session.gate().exit_code(), 2);
    }
}
