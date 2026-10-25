//! 反馈更新（FR-047）。
//!
//! # 为什么反馈只修改计划，不直接执行
//!
//! 需求文档 FR-047 要求根据「测试反馈动态调整验证任务」。调整发生在**计划层**
//! 而不是执行层：一次工具执行（例如 `run_existing_tests`）的结果只改变计划中
//! 后续任务的优先级，而不直接启动新任务。这保证「每一轮只做一件事，做完再看
//! 反馈」——否则执行顺序与反馈顺序无法对应，轨迹无法审计。
//!
//! # 为什么没有随机
//!
//! 优先级调整是确定函数：根据已完成任务的结果（通过/失败/不确定）加或减固定
//! 数值。这让同一份初始状态必然产生同一份调整后的计划——FR-043 的前提。

use std::collections::BTreeMap;

use crate::plan::VerificationPlan;

use serde::{Deserialize, Serialize};

/// 一次反馈更新的记录：哪些任务被调整了、为什么。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackUpdate {
    /// 被调整的任务标识（任务类型的 `key()`）。
    pub task_key: String,
    /// 调整方向：正数表示提升优先级，负数表示降低。
    pub delta: i32,
    /// 依据：根据哪个已完成任务的结果做了调整。
    pub based_on: Option<String>,
    /// 人类可读说明。
    pub rationale: String,
}

/// 反馈引擎：根据已执行任务的结果调整计划优先级。
///
/// 当前实现完全确定：每种结果（通过、违反、崩溃、不确定）对应固定的优先级
/// 调整。这样同一份执行历史必然给出同一份调整后的计划。
#[derive(Debug, Clone, Copy, Default)]
pub struct FeedbackEngine;

impl FeedbackEngine {
    /// 构造实例。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// 根据已执行任务的结果，调整计划中未执行任务的优先级。
    ///
    /// # 调整规则
    ///
    /// - 如果已有任务报告了违反（`PropertyResult::Violated`），则把同一目标的
    ///   后续性质任务优先级提升：违反说明这个模块值得更仔细地检查。
    /// - 如果已有任务报告了不确定（无法编译、工具缺失），则降低同一目标后续
    ///   任务的优先级：继续在无法执行的目标上花预算没有意义。
    /// - 通过的任务不引起调整：通过只说明「在这些样本下没观察到违反」，不说明
    ///   该模块安全，因此不降低也不提升优先级。
    ///
    /// 返回调整记录列表（进轨迹），以及调整后的计划（同一对象原地修改）。
    ///
    /// # 为什么原地修改计划
    ///
    /// 计划对象本身就是可重放的（不含时钟）：调整只改变数值字段，因此调整后的
    /// 计划仍然可以被序列化、比较、重放。这满足 FR-043「同样配置给出同样结果」。
    #[must_use]
    pub fn apply(
        &self,
        completed: &BTreeMap<String, crate::tool::ToolOutput>,
        plan: &mut VerificationPlan,
    ) -> Vec<FeedbackUpdate> {
        let mut updates = Vec::new();

        // 根据已完成任务的结果计算每个任务的调整增量。
        let adjustments: BTreeMap<String, Adjustment> = compute_adjustments(completed);

        for task in &mut plan.tasks {
            let key = task.kind.key();
            if let Some(adj) = adjustments.get(&key) {
                if adj.delta != 0 && task.priority > 0 {
                    // 优先级不允许掉到 0 以下：0 会被策略层当成「没有依据」。
                    let new_priority = u32::try_from(
                        (i32::try_from(task.priority).unwrap_or(i32::MAX) + adj.delta).max(1),
                    )
                    .unwrap_or(1);
                    if new_priority != task.priority {
                        updates.push(FeedbackUpdate {
                            task_key: key.clone(),
                            delta: adj.delta,
                            based_on: adj.based_on.clone(),
                            rationale: format!(
                                "{}，优先级 {} -> {}",
                                adj.rationale, task.priority, new_priority
                            ),
                        });
                        task.priority = new_priority;
                    }
                }
            }
        }

        // 调整后重新排序，保证最高优先级先执行。
        plan.sort_by_priority();
        updates
    }
}

/// 一次调整的内部描述。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Adjustment {
    /// 调整增量。
    delta: i32,
    /// 依据说明。
    rationale: String,
    /// 基于哪个已完成任务。
    based_on: Option<String>,
}

