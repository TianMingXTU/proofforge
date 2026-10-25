//! 验证计划（FR-044）。
//!
//! # 计划从哪里来
//!
//! 需求文档 FR-044 要求「根据需求、代码结构和风险分析制定验证计划」。其中的
//! 「需求」需要语义理解（阶段二的 AI 语义分析，依赖阶段四的 Provider），因此这里
//! 只做**结构与风险**那一半，并且做扎实：
//!
//! - 静态规则命中的函数优先——一条 `no-unwrap-in-library` 的发现就是「这里可能
//!   在某个输入下 panic」的具体假设。
//! - 没有调用者的公共 API 次之——它没有被现有测试覆盖过。
//! - 跑项目自带的测试套件永远排在前面：它最便宜，也最能说明「 baseline 是否
//!   已经坏了」。baseline 坏掉时后面所有结论都要打个问号。
//!
//! # 计划是可重放的
//!
//! [`VerificationPlan`] 只包含数据（任务、优先级、依据），不含时钟与随机源。
//! 同一份结构 + 同一份配置必然产出同一份计划——这是 FR-043 结果复现的前提。

use serde::{Deserialize, Serialize};

use pf_core::location::Location;
use pf_core::rule::RuleSeverity;

/// 一个验证任务。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationTask {
    /// 任务类型与目标。
    pub kind: TaskKind,
    /// 优先级：越大越先跑。
    ///
    /// 优先级是计划器与反馈回路**唯一**的排序依据。任务之间不共享可变状态，
    /// 因此同一份计划 + 同一份反馈必然给出同一执行顺序。
    pub priority: u32,
    /// 排进这条任务的依据（人类可读，进轨迹）。
    ///
    /// 不可省略：一个说不出理由的任务在审计时与随机选中的没有区别。
    pub rationale: String,
}

/// 任务类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TaskKind {
    /// 跑目标项目自带的测试套件。
    RunExistingTests,
    /// 验证一条性质。
    VerifyProperty {
        /// 目标函数或方法的限定名。
        target: String,
        /// 调用模板。
        call_template: String,
        /// 输入域（JSON，形状见 `pf-verify::spec`）。
        domain: serde_json::Value,
        /// 不变式表达式。
        invariant: String,
    },
    /// 对一个目标做反馈驱动的模糊测试。
    Fuzz {
        /// 目标限定名。
        target: String,
        /// 调用模板。
        call_template: String,
        /// 输入域。
        domain: serde_json::Value,
        /// 不变式表达式。
        invariant: String,
    },
}

impl TaskKind {
    /// 任务的稳定标识，用于去重与轨迹。
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::RunExistingTests => "run-existing-tests".to_owned(),
            Self::VerifyProperty { target, .. } => format!("verify:{target}"),
            Self::Fuzz { target, .. } => format!("fuzz:{target}"),
        }
    }

    /// 任务是否执行外部进程。
    ///
    /// 计划器用它估算子进程时间预算：一个只读任务不花执行秒数。
    #[must_use]
    pub const fn spawns_processes(&self) -> bool {
        true
    }
}

/// 一份验证计划。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationPlan {
    /// 任务列表，按优先级降序。
    pub tasks: Vec<VerificationTask>,
}

impl VerificationPlan {
    /// 空计划。
    #[must_use]
    pub const fn new() -> Self {
        Self { tasks: Vec::new() }
    }

    /// 按优先级降序排列任务。
    ///
    /// 排序是稳定的：优先级相同时保持插入顺序，而插入顺序由计划器按确定规则
    /// 产生，因此整份计划对相同输入可重放。
    pub fn sort_by_priority(&mut self) {
        self.tasks
            .sort_by_key(|task| std::cmp::Reverse(task.priority));
    }

    /// 去掉 `key` 重复的任务（保留先出现的那个，即优先级更高的）。
    pub fn dedup_by_key(&mut self) {
        let mut seen = Vec::new();
        self.tasks.retain(|task| {
            let key = task.kind.key();
            if seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        });
    }

    /// 任务条数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// 是否没有任务。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// 按优先级取出下一个任务。
    pub fn pop_next(&mut self) -> Option<VerificationTask> {
        if self.tasks.is_empty() {
            return None;
        }
        self.sort_by_priority();
        Some(self.tasks.remove(0))
    }
}

