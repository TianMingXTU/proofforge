//! 沙箱隔离（需求文档 9.2 与 FR-028）。
//!
//! # 为什么必须复制项目
//!
//! 属性测试与模糊测试都要往目标项目里写文件（驱动程序、变异体）。AGENTS.md
//! 安全红线 3 规定「未经用户明确授权不得修改被审查仓库的文件」，因此所有写入
//! 都发生在一份**独立工作区副本**上：原仓库保持只读，补丁与产物落在临时目录，
//! 由人工确认后才可能被应用（FR-055 / FR-056）。
//!
//! # 隔离措施
//!
//! | 措施 | 对应风险 |
//! |---|---|
//! | 副本运行，原仓库只读 | 意外修改用户代码 |
//! | 环境变量剔除凭据 | 密钥经子进程泄露（红线 2） |
//! | 每个命令带超时 | 目标项目里的死循环拖垮整个审查 |
//! | 不跟随符号链接 | 借链接把写入引出工作区 |
//! | 跳过 `target/`、`.git/` 等目录 | 复制成本与历史泄露 |
//! | 命令只能来自 [`pf_adapters::ToolAdapter`] | 执行被审查仓库里的任意字符串（红线 1） |

use std::path::{Path, PathBuf};

use pf_adapters::{detect_adapter, run_tool_with_env, EnvPolicy, Tool, ToolOptions};
use pf_core::config::Budget;
use pf_core::evidence::{ExecutionResult, ExecutionStatus};

/// 复制时跳过的目录名。
///
/// 这些目录要么是构建产物（`target`）、要么是版本历史（`.git`）、要么是体积
/// 远大于信息量的依赖缓存。把它们带进沙箱只会让复制变慢、变危险。
const SKIPPED_DIRECTORIES: &[&str] = &[
    "target",
    ".git",
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".venv",
    "venv",
    ".tox",
    "dist",
    "build",
];

/// 一个隔离的工作区。
///
/// 持有临时目录：`Sandbox` 被丢弃时工作区随之删除，因此不存在「忘了清理」的
/// 路径。原仓库路径只用于证据说明，不参与任何写入。
#[derive(Debug)]
pub struct Sandbox {
    /// 独立工作区（临时目录）。
    workspace: tempfile::TempDir,
    /// 被隔离的原始项目路径。
    source_root: PathBuf,
    /// 复制进来的文件数。
    file_count: usize,
    /// 因预算或体积被跳过的条目（相对路径 + 原因）。
    skipped: Vec<String>,
}

impl Sandbox {
    /// 把目标项目复制到独立工作区。
    ///
    /// # Errors
    /// 目标不存在、不可读，或工作区创建失败时返回 [`pf_core::Error`]。
    /// 复制过程中单个文件失败不会让整体失败——它被记入跳过列表，
    /// 因为「少一个文件」与「没有沙箱」是严重程度完全不同的两件事。
    pub fn isolate(target: &Path, budget: &Budget) -> pf_core::Result<Self> {
        let canonical = target
            .canonicalize()
            .map_err(|_| pf_core::Error::TargetNotFound {
                path: target.to_path_buf(),
            })?;
        if !canonical.is_dir() {
            return Err(pf_core::Error::TargetNotDirectory { path: canonical });
        }
        let workspace = tempfile::tempdir().map_err(|reason| pf_core::Error::SourceRead {
            path: target.to_path_buf(),
            reason: reason.to_string(),
        })?;
        let mut sandbox = Self {
            workspace,
            source_root: canonical,
            file_count: 0,
            skipped: Vec::new(),
        };
        // 先把根路径取出来再借 `sandbox` 为可变：`copy_tree` 要写 `skipped`，
        // 同时又要读 `source_root` 与工作区路径，分开取让借用不重叠。
        let root = sandbox.workspace.path().to_path_buf();
        let source = sandbox.source_root.clone();
        sandbox.copy_tree(&source, &root, budget);
        Ok(sandbox)
    }

    /// 工作区根目录。
    #[must_use]
    pub fn root(&self) -> &Path {
        self.workspace.path()
    }

