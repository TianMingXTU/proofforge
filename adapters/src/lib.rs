//! 语言与工具链适配层。
//!
//! 需求文档 6.1：对于 Python、Java、Go 等被验证语言，允许通过受控子进程和
//! 容器运行原生工具；核心架构不依赖 Python 后端服务。
//!
//! 本 crate 定义适配器的公共契约，具体实现按语言拆分。当前只实现 Rust 与
//! Python 两条路径（MVP 范围），其余语言留出扩展点而不提前抽象（需求文档
//! 11.4：扩展接口由真实需求驱动）。
//!
//! # 安全约束
//!
//! 适配器只产出**数据**（[`pf_core::evidence::Command`] 记录），不持有执行
//! 能力；执行统一由 [`run_tool`] 完成。因此被审查仓库里的任何字符串都不可
//! 能被当作命令执行 —— 唯一可执行的东西是适配器自己拼出来的命令。

#![warn(missing_docs)]

use std::path::Path;
use std::process::Command as ProcessCommand;
use std::time::Duration;

use pf_core::evidence::{Command, ExecutionResult, ExecutionStatus};

/// 默认外部命令超时（秒）。
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// 工具链适配器：描述如何在目标项目上运行某个外部工具。
///
/// 适配器只负责「拼命令」，不负责执行；执行统一由 [`run_tool`] 完成，
/// 以便集中施加超时、环境与证据记录。
pub trait ToolAdapter: Send + Sync {
    /// 适配器名称，例如 `rust`、`python`。
    fn name(&self) -> &'static str;

    /// 目标项目是否适用该适配器。
    fn detects(&self, root: &Path) -> bool;

    /// 为给定工具生成命令记录；该语言不适用时返回 `None`。
    fn command(&self, root: &Path, tool: Tool, options: &ToolOptions) -> Option<Command>;
}

/// 可运行的外部工具。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// 构建 / 类型检查。
    Build,
    /// 运行已有测试。
    Test,
    /// 只跑调用方指定的测试目标（[`ToolOptions::extra_args`] 里带目标选择参数）。
    ///
    /// 与 [`Self::Test`] 的唯一区别是不带 `--all-targets`。`ProofForge` 生成的
    /// 驱动程序只关心自己那一个测试目标的结论；把项目自己的测试套件一起跑进来
    /// 既浪费时间，更糟的是项目的失败会让驱动程序明明没问题的结论变成「非零
    /// 退出、没有标记」，于是被误读成证据不足。
    SingleTest,
    /// 静态检查（clippy / ruff）。
    Lint,
    /// 格式化检查。
    Format,
}

impl Tool {
    /// 工具标识。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Test => "test",
            Self::SingleTest => "single-test",
            Self::Lint => "lint",
            Self::Format => "format",
        }
    }
}

/// 工具调用选项。
#[derive(Debug, Clone, Default)]
pub struct ToolOptions {
    /// 额外参数（追加在适配器生成的参数之后）。
    pub extra_args: Vec<String>,
    /// 超时（秒）；`None` 表示使用 [`DEFAULT_TIMEOUT_SECS`]。
    pub timeout_secs: Option<u64>,
    /// 只检查变更文件。
    pub only_changed: bool,
}

/// Rust 工具链适配器（cargo）。
#[derive(Debug, Default)]
pub struct RustAdapter;

impl ToolAdapter for RustAdapter {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn detects(&self, root: &Path) -> bool {
        root.join("Cargo.toml").is_file()
    }

    fn command(&self, root: &Path, tool: Tool, options: &ToolOptions) -> Option<Command> {
        let mut command = Command::new("cargo").cwd(root.display().to_string());
        command = match tool {
            Tool::Build => command.arg("check").arg("--all-targets"),
            Tool::Test => command.arg("test").arg("--all-targets"),
            Tool::SingleTest => command.arg("test"),
            Tool::Lint => command
                .arg("clippy")
                .arg("--all-targets")
                .arg("--")
                .arg("-D")
                .arg("warnings"),
            Tool::Format => command.arg("fmt").arg("--all").arg("--check"),
        };
        Some(with_options(command, options))
    }
}

/// Python 工具链适配器（pytest / ruff）。
#[derive(Debug, Default)]
pub struct PythonAdapter;