/// 计划器：把静态结构变成一份 [`VerificationPlan`]。
///
/// # 为什么优先级是这几个数
///
/// 数值本身没有意义，有意义的是**间距**：跑现有测试比任何性质验证高出一档，
/// 因此即使有十条性质任务，baseline 也总是第一个跑。规则命中比无调用者 API
/// 高一档，因此同一模块里两个候选同时存在时，有规则依据的那个先跑。
#[derive(Debug, Clone, Copy)]
pub struct StructuralPlanner {
    /// 是否把「跑现有测试」放进计划。
    ///
    /// 关掉它的唯一正当理由是目标项目没有测试套件——那时这条任务只会浪费一次
    /// 编译。
    pub include_existing_tests: bool,
    /// 每个候选目标最多生成几条性质。
    ///
    /// 上限存在的理由是执行成本：一条性质的验证要编译并运行一次目标项目，
    /// 无上限的计划会在预算内只跑到前几个目标。
    pub max_properties_per_target: usize,
    /// 是否对高优先级目标追加模糊测试任务。
    pub fuzz_high_priority_targets: bool,
}

impl Default for StructuralPlanner {
    fn default() -> Self {
        Self {
            include_existing_tests: true,
            max_properties_per_target: 2,
            fuzz_high_priority_targets: true,
        }
    }
}

/// 一个候选验证目标。
#[derive(Debug, Clone)]
pub struct PlanCandidate {
    /// 目标限定名（例如 `mycrate::parse`）。
    pub target: String,
    /// 调用模板。
    pub call_template: String,
    /// 输入域（JSON）。
    pub domain: serde_json::Value,
    /// 候选的不变式。
    pub invariant: String,
    /// 支撑这个候选的结构证据。
    pub evidence: CandidateEvidence,
}

/// 支撑一个候选目标的结构证据。
#[derive(Debug, Clone, Default)]
pub struct CandidateEvidence {
    /// 命中该目标的规则 ID。
    pub rules: Vec<String>,
    /// 命中的最高严重程度。
    pub severity: Option<RuleSeverity>,
    /// 命中位置。
    pub locations: Vec<Location>,
    /// 项目内是否有调用者。
    pub has_callers: bool,
    /// 是否是公共 API。
    pub is_public: bool,
}

impl StructuralPlanner {
    /// 用候选列表生成计划。
    ///
    /// 候选由调用方从符号索引与规则发现中提取——计划器本身不做 I/O，因此可以在
    /// 没有文件系统的情况下被完整测试。
    #[must_use]
    pub fn plan(&self, candidates: &[PlanCandidate]) -> VerificationPlan {
        let mut plan = VerificationPlan::new();
        if self.include_existing_tests {
            plan.tasks.push(VerificationTask {
                kind: TaskKind::RunExistingTests,
                priority: PRIORITY_BASELINE,
                rationale: "先确认目标项目自己的测试是绿的；baseline 坏了的话，\
                            后面每条结论都要打问号"
                    .to_owned(),
            });
        }
        for candidate in candidates {
            let priority = Self::priority_for(candidate);
            // 至少出一条性质：一条只有一个不变式的性质也是性质。上限由配置给，
            // 这里不再额外收紧——否则 max_properties_per_target 永远等于 1。
            let properties = self.max_properties_per_target.max(1);
            for index in 0..properties {
                plan.tasks.push(VerificationTask {
                    kind: TaskKind::VerifyProperty {
                        target: candidate.target.clone(),
                        call_template: candidate.call_template.clone(),
                        domain: candidate.domain.clone(),
                        invariant: candidate.invariant.clone(),
                    },
                    // 同一目标的第二条性质略低，保证先跑最可能失败的那条。
                    priority: priority.saturating_sub(u32::try_from(index).unwrap_or(u32::MAX)),
                    rationale: rationale_for(candidate),
                });
            }
            if self.fuzz_high_priority_targets && priority >= PRIORITY_RULE_HIT {
                plan.tasks.push(VerificationTask {
                    kind: TaskKind::Fuzz {
                        target: candidate.target.clone(),
                        call_template: candidate.call_template.clone(),
                        domain: candidate.domain.clone(),
                        invariant: candidate.invariant.clone(),
                    },
                    // 模糊测试比单条性质低一档：它贵，且通常在前者已经失败时没有
                    // 额外信息。
                    priority: priority.saturating_sub(PRIORITY_FUZZ_PENALTY),
                    rationale: format!(
                        "{}；对该目标追加反馈驱动的模糊测试",
                        rationale_for(candidate)
                    ),
                });
            }
        }
        plan.sort_by_priority();
        plan.dedup_by_key();
        plan
    }

