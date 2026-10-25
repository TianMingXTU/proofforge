//! Agent 会话状态机（需求文档 8.5）。
//!
//! # 为什么状态必须持久化
//!
//! 需求文档 8.5 要求「状态必须持久化」，并且「任务发生错误后，不得将不完整结果
//! 记录为成功验收」。第二条落地成一条硬规则：**只有 [`AgentState::Completed`]
//! 能被读成成功**。`PartiallyCompleted`、`BudgetExhausted`、`Failed`、`Cancelled`
//! 全都让门禁返回证据不足——哪怕前面已经跑完的任务全部通过。
//!
//! # 状态转移为什么集中在一处
//!
//! 每个 `fn to_*` 都返回 `Result` 而不是直接改字段。非法转移是**编程错误**，
//! 必须在开发期暴露；如果允许任意赋值，一个漏掉的分支就会把 `Failed` 会话
//! 推回 `Verifying`，而那种会话的结论是不可信的。

use serde::{Deserialize, Serialize};

/// Agent 会话的状态。
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// 已创建，尚未读取上下文。
    #[default]
    Created,
    /// 上下文已就绪（目标快照、符号索引、静态发现已装入）。
    ContextReady,
    /// 正在分析代码结构与风险。
    Analyzing,
    /// 正在生成或调整验证计划。
    Planning,
    /// 正在执行验证任务。
    Verifying,
    /// 正在评估结果并更新反馈。
    Evaluating,
    /// 正在汇总报告。
    Reporting,
    /// 计划全部完成，结论完整。
    Completed,
    /// 部分完成：停在了非终点状态上，结论不完整。
    PartiallyCompleted,
    /// 预算耗尽。
    BudgetExhausted,
    /// 被用户或外部取消。
    Cancelled,
    /// 出现无法继续的异常。
    Failed,
    /// 等待人工批准（例如补丁应用）。
    WaitingApproval,
}

impl AgentState {
    /// 机器可读标识。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::ContextReady => "context_ready",
            Self::Analyzing => "analyzing",
            Self::Planning => "planning",
            Self::Verifying => "verifying",
            Self::Evaluating => "evaluating",
            Self::Reporting => "reporting",
            Self::Completed => "completed",
            Self::PartiallyCompleted => "partially_completed",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::WaitingApproval => "waiting_approval",
        }
    }

    /// 是否已到达不再变化的终态。
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::PartiallyCompleted
                | Self::BudgetExhausted
                | Self::Cancelled
                | Self::Failed
        )
    }

    /// 该状态是否代表「这次会话给出了完整结论」。
    ///
    /// 只有 [`Self::Completed`]。其余终态都让门禁返回证据不足——这是红线 4 的
    /// 直接落地：没跑完的检查与通过的检查是两件事。
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Completed)
    }

    /// 是否处于活跃执行中（还能继续推进）。
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Created
                | Self::ContextReady
                | Self::Analyzing
                | Self::Planning
                | Self::Verifying
                | Self::Evaluating
                | Self::Reporting
        )
    }
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次非法的状态转移。
#[derive(Debug, thiserror::Error)]
#[error("illegal state transition: {from} -> {to}")]
pub struct IllegalTransition {
    /// 源状态。
    pub from: AgentState,
    /// 目标状态。
    pub to: AgentState,
}

/// 会话状态机。
///
/// 只暴露具名转移方法，不暴露 `set`：这样「哪些路径合法」在类型上就是显式的，
/// 而不是散落在各处的字符串比较里。
#[derive(Debug, Clone, Default)]
pub struct StateMachine {
    current: AgentState,
}

