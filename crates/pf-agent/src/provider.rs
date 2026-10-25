//! 模型 Provider 接缝（需求文档 8.3「LLM Provider Adapter」）。
//!
//! # 为什么有接缝但没有外部实现
//!
//! 需求文档 8.3 列出「LLM Provider Adapter」作为 Runtime 核心能力之一。但阶段四
//! 的验收要求是「能够根据程序结构和测试反馈动态调整验证任务，而非只执行固定
//! 测试列表」——这件事由 [`crate::planner`] 与 [`crate::strategy`] 完成，不需要
//! 模型参与。
//!
//! 因此这里定义接缝与一个内置的、完全确定的结构化 Provider，**不**引入任何
//! 外部 API 依赖。理由有三：
//!
//! 1. 测试必须确定（AGENTS.md 第 5 节）：调用外部模型的测试既不确定也不可重放。
//! 2. 安全红线 2 禁止把凭据传进子进程；一个需要 API 密钥的默认 Provider 会让
//!    「默认配置」变成「需要密钥」。
//! 3. 提示词注入（9.3）要求权限由 Runtime 强制。在没有 Provider 的情况下先把
//!    这条约束做实，比先接上模型再补约束安全。
//!
//! 接外部 Provider 时只需要实现本 trait，并在 [`crate::AgentConfig`] 里显式启用。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 一次模型补全请求。
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// 系统提示。
    pub system: String,
    /// 用户提示（已由 Runtime 组装上下文）。
    pub user: String,
    /// 期望的结构化输出 Schema（`None` 表示自由文本）。
    pub response_schema: Option<Value>,
}

/// 一次模型补全结果。
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    /// 结构化结果（若请求了 Schema）。
    pub payload: Option<Value>,
    /// 自由文本部分。
    pub text: String,
    /// 本次调用的费用（微美元）。内置 Provider 恒为 0。
    pub cost_micros: u64,
}

/// 模型 Provider。
///
/// 实现方负责把请求发给具体的模型服务；Runtime 负责**调用前后**的一切：
/// 预算扣减、权限检查、轨迹记录、超时与取消。
pub trait LlmProvider: Send + Sync {
    /// Provider 名称，进轨迹。
    fn name(&self) -> &'static str;

    /// 是否可用（例如已配置密钥）。
    ///
    /// 返回 `false` 时 Runtime 不会尝试调用，而是退回内置的结构化路径——绝不
    /// 因为「模型没配好」就让整次会话失败。
    fn is_available(&self) -> bool;

    /// 发起一次补全。
    ///
    /// # Errors
    /// 网络失败、模型返回不可解析、或超出 Provider 自己的限额时返回
    /// [`ProviderError`]。
    fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, ProviderError>;
}

/// Provider 调用失败。
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Provider 未配置或不可用。
    #[error("provider `{name}` is not available: {reason}")]
    Unavailable {
        /// Provider 名。
        name: String,
        /// 原因（缺密钥、缺端点等）。
        reason: String,
    },

    /// 模型返回了无法解析的内容。
    #[error("provider `{name}` returned an unparseable response: {reason}")]
    MalformedResponse {
        /// Provider 名。
        name: String,
        /// 原因。
        reason: String,
    },

    /// 调用超时或被取消。
    #[error("provider `{name}` timed out or was cancelled")]
    Cancelled {
        /// Provider 名。
        name: String,
    },
}

/// 内置的结构化 Provider：不调用任何模型，按确定规则产出计划。
///
/// 它存在的意义是让 Runtime 在**没有模型**的情况下也完整可跑、可测、可复现。
/// 它返回的结果标注为 [`ToolConfidence::Inferred`] 级别的推断，而不是观测——
/// 因为「该测哪个函数」始终是启发式判断，不是事实。
#[derive(Debug, Default, Clone, Copy)]
pub struct StructuralProvider;

impl StructuralProvider {
    /// 构造一个实例。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl LlmProvider for StructuralProvider {
    fn name(&self) -> &'static str {
        "structural"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn complete(&self, _request: &CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        // 内置 Provider 不产出自由文本：它的「判断」由 planner 直接以类型化
        // 数据表达，绕一层 JSON 只会增加解析失败的可能。
        Ok(CompletionResponse {
            payload: None,
            text: String::new(),
            cost_micros: 0,
        })
    }
}

/// 会话使用的模型策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPolicy {
    /// 只用内置结构化路径（默认，零费用、完全确定）。
    #[default]
    StructuralOnly,
    /// 优先用外部 Provider，不可用时退回结构化路径。
    PreferExternal,
}

impl ModelPolicy {
    /// 机器可读标识。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StructuralOnly => "structural_only",
            Self::PreferExternal => "prefer_external",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_structural_provider_is_always_available_and_free() {
        let provider = StructuralProvider::new();
        assert!(provider.is_available());
        let response = provider
            .complete(&CompletionRequest {
                system: "s".to_owned(),
                user: "u".to_owned(),
                response_schema: None,
            })
            .expect("complete");
        assert_eq!(response.cost_micros, 0, "内置 Provider 不得产生费用");
        assert!(response.payload.is_none());
        assert_eq!(provider.name(), "structural");
    }

    #[test]
    fn the_default_policy_costs_nothing() {
        // 默认策略必须是不花钱的那一档：否则「忘记配置」会变成「开始计费」。
        assert_eq!(ModelPolicy::default(), ModelPolicy::StructuralOnly);
        assert_eq!(ModelPolicy::StructuralOnly.as_str(), "structural_only");
        assert_eq!(ModelPolicy::PreferExternal.as_str(), "prefer_external");
    }
}