    /// 计算一个候选的优先级。
    ///
    /// 关联函数而不是方法：优先级只取决于候选本身，与计划器的配置无关。
    fn priority_for(candidate: &PlanCandidate) -> u32 {
        let mut priority = PRIORITY_NO_EVIDENCE;
        if !candidate.evidence.rules.is_empty() {
            priority = priority.max(PRIORITY_RULE_HIT);
            // 严重程度越高越先跑：一条 error 级发现比 info 级更可能是真缺陷。
            priority = priority.saturating_add(severity_bonus(candidate.evidence.severity));
        }
        if candidate.evidence.is_public && !candidate.evidence.has_callers {
            priority = priority.max(PRIORITY_UNTESTED_API);
        }
        priority
    }
}

/// 跑现有测试的优先级。
const PRIORITY_BASELINE: u32 = 100;

/// 有规则依据的候选。
pub const PRIORITY_RULE_HIT: u32 = 50;

/// 公共但没有调用者的 API。
const PRIORITY_UNTESTED_API: u32 = 30;

/// 什么证据都没有的候选。
const PRIORITY_NO_EVIDENCE: u32 = 10;

/// 模糊测试相对性质的优先级惩罚。
const PRIORITY_FUZZ_PENALTY: u32 = 10;

/// 严重程度带来的优先级加成。
fn severity_bonus(severity: Option<RuleSeverity>) -> u32 {
    match severity {
        Some(RuleSeverity::Mandatory) => 20,
        Some(RuleSeverity::Project) => 12,
        Some(RuleSeverity::Advisory) => 6,
        None => 0,
    }
}

