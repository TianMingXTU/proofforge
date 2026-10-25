//! 执行预算与停止机制（FR-046、FR-048）。
//!
//! # 预算耗尽是「证据不足」，不是「通过」
//!
//! 一个跑到一半被预算掐断的会话，与一个跑完且没发现问题的会话，在结论上必须
//! 完全不同。前者返回 [`pf_core::status::GateStatus::InsufficientEvidence`]——
//! 把「没跑完」说成「没问题」是 AGENTS.md 安全红线 4 禁止的事。
//!
//! # 预算的四条独立维度
//!
//! 需求文档 FR-046 要求限制 CPU、内存、执行时间、测试数量与模型费用。这里实现
//! 其中三条可以**在进程内精确计量**的维度：墙钟时间、工具调用次数、以及派生的
//! 子进程执行秒数。内存需要内核级采样（cgroup 或 `/proc` 轮询），那属于沙箱
//! 能力而不是 Runtime 能力，记录在 `docs/known-limitations.md`。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 会话的执行预算。
///
/// 所有限额都是**上限**：`None` 表示该维度不设限。不设限的维度仍然被计量，
/// 这样报告能说出「这次跑了多久」而不只是「没超时」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionBudget {
    /// 墙钟时间上限（秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_time_secs: Option<u64>,
    /// 工具调用次数上限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u32>,
    /// 派生子进程累计执行秒数上限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subprocess_secs: Option<u64>,
    /// 模型费用上限（百万分之一美元，避免浮点累计误差）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_cost_micros: Option<u64>,
}

impl Default for ExecutionBudget {
    fn default() -> Self {
        Self {
            wall_time_secs: Some(900),
            tool_calls: Some(200),
            subprocess_secs: Some(600),
            // 0 而不是 None：内置的计划器不调用模型，因此默认预算就是「不许花钱」。
            // 接外部 Provider 时必须显式放宽，而不是默默开始计费。
            model_cost_micros: Some(0),
        }
    }
}

impl ExecutionBudget {
    /// 换算成 `pf-verify` 的资源预算。
    ///
    /// Agent 的墙钟与子进程上限直接映射过去：沙箱里的每次编译执行都受同一个
    /// 上限约束，否则一个卡死的驱动会拖垮整个会话。没有设置上限时用
    /// `pf-core` 的默认值，而不是 0——0 会让每条命令立刻超时。
    #[must_use]
    pub fn into_verify_budget(&self) -> pf_core::config::Budget {
        let fallback = pf_core::config::Budget::default();
        pf_core::config::Budget {
            command_timeout_secs: self
                .wall_time_secs
                .unwrap_or(fallback.command_timeout_secs)
                .max(1),
            task_timeout_secs: self
                .wall_time_secs
                .unwrap_or(fallback.task_timeout_secs)
                .max(1),
            ..fallback
        }
    }

    /// 判断一份用量快照是否已触达某个限额。
    ///
    /// 唯一的判定入口：[`BudgetTracker::exhaustion`]（执行循环）与
    /// `StrategyEngine::choose`（选下一个任务）都要回答同一个问题，各写一份的判断
    /// 必然会出现分歧——那种分歧的表现是「循环说还能跑，策略说该停了」，极难定位。
    ///
    /// 模型费用用 `>` 而不是 `>=`：默认限额是 0，含义是「不许花钱」而不是
    /// 「不许跑」。`>=` 会让一次模型调用都没有的会话在第一轮就被判耗尽，
    /// 那让 `AgentConfig::default()` 根本不能用。其余维度保持 `>=`：
    /// 「最多 0 次工具调用」确实应该在第一轮之前就停。
    #[must_use]
    pub fn exhausted_by(&self, usage: BudgetUsage) -> Option<BudgetDimension> {
        if let Some(limit) = self.wall_time_secs {
            if usage.wall_time_secs >= limit {
                return Some(BudgetDimension::WallTime);
            }
        }
        if let Some(limit) = self.tool_calls {
            if usage.tool_calls >= limit {
                return Some(BudgetDimension::ToolCalls);
            }
        }
        if let Some(limit) = self.subprocess_secs {
            if usage.subprocess_secs >= limit {
                return Some(BudgetDimension::SubprocessTime);
            }
        }
        if let Some(limit) = self.model_cost_micros {
            if usage.model_cost_micros > limit {
                return Some(BudgetDimension::ModelCost);
            }
        }
        None
    }
}

