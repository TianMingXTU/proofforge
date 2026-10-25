//! 策略选择（FR-045）。
//!
//! # 为什么策略要可重放
//!
//! 需求文档 FR-045 要求根据「程序结构和测试反馈动态调整验证任务」。这里不引入
//! 随机，而是用**确定规则**：当前状态、剩余预算、已完成任务的结果，三者决定下一
//! 个任务。这样同一份输入状态必然给出同一选择——这是 FR-049 审计轨迹与
//! FR-043 结果复现的前提。
//!
//! # 为什么没有「策略 trait」
//!
//! 当前只有一种策略实现：按计划顺序逐任务执行，并根据反馈调整优先级。没有第二
//! 种实现，因此不用 trait：AGENTS.md §4.5「不提前抽象」同样适用。

use serde::{Deserialize, Serialize};

use crate::budget::BudgetUsage;
use crate::plan::{VerificationPlan, VerificationTask};
use crate::state::AgentState;

/// 一次策略选择的结果：要执行哪个任务，以及为什么选它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyChoice {
    /// 被选中的任务（`None` 表示没有可执行任务）。
    pub task: Option<VerificationTask>,
    /// 选择依据（人类可读，进轨迹）。
    ///
    /// 不可省略：一个没有依据的选择与随机选中的没有区别。
    pub rationale: String,
    /// 是否因为预算耗尽而停止。
    pub stopped: bool,
}

/// 策略引擎：把当前状态与计划变成一个任务选择。
///
/// 当前实现完全由静态规则驱动：先跑计划里的最高优先级任务，执行后根据结果
/// 调整下一轮计划（反馈回路）。这满足阶段四「根据程序结构和测试反馈动态调整」
/// 的验收标准，不需要模型参与。
#[derive(Debug, Clone, Copy, Default)]
pub struct StrategyEngine;

impl StrategyEngine {
    /// 构造引擎实例。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// 从当前状态、计划与预算中选出下一个任务。
    ///
    /// # 规则
    ///
    /// 1. 当前状态不是活跃执行状态（`Created`、`Planning`、`Verifying`、`Evaluating`、
    ///    `Reporting`）时，不选任何任务，直接停止。
    /// 2. 预算已耗尽时停止，并在依据中说明耗尽的维度。
    /// 3. 否则从计划中取出最高优先级任务，执行它。
    ///
    /// # 为什么只有这三条
    ///
    /// 阶段四的验收要求是「根据结构和反馈动态调整」，不是「做出最优调度」：
    /// 调整发生在任务执行后反馈更新优先级那一步，而不是在策略选择时。策略
    /// 只负责「按调整后的顺序取下一个」，因此规则可以极简。
    #[must_use]
    pub fn choose(
        &self,
        state: AgentState,
        plan: &mut VerificationPlan,
        usage: BudgetUsage,
        budget: &crate::budget::ExecutionBudget,
    ) -> StrategyChoice {
        if !state.is_active() {
            return StrategyChoice {
                task: None,
                rationale: format!("当前状态 `{}` 不是活跃执行状态，停止", state.as_str()),
                stopped: true,
            };
        }

        // 检查预算耗尽（用配置上限比较已用量，而不是直接看 tracker 状态——
        // 这样策略函数可以在没有 tracker 引用时独立工作）。
        if let Some(dimension) = exhaustion_at(usage, budget) {
            return StrategyChoice {
                task: None,
                rationale: format!("预算已耗尽（维度：{}），停止执行", dimension.as_str()),
                stopped: true,
            };
        }

        if plan.is_empty() {
            return StrategyChoice {
                task: None,
                rationale: "计划中已无任务，停止".to_owned(),
                stopped: true,
            };
        }

        let task = plan.pop_next().expect("计划非空，pop_next 不会返回 None");
        StrategyChoice {
            task: Some(task.clone()),
            rationale: format!(
                "按优先级选择任务 `{}`（优先级 {}，依据：{}）",
                task.kind.key(),
                task.priority,
                task.rationale
            ),
            stopped: false,
        }
    }
}