/// 根据已完成任务计算每个任务的调整增量。
fn compute_adjustments(
    completed: &BTreeMap<String, crate::tool::ToolOutput>,
) -> BTreeMap<String, Adjustment> {
    let mut adjustments = BTreeMap::new();

    // 简化规则：如果有任何一个已完成任务的结果包含「违反」标记（由工具输出
    // 的 `summary` 字段传递），则提升同一模块相关任务的优先级；如果包含
    // 「无法执行」标记，则降低优先级。实际工具输出由 `pf-verify` 的工具
    // 实现提供，这里只做启发式解析：看 `summary` 是否包含关键字。
    for (key, output) in completed {
        let summary_lower = output.summary.to_lowercase();
        if summary_lower.contains("violated") || summary_lower.contains("violation") {
            // 键的格式是 `<任务类型>:<目标>`，取冒号后的目标部分。用 splitn
            // 而不是 split：目标本身可能含冒号（`mod::a`）。
            let target = key.split_once(':').map_or("", |(_, target)| target);
            if !target.is_empty() {
                // 提升同一目标的所有后续性质验证任务。
                adjustments.insert(
                    format!("verify:{target}"),
                    Adjustment {
                        delta: 10,
                        rationale: format!("已发现违反（任务 `{key}`），提升同目标后续检查优先级"),
                        based_on: Some(key.clone()),
                    },
                );
            }
        } else if summary_lower.contains("inconclusive")
            || summary_lower.contains("timed out")
            || summary_lower.contains("tool missing")
        {
            let target = key.split_once(':').map_or("", |(_, target)| target);
            if !target.is_empty() {
                adjustments.insert(
                    format!("verify:{target}"),
                    Adjustment {
                        delta: -5,
                        rationale: format!("任务 `{key}` 无法得到结论，降低同目标后续检查优先级"),
                        based_on: Some(key.clone()),
                    },
                );
            }
        }
    }

    adjustments
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{TaskKind, VerificationPlan, VerificationTask};

    fn dummy_output(summary: &str) -> crate::tool::ToolOutput {
        crate::tool::ToolOutput {
            payload: serde_json::Value::Null,
            summary: summary.to_owned(),
            confidence: crate::tool::ToolConfidence::Observed,
            side_effects: Vec::new(),
        }
    }

    #[test]
    fn violation_raises_priority_for_same_target() {
        let mut plan = VerificationPlan {
            tasks: vec![
                VerificationTask {
                    kind: TaskKind::VerifyProperty {
                        target: "mod::a".to_owned(),
                        call_template: String::new(),
                        domain: serde_json::Value::Null,
                        invariant: String::new(),
                    },
                    priority: 30,
                    rationale: String::new(),
                },
                VerificationTask {
                    kind: TaskKind::Fuzz {
                        target: "mod::a".to_owned(),
                        call_template: String::new(),
                        domain: serde_json::Value::Null,
                        invariant: String::new(),
                    },
                    priority: 20,
                    rationale: String::new(),
                },
            ],
        };
        let mut completed = BTreeMap::new();
        completed.insert(
            "verify:mod::a".to_owned(),
            dummy_output("property violated"),
        );
        let engine = FeedbackEngine::new();
        let updates = engine.apply(&completed, &mut plan);
        assert!(
            !updates.is_empty(),
            "发现违反后必须产生优先级调整：{updates:?}"
        );
        // 调整后同目标任务优先级应提升。
        assert!(plan.tasks[0].priority > 30 || updates.iter().any(|u| u.delta > 0));
    }

    #[test]
    fn inconclusive_lowers_priority_for_same_target() {
        let mut plan = VerificationPlan {
            tasks: vec![VerificationTask {
                kind: TaskKind::VerifyProperty {
                    target: "mod::b".to_owned(),
                    call_template: String::new(),
                    domain: serde_json::Value::Null,
                    invariant: String::new(),
                },
                priority: 50,
                rationale: String::new(),
            }],
        };
        let mut completed = BTreeMap::new();
        completed.insert(
            "verify:mod::b".to_owned(),
            dummy_output("tool unavailable, inconclusive"),
        );
        let engine = FeedbackEngine::new();
        let updates = engine.apply(&completed, &mut plan);
        assert!(
            !updates.is_empty(),
            "任务无法得到结论时必须产生优先级调整：{updates:?}"
        );
        assert!(updates.iter().all(|u| u.delta == -5));
    }

    #[test]
    fn satisfied_does_not_change_priority() {
        let mut plan = VerificationPlan {
            tasks: vec![VerificationTask {
                kind: TaskKind::VerifyProperty {
                    target: "mod::c".to_owned(),
                    call_template: String::new(),
                    domain: serde_json::Value::Null,
                    invariant: String::new(),
                },
                priority: 40,
                rationale: String::new(),
            }],
        };
        let mut completed = BTreeMap::new();
        completed.insert(
            "verify:mod::c".to_owned(),
            dummy_output("all samples satisfied"),
        );
        let engine = FeedbackEngine::new();
        let updates = engine.apply(&completed, &mut plan);
        assert!(
            updates.is_empty(),
            "「未观察到违反」不应改变优先级：{updates:?}"
        );
        assert_eq!(plan.tasks[0].priority, 40);
    }

    #[test]
    fn adjustments_are_reproducible_for_the_same_input() {
        // 同一份已完成任务必然产生同一份调整记录。
        let plan = VerificationPlan {
            tasks: vec![VerificationTask {
                kind: TaskKind::VerifyProperty {
                    target: "mod::d".to_owned(),
                    call_template: String::new(),
                    domain: serde_json::Value::Null,
                    invariant: String::new(),
                },
                priority: 25,
                rationale: String::new(),
            }],
        };
        let mut completed = BTreeMap::new();
        completed.insert(
            "verify:mod::d".to_owned(),
            dummy_output("violated: counterexample at index 3"),
        );
        let engine = FeedbackEngine::new();
        let updates1 = engine.apply(
            &completed,
            &mut VerificationPlan {
                tasks: plan.tasks.clone(),
            },
        );
        let updates2 = engine.apply(
            &completed,
            &mut VerificationPlan {
                tasks: plan.tasks.clone(),
            },
        );
        assert_eq!(updates1, updates2);
    }

    #[test]
    fn every_update_has_a_rationale() {
        // 说不出理由的调整在审计时与随机调整没有区别（FR-049）。
        let mut plan = VerificationPlan {
            tasks: vec![VerificationTask {
                kind: TaskKind::RunExistingTests,
                priority: 100,
                rationale: String::new(),
            }],
        };
        let mut completed = BTreeMap::new();
        completed.insert("verify:x".to_owned(), dummy_output("violated"));
        let engine = FeedbackEngine::new();
        let updates = engine.apply(&completed, &mut plan);
        assert!(updates.iter().all(|u| !u.rationale.trim().is_empty()));
    }
}