/// 预算的实时计量器。
///
/// 与 [`ExecutionBudget`] 分开：前者是**配置**（可序列化、可复现），后者是
/// **状态**（含时钟，不可序列化）。混在一起会让「重放一次会话」变成不可能——
/// 而结果复现（FR-043）要求同样的配置必须给出同样的计划。
#[derive(Debug)]
pub struct BudgetTracker {
    /// 配置的限额。
    limits: ExecutionBudget,
    /// 会话起点。
    started: Instant,
    /// 已发生的工具调用次数。
    tool_calls: u32,
    /// 已累计的子进程执行秒数。
    subprocess_secs: u64,
    /// 已累计的模型费用（微美元）。
    model_cost_micros: u64,
}

impl BudgetTracker {
    /// 按给定限额开始计量。
    #[must_use]
    pub fn new(limits: ExecutionBudget) -> Self {
        Self {
            limits,
            started: Instant::now(),
            tool_calls: 0,
            subprocess_secs: 0,
            model_cost_micros: 0,
        }
    }

    /// 当前已消耗的墙钟秒数。
    #[must_use]
    pub fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// 已发生的工具调用次数。
    #[must_use]
    pub const fn tool_calls(&self) -> u32 {
        self.tool_calls
    }

    /// 已累计的子进程执行秒数。
    #[must_use]
    pub const fn subprocess_secs(&self) -> u64 {
        self.subprocess_secs
    }

    /// 已累计的模型费用（微美元）。
    #[must_use]
    pub const fn model_cost_micros(&self) -> u64 {
        self.model_cost_micros
    }

    /// 配置的限额。
    #[must_use]
    pub const fn limits(&self) -> &ExecutionBudget {
        &self.limits
    }

    /// 记录一次工具调用。
    pub fn record_call(&mut self, subprocess_secs: u64) {
        self.tool_calls = self.tool_calls.saturating_add(1);
        self.subprocess_secs = self.subprocess_secs.saturating_add(subprocess_secs);
    }

    /// 记录一笔模型费用。
    pub fn record_model_cost(&mut self, micros: u64) {
        self.model_cost_micros = self.model_cost_micros.saturating_add(micros);
    }

    /// 是否还有余量。
    ///
    /// 判定规则见 [`ExecutionBudget::exhausted_by`]：模型费用用 `>`，其余维度用
    /// `>=`。这里只做转述，不再写第二份规则。
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.exhaustion().is_some()
    }

    /// 找出第一个被耗尽的维度。
    ///
    /// 返回具体维度而不是一个布尔值：报告要能说出「为什么停」，否则使用者无法
    /// 判断该调大哪个限额。
    #[must_use]
    pub fn exhaustion(&self) -> Option<BudgetDimension> {
        self.limits.exhausted_by(self.usage())
    }

    /// 快照当前用量，供轨迹与报告使用。
    #[must_use]
    pub fn usage(&self) -> BudgetUsage {
        BudgetUsage {
            wall_time_secs: self.elapsed_secs(),
            tool_calls: self.tool_calls,
            subprocess_secs: self.subprocess_secs,
            model_cost_micros: self.model_cost_micros,
        }
    }

    /// 剩余墙钟时间的下界，用于给单个工具设超时。
    ///
    /// 返回 [`Duration::ZERO`] 表示已经没有时间了——此时任何工具都不该启动。
    #[must_use]
    pub fn remaining_wall_time(&self) -> Duration {
        match self.limits.wall_time_secs {
            Some(limit) => Duration::from_secs(limit).saturating_sub(self.started.elapsed()),
            None => Duration::from_secs(3600),
        }
    }
}

/// 被耗尽的预算维度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetDimension {
    /// 墙钟时间。
    WallTime,
    /// 工具调用次数。
    ToolCalls,
    /// 子进程累计执行时间。
    SubprocessTime,
    /// 模型费用。
    ModelCost,
}

impl BudgetDimension {
    /// 机器可读标识。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WallTime => "wall_time",
            Self::ToolCalls => "tool_calls",
            Self::SubprocessTime => "subprocess_time",
            Self::ModelCost => "model_cost",
        }
    }
}

/// 一次会话的预算用量快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetUsage {
    /// 已用墙钟秒数。
    pub wall_time_secs: u64,
    /// 已发生的工具调用次数。
    pub tool_calls: u32,
    /// 已累计的子进程执行秒数。
    pub subprocess_secs: u64,
    /// 已累计的模型费用（微美元）。
    pub model_cost_micros: u64,
}

/// 会话停止的原因（FR-048）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum StopReason {
    /// 计划里的任务全部完成。
    PlanCompleted,
    /// 预算耗尽。
    BudgetExhausted(BudgetDimension),
    /// 用户取消。
    Cancelled,
    /// 达到停止目标（例如已经复现了足够多的缺陷）。
    GoalReached {
        /// 目标描述，例如「复现 3 个缺陷」。
        goal: String,
    },
    /// 出现无法继续的异常。
    Failed {
        /// 失败原因。
        reason: String,
    },
}

