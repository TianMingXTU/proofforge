//! `ProofForge` Agent Runtime：工具注册与权限、验证计划、策略调度、执行预算与轨迹。
//!
//! 对应需求文档第八章与阶段四（FR-044 … FR-049）。
//!
//! # 它解决什么问题
//!
//! 阶段三的 `pf-verify` 能跑**给定**的性质。阶段四要回答的是另一个问题：该跑哪些
//! 性质、先跑哪个、什么时候停、以及为什么这么选。这三件事分别由
//! [`planner`]、[`strategy`] 与 [`budget`] 承担，全部由 [`runner`] 串成一个带
//! 状态机的循环。
//!
//! # 三条不可谈判的约束
//!
//! 1. **权限由 Runtime 强制**（9.3）。仓库内容是不可信输入，其中的「忽略系统
//!    规则」不得改变权限。因此检查发生在模型输出之后、工具执行之前的 Rust
//!    代码里，而不是写在提示词里。
//! 2. **没有 Shell 工具**（红线 1）。所有外部执行都经 [`pf_adapters::ToolAdapter`]
//!    生成命令记录，Agent 只能选「哪个适配器的哪个工具」。
//! 3. **不完整不等于通过**（红线 4）。预算耗尽、取消、失败一律让门禁返回证据
//!    不足，即使前面已经跑完的任务全都通过。
//!
//! # 关于模型
//!
//! Runtime 定义 [`provider::LlmProvider`] 接缝，但**不内置**任何外部模型调用：
//! 没有网络依赖、不需要 API 密钥、测试因此确定。默认的计划器
//! （[`planner::StructuralPlanner`]）完全由静态结构驱动，这已经满足阶段四的验收
//! 要求——「根据程序结构和测试反馈动态调整验证任务」。接外部 Provider 是后续的
//! 增量，不需要改 Runtime 的任何一条约束。

#![warn(missing_docs)]

pub mod budget;
pub mod feedback;
pub mod plan;
pub mod provider;
pub mod runner;
pub mod state;
pub mod strategy;
pub mod tool;
pub mod tools;
pub mod trace;

pub use budget::{BudgetDimension, BudgetTracker, BudgetUsage, ExecutionBudget, StopReason};
pub use feedback::{FeedbackEngine, FeedbackUpdate};
pub use plan::{
    CandidateEvidence, PlanCandidate, StructuralPlanner, TaskKind, VerificationPlan,
    VerificationTask,
};
pub use provider::{LlmProvider, ModelPolicy, StructuralProvider};
pub use runner::{run, AgentConfig, AgentRunError, AgentSession};
pub use state::{AgentState, StateMachine};
pub use strategy::{StrategyChoice, StrategyEngine};
pub use tool::{Permission, SideEffect, Tool, ToolConfidence, ToolError, ToolOutput, ToolSchema};
pub use trace::{ExecutionTrace, TraceEntry, TraceKind};
