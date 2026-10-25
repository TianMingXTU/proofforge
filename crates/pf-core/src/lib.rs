//! `ProofForge` 核心领域模型。
//!
//! 本 crate 只包含纯数据结构与值对象，不依赖 I/O、网络或外部进程，因此
//! 可以被 `pf-cli`、`pf-server` 以及各分析 crate 安全复用，也便于单元测试。
//!
//! # 设计约定
//!
//! - 所有跨 crate 传递的标识符都使用 [`ids`] 中的强类型包装，避免把
//!   `String` 当作 ID 到处传递而丢失类型信息。
//! - 所有序列化类型都实现 [`serde::Serialize`] / [`serde::Deserialize`]，以
//!   便落库与生成 JSON 报告。
//! - 领域类型全部实现 `Clone`、`Debug`、`PartialEq`（除显式排除者外），保持
//!   在测试中易于断言。

#![warn(missing_docs)]

pub mod config;
pub mod error;
pub mod evidence;
pub mod exempt;
pub mod finding;
pub mod ids;
pub mod location;
pub mod review;
pub mod rule;
pub mod source;
pub mod status;

pub use config::Config;
pub use error::{Error, Result};
pub use evidence::{Command, EvidenceArtifact, ExecutionResult, ExecutionStatus};
pub use exempt::{Exemption, ExemptionError, ExemptionScope, ExemptionSet};
pub use finding::{Finding, FindingCategory, FindingKind, Remediation};
pub use ids::{FindingId, ProjectId, RuleId, RunId, SnapshotId, TaskId};
pub use location::{Location, Span};
pub use review::{decide_gate, summarize, LayerSummary, QualityLayers, ReviewReport};
pub use rule::{Rule, RuleSet, RuleSeverity};
pub use source::{Language, SourceFile, SourceMap};
pub use status::{EvidenceStatus, GateStatus, Severity};

/// 版本号，用于报告与证据的溯源。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 项目名称，用于日志与报告的默认署名。
pub const PROJECT_NAME: &str = "ProofForge";

/// 计算给定内容的十六进制 SHA-256 摘要。
///
/// 证据完整性校验统一使用该函数，确保证据内容可被独立复算。
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_empty_input_is_stable() {
        // SHA-256("") 的已知取值，用于验证实现没有引入意外的编码问题。
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_of_known_input_matches_reference() {
        // SHA-256("abc") 的公开测试向量。
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn version_is_non_empty() {
        // cargo 注入的版本号永远非空；空版本会让报告无法溯源。
        assert_ne!(VERSION, "");
    }
}
