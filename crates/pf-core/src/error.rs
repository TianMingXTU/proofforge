//! 统一错误类型。
//!
//! `ProofForge` 中「预期内的失败」使用 [`Error`] 表达（配置错误、目标路径
//! 不存在、规则集冲突等），而「不该发生的意外」使用 [`anyhow::Error`] 在
//! 二进制层兜底。遵循 Google Rust Style Guide：公开 API 返回具体错误类型，
//! 不在库代码中 `panic!`。

use std::path::PathBuf;

use thiserror::Error as ThisError;

/// 库级结果别名。
pub type Result<T> = std::result::Result<T, Error>;

/// `ProofForge` 的领域错误集合。
///
/// 每个变体都对应一类用户可理解、可操作的失败原因；错误信息面向使用者，
/// 不暴露内部实现细节。
#[derive(Debug, ThisError)]
pub enum Error {
    /// 目标路径不存在或不可访问。
    #[error("target path does not exist or is not readable: {path}")]
    TargetNotFound {
        /// 出问题的路径。
        path: PathBuf,
    },

    /// 目标不是一个目录（例如传入了单个文件，但命令要求目录）。
    #[error("expected a directory but got a file: {path}")]
    TargetNotDirectory {
        /// 出问题的路径。
        path: PathBuf,
    },

    /// 仓库无法作为 Git 仓库打开。
    #[error("failed to open git repository at {path}: {reason}")]
    RepositoryOpen {
        /// 仓库路径。
        path: PathBuf,
        /// 底层原因，已脱敏为人类可读文本。
        reason: String,
    },

    /// Git 引用（分支、tag、commit）无法解析。
    #[error("failed to resolve git revision {revision:?}: {reason}")]
    RevisionNotFound {
        /// 用户提供的引用。
        revision: String,
        /// 底层原因。
        reason: String,
    },

    /// 无法识别或打开源文件。
    #[error("failed to read source file {path}: {reason}")]
    SourceRead {
        /// 文件路径。
        path: PathBuf,
        /// 底层原因。
        reason: String,
    },

    /// 文件不是合法的 UTF-8，因此无法建立可靠的符号索引。
    #[error("source file is not valid UTF-8: {path}")]
    SourceNotUtf8 {
        /// 文件路径。
        path: PathBuf,
    },

    /// 语言不被支持。
    #[error("language {language:?} is not supported by this build")]
    LanguageUnsupported {
        /// 语言标识。
        language: String,
    },

    /// 配置解析失败。
    #[error("failed to parse configuration from {origin}: {reason}")]
    ConfigParse {
        /// 配置来源（文件路径或 `inline`）。
        ///
        /// 注意：不能命名为 `source`，否则 thiserror 会把它当作底层错误来源。
        origin: String,
        /// 底层原因。
        reason: String,
    },

    /// 规则集内部冲突。
    #[error("rule set conflict: {reason}")]
    RuleConflict {
        /// 冲突说明。
        reason: String,
    },

    /// 规则定义本身不合法。
    #[error("invalid rule definition {rule_id}: {reason}")]
    InvalidRule {
        /// 规则 ID。
        rule_id: String,
        /// 不合法原因。
        reason: String,
    },

    /// 外部工具执行失败（编译器、测试运行器等）。
    #[error("external tool {tool} failed with status {status:?}: {detail}")]
    ToolExecution {
        /// 工具名。
        tool: String,
        /// 退出状态描述。
        status: Option<i32>,
        /// 关键输出摘要。
        detail: String,
    },

    /// 外部工具不存在或不可执行。
    #[error("required external tool {tool:?} is not available on PATH")]
    ToolNotFound {
        /// 工具名。
        tool: String,
    },

    /// 预算耗尽（Token、时间、进程数等）。
    #[error("budget exhausted: {resource} (limit {limit})")]
    BudgetExhausted {
        /// 资源名称。
        resource: String,
        /// 上限值（已格式化为可读文本）。
        limit: String,
    },

    /// 操作被用户取消。
    #[error("operation cancelled by user")]
    Cancelled,

    /// 存储层失败。
    #[error("storage failure: {reason}")]
    Storage {
        /// 底层原因。
        reason: String,
    },

    /// 报告渲染失败。
    #[error("report rendering failed: {reason}")]
    ReportRender {
        /// 底层原因。
        reason: String,
    },

    /// 序列化 / 反序列化失败。
    #[error("serialization failure: {reason}")]
    Serialization {
        /// 底层原因。
        reason: String,
    },
}

impl Error {
    /// 是否表示「用户取消」。
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// 是否表示「资源或预算耗尽」。
    #[must_use]
    pub fn is_budget_exhausted(&self) -> bool {
        matches!(self, Self::BudgetExhausted { .. })
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization {
            reason: value.to_string(),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::SourceRead {
            path: PathBuf::from("<unknown>"),
            reason: value.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_and_budget_helpers_classify() {
        assert!(Error::Cancelled.is_cancelled());
        assert!(!Error::Cancelled.is_budget_exhausted());

        let exhausted = Error::BudgetExhausted {
            resource: "tokens".to_owned(),
            limit: "1000".to_owned(),
        };
        assert!(exhausted.is_budget_exhausted());
    }

    #[test]
    fn io_error_is_mapped_into_domain_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let err = Error::from(io_err);
        assert!(matches!(err, Error::SourceRead { .. }));
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn target_not_found_message_includes_path() {
        let err = Error::TargetNotFound {
            path: PathBuf::from("/tmp/missing"),
        };
        assert_eq!(
            err.to_string(),
            "target path does not exist or is not readable: /tmp/missing"
        );
    }
}