impl ToolAdapter for PythonAdapter {
    fn name(&self) -> &'static str {
        "python"
    }

    fn detects(&self, root: &Path) -> bool {
        root.join("pyproject.toml").is_file()
            || root.join("setup.py").is_file()
            || root.join("requirements.txt").is_file()
    }

    fn command(&self, root: &Path, tool: Tool, options: &ToolOptions) -> Option<Command> {
        let (program, args): (&str, &[&str]) = match tool {
            // Python 无需显式构建步骤，因此没有 Build 分支。
            Tool::Build => return None,
            // pytest 没有「跑全部目标」的开关，目标选择完全靠位置参数，
            // 因此这两个工具在 Python 下渲染相同。
            Tool::Test | Tool::SingleTest => ("pytest", &["-q"]),
            Tool::Lint => ("ruff", &["check", "."]),
            Tool::Format => ("ruff", &["format", "--check", "."]),
        };
        let mut command = Command::new(program).cwd(root.display().to_string());
        for arg in args {
            command = command.arg(*arg);
        }
        Some(with_options(command, options))
    }
}

/// 套用调用选项：追加额外参数并固定超时。
fn with_options(command: Command, options: &ToolOptions) -> Command {
    let mut command = command;
    for arg in &options.extra_args {
        command = command.arg(arg.clone());
    }
    command.timeout(options.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS))
}

/// 全部内置适配器。
#[must_use]
pub fn builtin_adapters() -> Vec<Box<dyn ToolAdapter>> {
    vec![Box::new(RustAdapter), Box::new(PythonAdapter)]
}

/// 为项目挑选第一个匹配的适配器。
#[must_use]
pub fn detect_adapter(root: &Path) -> Option<Box<dyn ToolAdapter>> {
    builtin_adapters()
        .into_iter()
        .find(|adapter| adapter.detects(root))
}

/// 子进程环境策略。
///
/// 需求文档 9.2 与 AGENTS.md 安全红线 2：模型 API 密钥、`GITHUB_TOKEN` 等
/// 凭据不得进入被审查项目的子进程环境——被审查仓库可以读取自己的环境，
/// 凭据泄露的代价不可逆。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EnvPolicy {
    /// 继承当前进程环境（默认，用于普通审查）。
    #[default]
    Inherit,
    /// 移除名字命中凭据模式的环境变量，其余保留。
    ///
    /// 选择「按模式剔除」而不是「白名单放行」：cargo / rustc 依赖大量环境变量
    /// 才能工作（`PATH`、`HOME`、`CARGO_HOME`、`RUSTUP_HOME`、`TMPDIR`、
    /// 代理设置……），白名单漏一项就会让工具链莫名其妙失败，而这种失败会被
    /// 误读成「目标项目有问题」。凭据的命名则足够规整，可以可靠识别。
    Scrubbed,
}

/// 按完整变量名匹配的多词凭据标记。
///
/// 这些标记自身含下划线，只能整体比对：拆成单词后 `API_KEY` 会退化成 `KEY`，
/// 误伤面立刻扩大。
const CREDENTIAL_PHRASES: &[&str] = &["API_KEY", "APIKEY", "ACCESS_KEY", "PRIVATE_KEY", "BEARER"];

/// 按 `_` 分段匹配的单词凭据标记。
///
/// **必须分段而不能用子串。** 回归测试守护的正是这一点：`PAT` 作为子串会命中
/// `PATH`、`CARGO_MANIFEST_PATH`、`CARGO_PKG_VERSION_PATCH`，把工具链赖以
/// 工作的 `PATH` 一并删掉，于是 cargo「找不到」——这种失败看起来像目标项目
/// 有问题，实际上是环境策略自己闯的祸，而且极难定位。
const CREDENTIAL_WORDS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PAT", // GitHub personal access token，例如 `GH_PAT`。
];

/// 返回剔除凭据之后的环境变量。
///
/// 只保留当前进程里真实存在的变量：`env_clear()` 之后逐个写回，避免把空值
/// 写进子进程（某些工具把「存在但为空」与「不存在」区分对待）。
fn scrubbed_environment() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(name, _)| !is_credential_name(name))
        .collect()
}

/// 判断环境变量名是否指向凭据。
///
/// 大小写不敏感：`github_token` 与 `GITHUB_TOKEN` 同样危险。
///
/// 两级匹配：多词标记按完整名字包含，单词标记按 `_` 分段相等。分段的理由是
/// 误伤的代价远高于漏删——删掉 `PATH` 会让 cargo 直接「找不到」，而漏掉一个
/// 名字生僻的密钥至少还有环境变量本身可查。
fn is_credential_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if CREDENTIAL_PHRASES
        .iter()
        .any(|phrase| upper.contains(phrase))
    {
        return true;
    }
    upper
        .split('_')
        .any(|segment| CREDENTIAL_WORDS.contains(&segment))
}