/// 根据已用快照与配置限额判断是否已耗尽某维度。
///
/// 委托给 [`crate::budget::ExecutionBudget::exhausted_by`]：判定只能有一个入口，
/// 否则「循环说还能跑、策略说该停了」这种分歧会出现，而它极难定位。
fn exhaustion_at(
    usage: BudgetUsage,
    budget: &crate::budget::ExecutionBudget,
) -> Option<crate::budget::BudgetDimension> {
    budget.exhausted_by(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{BudgetUsage, ExecutionBudget};
    use crate::plan::{CandidateEvidence, PlanCandidate, StructuralPlanner};

    fn sample_plan() -> VerificationPlan {
        let planner = StructuralPlanner::default();
        planner.plan(&[PlanCandidate {
            target: "mycrate::parse".to_owned(),
            call_template: String::new(),
            domain: serde_json::Value::Null,
            invariant: String::new(),
            evidence: CandidateEvidence {
                rules: vec!["no-unwrap-in-library".to_owned()],
                severity: Some(pf_core::rule::RuleSeverity::Project),
                ..CandidateEvidence::default()
            },
        }])
    }

    #[test]
    fn a_non_active_state_stops_immediately() {
        let engine = StrategyEngine::new();
        let mut plan = sample_plan();
        let choice = engine.choose(
            AgentState::Failed,
            &mut plan,
            BudgetUsage::default(),
            &ExecutionBudget {
                wall_time_secs: Some(900),
                tool_calls: Some(200),
                subprocess_secs: Some(600),
                model_cost_micros: Some(1000),
            },
        );
        assert!(choice.stopped);
        assert!(choice.task.is_none());
        assert!(choice.rationale.contains("failed"));
    }

    #[test]
    fn a_budget_exhausted_state_stops_with_rationale() {
        let engine = StrategyEngine::new();
        let mut plan = sample_plan();
        let budget = ExecutionBudget {
            wall_time_secs: Some(0),
            tool_calls: Some(100),
            subprocess_secs: Some(100),
            model_cost_micros: Some(0),
        };
        let usage = BudgetUsage {
            wall_time_secs: 1,
            tool_calls: 0,
            subprocess_secs: 0,
            model_cost_micros: 0,
        };
        let choice = engine.choose(AgentState::Verifying, &mut plan, usage, &budget);
        assert!(choice.stopped);
        assert!(choice.task.is_none());
        assert!(choice.rationale.contains("耗尽"));
    }

    #[test]
    fn a_valid_active_state_picks_the_highest_priority_task() {
        let engine = StrategyEngine::new();
        let mut plan = sample_plan();
        let choice = engine.choose(
            AgentState::Verifying,
            &mut plan,
            BudgetUsage::default(),
            &ExecutionBudget {
                wall_time_secs: Some(900),
                tool_calls: Some(200),
                subprocess_secs: Some(600),
                model_cost_micros: Some(1000),
            },
        );
        assert!(!choice.stopped);
        assert!(choice.task.is_some());
        // 默认计划包含 baseline（run-existing-tests），其优先级最高，因此第一轮选中它；
        // 取出后计划仍有性质任务（非空）。
        assert!(!plan.is_empty());
    }

    #[test]
    fn an_empty_plan_stops() {
        let engine = StrategyEngine::new();
        let mut plan = VerificationPlan::new();
        let choice = engine.choose(
            AgentState::Verifying,
            &mut plan,
            BudgetUsage::default(),
            &ExecutionBudget {
                wall_time_secs: Some(900),
                tool_calls: Some(200),
                subprocess_secs: Some(600),
                model_cost_micros: Some(1000),
            },
        );
        assert!(choice.stopped);
        assert!(choice.task.is_none());
    }

    #[test]
    fn every_choice_carries_a_rationale() {
        // 说不出理由的选择在审计时与随机选中的没有区别（FR-045 / FR-049）。
        let engine = StrategyEngine::new();
        let mut plan = sample_plan();
        let choice = engine.choose(
            AgentState::Planning,
            &mut plan,
            BudgetUsage::default(),
            &ExecutionBudget {
                wall_time_secs: Some(900),
                tool_calls: Some(200),
                subprocess_secs: Some(600),
                model_cost_micros: Some(1000),
            },
        );
        assert!(
            !choice.rationale.trim().is_empty(),
            "说不出理由的选择与随机选中没有区别"
        );
    }

    #[test]
    fn budget_wall_time_is_checked_against_usage() {
        // 预算上限严格为 0 秒时，任何已用时间都触发耗尽。
        let budget = ExecutionBudget {
            wall_time_secs: Some(0),
            tool_calls: Some(200),
            subprocess_secs: Some(600),
            model_cost_micros: Some(1000),
        };
        assert!(exhaustion_at(
            BudgetUsage {
                wall_time_secs: 0,
                ..Default::default()
            },
            &budget
        )
        .is_some());
        assert!(exhaustion_at(
            BudgetUsage {
                wall_time_secs: 5,
                ..Default::default()
            },
            &budget
        )
        .is_some());
    }

    #[test]
    fn budget_tool_calls_is_checked_against_usage() {
        let budget = ExecutionBudget {
            tool_calls: Some(3),
            model_cost_micros: Some(1000),
            ..Default::default()
        };
        assert!(exhaustion_at(
            BudgetUsage {
                tool_calls: 2,
                ..Default::default()
            },
            &budget
        )
        .is_none());
        assert!(exhaustion_at(
            BudgetUsage {
                tool_calls: 3,
                ..Default::default()
            },
            &budget
        )
        .is_some());
    }
}