    /// 被隔离的原始项目路径。
    #[must_use]
    pub fn source_root(&self) -> &Path {
        &self.source_root
    }

    /// 复制进来的文件数。
    #[must_use]
    pub const fn file_count(&self) -> usize {
        self.file_count
    }

    /// 因预算或体积被跳过的条目。
    #[must_use]
    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    /// 在沙箱里运行一个工具。
    ///
    /// 命令由适配器生成、环境已剔除凭据。适配器不适配该项目时返回一条
    /// [`ExecutionStatus::FailedToStart`] 的结果而不是 `None`：调用方需要能
    /// 把「没有可用工具链」写成证据，而不是当成空结果处理（红线 4）。
    #[must_use]
    pub fn run(&self, tool: Tool, options: &ToolOptions) -> ExecutionResult {
        let Some(adapter) = detect_adapter(self.root()) else {
            return missing_adapter(tool);
        };
        let Some(recorded) = adapter.command(self.root(), tool, options) else {
            return missing_adapter(tool);
        };
        let timeout = recorded
            .timeout_secs
            .unwrap_or(options.timeout_secs.unwrap_or(300));
        run_tool_with_env(&recorded, timeout, EnvPolicy::Scrubbed)
    }

    /// 往工作区写入一个文件（相对路径）。
    ///
    /// 只接受**相对**路径且不含 `..`：调用方想写的位置必须落在工作区内。
    /// 这一条不是防御性编程——它是「不允许借驱动程序把文件写出沙箱」的硬边界。
    ///
    /// # Errors
    /// 路径越出工作区、或写入失败时返回 [`pf_core::Error`]。
    pub fn write_file(&self, relative: &str, content: &str) -> pf_core::Result<PathBuf> {
        let destination = self.resolve_inside(relative)?;
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|reason| pf_core::Error::SourceRead {
                path: parent.to_path_buf(),
                reason: reason.to_string(),
            })?;
        }
        std::fs::write(&destination, content).map_err(|reason| pf_core::Error::SourceRead {
            path: destination.clone(),
            reason: reason.to_string(),
        })?;
        Ok(destination)
    }

    /// 把相对路径解析为工作区内的绝对路径。
    ///
    /// # Errors
    /// 路径是绝对路径、含 `..`、或规范化后越出工作区时返回
    /// [`pf_core::Error::SourceRead`]。
    fn resolve_inside(&self, relative: &str) -> pf_core::Result<PathBuf> {
        let candidate = Path::new(relative);
        if candidate.is_absolute() {
            return Err(pf_core::Error::SourceRead {
                path: candidate.to_path_buf(),
                reason: "sandbox writes must use a relative path".to_owned(),
            });
        }
        if candidate
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(pf_core::Error::SourceRead {
                path: candidate.to_path_buf(),
                reason: "sandbox writes must not escape the workspace".to_owned(),
            });
        }
        let joined = self.workspace.path().join(candidate);
        // 规范化后再次校验：符号链接可能把 `a/b` 指向工作区之外。
        let canonical = joined
            .canonicalize()
            .unwrap_or_else(|_| normalize_without_io(&joined));
        let root = self
            .workspace
            .path()
            .canonicalize()
            .unwrap_or_else(|_| self.workspace.path().to_path_buf());
        if !canonical.starts_with(&root) {
            return Err(pf_core::Error::SourceRead {
                path: joined,
                reason: "resolved path escapes the sandbox workspace".to_owned(),
            });
        }
        Ok(joined)
    }

    /// 递归复制目录树。
    ///
    /// 深度优先、顺序确定：同样的输入得到同样的工作区内容，因此沙箱里的执行
    /// 是可复现的。跳过决策在进入目录前做出，避免先复制再删除。
    fn copy_tree(&mut self, source: &Path, destination: &Path, budget: &Budget) {
        let entries = match std::fs::read_dir(source) {
            Ok(entries) => entries,
            Err(reason) => {
                self.skipped
                    .push(format!("{}: unreadable ({reason})", source.display()));
                return;
            }
        };
        let mut names: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        names.sort();
        for path in names {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if SKIPPED_DIRECTORIES.contains(&name) {
                continue;
            }
            let relative = path
                .strip_prefix(&self.source_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(reason) => {
                    self.skipped.push(format!("{relative}: {reason}"));
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                // 不跟随符号链接：它可能指向工作区之外，复制它会破坏隔离边界。
                self.skipped.push(format!("{relative}: symlink"));
                continue;
            }
            if metadata.is_dir() {
                let next = destination.join(name);
                if std::fs::create_dir_all(&next).is_err() {
                    self.skipped
                        .push(format!("{relative}: cannot create directory"));
                    continue;
                }
                self.copy_tree(&path, &next, budget);
                continue;
            }
            if self.file_count >= budget.max_files {
                self.skipped
                    .push(format!("{relative}: file budget exhausted"));
                continue;
            }
            if metadata.len() > budget.max_file_bytes as u64 {
                self.skipped
                    .push(format!("{relative}: exceeds max_file_bytes"));
                continue;
            }
            if std::fs::copy(&path, destination.join(name)).is_err() {
                self.skipped.push(format!("{relative}: copy failed"));
                continue;
            }
            self.file_count += 1;
        }
    }
}

