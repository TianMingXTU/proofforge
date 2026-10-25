//! 工具契约与权限模型（需求文档 8.3、8.4；AGENTS.md 安全红线 1）。
//!
//! # 为什么权限由 Runtime 强制，而不是写在提示词里
//!
//! 需求文档 9.3 把仓库文件、README、代码注释与测试数据全部视为**不可信输入**。
//! 其中出现的「忽略系统规则」「读取密钥」不得改变 Agent 的权限。这意味着权限
//! 检查必须发生在模型输出**之后**、工具执行**之前**的那一层 Rust 代码里——写在
//! 系统提示词里的约束是可以被注入内容覆盖的，写在 [`crate::ToolRegistry`] 里的
//! 不行。
//!
//! # 为什么没有 Shell 工具
//!
//! 初期工具表（8.4）里没有任何一项是「执行任意命令」。`run_static_check`、
//! `run_existing_tests`、`run_verification` 全部经由 [`pf_adapters::ToolAdapter`]
//! 生成命令记录，再由 [`pf_adapters::run_tool`] 执行。Agent 能选的只是「哪个
//! 适配器的哪个工具、超时多少」，不是命令行字符串。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 工具执行所需的权限级别。
///
/// 级别是**有序的**：调用方持有 `Execute` 时不会自动获得 `Write`。排序依据是
/// 「出了事能不能撤」——读错可以重读，跑错可以重跑，写错会留下痕迹。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// 只读取目标项目内已被索引的内容。
    Read,
    /// 运行外部工具（静态检查、测试、验证）。
    ///
    /// 会启动子进程，因此需要超时与凭据剔除——这两件事由
    /// [`pf_adapters`] 统一施加，工具实现无法绕过。
    Execute,
    /// 写入独立工作区（沙箱 / 临时目录）。
    ///
    /// **不包含**写入被审查仓库：那条路径需要 [`Permission::Mutate`] 加人工批准。
    Write,
    /// 修改被审查仓库的文件。
    ///
    /// 这是最高级别，且只能由人显式授予（FR-055 / FR-056）。Runtime 在分发
    /// 这类工具前会把会话切到 [`crate::AgentState::WaitingApproval`]。
    Mutate,
}

impl Permission {
    /// 机器可读标识。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Execute => "execute",
            Self::Write => "write",
            Self::Mutate => "mutate",
        }
    }

    /// 该权限是否会被「默认拒绝」策略挡住。
    ///
    /// [`Self::Read`] 是分析类会话的默认许可；其余都要在 [`crate::AgentConfig`]
    /// 里显式打开。默认值取最窄的一档，这样「忘记授权」的表现是「工具不可用」，
    /// 而不是「工具想干嘛干嘛」。
    #[must_use]
    pub const fn requires_explicit_grant(self) -> bool {
        !matches!(self, Self::Read)
    }
}

/// 工具的副作用类型。
///
/// 声明副作用的目的是让**计划器**能判断一个任务是否可重放：纯读取的任务失败
/// 了可以原样重试，产生过副作用的任务重试前必须先清理工作区。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffect {
    /// 不改变任何可见状态。
    None,
    /// 在独立工作区里写文件。
    SandboxWrite,
    /// 运行外部进程。
    ProcessSpawn,
    /// 追加证据记录。
    EvidenceAppend,
    /// 修改被审查仓库（必须经人工批准）。
    RepositoryMutate,
}

/// 工具的输入 Schema。
///
/// 用 JSON Schema 的子集表达：当前只用到 `type`、`properties`、`required` 与
/// `additionalProperties`。不引入完整的 JSON Schema 校验库——工具的输入由
/// Runtime 自己构造， schema 的用途是**文档与契约测试**，不是防御模型乱说。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Schema 标识（`object` 之外的根类型当前不支持）。
    pub schema_type: String,
    /// 每个输入字段的类型名。
    pub properties: Vec<(String, String)>,
    /// 必填字段。
    pub required: Vec<String>,
}

