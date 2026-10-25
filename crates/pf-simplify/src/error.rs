//! 简化管线的错误类型。
//!
//! 全部错误都带「哪个候选、哪个文件、为什么」，因为简化失败的信息要能直接
//! 贴进 issue 或审计记录（AGENTS.md §4.3：错误信息面向使用者）。

use std::path::PathBuf;

/// 把底层领域错误折算成简化管线错误。
///
/// 沙箱隔离失败（目标不存在、不是目录、工作区创建失败）在管线上表现为
/// 「这一步做不了」，因此统一折算成 [`SimplifyError::TargetNotFound`]，
/// 由调用方决定是终止还是记为证据不足。
impl From<pf_core::Error> for SimplifyError {
    fn from(error: pf_core::Error) -> Self {
        Self::TargetNotFound {
            path: error_path(&error),
        }
    }
}

/// 从领域错误里尽量取出路径，取不到就用占位符而不是编造一个路径。
fn error_path(error: &pf_core::Error) -> PathBuf {
    match error {
        pf_core::Error::TargetNotFound { path } | pf_core::Error::TargetNotDirectory { path } => {
            path.clone()
        }
        other => PathBuf::from(other.to_string()),
    }
}

/// 简化管线失败的原因。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SimplifyError {
    /// 目标路径不存在或不可读。
    #[error("target not usable: {path}")]
    TargetNotFound {
        /// 目标路径。
        path: PathBuf,
    },
    /// 候选编辑无法应用到文件上。
    #[error("candidate `{candidate}` produced an unusable edit: {reason}")]
    EditRejected {
        /// 候选 ID。
        candidate: String,
        /// 具体原因（越界、重叠等）。
        reason: String,
    },
    /// 沙箱里找不到待修补的文件（被预算跳过或路径不在复制范围）。
    #[error("`{path}` was not copied into the isolated workspace: {reason}")]
    FileNotCopied {
        /// 仓库相对路径。
        path: String,
        /// 底层原因。
        reason: String,
    },
    /// 没有获得明确授权，因此不得写入目标仓库（FR-055 / FR-056）。
    #[error(
        "applying a patch requires explicit authorization; pass --yes to confirm after review"
    )]
    NotAuthorized,
    /// 候选的验证结论不是「行为保留」，不允许应用。
    #[error("candidate `{candidate}` is not verified as behavior-preserving ({verdict}); refusing to apply")]
    NotVerified {
        /// 候选 ID。
        candidate: String,
        /// 候选结论。
        verdict: String,
    },
    /// 写目标仓库失败。
    #[error("writing {path} failed: {reason}")]
    WriteFailed {
        /// 目标文件。
        path: String,
        /// 底层原因。
        reason: String,
    },
    /// 一个补丁都没有应用。
    ///
    /// 「什么都没改」不能以成功退出码结束：调用方运行 simplify 的目的是落地
    /// 补丁，一次全部跳过的应用与一次失败的应用在结果上没有区别（红线 4）。
    #[error("no patch was applied: {reasons:?}")]
    NothingApplied {
        /// 每个候选被跳过的原因。
        reasons: Vec<String>,
    },
    /// 报告序列化失败。
    #[error("serializing the simplify report failed: {reason}")]
    Serialize {
        /// 底层原因。
        reason: String,
    },
}