impl StopReason {
    /// 该停止原因是否意味着「这次会话给出了完整结论」。
    ///
    /// 只有 [`Self::PlanCompleted`] 与 [`Self::GoalReached`] 是。预算耗尽、取消、
    /// 失败都让会话的结论**不完整**，门禁必须据此返回证据不足。
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self, Self::PlanCompleted | Self::GoalReached { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> ExecutionBudget {
        ExecutionBudget {
            wall_time_secs: Some(3600),
            tool_calls: Some(2),
            subprocess_secs: Some(3),
            // 给一个非零额度：这里测的是另外三个维度，让模型费用抢先生效只会让
            // 每个断言都拿到 ModelCost。
            model_cost_micros: Some(1000),
        }
    }

    #[test]
    fn a_fresh_tracker_is_not_exhausted() {
        let tracker = BudgetTracker::new(budget());
        assert!(!tracker.is_exhausted());
        assert_eq!(tracker.exhaustion(), None);
        assert_eq!(tracker.tool_calls(), 0);
    }

    #[test]
    fn a_zero_model_budget_only_bites_after_spending() {
        // 默认的模型费用预算是 0，含义是「不许花钱」而不是「不许跑」：
        // 一次模型都没调用的会话不能因此被判耗尽，否则默认配置根本不能用。
        let tracker = BudgetTracker::new(ExecutionBudget::default());
        assert_eq!(tracker.exhaustion(), None);
        assert!(!tracker.is_exhausted());
    }

    #[test]
    fn spending_under_a_zero_model_budget_stops_the_session() {
        // 上限 0 时第一笔费用就超限——「不许花钱」在这里真的生效。
        let mut tracker = BudgetTracker::new(ExecutionBudget::default());
        tracker.record_model_cost(1);
        assert_eq!(tracker.exhaustion(), Some(BudgetDimension::ModelCost));
        assert!(tracker.is_exhausted());
    }

    #[test]
    fn tool_call_limit_is_enforced() {
        let mut tracker = BudgetTracker::new(budget());
        tracker.record_call(0);
        assert_eq!(tracker.exhaustion(), None, "第一次调用还没到上限");
        tracker.record_call(0);
        assert_eq!(tracker.exhaustion(), Some(BudgetDimension::ToolCalls));
    }

    #[test]
    fn subprocess_seconds_accumulate_across_calls() {
        // 单独放宽调用次数：这里要观察的是子进程时间的累计，
        // 让调用次数先抢走「第一个耗尽的维度」只会测到别的分支。
        let mut tracker = BudgetTracker::new(ExecutionBudget {
            tool_calls: Some(50),
            ..budget()
        });
        tracker.record_call(2);
        assert_eq!(tracker.exhaustion(), None, "累计 2 秒还没到 3 秒上限");
        tracker.record_call(1);
        assert_eq!(tracker.subprocess_secs(), 3);
        assert_eq!(tracker.exhaustion(), Some(BudgetDimension::SubprocessTime));
    }

    #[test]
    fn usage_snapshot_reports_every_dimension() {
        let mut tracker = BudgetTracker::new(budget());
        tracker.record_call(5);
        tracker.record_model_cost(7);
        let usage = tracker.usage();
        assert_eq!(usage.tool_calls, 1);
        assert_eq!(usage.subprocess_secs, 5);
        assert_eq!(usage.model_cost_micros, 7);
    }

    #[test]
    fn remaining_time_never_goes_negative() {
        let tracker = BudgetTracker::new(ExecutionBudget {
            wall_time_secs: Some(0),
            ..ExecutionBudget::default()
        });
        assert_eq!(tracker.remaining_wall_time(), Duration::ZERO);
    }

    #[test]
    fn only_full_completion_counts_as_complete() {
        // 预算耗尽与取消都让结论不完整——门禁据此返回证据不足，而不是通过。
        assert!(StopReason::PlanCompleted.is_complete());
        assert!(StopReason::GoalReached {
            goal: "reproduce 1 defect".to_owned()
        }
        .is_complete());
        assert!(!StopReason::BudgetExhausted(BudgetDimension::WallTime).is_complete());
        assert!(!StopReason::Cancelled.is_complete());
        assert!(!StopReason::Failed {
            reason: "boom".to_owned()
        }
        .is_complete());
    }

    #[test]
    fn dimensions_have_stable_identifiers() {
        // 标识会进轨迹与审计记录，不得随意更改。
        assert_eq!(BudgetDimension::WallTime.as_str(), "wall_time");
        assert_eq!(BudgetDimension::ToolCalls.as_str(), "tool_calls");
        assert_eq!(BudgetDimension::SubprocessTime.as_str(), "subprocess_time");
        assert_eq!(BudgetDimension::ModelCost.as_str(), "model_cost");
    }
}