/// 生成一条任务的依据说明。
fn rationale_for(candidate: &PlanCandidate) -> String {
    if candidate.evidence.rules.is_empty() {
        return format!("{} 是未被现有调用覆盖的公共 API", candidate.target);
    }
    format!(
        "{} 命中规则 {}",
        candidate.target,
        candidate.evidence.rules.join("、")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(target: &str) -> PlanCandidate {
        PlanCandidate {
            target: target.to_owned(),
            call_template: format!("{target}({{args}})"),
            domain: serde_json::json!({"kind": "int", "min": -10, "max": 10}),
            invariant: "output >= 0".to_owned(),
            evidence: CandidateEvidence::default(),
        }
    }

    fn with_rules(target: &str, rules: &[&str]) -> PlanCandidate {
        PlanCandidate {
            evidence: CandidateEvidence {
                rules: rules.iter().map(|rule| (*rule).to_owned()).collect(),
                ..CandidateEvidence::default()
            },
            ..candidate(target)
        }
    }

    #[test]
    fn the_baseline_task_always_comes_first() {
        let planner = StructuralPlanner::default();
        let plan = planner.plan(&[with_rules("mycrate::parse", &["no-unwrap-in-library"])]);
        assert_eq!(plan.tasks[0].kind, TaskKind::RunExistingTests);
        assert!(plan.tasks[0].rationale.contains("baseline"));
    }

    #[test]
    fn the_baseline_can_be_switched_off() {
        // 目标项目没有测试套件时，这条任务只会浪费一次编译。
        let planner = StructuralPlanner {
            include_existing_tests: false,
            ..StructuralPlanner::default()
        };
        let plan = planner.plan(&[candidate("mycrate::parse")]);
        assert!(!plan
            .tasks
            .iter()
            .any(|task| task.kind == TaskKind::RunExistingTests));
    }

    #[test]
    fn a_rule_hit_outranks_an_untested_api() {
        let planner = StructuralPlanner::default();
        let plan = planner.plan(&[
            PlanCandidate {
                evidence: CandidateEvidence {
                    is_public: true,
                    has_callers: false,
                    ..CandidateEvidence::default()
                },
                ..candidate("mycrate::unused")
            },
            with_rules("mycrate::parse", &["no-unwrap-in-library"]),
        ]);
        // 排序后规则命中必须排在无调用者 API 前面。
        let keys: Vec<String> = plan.tasks.iter().map(|task| task.kind.key()).collect();
        let hit = keys.iter().position(|key| key == "verify:mycrate::parse");
        let unused = keys.iter().position(|key| key == "verify:mycrate::unused");
        assert!(hit < unused, "{keys:?}");
    }

    #[test]
    fn severity_raises_priority_within_the_same_rule_class() {
        let planner = StructuralPlanner {
            fuzz_high_priority_targets: false,
            ..StructuralPlanner::default()
        };
        let mut blocker = with_rules("mycrate::a", &["no-unwrap-in-library"]);
        blocker.evidence.severity = Some(RuleSeverity::Mandatory);
        let mut info = with_rules("mycrate::b", &["no-unwrap-in-library"]);
        info.evidence.severity = Some(RuleSeverity::Advisory);
        let plan = planner.plan(&[info, blocker]);
        // 基线任务（run-existing-tests）优先级最高，排在第一位；在性质任务中，blocker 应在前。
        let property_keys: Vec<String> = plan
            .tasks
            .iter()
            .filter(|task| task.kind.key().starts_with("verify:"))
            .map(|task| task.kind.key())
            .collect();
        assert_eq!(property_keys[0], "verify:mycrate::a");
    }

    #[test]
    fn every_task_carries_a_rationale() {
        // 说不出理由的任务在审计时与随机选中的没有区别（FR-049）。
        let planner = StructuralPlanner::default();
        let plan = planner.plan(&[with_rules("mycrate::parse", &["no-unwrap-in-library"])]);
        assert!(plan
            .tasks
            .iter()
            .all(|task| !task.rationale.trim().is_empty()));
        assert!(plan
            .tasks
            .iter()
            .any(|task| task.rationale.contains("no-unwrap-in-library")));
    }

    #[test]
    fn fuzzing_is_appended_only_to_high_priority_targets() {
        let planner = StructuralPlanner::default();
        let plan = planner.plan(&[
            with_rules("mycrate::parse", &["no-unwrap-in-library"]),
            candidate("mycrate::plain"),
        ]);
        let fuzzed: Vec<&str> = plan
            .tasks
            .iter()
            .filter_map(|task| match &task.kind {
                TaskKind::Fuzz { target, .. } => Some(target.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(fuzzed, vec!["mycrate::parse"]);
        // 模糊测试必须比同目标的性质低一档。
        let property = plan
            .tasks
            .iter()
            .find(|task| task.kind.key() == "verify:mycrate::parse")
            .expect("property task");
        let fuzz = plan
            .tasks
            .iter()
            .find(|task| task.kind.key() == "fuzz:mycrate::parse")
            .expect("fuzz task");
        assert!(fuzz.priority < property.priority);
    }

    #[test]
    fn duplicate_targets_collapse_to_one_task() {
        // 同一个目标从两条规则各来一次时只验证一次：执行成本与目标数成正比。
        let planner = StructuralPlanner::default();
        let plan = planner.plan(&[
            with_rules("mycrate::parse", &["no-unwrap-in-library"]),
            with_rules("mycrate::parse", &["function-too-long"]),
        ]);
        assert_eq!(
            plan.tasks
                .iter()
                .filter(|task| task.kind.key() == "verify:mycrate::parse")
                .count(),
            1
        );
    }

    #[test]
    fn the_plan_is_reproducible_for_the_same_input() {
        // FR-043：同一份结构必须产出同一份计划，否则「重放一次会话」没有意义。
        let planner = StructuralPlanner::default();
        let candidates = vec![
            with_rules("mycrate::parse", &["no-unwrap-in-library"]),
            candidate("mycrate::plain"),
        ];
        assert_eq!(planner.plan(&candidates), planner.plan(&candidates));
    }

    #[test]
    fn an_empty_candidate_list_still_runs_the_baseline() {
        // 没有候选也要跑 baseline：那本身是一条信息。
        let plan = StructuralPlanner::default().plan(&[]);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.tasks[0].kind, TaskKind::RunExistingTests);
    }

    #[test]
    fn pop_next_returns_the_highest_priority_task() {
        let mut plan = StructuralPlanner::default().plan(&[
            candidate("mycrate::plain"),
            with_rules("mycrate::parse", &["no-unwrap-in-library"]),
        ]);
        let first = plan.pop_next().expect("a task");
        assert_eq!(first.kind, TaskKind::RunExistingTests);
        let second = plan.pop_next().expect("a task");
        assert_eq!(second.kind.key(), "verify:mycrate::parse");
    }

    #[test]
    fn an_empty_plan_yields_no_task() {
        let mut plan = VerificationPlan::new();
        assert!(plan.is_empty());
        assert!(plan.pop_next().is_none());
    }

    #[test]
    fn task_keys_are_stable_identifiers() {
        // key 进轨迹与审计记录。
        assert_eq!(TaskKind::RunExistingTests.key(), "run-existing-tests");
        assert_eq!(
            TaskKind::VerifyProperty {
                target: "a::b".to_owned(),
                call_template: String::new(),
                domain: serde_json::Value::Null,
                invariant: String::new(),
            }
            .key(),
            "verify:a::b"
        );
        assert_eq!(
            TaskKind::Fuzz {
                target: "a::b".to_owned(),
                call_template: String::new(),
                domain: serde_json::Value::Null,
                invariant: String::new(),
            }
            .key(),
            "fuzz:a::b"
        );
    }
}