impl ToolSchema {
    /// 构造一个只有必填字段的对象 Schema。
    #[must_use]
    pub fn object(fields: &[(&str, &str)], required: &[&str]) -> Self {
        Self {
            schema_type: "object".to_owned(),
            properties: fields
                .iter()
                .map(|(name, kind)| ((*name).to_owned(), (*kind).to_owned()))
                .collect(),
            required: required.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    /// 按 Schema 校验一份输入。
    ///
    /// # Errors
    /// 输入不是对象、缺少必填字段、字段类型不符，或出现 Schema 未声明的字段时
    /// 返回 [`ToolError::InvalidInput`]。错误信息指出具体字段——调用方需要知道
    /// 改哪里，而不只是「不合法」。
    pub fn validate(&self, input: &Value) -> Result<(), ToolError> {
        let object = input
            .as_object()
            .ok_or_else(|| ToolError::invalid_input("schema", "expected a JSON object"))?;
        for name in &self.required {
            if !object.contains_key(name) {
                return Err(ToolError::invalid_input(
                    "schema",
                    format!("missing field `{name}`"),
                ));
            }
        }
        for (name, kind) in &self.properties {
            let Some(value) = object.get(name) else {
                continue;
            };
            if !value_matches_kind(value, kind) {
                return Err(ToolError::invalid_input(
                    "schema",
                    format!("field `{name}` should be {kind}, got {}", value_kind(value)),
                ));
            }
        }
        for name in object.keys() {
            if !self.properties.iter().any(|(field, _)| field == name) {
                return Err(ToolError::invalid_input(
                    "schema",
                    format!(
                        "unexpected field `{name}`; declared fields are {}",
                        self.properties
                            .iter()
                            .map(|(field, _)| field.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// JSON 值的类型名，用于错误信息。
fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 值是否符合声明的类型名。
fn value_matches_kind(value: &Value, kind: &str) -> bool {
    match kind {
        // `integer` 与 `number` 都接受 JSON 数字：serde_json 不区分两者，
        // 而在输入端做 1.0 vs 1 的区分只会把问题推给调用方。
        "number" | "integer" => value.is_number(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    }
}

/// 工具调用结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// 结构化结果，供后续工具与计划器消费。
    pub payload: Value,
    /// 给模型看的人类可读摘要。
    pub summary: String,
    /// 结果可信度。
    ///
    /// 与 [`pf_core::status::EvidenceStatus`] 对应：工具报告的是**观测**还是
    /// **推断**必须写清楚，否则计划器会把猜测当成事实排进下一轮。
    pub confidence: ToolConfidence,
    /// 本次调用产生的副作用描述（若有），用于轨迹与重放判断。
    pub side_effects: Vec<String>,
}

/// 工具结果的可信度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolConfidence {
    /// 直接观测到的事实（文件内容、进程退出码）。
    Observed,
    /// 由观测推导出的结论（启发式匹配、相似度排名）。
    Inferred,
}

impl std::fmt::Display for Permission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 工具契约：一个可被 Agent 调用的能力。
///
/// 实现方只需要提供 [`Self::call`]；Schema、权限、超时与副作用都在注册时声明，
/// 由 Runtime 强制执行。这样「工具作者忘记检查权限」不会变成安全漏洞。
pub trait Tool: Send + Sync {
    /// 工具名（稳定标识，出现在轨迹与审计记录里）。
    fn name(&self) -> &'static str;

    /// 工具用途说明，进上下文给模型看。
    fn description(&self) -> &'static str;

    /// 输入 Schema。
    fn input_schema(&self) -> ToolSchema;

    /// 所需权限。
    fn permission(&self) -> Permission;

    /// 副作用类型。
    fn side_effect(&self) -> SideEffect;

    /// 建议超时（秒）。Runtime 会在执行预算内取两者较小值。
    fn suggested_timeout_secs(&self) -> u64 {
        60
    }

    /// 执行工具。
    ///
    /// # Errors
    /// 输入不合法、底层能力失败（读不到文件、工具缺失）时返回 [`ToolError`]。
    /// **不要**在这里做权限判断——Runtime 在调用之前已经查过。
    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError>;
}

/// 工具调用失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// 输入不满足 Schema。
    #[error("invalid input for `{tool}`: {reason}")]
    InvalidInput {
        /// 工具名。
        tool: String,
        /// 具体原因（哪个字段、期望什么）。
        reason: String,
    },

    /// 调用方没有所需权限。
    #[error("tool `{tool}` requires {permission} permission, but this session only holds {held}")]
    PermissionDenied {
        /// 工具名。
        tool: String,
        /// 会话实际持有的权限。
        held: Permission,
        /// 工具所需的权限。
        permission: Permission,
    },

    /// 底层能力失败（文件读不到、外部工具缺失、超时）。
    #[error("tool `{tool}` failed: {reason}")]
    Execution {
        /// 工具名。
        tool: String,
        /// 失败原因，面向使用者。
        reason: String,
    },
}

impl ToolError {
    /// 构造一个输入错误。
    pub(crate) fn invalid_input(tool: &str, reason: impl Into<String>) -> Self {
        Self::InvalidInput {
            tool: tool.to_owned(),
            reason: reason.into(),
        }
    }

    /// 构造一个权限错误。
    ///
    /// 单独一个构造器而不是让调用方拼装：权限检查必须只有一个入口，否则
    /// 「忘了检查」会以「构造了一个别的错误变体」的形式出现。
    pub(crate) fn permission_denied(tool: &str, held: Permission, required: Permission) -> Self {
        Self::PermissionDenied {
            tool: tool.to_owned(),
            held,
            permission: required,
        }
    }

    /// 构造一个执行错误。
    pub(crate) fn execution_failed(tool: &str, reason: impl Into<String>) -> Self {
        Self::Execution {
            tool: tool.to_owned(),
            reason: reason.into(),
        }
    }

    /// 构造一个「工具不存在」错误。
    ///
    /// 归到输入错误而不是单独一个变体：模型请求一个不存在的工具，与请求一个
    /// 参数不对的工具是同一类失败——它的输出不可信，因此不能执行。
    pub(crate) fn unknown_tool(name: &str) -> Self {
        Self::InvalidInput {
            tool: name.to_owned(),
            reason: "no such tool is registered".to_owned(),
        }
    }
}