/// 不访问文件系统地把路径规范化（折叠 `.` 与 `..`）。
///
/// 用于「目标还不存在」的场合：`std::fs::canonicalize` 要求路径已存在，
/// 而写入前的校验必须能判断一个尚未创建的路径是否越界。
fn normalize_without_io(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 构造一条「没有可用工具链」的执行结果。
///
/// 状态取 [`ExecutionStatus::FailedToStart`] 而不是成功：没有工具链意味着
/// 验证根本没有发生，把它记成通过就是伪造结论（AGENTS.md 安全红线 4）。
fn missing_adapter(tool: Tool) -> ExecutionResult {
    ExecutionResult {
        command: pf_core::evidence::Command::new("<none>"),
        status: ExecutionStatus::FailedToStart,
        exit_code: None,
        signal: None,
        duration_ms: 0,
        stdout: String::new(),
        stderr: format!(
            "no toolchain adapter matches this project; cannot run `{}`",
            tool.as_str()
        ),
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::evidence::Command;

    fn budget() -> Budget {
        Budget::default()
    }

    /// 构造一个最小的 Rust 项目。
    fn rust_project(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).expect("mkdir");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write manifest");
        std::fs::write(dir.join("src/lib.rs"), "pub fn one() -> i32 { 1 }\n").expect("write lib");
    }

    #[test]
    fn isolate_copies_the_project_and_skips_build_artifacts() {
        let source = tempfile::tempdir().expect("tempdir");
        rust_project(source.path());
        std::fs::create_dir_all(source.path().join("target/debug")).expect("mkdir target");
        std::fs::write(source.path().join("target/debug/junk"), "x").expect("write junk");
        std::fs::create_dir_all(source.path().join(".git")).expect("mkdir git");
        std::fs::write(source.path().join(".git/HEAD"), "ref: refs/heads/main\n").expect("write");

        let sandbox = Sandbox::isolate(source.path(), &budget()).expect("isolate");
        assert_eq!(sandbox.file_count(), 2, "只应复制 Cargo.toml 与 src/lib.rs");
        assert!(sandbox.root().join("src/lib.rs").is_file());
        assert!(
            !sandbox.root().join("target").exists(),
            "构建产物不应进入沙箱"
        );
        assert!(
            !sandbox.root().join(".git").exists(),
            "版本历史不应进入沙箱"
        );
        assert_eq!(sandbox.source_root(), source.path().canonicalize().unwrap());
    }

    #[test]
    fn symlinks_are_not_followed() {
        let source = tempfile::tempdir().expect("tempdir");
        rust_project(source.path());
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.txt"), "classified").expect("write");
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            source.path().join("link.txt"),
        )
        .expect("symlink");

        let sandbox = Sandbox::isolate(source.path(), &budget()).expect("isolate");
        assert!(
            !sandbox.root().join("link.txt").exists(),
            "符号链接不应被复制"
        );
        assert!(
            sandbox
                .skipped()
                .iter()
                .any(|entry| entry.contains("symlink")),
            "跳过的符号链接必须记录在案：{:?}",
            sandbox.skipped()
        );
    }

    #[test]
    fn file_budget_is_enforced_and_recorded() {
        let source = tempfile::tempdir().expect("tempdir");
        rust_project(source.path());
        std::fs::write(source.path().join("extra.rs"), "pub fn extra() {}\n").expect("write");
        let budget = Budget {
            max_files: 2,
            ..Budget::default()
        };
        let sandbox = Sandbox::isolate(source.path(), &budget).expect("isolate");
        assert_eq!(sandbox.file_count(), 2);
        assert!(
            sandbox
                .skipped()
                .iter()
                .any(|entry| entry.contains("budget exhausted")),
            "超预算跳过的文件必须出现在跳过列表里：{:?}",
            sandbox.skipped()
        );
    }

    #[test]
    fn oversize_files_are_skipped() {
        let source = tempfile::tempdir().expect("tempdir");
        rust_project(source.path());
        std::fs::write(source.path().join("huge.bin"), vec![0u8; 4096]).expect("write");
        let budget = Budget {
            max_file_bytes: 1024,
            ..Budget::default()
        };
        let sandbox = Sandbox::isolate(source.path(), &budget).expect("isolate");
        assert!(!sandbox.root().join("huge.bin").exists());
        assert!(sandbox
            .skipped()
            .iter()
            .any(|entry| entry.contains("max_file_bytes")));
    }

    #[test]
    fn write_file_accepts_relative_paths_inside_the_workspace() {
        let source = tempfile::tempdir().expect("tempdir");
        rust_project(source.path());
        let sandbox = Sandbox::isolate(source.path(), &budget()).expect("isolate");
        let written = sandbox
            .write_file("tests/proofforge_driver.rs", "// generated\n")
            .expect("write");
        assert!(written.is_file());
        assert_eq!(
            std::fs::read_to_string(&written).expect("read"),
            "// generated\n"
        );
    }

    #[test]
    fn write_file_rejects_absolute_and_parent_paths() {
        let source = tempfile::tempdir().expect("tempdir");
        rust_project(source.path());
        let sandbox = Sandbox::isolate(source.path(), &budget()).expect("isolate");

        let absolute = sandbox.write_file("/etc/passwd", "x");
        assert!(absolute.is_err(), "绝对路径必须被拒绝");

        let escape = sandbox.write_file("../../escape.txt", "x");
        assert!(escape.is_err(), "`..` 必须被拒绝");
    }

    #[test]
    fn missing_target_is_an_error() {
        let err = Sandbox::isolate(Path::new("/definitely/not/real"), &budget())
            .expect_err("should fail");
        assert!(matches!(err, pf_core::Error::TargetNotFound { .. }));
    }

    #[test]
    fn file_target_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("Cargo.toml");
        std::fs::write(&file, "[package]\n").expect("write");
        let err = Sandbox::isolate(&file, &budget()).expect_err("should fail");
        assert!(matches!(err, pf_core::Error::TargetNotDirectory { .. }));
    }

    #[test]
    fn run_without_a_matching_adapter_reports_failure_to_start() {
        // 空目录没有任何工具链：必须得到「没跑起来」，绝不能是成功。
        let dir = tempfile::tempdir().expect("tempdir");
        let sandbox = Sandbox::isolate(dir.path(), &budget()).expect("isolate");
        let result = sandbox.run(Tool::Test, &ToolOptions::default());
        assert_eq!(result.status, ExecutionStatus::FailedToStart);
        assert!(result.stderr.contains("no toolchain adapter"));
    }

    #[test]
    fn normalize_without_io_folds_parent_components() {
        let normalized = normalize_without_io(Path::new("/a/b/../c/./d"));
        assert_eq!(normalized, PathBuf::from("/a/c/d"));
    }

    #[test]
    fn missing_adapter_result_carries_no_exit_code() {
        let result = missing_adapter(Tool::Build);
        assert_eq!(result.exit_code, None);
        assert!(!result.succeeded());
        assert_eq!(result.command, Command::new("<none>"));
    }
}
