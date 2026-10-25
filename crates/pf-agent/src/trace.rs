//! 执行轨迹（FR-049）。
//!
//! 需求文档 FR-049 要求保存「验证策略、选择依据、反馈和成本数据」。这条要求
//! 的落点不是一份总结报告，而是**逐条可追溯的记录**：为什么选了这个任务、依据
//! 是什么、花了多少、结果如何。
//!
//! # 为什么轨迹不可变
//!
//! [`ExecutionTrace::record`] 只追加，不提供修改入口。轨迹的用途是事后审计
//! 「当时为什么这么做」；一个能被改写的轨迹做不到这件事。会话结束后整个结构
//! 被序列化落盘，与 [`pf_core::evidence::EvidenceArtifact`] 一样靠内容哈希校验。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::budget::BudgetUsage;

/// 一条轨迹记录的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    /// 状态转移。
    StateChange,
    /// 策略选择了一个任务（含依据）。
    StrategyChoice,
    /// 一次工具调用。
    ToolCall,
    /// 一次反馈更新（优先级调整）。
    FeedbackUpdate,
    /// 预算检查。
    BudgetCheck,
    /// 人工批准请求或结果。
    Approval,
    /// 会话结束。
    SessionEnd,
}

impl TraceKind {
    /// 机器可读标识。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StateChange => "state_change",
            Self::StrategyChoice => "strategy_choice",
            Self::ToolCall => "tool_call",
            Self::FeedbackUpdate => "feedback_update",
            Self::BudgetCheck => "budget_check",
            Self::Approval => "approval",
            Self::SessionEnd => "session_end",
        }
    }
}

/// 一条轨迹记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEntry {
    /// 记录类型。
    pub kind: TraceKind,
    /// 人类可读的一行说明。
    pub message: String,
    /// 结构化细节（任务名、工具名、参数摘要、优先级变化等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    /// 记录时的预算用量快照。
    pub usage: BudgetUsage,
    /// 记录时的会话状态。
    pub state: crate::state::AgentState,
}

/// 一次会话的执行轨迹。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionTrace {
    entries: Vec<TraceEntry>,
}

impl ExecutionTrace {
    /// 空轨迹。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 追加一条记录。
    ///
    /// 只追加，不修改：轨迹的用途是事后审计「当时为什么这么做」，一个能被改写
    /// 的轨迹做不到这件事。
    pub fn record(&mut self, entry: TraceEntry) {
        self.entries.push(entry);
    }

    /// 全部记录（按时间顺序）。
    #[must_use]
    pub fn entries(&self) -> &[TraceEntry] {
        &self.entries
    }

    /// 记录条数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否没有任何记录。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 某类记录的条数。
    #[must_use]
    pub fn count(&self, kind: TraceKind) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.kind == kind)
            .count()
    }

    /// 序列化成 JSON 文本。
    ///
    /// # Errors
    /// 序列化失败时返回 [`pf_core::Error::Serialization`]。轨迹里只有自有字段、
    /// 字符串与 `serde_json::Value`，正常情况下不会失败。
    pub fn to_json(&self) -> Result<String, pf_core::Error> {
        serde_json::to_string_pretty(&self.entries).map_err(|reason| {
            pf_core::Error::Serialization {
                reason: reason.to_string(),
            }
        })
    }

    /// 渲染成人类可读的轨迹日志。
    #[must_use]
    pub fn render_terminal(&self) -> String {
        let mut out = String::from("`ProofForge` Agent 执行轨迹\n\n");
        if self.entries.is_empty() {
            out.push_str("（空）\n");
            return out;
        }
        for entry in &self.entries {
            out.push_str(&format!(
                "[{:>9}] {:<16} {}\n",
                entry.state.as_str(),
                entry.kind.as_str(),
                entry.message
            ));
            out.push_str(&format!(
                "            calls={} subprocess={}s wall={}s\n",
                entry.usage.tool_calls, entry.usage.subprocess_secs, entry.usage.wall_time_secs
            ));
            if let Some(detail) = &entry.detail {
                out.push_str(&format!(
                    "            detail={}\n",
                    serde_json::to_string(detail).unwrap_or_default()
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetTracker;
    use crate::state::{AgentState, StateMachine};

    fn entry(kind: TraceKind, message: &str) -> TraceEntry {
        TraceEntry {
            kind,
            message: message.to_owned(),
            detail: None,
            usage: BudgetTracker::new(crate::budget::ExecutionBudget::default()).usage(),
            state: AgentState::Created,
        }
    }

    #[test]
    fn records_are_appended_in_order_and_never_rewritten() {
        let mut trace = ExecutionTrace::new();
        assert!(trace.is_empty());
        trace.record(entry(TraceKind::StateChange, "context ready"));
        trace.record(entry(TraceKind::StrategyChoice, "pick run_tests"));
        assert_eq!(trace.len(), 2);
        assert_eq!(trace.entries()[0].message, "context ready");
        assert_eq!(trace.entries()[1].message, "pick run_tests");
        assert_eq!(trace.count(TraceKind::StrategyChoice), 1);
    }

    #[test]
    fn a_state_change_carries_the_state_it_happened_in() {
        // 轨迹要说清「在哪个状态下做的决定」，否则「为什么这么选」无法复盘。
        let mut machine = StateMachine::new();
        machine.context_ready().expect("context_ready");
        let mut trace = ExecutionTrace::new();
        trace.record(TraceEntry {
            kind: TraceKind::StateChange,
            message: "context ready".to_owned(),
            detail: None,
            usage: BudgetTracker::new(crate::budget::ExecutionBudget::default()).usage(),
            state: machine.current(),
        });
        assert_eq!(trace.entries()[0].state, AgentState::ContextReady);
    }

    #[test]
    fn json_round_trips_every_field() {
        let mut trace = ExecutionTrace::new();
        trace.record(TraceEntry {
            kind: TraceKind::ToolCall,
            message: "ran cargo test".to_owned(),
            detail: Some(serde_json::json!({"tool": "run_existing_tests", "exit": 101})),
            usage: BudgetTracker::new(crate::budget::ExecutionBudget::default()).usage(),
            state: AgentState::Verifying,
        });
        let json = trace.to_json().expect("serialize");
        let parsed: Vec<TraceEntry> = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed, trace.entries());
    }

    #[test]
    fn terminal_rendering_mentions_the_state_and_cost() {
        let mut trace = ExecutionTrace::new();
        trace.record(entry(TraceKind::StrategyChoice, "pick verify parse"));
        let text = trace.render_terminal();
        assert!(text.contains("strategy_choice"), "{text}");
        assert!(text.contains("pick verify parse"), "{text}");
        assert!(text.contains("calls="), "{text}");
    }

    #[test]
    fn an_empty_trace_renders_as_empty() {
        assert!(ExecutionTrace::new().render_terminal().contains("（空）"));
    }

    #[test]
    fn trace_kind_identifiers_are_stable() {
        assert_eq!(TraceKind::StateChange.as_str(), "state_change");
        assert_eq!(TraceKind::StrategyChoice.as_str(), "strategy_choice");
        assert_eq!(TraceKind::FeedbackUpdate.as_str(), "feedback_update");
        assert_eq!(TraceKind::SessionEnd.as_str(), "session_end");
    }
}