/// 执行外部工具并采集证据，可指定子进程环境策略。
///
/// # 安全性
///
/// 该函数只运行由 [`ToolAdapter`] 生成的命令，绝不执行来自被审查仓库的
/// 任意字符串。命令、超时与输出会一并记录到 [`ExecutionResult`]，作为
/// 可复现证据（需求文档 3.7）。
///
/// # 超时
///
/// 采用阻塞等待加超时轮询：这样无需为一个同步 CLI 引入 async 运行时。
/// 超时到期后子进程被杀死，结果标记为 [`ExecutionStatus::Signalled`]。
#[must_use]
pub fn run_tool_with_env(recorded: &Command, timeout_secs: u64, env: EnvPolicy) -> ExecutionResult {
    let mut process = ProcessCommand::new(&recorded.program);
    process.args(&recorded.args);
    if let Some(cwd) = &recorded.cwd {
        process.current_dir(cwd);
    }
    if let EnvPolicy::Scrubbed = env {
        process.env_clear();
        for (key, value) in scrubbed_environment() {
            process.env(key, value);
        }
    }
    run_and_collect(&mut process, recorded, timeout_secs)
}

/// 执行外部工具并采集证据（继承当前进程环境）。
///
/// # 安全性
///
/// 该函数只运行由 [`ToolAdapter`] 生成的命令，绝不执行来自被审查仓库的
/// 任意字符串。命令、超时与输出会一并记录到 [`ExecutionResult`]，作为
/// 可复现证据（需求文档 3.7）。
///
/// # 超时
///
/// 采用阻塞等待加超时轮询：这样无需为一个同步 CLI 引入 async 运行时。
/// 超时到期后子进程被杀死，结果标记为 [`ExecutionStatus::Signalled`]。
#[must_use]
pub fn run_tool(recorded: &Command, timeout_secs: u64) -> ExecutionResult {
    let mut process = ProcessCommand::new(&recorded.program);
    process.args(&recorded.args);
    if let Some(cwd) = &recorded.cwd {
        process.current_dir(cwd);
    }
    run_and_collect(&mut process, recorded, timeout_secs)
}

/// 启动子进程、等待退出并采集输出。
///
/// 拆出来是为了让 [`run_tool`] 与 [`run_tool_with_env`] 共享同一套超时与
/// 证据逻辑——两条路径对「拿不到结论」的处理必须完全一致。
///
/// # 输出采集
///
/// 标准输出与标准错误必须显式设成 [`std::process::Stdio::piped`]，否则
/// `wait_with_output()` 拿到的是空串——继承来的句柄不归 `Child` 所有，
/// 它读不到任何东西。回归测试守护这一点：没有输出，`pf-verify` 就看不见
/// 驱动程序打印的反例标签，整个验证层会退化成「永远满足」。
///
/// 读线程必须在等待退出**之前**启动。反过来做会让子进程在写满管道缓冲区后
/// 阻塞，我们这边一直等到超时才把它杀掉，于是把一次正常的慢执行记成
/// `Signalled`——结论错了，而且错得看不出来。
fn run_and_collect(
    process: &mut ProcessCommand,
    recorded: &Command,
    timeout_secs: u64,
) -> ExecutionResult {
    use std::time::Instant;
    let started = Instant::now();
    process.stdout(std::process::Stdio::piped());
    process.stderr(std::process::Stdio::piped());
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(reason) => {
            return failed_result(
                recorded,
                started,
                ExecutionStatus::FailedToStart,
                reason.to_string(),
            );
        }
    };

    let stdout_reader = spawn_reader(child.stdout.take());
    let stderr_reader = spawn_reader(child.stderr.take());

    let status = match wait_for_exit(&mut child, started, timeout_secs) {
        Ok(status) => status,
        Err((status, reason)) => return failed_result(recorded, started, status, reason),
    };

    ExecutionResult {
        command: recorded.clone(),
        status: ExecutionStatus::Exited,
        exit_code: status.code(),
        signal: None,
        duration_ms: elapsed_ms(started),
        stdout: stdout_reader.finish(),
        stderr: stderr_reader.finish(),
        truncated: false,
    }
}

/// 一个在后台线程里读干的管道。
///
/// 读线程的返回值通过 channel 传回：子进程退出后管道随即 EOF，读线程很快就会
/// 结束；给它一个宽限期是为了不让一次异常持久的读取挂住整个审查。
struct PipeReader {
    receiver: std::sync::mpsc::Receiver<String>,
}

impl PipeReader {
    /// 取回读到的内容；读线程异常时退回空串。
    ///
    /// 退回空串而不是报错：子进程已经退出，此时拿不到输出不影响「它是否通过」
    /// 这个结论，只影响证据的完整度，因此由调用方在报告里说明即可。
    fn finish(self) -> String {
        self.receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default()
    }
}

