//! 证据与执行记录。
//!
//! 需求文档 3.7 与第十章：每项重要发现都必须有可追溯、可复算的证据记录。

use serde::{Deserialize, Serialize};

use crate::sha256_hex;
use crate::status::EvidenceStatus;

/// 一条被记录的执行命令。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// 可执行文件（可能是绝对路径或 PATH 中的名字）。
    pub program: String,
    /// 参数列表。
    pub args: Vec<String>,
    /// 工作目录。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 环境变量差异（仅记录与默认环境不同的关键项，避免泄露密钥）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// 超时时间（秒）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl Command {
    /// 构造一条命令记录。
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            timeout_secs: None,
        }
    }

    /// 追加参数（构建器风格）。
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// 设置工作目录。
    #[must_use]
    pub fn cwd(mut self, cwd: String) -> Self {
        self.cwd = Some(cwd);
        self
    }

    /// 设置超时（秒）。
    #[must_use]
    pub const fn timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    /// 渲染为可直接粘贴到 shell 的字符串。
    #[must_use]
    pub fn render(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.iter().map(|arg| {
            if arg.contains(' ') || arg.contains('\'') {
                format!("'{arg}'")
            } else {
                arg.clone()
            }
        }));
        parts.join(" ")
    }
}

/// 外部进程的执行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    /// 正常结束。
    Exited,
    /// 被信号终止（例如超时 kill）。
    Signalled,
    /// 无法启动（工具缺失、权限不足）。
    FailedToStart,
}

/// 一次外部执行的完整记录，是复现的最小单元。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionResult {
    /// 被执行的命令。
    pub command: Command,
    /// 结束状态。
    pub status: ExecutionStatus,
    /// 退出码（无法获得时为 `None`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// 收到的信号编号。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// 耗时（毫秒）。
    pub duration_ms: u64,
    /// 标准输出（可能被截断，见 `stdout_truncated`）。
    pub stdout: String,
    /// 标准错误（可能被截断）。
    pub stderr: String,
    /// 输出是否被截断。
    #[serde(default)]
    pub truncated: bool,
}

impl ExecutionResult {
    /// 命令是否成功结束（退出码为 0）。
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.status == ExecutionStatus::Exited && self.exit_code == Some(0)
    }

    /// 是否因为无法启动而失败。
    #[must_use]
    pub fn failed_to_start(&self) -> bool {
        self.status == ExecutionStatus::FailedToStart
    }

    /// 该执行结果对应的证据状态。
    ///
    /// 工具无法启动只能得到 [`EvidenceStatus::Inconclusive`]，绝不能记为通过。
    #[must_use]
    pub fn evidence_status(&self) -> EvidenceStatus {
        if self.failed_to_start() {
            EvidenceStatus::Inconclusive
        } else if self.succeeded() {
            EvidenceStatus::StaticConfirmed
        } else {
            EvidenceStatus::Suspected
        }
    }
}

/// 一条证据产物（日志、反例输入、报告片段等）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceArtifact {
    /// 证据类型标签，例如 `test-input`、`stderr-log`、`counterexample`。
    pub kind: String,
    /// 证据内容（文本形式，二进制内容先编码）。
    pub content: String,
    /// 内容的 SHA-256 十六进制摘要。
    pub sha256: String,
    /// 人类可读的来源说明。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl EvidenceArtifact {
    /// 从文本内容构造证据，自动计算摘要。
    #[must_use]
    pub fn from_text(kind: impl Into<String>, content: impl Into<String>) -> Self {
        let content = content.into();
        let sha256 = sha256_hex(content.as_bytes());
        Self {
            kind: kind.into(),
            content,
            sha256,
            source: None,
        }
    }

    /// 设置来源说明。
    #[must_use]
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// 校验内容是否与记录的摘要一致。
    #[must_use]
    pub fn verify(&self) -> bool {
        sha256_hex(self.content.as_bytes()) == self.sha256
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_render_quotes_arguments_with_spaces() {
        let command = Command::new("cargo")
            .arg("test")
            .arg("--message-format=json")
            .arg("filter with space");
        assert_eq!(
            command.render(),
            "cargo test --message-format=json 'filter with space'"
        );
    }

    #[test]
    fn execution_status_maps_to_evidence_status() {
        let ok = ExecutionResult {
            command: Command::new("true"),
            status: ExecutionStatus::Exited,
            exit_code: Some(0),
            signal: None,
            duration_ms: 1,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        };
        assert!(ok.succeeded());
        assert_eq!(ok.evidence_status(), EvidenceStatus::StaticConfirmed);

        let missing = ExecutionResult {
            command: Command::new("nope"),
            status: ExecutionStatus::FailedToStart,
            exit_code: None,
            signal: None,
            duration_ms: 0,
            stdout: String::new(),
            stderr: "spawn error".to_owned(),
            truncated: false,
        };
        assert!(!missing.succeeded());
        assert_eq!(missing.evidence_status(), EvidenceStatus::Inconclusive);
    }

    #[test]
    fn evidence_digest_verifies() {
        let artifact = EvidenceArtifact::from_text("test-input", "[1, 2, 3]");
        assert!(artifact.verify());
        assert_eq!(artifact.sha256.len(), 64);

        let mut tampered = artifact.clone();
        tampered.content.push_str(" + 4");
        assert!(!tampered.verify());
    }

    #[test]
    fn evidence_serde_round_trip() {
        let artifact = EvidenceArtifact::from_text("log", "hello").with_source("run-1");
        let json = serde_json::to_string(&artifact).expect("serialize");
        let back: EvidenceArtifact = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(artifact, back);
    }
}