impl StateMachine {
    /// 从 [`AgentState::Created`] 开始。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current: AgentState::Created,
        }
    }

    /// 当前状态。
    #[must_use]
    pub const fn current(&self) -> AgentState {
        self.current
    }

    /// 转移到一个新状态，非法转移返回错误。
    ///
    /// # Errors
    /// 源状态已是终态、或该转移不在允许集合里时返回 [`IllegalTransition`]。
    pub fn transition(&mut self, to: AgentState) -> Result<AgentState, IllegalTransition> {
        if !Self::allows(self.current, to) {
            return Err(IllegalTransition {
                from: self.current,
                to,
            });
        }
        self.current = to;
        Ok(to)
    }

    /// 上下文就绪。
    ///
    /// # Errors
    /// 当前不在 [`AgentState::Created`] 时返回 [`IllegalTransition`]。
    pub fn context_ready(&mut self) -> Result<AgentState, IllegalTransition> {
        self.transition(AgentState::ContextReady)
    }

    /// 进入分析。
    ///
    /// # Errors
    /// 当前不是 `ContextReady` 或 `Evaluating` 时返回 [`IllegalTransition`]。
    pub fn analyzing(&mut self) -> Result<AgentState, IllegalTransition> {
        self.transition(AgentState::Analyzing)
    }

    /// 进入计划。
    ///
    /// # Errors
    /// 当前不是 `Analyzing` 或 `Evaluating` 时返回 [`IllegalTransition`]。
    pub fn planning(&mut self) -> Result<AgentState, IllegalTransition> {
        self.transition(AgentState::Planning)
    }

    /// 进入验证。
    ///
    /// # Errors
    /// 当前不是 `Planning` 或 `Evaluating` 时返回 [`IllegalTransition`]。
    pub fn verifying(&mut self) -> Result<AgentState, IllegalTransition> {
        self.transition(AgentState::Verifying)
    }

    /// 进入评估。
    ///
    /// # Errors
    /// 当前不是 `Verifying` 时返回 [`IllegalTransition`]。
    pub fn evaluating(&mut self) -> Result<AgentState, IllegalTransition> {
        self.transition(AgentState::Evaluating)
    }

    /// 进入报告。
    ///
    /// # Errors
    /// 当前不是 `Evaluating` 时返回 [`IllegalTransition`]。
    pub fn reporting(&mut self) -> Result<AgentState, IllegalTransition> {
        self.transition(AgentState::Reporting)
    }

    /// 正常完成。
    ///
    /// # Errors
    /// 当前不是 `Reporting` 时返回 [`IllegalTransition`]。
    pub fn complete(&mut self) -> Result<AgentState, IllegalTransition> {
        self.transition(AgentState::Completed)
    }

    /// 进入等待人工批准。
    ///
    /// 可以从任何活跃状态进入：一个需要批准的请求不挑时机。
    ///
    /// # Errors
    /// 当前已是终态时返回 [`IllegalTransition`]。
    pub fn wait_for_approval(&mut self) -> Result<AgentState, IllegalTransition> {
        self.transition(AgentState::WaitingApproval)
    }

    /// 从等待批准回到执行。
    ///
    /// # Errors
    /// 当前不是 `WaitingApproval` 时返回 [`IllegalTransition`]。
    pub fn resume(&mut self, to: AgentState) -> Result<AgentState, IllegalTransition> {
        self.transition(to)
    }

    /// 以一个不完整的终态收尾。
    ///
    /// 五个不完整终态（部分完成、预算耗尽、取消、失败）都可以从任何活跃状态或
    /// 等待批准进入：错误与取消不挑时机，而「挑时机」会让取消请求被拒绝。
    ///
    /// # Errors
    /// 当前已是终态时返回 [`IllegalTransition`]。
    pub fn abort(&mut self, to: AgentState) -> Result<AgentState, IllegalTransition> {
        if !to.is_terminal() || to == AgentState::Completed {
            return Err(IllegalTransition {
                from: self.current,
                to,
            });
        }
        self.transition(to)
    }

    /// 转移是否合法。
    ///
    /// 用显式表而不是「活跃状态之间随便走」：后者会让 `Created` 直接跳到
    /// `Verifying`——上下文没读就跑验证，跑的是空气，而报告上看不出来。
    fn allows(from: AgentState, to: AgentState) -> bool {
        // 五个不完整终态可以从任何非终态进入：取消与失败不挑时机。
        if matches!(
            to,
            AgentState::PartiallyCompleted
                | AgentState::BudgetExhausted
                | AgentState::Cancelled
                | AgentState::Failed
        ) {
            return from.is_active() || from == AgentState::WaitingApproval;
        }
        match from {
            AgentState::Created => matches!(to, AgentState::ContextReady),
            AgentState::ContextReady => {
                matches!(to, AgentState::Analyzing | AgentState::WaitingApproval)
            }
            AgentState::Analyzing => {
                matches!(to, AgentState::Planning | AgentState::WaitingApproval)
            }
            // 计划阶段可以回到分析：反馈表明计划本身需要修正。
            AgentState::Planning => matches!(
                to,
                AgentState::Verifying | AgentState::Analyzing | AgentState::WaitingApproval
            ),
            AgentState::Verifying => {
                matches!(to, AgentState::Evaluating | AgentState::WaitingApproval)
            }
            // 评估阶段是反馈回路的入口：要么调整计划，要么进入报告。
            AgentState::Evaluating => matches!(
                to,
                AgentState::Planning | AgentState::Reporting | AgentState::WaitingApproval
            ),
            AgentState::Reporting => {
                matches!(to, AgentState::Completed | AgentState::WaitingApproval)
            }
            AgentState::WaitingApproval => matches!(
                to,
                AgentState::Analyzing
                    | AgentState::Planning
                    | AgentState::Verifying
                    | AgentState::Evaluating
                    | AgentState::Reporting
            ),
            AgentState::Completed
            | AgentState::PartiallyCompleted
            | AgentState::BudgetExhausted
            | AgentState::Cancelled
            | AgentState::Failed => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_happy_path_walks_the_documented_sequence() {
        let mut machine = StateMachine::new();
        assert_eq!(machine.current(), AgentState::Created);
        machine.context_ready().expect("context_ready");
        machine.analyzing().expect("analyzing");
        machine.planning().expect("planning");
        machine.verifying().expect("verifying");
        machine.evaluating().expect("evaluating");
        // 反馈回路：评估后回到计划调整下一轮，而不是只能一路向前。
        machine.planning().expect("re-plan");
        machine.verifying().expect("re-verify");
        machine.evaluating().expect("re-evaluate");
        machine.reporting().expect("reporting");
        machine.complete().expect("complete");
        assert!(machine.current().is_terminal());
        assert!(machine.current().is_success());
    }

    #[test]
    fn a_terminal_state_never_moves_again() {
        // 一个已经失败的会话不能被推回 Verifying：它的结论建立在中断的执行上。
        let mut machine = StateMachine::new();
        machine.abort(AgentState::Failed).expect("fail");
        assert!(machine.transition(AgentState::Verifying).is_err());
        assert!(machine.transition(AgentState::Completed).is_err());
        assert_eq!(machine.current(), AgentState::Failed);
    }

    #[test]
    fn completed_is_the_only_success_state() {
        // 红线 4：没跑完不等于通过。
        for state in [
            AgentState::PartiallyCompleted,
            AgentState::BudgetExhausted,
            AgentState::Cancelled,
            AgentState::Failed,
        ] {
            assert!(state.is_terminal(), "{state} should be terminal");
            assert!(!state.is_success(), "{state} must not count as success");
        }
        assert!(AgentState::Completed.is_success());
        assert!(AgentState::Verifying.is_active());
        assert!(!AgentState::Verifying.is_terminal());
    }

    #[test]
    fn waiting_for_approval_suspends_and_resumes() {
        let mut machine = StateMachine::new();
        machine.context_ready().expect("context_ready");
        machine.analyzing().expect("analyzing");
        machine.planning().expect("planning");
        machine.verifying().expect("verifying");
        machine.wait_for_approval().expect("wait");
        assert!(!machine.current().is_active(), "等待批准时不应算活跃");
        machine.resume(AgentState::Verifying).expect("resume");
        assert_eq!(machine.current(), AgentState::Verifying);
    }

    #[test]
    fn resume_only_works_from_the_approval_state() {
        let mut machine = StateMachine::new();
        machine.context_ready().expect("context_ready");
        assert!(machine.resume(AgentState::Verifying).is_err());
    }

    #[test]
    fn any_active_state_can_be_aborted() {
        // 取消与失败不挑时机——「挑时机」会让取消请求被状态机拒绝。
        for entry in [
            AgentState::ContextReady,
            AgentState::Analyzing,
            AgentState::Planning,
            AgentState::Verifying,
            AgentState::Evaluating,
            AgentState::Reporting,
        ] {
            let mut machine = StateMachine::new();
            machine.context_ready().expect("context_ready");
            if entry != AgentState::ContextReady {
                machine.analyzing().expect("analyzing");
            }
            if matches!(
                entry,
                AgentState::Planning
                    | AgentState::Verifying
                    | AgentState::Evaluating
                    | AgentState::Reporting
            ) {
                machine.planning().expect("planning");
            }
            if matches!(
                entry,
                AgentState::Verifying | AgentState::Evaluating | AgentState::Reporting
            ) {
                machine.verifying().expect("verifying");
            }
            if matches!(entry, AgentState::Evaluating | AgentState::Reporting) {
                machine.evaluating().expect("evaluating");
            }
            if entry == AgentState::Reporting {
                machine.reporting().expect("reporting");
            }
            machine.abort(AgentState::Cancelled).expect("cancel");
            assert_eq!(machine.current(), AgentState::Cancelled);
        }
    }

    #[test]
    fn abort_rejects_the_success_state() {
        // `complete` 只能从 Reporting 走：否则任何地方都能宣布成功。
        let mut machine = StateMachine::new();
        assert!(machine.abort(AgentState::Completed).is_err());
        machine.context_ready().expect("context_ready");
        assert!(machine.abort(AgentState::Completed).is_err());
    }

    #[test]
    fn skipping_ahead_is_rejected() {
        let mut machine = StateMachine::new();
        // Created 不能直接跳到 Verifying：上下文没读就跑验证，跑的是空气。
        assert!(machine.verifying().is_err());
        assert!(machine.complete().is_err());
        assert_eq!(machine.current(), AgentState::Created);
    }

    #[test]
    fn state_identifiers_are_stable() {
        // 标识会进轨迹与审计记录。
        assert_eq!(AgentState::Created.as_str(), "created");
        assert_eq!(AgentState::ContextReady.as_str(), "context_ready");
        assert_eq!(AgentState::WaitingApproval.as_str(), "waiting_approval");
        assert_eq!(
            AgentState::PartiallyCompleted.as_str(),
            "partially_completed"
        );
        assert_eq!(AgentState::BudgetExhausted.as_str(), "budget_exhausted");
    }
}