/// 启动一个读线程；句柄缺失时直接返回一个已关闭的 channel。
///
/// 两条管道用同一个函数：`ChildStdout` 与 `ChildStderr` 都实现 `Read`，
/// 泛型参数 `P` 让它们共享同一段逻辑而不复制两份。
fn spawn_reader<P: std::io::Read + Send + 'static>(pipe: Option<P>) -> PipeReader {
    let (sender, receiver) = std::sync::mpsc::channel();
    let Some(mut pipe) = pipe else {
        drop(sender);
        return PipeReader { receiver };
    };
    std::thread::spawn(move || {
        let mut buffer = String::new();
        let _ = pipe.read_to_string(&mut buffer);
        let _ = sender.send(buffer);
    });
    PipeReader { receiver }
}

/// 轮询等待子进程退出，超时则杀死它。
///
/// 错误携带具体的 [`ExecutionStatus`]：超时与被杀死是「工具跑不完」，轮询
/// 出错是「拿不到结论」。二者都写不进 `exit_code`，但原因必须能在证据里区分
/// （AGENTS.md 安全红线 4：不得把没做完的检查报告成通过）。
fn wait_for_exit(
    child: &mut std::process::Child,
    started: std::time::Instant,
    timeout_secs: u64,
) -> Result<std::process::ExitStatus, (ExecutionStatus, String)> {
    let limit = Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if started.elapsed() >= limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err((
                        ExecutionStatus::Signalled,
                        format!("timed out after {timeout_secs}s"),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(reason) => {
                return Err((ExecutionStatus::FailedToStart, reason.to_string()));
            }
        }
    }
}

/// 构造一条「没能拿到退出码」的执行结果。
///
/// 启动失败、轮询失败与超时共用一条路径，只是状态与原因不同：三者都无法
/// 判断工具是否通过，因此都不写 `exit_code`，让上游按证据不足处理。
fn failed_result(
    recorded: &Command,
    started: std::time::Instant,
    status: ExecutionStatus,
    reason: String,
) -> ExecutionResult {
    ExecutionResult {
        command: recorded.clone(),
        status,
        exit_code: None,
        signal: None,
        duration_ms: elapsed_ms(started),
        stdout: String::new(),
        stderr: reason,
        truncated: false,
    }
}

/// 从给定的起点计算已耗时毫秒数。
///
/// 饱和到 `u64::MAX` 而不是截断：一次耗时不可能接近该量级，但静默截断会让
/// 证据里的数字偏小而无法察觉。
fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// 便捷函数：在目标项目上运行某个工具。
#[must_use]
pub fn run_on_project(root: &Path, tool: Tool, options: &ToolOptions) -> Option<ExecutionResult> {
    let adapter = detect_adapter(root)?;
    let recorded = adapter.command(root, tool, options)?;
    let timeout = recorded.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    Some(run_tool(&recorded, timeout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(command: &Command) -> Vec<String> {
        command.args.clone()
    }

    #[test]
    fn rust_adapter_detects_cargo_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!RustAdapter.detects(dir.path()));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").expect("write");
        assert!(RustAdapter.detects(dir.path()));
    }

    #[test]
    fn python_adapter_detects_pyproject() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("pyproject.toml"), "[tool]\n").expect("write");
        assert!(PythonAdapter.detects(dir.path()));
        assert!(!RustAdapter.detects(dir.path()));
    }

    #[test]
    fn rust_adapter_generates_expected_commands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let adapter = RustAdapter;
        let options = ToolOptions::default();

        let build = adapter
            .command(dir.path(), Tool::Build, &options)
            .expect("build");
        assert_eq!(build.program, "cargo");
        assert_eq!(args_of(&build), vec!["check", "--all-targets"]);

        let lint = adapter
            .command(dir.path(), Tool::Lint, &options)
            .expect("lint");
        assert_eq!(
            args_of(&lint),
            vec!["clippy", "--all-targets", "--", "-D", "warnings"]
        );
    }

    #[test]
    fn python_adapter_skips_build_step() {
        let dir = tempfile::tempdir().expect("tempdir");
        let adapter = PythonAdapter;
        assert!(adapter
            .command(dir.path(), Tool::Build, &ToolOptions::default())
            .is_none());
    }

    #[test]
    fn python_adapter_generates_expected_commands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let adapter = PythonAdapter;
        let test = adapter
            .command(dir.path(), Tool::Test, &ToolOptions::default())
            .expect("test");
        assert_eq!(test.program, "pytest");
        assert_eq!(args_of(&test), vec!["-q"]);
    }

    #[test]
    fn extra_args_are_appended() {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = ToolOptions {
            extra_args: vec!["--offline".to_owned()],
            ..ToolOptions::default()
        };
        let command = RustAdapter
            .command(dir.path(), Tool::Build, &options)
            .expect("build");
        assert_eq!(
            args_of(&command),
            vec!["check", "--all-targets", "--offline"]
        );
    }

    #[test]
    fn generated_command_records_cwd_and_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let command = RustAdapter
            .command(dir.path(), Tool::Test, &ToolOptions::default())
            .expect("test");
        assert_eq!(
            command.cwd.as_deref(),
            Some(dir.path().to_str().expect("utf8"))
        );
        assert_eq!(command.timeout_secs, Some(DEFAULT_TIMEOUT_SECS));
    }

    #[test]
    fn missing_tool_is_reported_as_inconclusive() {
        let recorded = Command::new("proofforge-nonexistent-tool-xyz");
        let result = run_tool(&recorded, 5);
        assert!(result.failed_to_start());
        // 工具缺失绝不能记为「通过」（需求文档 14.3）。
        assert_eq!(
            result.evidence_status(),
            pf_core::status::EvidenceStatus::Inconclusive
        );
    }

    #[test]
    fn successful_tool_records_exit_code() {
        let recorded = Command::new("true");
        let result = run_tool(&recorded, 5);
        assert!(result.succeeded());
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.command.program, "true");
    }

    #[test]
    fn failing_tool_records_nonzero_status() {
        let recorded = Command::new("false");
        let result = run_tool(&recorded, 5);
        assert!(!result.succeeded());
        assert_ne!(result.exit_code, Some(0));
    }

    #[test]
    fn timeout_is_enforced_and_marked_as_signalled() {
        // sleep 60 但只给 1 秒预算：必须被杀掉并标记为 Signalled，
        // 而不是无限期挂住审查流程。
        let recorded = Command::new("sleep").arg("60");
        let result = run_tool(&recorded, 1);
        assert_eq!(result.status, ExecutionStatus::Signalled);
        assert!(!result.succeeded());
        assert!(result.duration_ms < 10_000);
    }

    #[test]
    fn detect_adapter_picks_rust_for_this_repo() {
        // `adapters/` 的上一级就是 workspace 根，那里有 Cargo.toml。
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let adapter = detect_adapter(&root).expect("adapter");
        assert_eq!(adapter.name(), "rust");
    }

    #[test]
    fn credential_names_are_recognised() {
        for name in [
            "GITHUB_TOKEN",
            "github_token",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "GH_PAT",
            "MY_PASSWORD",
            "DB_CREDENTIAL",
            "SIGNING_SECRET",
        ] {
            assert!(is_credential_name(name), "{name} 应当被识别为凭据");
        }
    }

    #[test]
    fn path_is_never_treated_as_a_credential() {
        // 回归测试：`PAT` 曾经按子串匹配，于是 `PATH`、`CARGO_MANIFEST_PATH`、
        // `CARGO_PKG_VERSION_PATCH` 全部被删，cargo 直接「找不到」。
        // 这种失败看起来像目标项目有问题，实际上是环境策略自己闯的祸。
        for name in [
            "PATH",
            "CARGO_MANIFEST_PATH",
            "CARGO_PKG_VERSION_PATCH",
            "LD_LIBRARY_PATH",
            "PATCH",
            "PATCHES",
            "NoDefaultCurrentDirectoryInExePath",
            "HOME",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "TMPDIR",
        ] {
            assert!(
                !is_credential_name(name),
                "{name} 不是凭据，删掉它会破坏工具链"
            );
        }
    }

    #[test]
    fn scrubbed_environment_keeps_path_and_drops_tokens() {
        let kept: Vec<String> = scrubbed_environment()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        // PATH 必须保留：沙箱靠它找到 cargo。
        assert!(
            kept.iter().any(|name| name == "PATH"),
            "PATH 必须保留在子进程环境里"
        );
        assert!(
            kept.iter().all(|name| !is_credential_name(name)),
            "剔除后的环境不得再含凭据"
        );
    }

    #[test]
    fn scrubbed_run_actually_starts_the_tool() {
        // 端到端守护上面那条回归：环境剔除之后 cargo 必须还能启动。
        let recorded = Command::new("cargo").arg("--version");
        let result = run_tool_with_env(&recorded, 30, EnvPolicy::Scrubbed);
        assert!(
            result.succeeded(),
            "stderr: {}",
            result.stderr.lines().next().unwrap_or_default()
        );
        assert!(
            result.stdout.contains("cargo"),
            "stdout was {:?}",
            result.stdout
        );
    }
}
