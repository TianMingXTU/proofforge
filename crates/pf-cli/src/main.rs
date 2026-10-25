//! `ProofForge` 命令行入口。
//!
//! 目标接口见需求文档 14.2：
//!
//! ```text
//! proofforge inspect .
//! proofforge verify --requirements requirements.md --target src/
//! proofforge simplify --target src/user_service.rs
//! proofforge report --latest
//! ```
//!
//! 当前实现：`inspect`（工程质量审查）、`verify`（行为验证）、`simplify`
//! （保留行为的代码简化）与 `agent`（自主验证循环）已可用。
//! 任何一步没能跑完时返回「证据不足」退出码 2，而不是伪造成功
//! （需求文档 1.3：证据优先；AGENTS.md 安全红线 4）。

#![warn(missing_docs)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use pf_core::config::{Config, ReportFormat};
use pf_core::status::{GateStatus, Severity};
use pf_report::render as render_report;
use pf_review::ReviewEngine;
use pf_simplify::{Authorization, SimplifyConfig, SimplifyEngine};
use pf_store::Store;

/// `ProofForge`：智能软件工程质量审查与验证系统。
#[derive(Debug, Parser)]
#[command(
    name = "proofforge",
    version,
    about = "独立判断软件实现是否符合需求、是否存在真实缺陷的工程质量验证工具",
    propagate_version = true
)]
struct Cli {
    /// 全局参数。
    #[command(flatten)]
    global: GlobalArgs,

    /// 子命令。
    #[command(subcommand)]
    command: Command,
}

/// 全局参数（所有子命令共享）。
#[derive(Debug, Args)]
struct GlobalArgs {
    /// 提高日志详细程度（`-v` 显示 info，`-vv` 显示 debug）。
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// 静默模式，只输出报告本身。
    #[arg(short, long, global = true)]
    quiet: bool,
}

/// 子命令。
#[derive(Debug, Subcommand)]
enum Command {
    /// 工程质量审查：检查规范、冗余包装、无意义抽象与低质量注释。
    Inspect(InspectArgs),

    /// 需求驱动的行为验证（阶段三提供）。
    Verify(VerifyArgs),

    /// 变异测试：度量测试套件能否发现行为变化（FR-037）。
    Mutate(MutateArgs),

    /// 简化方案分析：生成候选、隔离验证，人工确认后才落地（FR-050 … FR-056）。
    Simplify(SimplifyArgs),

    /// 自主验证循环：由程序结构与测试反馈动态调整验证任务（FR-044 … FR-049）。
    Agent(AgentArgs),

    /// 查看最近一次报告。
    Report(ReportArgs),

    /// 工程规范与豁免管理（FR-057 … FR-062）。
    Rules(RulesArgs),

    /// 生成 `proofforge.toml` 配置模板。
    Init(InitArgs),
}

/// `rules` 参数。
#[derive(Debug, Args)]
struct RulesArgs {
    /// 子操作。
    #[command(subcommand)]
    command: RulesCommand,
}

/// `rules` 的子操作。
#[derive(Debug, Subcommand)]
enum RulesCommand {
    /// 把内置工程规范渲染成 Markdown（FR-057）。
    Standard,

    /// 导入项目规范并打印提取到的规则与导入时做出的判断（FR-058）。
    Import {
        /// 规范文件（AGENTS.md / CLAUDE.md / Markdown / YAML / TOML）。
        #[arg(value_name = "FILE")]
        path: PathBuf,
    },

    /// 检测一组规范里的相互矛盾（FR-060）。
    Conflicts {
        /// 参与检测的规范文件；缺省只检测内置规范。
        #[arg(value_name = "FILE")]
        paths: Vec<PathBuf>,
    },

    /// 列出豁免文件里的全部豁免，含是否已过期（FR-062）。
    Exemptions {
        /// 豁免文件（TOML 或 YAML）。
        #[arg(value_name = "FILE")]
        path: PathBuf,

        /// 判断过期的参考日期，缺省为今天。
        #[arg(long, value_name = "DATE")]
        reference_date: Option<String>,
    },
}

/// `inspect` 参数。
#[derive(Debug, Args)]
struct InspectArgs {
    /// 目标路径（本地 Git 仓库或代码目录）。
    ///
    /// 省略时为 `.`，或被 `--config` 里 `target` 指定的路径取代。
    #[arg(value_name = "PATH")]
    target: Option<PathBuf>,

    /// 从 TOML 配置文件读取设置；显式给出的命令行参数优先。
    ///
    /// 文件里的相对路径按**该配置文件所在目录**解析，因此配置放在 `configs/`
    /// 下也能指向仓库根。`rule_files` 与 `exemption_files` 例外：它们按目标
    /// 根目录解析，因为规范与豁免是跟着项目走的决策。
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// 审查指定 Git 引用而不是工作区。
    #[arg(long, value_name = "REV")]
    revision: Option<String>,

    /// 只审查相对该引用的变更文件。
    #[arg(long, value_name = "REV")]
    baseline: Option<String>,

    /// 报告格式。
    #[arg(long, value_enum)]
    format: Option<CliReportFormat>,

    /// 报告输出路径（默认写到标准输出）。
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// 阻断合并的最低严重程度。
    #[arg(long, value_enum)]
    block_on: Option<CliSeverity>,

    /// 单个外部命令超时（秒）。
    #[arg(long)]
    timeout: Option<u64>,

    /// 最多分析的文件数。
    #[arg(long)]
    max_files: Option<usize>,

    /// 把本次运行写入本地 SQLite 历史。
    #[arg(long, value_name = "FILE")]
    store: Option<PathBuf>,

    /// 导入项目规范（AGENTS.md / CLAUDE.md / Markdown / YAML / TOML），可重复。
    ///
    /// Markdown 里的规范会被登记为「人工审查项」而不是自动检查：把一句意图猜成
    /// 一条可执行查询，再报告「该规则已检查并通过」，等于伪造证据（红线 4）。
    #[arg(long, value_name = "FILE")]
    standard: Vec<PathBuf>,

    /// 加载豁免文件（TOML 或 YAML），可重复（FR-062）。
    ///
    /// 豁免只把发现的呈现改成「已豁免」并附上理由与批准人，发现本身仍在报告里。
    #[arg(long, value_name = "FILE")]
    exemption: Vec<PathBuf>,

    /// 判断豁免是否过期的参考日期（`YYYY-MM-DD`），缺省为今天。
    #[arg(long, value_name = "DATE")]
    reference_date: Option<String>,
}

/// `verify` 参数。
#[derive(Debug, Args)]
struct VerifyArgs {
    /// 需求文档路径（记录进报告，供人工追溯性质从哪条需求来）。
    #[arg(long, value_name = "FILE")]
    requirements: Option<PathBuf>,

    /// 被验证的目标路径。
    #[arg(long, value_name = "PATH", default_value = ".")]
    target: PathBuf,

    /// 性质规格文件（JSON）。缺省时只报告「没有可验证的性质」，不假装通过。
    #[arg(long, value_name = "FILE")]
    properties: Option<PathBuf>,

    /// 蜕变关系规格文件（JSON，FR-039）。
    ///
    /// 与 `--properties` 可以同时给出：一条性质检查「输出应该是什么」，一条蜕变
    /// 关系检查「两个相关输入的输出之间有什么关系」。两者都不给时返回证据不足。
    #[arg(long, value_name = "FILE")]
    metamorphic: Option<PathBuf>,

    /// 报告格式。
    #[arg(long, value_enum, default_value = "terminal")]
    format: CliReportFormat,

    /// 报告输出路径（默认写到标准输出）。
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// 单个外部命令超时（秒）。
    #[arg(long, default_value = "300")]
    timeout: u64,
}

/// `mutate` 参数。
#[derive(Debug, Args)]
struct MutateArgs {
    /// 被变异的目标路径。
    #[arg(long, value_name = "PATH", default_value = ".")]
    target: PathBuf,

    /// 最多验证多少个变异体。
    ///
    /// 每个变异体都要复制一份工作区并跑一遍完整测试套件，成本线性于这个数。
    /// 默认值是**抽样**而不是全量，报告会写明这一点。
    #[arg(long, default_value = "12")]
    max_mutants: usize,

    /// 报告格式。
    #[arg(long, value_enum, default_value = "terminal")]
    format: CliReportFormat,

    /// 报告输出路径（默认写到标准输出）。
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// 单个外部命令超时（秒）。
    #[arg(long, default_value = "300")]
    timeout: u64,
}

/// `simplify` 参数。
#[derive(Debug, Args)]
struct SimplifyArgs {
    /// 目标文件或目录。
    #[arg(long, value_name = "PATH", default_value = ".")]
    target: PathBuf,

    /// 用于行为比较的性质规格文件（JSON）。
    ///
    /// 不给这个参数时，行为比较只能依赖目标项目自带的测试套件，报告会明确
    /// 写出覆盖范围的缺口。
    #[arg(long, value_name = "FILE")]
    properties: Option<PathBuf>,

    /// 最多验证多少个候选。每个候选都要隔离一份工作区并跑一遍测试。
    #[arg(long, default_value = "3")]
    max_candidates: usize,

    /// 报告格式。
    #[arg(long, value_enum, default_value = "terminal")]
    format: CliReportFormat,

    /// 把通过验证的补丁应用到目标仓库。
    ///
    /// 只有在看过报告之后才应传这个 flag：它等价于 FR-055 的人工确认。
    /// 未通过回归或行为验证的候选仍然不会被应用。
    #[arg(long)]
    yes: bool,

    /// 只应用指定 ID 的候选（可重复）。缺省应用全部通过验证的候选。
    #[arg(long, value_name = "ID")]
    candidate: Vec<String>,

    /// 单个外部命令超时（秒）。
    #[arg(long, default_value = "300")]
    timeout: u64,
}

/// `agent` 参数。
#[derive(Debug, Args)]
struct AgentArgs {
    /// 目标文件或目录（用于生成验证计划的结构输入）。
    #[arg(long, value_name = "PATH", default_value = ".")]
    target: PathBuf,

    /// 性质规格文件（JSON），提供行为验证任务的性质。
    #[arg(long, value_name = "FILE")]
    properties: Option<PathBuf>,

    /// 是否把「跑现有测试」纳入计划。
    #[arg(long, default_value = "true")]
    include_existing_tests: bool,

    /// 每个目标最多生成几条性质。
    #[arg(long, default_value = "1")]
    max_properties_per_target: usize,

    /// 是否对高优先级目标追加模糊测试。
    #[arg(long)]
    fuzz: bool,

    /// 工具调用次数上限（FR-046 预算）。
    #[arg(long, default_value = "50")]
    max_tool_calls: u64,

    /// 会话标识（进轨迹与审计记录）。
    #[arg(long, value_name = "ID", default_value = "cli-session")]
    session_id: String,

    /// 报告格式。
    #[arg(long, value_enum, default_value = "terminal")]
    format: CliReportFormat,
}

/// `report` 参数。
#[derive(Debug, Args)]
struct ReportArgs {
    /// 展示最近一次运行。
    #[arg(long)]
    latest: bool,

    /// 展示质量趋势而不是最近一次运行（FR-069）。
    #[arg(long)]
    trend: bool,

    /// 展示规则集版本历史（FR-061）。
    #[arg(long)]
    rulesets: bool,

    /// 项目根目录（趋势与版本历史按项目过滤）。
    #[arg(long, value_name = "PATH")]
    project: Option<PathBuf>,

    /// 历史数据库路径。
    #[arg(long, value_name = "FILE", default_value = ".proofforge/history.db")]
    store: PathBuf,
}

/// `init` 参数。
#[derive(Debug, Args)]
struct InitArgs {
    /// 输出路径。
    #[arg(long, value_name = "FILE", default_value = "proofforge.toml")]
    output: PathBuf,
}

/// CLI 层的报告格式枚举（与 [`ReportFormat`] 解耦，便于独立演进帮助文本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliReportFormat {
    /// 人类可读的终端报告。
    Terminal,
    /// 机器可读 JSON。
    Json,
    /// GitHub 代码扫描格式。
    Sarif,
    /// 单文件 HTML 报告。
    Html,
}

impl From<CliReportFormat> for ReportFormat {
    fn from(value: CliReportFormat) -> Self {
        match value {
            CliReportFormat::Terminal => Self::Terminal,
            CliReportFormat::Json => Self::Json,
            CliReportFormat::Sarif => Self::Sarif,
            CliReportFormat::Html => Self::Html,
        }
    }
}

/// CLI 层的严重程度枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliSeverity {
    /// blocker
    Blocker,
    /// high
    High,
    /// medium
    Medium,
    /// low
    Low,
    /// info
    Info,
}

impl From<CliSeverity> for Severity {
    fn from(value: CliSeverity) -> Self {
        match value {
            CliSeverity::Blocker => Self::Blocker,
            CliSeverity::High => Self::High,
            CliSeverity::Medium => Self::Medium,
            CliSeverity::Low => Self::Low,
            CliSeverity::Info => Self::Info,
        }
    }
}

/// 程序入口。
fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.global.verbose, cli.global.quiet);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(reason) => {
            eprintln!("proofforge: 无法创建异步运行时: {reason}");
            return ExitCode::from(3);
        }
    };

    match runtime.block_on(run(&cli)) {
        Ok(code) => code,
        Err(reason) => {
            eprintln!("proofforge: {reason}");
            ExitCode::from(2)
        }
    }
}

/// 初始化结构化日志。
fn init_tracing(verbose: u8, quiet: bool) {
    use tracing_subscriber::EnvFilter;

    if quiet {
        return;
    }
    let level = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// 分发到具体子命令。
async fn run(cli: &Cli) -> anyhow::Result<ExitCode> {
    match &cli.command {
        Command::Inspect(args) => run_inspect(args, &cli.global).await,
        Command::Verify(args) => run_verify(args),
        Command::Mutate(args) => run_mutate(args),
        Command::Simplify(args) => run_simplify(args),
        Command::Agent(args) => run_agent(args),
        Command::Report(args) => run_report(args).await,
        Command::Rules(args) => run_rules(args),
        Command::Init(args) => run_init(args),
    }
}

/// 读取 `inspect` 的配置文件。
///
/// 不给 `--config` 时返回内置默认值。文件里的相对路径按配置文件所在目录解析，
/// 因此 `configs/proofforge.toml` 写 `target = ".."` 指向的就是仓库根；写 `.`
/// 时指向 `configs/` 自己——这个歧义靠「配置放在哪」消除，不靠魔法。
///
/// # Errors
///
/// 文件读不到或不是合法 TOML 时返回错误，并带上路径与原因：一份坏配置应该让人
/// 知道审查根本没按预期跑，而不是静默用默认值跑一遍。
fn load_config(path: Option<&Path>) -> anyhow::Result<Config> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    let text = std::fs::read_to_string(path)
        .map_err(|reason| anyhow::anyhow!("读不到配置 {}：{reason}", path.display()))?;
    let mut config = Config::from_toml(&text, &path.display().to_string())
        .map_err(|reason| anyhow::anyhow!("{}：{reason}", path.display()))?;
    if config.target.is_relative() {
        if let Some(base) = path.parent().filter(|base| !base.as_os_str().is_empty()) {
            config.target = base.join(&config.target);
        }
    }
    Ok(config)
}

/// 执行 `inspect`。
async fn run_inspect(args: &InspectArgs, global: &GlobalArgs) -> anyhow::Result<ExitCode> {
    let quiet = global.quiet;
    // 优先级：命令行 > 配置文件 > 内置默认值。只有显式给出的参数才覆盖配置，
    // 因此 `--block-on` 不传时用的是配置文件里的值，而不是默认的 high。
    let mut config = load_config(args.config.as_deref())?;
    if let Some(target) = &args.target {
        config.target = target.clone();
    }
    if let Some(revision) = &args.revision {
        config.revision = Some(revision.clone());
    }
    if let Some(baseline) = &args.baseline {
        config.baseline = Some(baseline.clone());
    }
    if let Some(format) = args.format {
        config.report_format = format.into();
    }
    if let Some(output) = &args.output {
        config.report_path = Some(output.clone());
    }
    if let Some(block_on) = args.block_on {
        config.block_on = block_on.into();
    }
    // 显式给出的规范与豁免**替换**而不是追加配置文件里的列表：追加会让
    // 「我以为只导入了这一份」变成假话。
    if !args.standard.is_empty() {
        config.rule_files = args.standard.clone();
    }
    if !args.exemption.is_empty() {
        config.exemption_files = args.exemption.clone();
    }
    if let Some(timeout) = args.timeout {
        config.budget.command_timeout_secs = timeout;
    }
    if let Some(max_files) = args.max_files {
        config.budget.max_files = max_files;
    }
    config.verbose = global.verbose > 0;

    // 豁免过期判断的「今天」由 CLI 提供而不是库自己读时钟：这样同一份代码加同一个
    // 日期必然得到同一份报告（FR-061 可审计性）。
    let reference_date = args
        .reference_date
        .clone()
        .unwrap_or_else(pf_core::exempt::today_iso);
    let report = ReviewEngine::review_at(&config, &reference_date)?;
    let rendered = render_report(&report, config.report_format)?;

    match &config.report_path {
        Some(path) => {
            std::fs::write(path, rendered)?;
            if !quiet {
                println!("报告已写入 {}", path.display());
                println!("{}", pf_report::summary_line(&report));
            }
        }
        None => println!("{rendered}"),
    }

    if let Some(path) = &args.store {
        let store = Store::open(path).await?;
        store.save_run(&report).await?;
        if !quiet {
            println!("运行记录已写入 {}", path.display());
        }
    }

    Ok(ExitCode::from(report.gate.exit_code()))
}

/// 执行 `verify`。
///
/// # 诚实的边界
///
/// 「从自然语言需求里自动抽出性质」需要语义理解，属于阶段二的 AI 语义分析，
/// 依赖阶段四的 Agent Runtime。在那之前，`--requirements` 只作为**追溯信息**
/// 记录进报告，不参与生成性质——把一句需求猜测成一条性质，再把它跑出来的结论
/// 说成「已验证」，正是红线 5 禁止的事。
///
/// 因此 `--properties` 是当前唯一能产出证据的输入。没有它时返回「证据不足」，
/// 而不是「通过」（红线 4）。
fn run_verify(args: &VerifyArgs) -> anyhow::Result<ExitCode> {
    let specs = match &args.properties {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|reason| anyhow::anyhow!("读不到性质规格 {}：{reason}", path.display()))?;
            pf_verify::PropertyFile::from_json(&text)
                .map_err(|reason| anyhow::anyhow!("{}：{reason}", path.display()))?
                .specs()
        }
        None => Vec::new(),
    };
    let relations = match &args.metamorphic {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|reason| {
                anyhow::anyhow!("读不到蜕变关系规格 {}：{reason}", path.display())
            })?;
            pf_verify::MetamorphicFile::from_json(&text)
                .map_err(|reason| anyhow::anyhow!("{}：{reason}", path.display()))?
                .specs()
        }
        None => Vec::new(),
    };
    if specs.is_empty() && relations.is_empty() {
        eprintln!(
            "proofforge verify: 没有给出 --properties 或 --metamorphic，因此没有任何东西被验证。\n  \
             需求文档到性质的自动转换尚未实现（依赖阶段四的 Agent Runtime），\n  \
             在此之前请用 --properties 指定显式的性质规格，或用 --metamorphic 指定蜕变关系。\n  \
             退出码 2 = 证据不足，不是「验证通过」。"
        );
        return Ok(ExitCode::from(GateStatus::InsufficientEvidence.exit_code()));
    }

    let mut config = Config {
        target: args.target.clone(),
        report_format: args.format.into(),
        report_path: args.output.clone(),
        ..Config::default()
    };
    config.budget.command_timeout_secs = args.timeout;

    let report = pf_verify::VerificationEngine::verify(&args.target, &specs, &relations, &config)?;
    match &config.report_path {
        Some(destination) => {
            let rendered = render_verification(&report, config.report_format)?;
            std::fs::write(destination, rendered)?;
            // 与 `inspect` 一致：写到文件时只打印摘要，不把整份报告再倒到终端。
            println!("报告已写入 {}", destination.display());
            for entry in &report.entries {
                println!(
                    "  [{}] {}",
                    entry.status,
                    entry.name.as_deref().unwrap_or(&entry.target)
                );
            }
            println!("  门禁：{}（退出码 {}）", report.gate(), report.exit_code());
        }
        None => println!("{}", render_verification(&report, config.report_format)?),
    }
    if let Some(requirements) = &args.requirements {
        println!("需求文档：{}", requirements.display());
    }
    Ok(ExitCode::from(report.exit_code()))
}

/// 执行 `mutate`（FR-037）。
///
/// # 诚实的边界
///
/// 变异通过率度量的是**测试套件**，不是程序。一个幸存变异体说的是「测试分不出
/// 原版与变异版」，不是「程序有缺陷」——因此幸存者以建议级呈现，门禁不因它们失败。
///
/// 反过来，编译不过的变异体**不计入分母**：把它算成幸存会凭空放大覆盖缺口，
/// 算成已杀会伪造「测试发现了问题」。基线测试本来就红着时整个运行没有结论，
/// 此时返回证据不足而不是一个分数（红线 4）。
fn run_mutate(args: &MutateArgs) -> anyhow::Result<ExitCode> {
    let mut config = Config {
        target: args.target.clone(),
        ..Config::default()
    };
    config.budget.command_timeout_secs = args.timeout;

    let outcome = pf_verify::VerificationEngine::mutate(
        &args.target,
        &pf_verify::MutationConfig {
            max_mutants: args.max_mutants.max(1),
        },
        &config.budget,
    )?;

    let rendered = match args.format {
        CliReportFormat::Json => outcome.to_json()?,
        _ => render_mutation(&outcome),
    };
    match &args.output {
        Some(destination) => {
            std::fs::write(destination, rendered)?;
            println!("报告已写入 {}", destination.display());
        }
        None => print!("{rendered}"),
    }
    Ok(ExitCode::from(mutation_exit_code(&outcome)))
}

/// 判断一个路径是否是测试代码。
///
/// 判据与 `pf-rules` 的 `is_test_path` 一致但刻意不共享：`pf-verify` 不依赖任何
/// 静态分析 crate（`docs/architecture.md`），而它需要这个区分来如实解释幸存条目。
/// 两处若漂移，表现是「报告把测试文件里的幸存说成生产代码缺口」——因此这里只认
/// 最不容易误判的两个特征：`tests/` 目录与 `test_` / `_test` 前缀后缀。
fn is_test_source(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.split('/')
        .any(|segment| segment == "tests" || segment == "test")
        || name.starts_with("test_")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.py")
}

/// `mutate` 的退出码。
///
/// 基线不可用时是 2（证据不足）：原版测试就红着的时候，任何变异体的失败都无法
/// 归因于变异本身。基线可用时是 0——幸存变异体是测试覆盖缺口这一侧的建议，
/// 不是「代码坏了」，让门禁失败会把两件事混为一谈。
fn mutation_exit_code(outcome: &pf_verify::MutationOutcome) -> u8 {
    if outcome.baseline_usable() {
        GateStatus::Passed.exit_code()
    } else {
        GateStatus::InsufficientEvidence.exit_code()
    }
}

/// 渲染变异测试报告（终端）。
fn render_mutation(outcome: &pf_verify::MutationOutcome) -> String {
    let mut out = String::from("`ProofForge` 变异测试（FR-037）\n\n");
    out.push_str(&format!("基线测试：{:?}\n", outcome.baseline));
    if !outcome.baseline_usable() {
        out.push_str(
            "\n基线测试没有通过，因此本次运行没有结论：原版就红着的时候，\n\
             任何变异体的失败都无法归因于变异本身。退出码 2 = 证据不足，\n\
             不是「测试完全无效」。\n",
        );
        return out;
    }
    out.push_str(&format!("{}\n", outcome.summary()));
    if let Some(score) = outcome.score() {
        out.push_str(&format!(
            "通过率 {:.0}%（基于已杀死 + 幸存的 {} 个变异体）\n",
            score * 100.0,
            outcome.killed() + outcome.survived()
        ));
    }
    let survivors: Vec<&pf_verify::MutantResult> = outcome.survivors().collect();
    if survivors.is_empty() {
        out.push_str("\n没有幸存变异体：这批抽样里的每一处行为变化都被测试发现了。\n");
    } else {
        out.push_str(&format!(
            "\n幸存的变异体 {} 个——测试套件没有发现这些行为变化：\n",
            survivors.len()
        ));
        for result in &survivors {
            out.push_str(&format!("  - {}\n", result.mutant.describe()));
        }
        out.push_str(
            "\n幸存说明的是测试覆盖有缺口，不说明程序有缺陷。这些条目是\n\
             `suspected`，不让门禁失败。\n",
        );
        // 幸存条目要按「被测代码」与「测试代码」分开解释：两者的含义不同。
        // 混在一个数里会让人把「测试自己的输入选得不够准」读成「生产代码没被覆盖」。
        let in_tests = survivors
            .iter()
            .filter(|result| is_test_source(&result.mutant.path))
            .count();
        if in_tests > 0 {
            out.push_str(&format!(
                "\n其中 {in_tests} 个位于测试文件：它们度量的是断言强度——\n\
                 测试选的输入落在不变式成立的区间里，改动因此被掩盖。\n\
                 剩下 {} 个位于被测代码，才是覆盖缺口。\n",
                survivors.len() - in_tests
            ));
        }
    }
    if outcome.invalid() > 0 || outcome.inconclusive() > 0 {
        out.push_str(&format!(
            "\n另有 {} 个变异体编译不过、{} 个没有得到结论：它们不进入通过率的分母。\n",
            outcome.invalid(),
            outcome.inconclusive()
        ));
    }
    out
}

/// 渲染验证报告。
fn render_verification(
    report: &pf_verify::VerificationReport,
    format: ReportFormat,
) -> anyhow::Result<String> {
    match format {
        ReportFormat::Terminal => Ok(report.render_terminal()),
        // 验证报告的 JSON 视图由 pf-verify 自己提供：它的证据结构（性质、反例、
        // 复现命令）与审查报告不同，硬塞进 pf-report 的 SARIF schema 只会得到
        // 一份谁都用不了的产物。
        ReportFormat::Json => report.to_json().map_err(Into::into),
        // SARIF 的 result 模型描述的是「代码位置上的一条规则命中」；一条被违反的
        // 性质不是位置命中，而是「某个输入让程序失败了」。把后者塞进前者要靠
        // logicalLocation 勉强表达，容易让消费方误读。宁可直接说不支持，也不给
        // 一份看起来对、其实语义错位的 SARIF。
        ReportFormat::Sarif => Err(anyhow::anyhow!(
            "verify 暂不支持 --format sarif：验证结论不是代码位置命中，\n  \
             硬套 SARIF 会让消费方误读。请用 --format terminal 或 --format json。"
        )),
        // HTML 渲染器只覆盖审查报告：验证报告的核心是性质、反例与复现命令，
        // 套用审查报告的 HTML 结构会漏掉最关键的部分。宁可不给，也不给一份
        // 看起来完整、其实把证据丢掉了的报告。
        ReportFormat::Html => Err(anyhow::anyhow!(
            "verify 暂不支持 --format html：验证报告的证据结构（性质、反例、\n  \
             复现命令）与审查报告不同。请用 --format terminal 或 --format json。"
        )),
    }
}

/// 执行 `simplify`（FR-050 … FR-056）。
///
/// # 诚实的边界
///
/// 分析阶段只读目标仓库；只有 `--yes` 才会写，而且只写通过回归与行为验证的
/// 候选。没有 `--yes` 时命令的产出是一份可审查的报告加一份补丁，不产生任何
/// 文件变化——因此它可以安全地跑在 CI 上做「这里有什么可以简化」的提示。
fn run_simplify(args: &SimplifyArgs) -> anyhow::Result<ExitCode> {
    let properties = match &args.properties {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|reason| anyhow::anyhow!("读不到性质规格 {}：{reason}", path.display()))?;
            Some(
                pf_verify::PropertyFile::from_json(&text)
                    .map_err(|reason| anyhow::anyhow!("{}：{reason}", path.display()))?,
            )
        }
        None => None,
    };

    let mut config = SimplifyConfig {
        review: Config {
            target: args.target.clone(),
            ..Config::default()
        },
        max_candidates: args.max_candidates.max(1),
        properties: properties
            .as_ref()
            .map_or_else(Vec::new, pf_verify::PropertyFile::specs),
    };
    config.review.budget.command_timeout_secs = args.timeout;

    let report = SimplifyEngine::analyze(&args.target, &config)?;
    match args.format {
        CliReportFormat::Json => println!("{}", report.to_json()?),
        _ => print!("{}", report.render_terminal()),
    }

    if !args.yes {
        // 提示写到 stderr：--format json 的 stdout 必须能整段解析。
        eprintln!(
            "以上为候选与验证结论。确认无误后用 `--yes` 应用（可配合 --candidate <ID> 只应用其中几条）。"
        );
        return Ok(ExitCode::from(report.exit_code()));
    }

    let authorization = Authorization::Explicit {
        decided_by: std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "cli".to_owned()),
        note: format!(
            "proofforge simplify --yes（{} 个候选）",
            report.candidates.len()
        ),
    };
    let outcome = pf_simplify::apply(&report, &args.target, &authorization, &args.candidate)?;
    println!("\n已应用 {} 个补丁：", outcome.applied.len());
    for id in &outcome.applied {
        println!("  + {id}");
    }
    for (id, reason) in &outcome.skipped {
        println!("  - {id}：{reason}");
    }
    for (id, path) in &outcome.revert_patches {
        println!("  回滚补丁：{path}（对应 {id}）");
    }
    Ok(ExitCode::from(GateStatus::Passed.exit_code()))
}

/// 构造 Agent 的候选列表（FR-044）。
///
/// 计划的结构输入有两类：显式性质规格（说明「要验证什么」，带调用模板与输入域）
/// 与规则命中（说明「这里值得验证」）。前者优先，因为它携带可执行信息。
fn agent_candidates(
    args: &AgentArgs,
    properties: &pf_verify::PropertyFile,
) -> anyhow::Result<Vec<pf_agent::PlanCandidate>> {
    let mut candidates: Vec<pf_agent::PlanCandidate> = properties
        .specs()
        .into_iter()
        .map(|spec| pf_agent::PlanCandidate {
            target: spec
                .name
                .clone()
                .unwrap_or_else(|| spec.call_template.clone()),
            call_template: spec.call_template.clone(),
            domain: serde_json::to_value(&spec.domain).unwrap_or(serde_json::Value::Null),
            invariant: spec.invariant.clone(),
            evidence: pf_agent::CandidateEvidence {
                rules: vec!["property-spec".to_owned()],
                severity: Some(pf_core::rule::RuleSeverity::Project),
                locations: Vec::new(),
                has_callers: true,
                is_public: true,
            },
        })
        .collect();

    let review = ReviewEngine::review(&Config {
        target: args.target.clone(),
        ..Config::default()
    })?;
    candidates.extend(
        review
            .findings
            .iter()
            .filter(|finding| {
                finding.kind.category() == pf_core::finding::FindingCategory::Correctness
            })
            .map(|finding| pf_agent::PlanCandidate {
                target: finding.primary_location().map_or_else(
                    || finding.rule_id.get().to_owned(),
                    |location| location.path.clone(),
                ),
                call_template: String::new(),
                domain: serde_json::Value::Null,
                invariant: finding.title.clone(),
                evidence: pf_agent::CandidateEvidence {
                    rules: vec![finding.rule_id.get().to_owned()],
                    // 计划器用 RuleSeverity 排优先级；Finding 上的 Severity 是报告
                    // 用的分级，两者语义不同，不能直接传。
                    severity: Some(match finding.severity {
                        Severity::Blocker | Severity::High => {
                            pf_core::rule::RuleSeverity::Mandatory
                        }
                        Severity::Medium => pf_core::rule::RuleSeverity::Project,
                        Severity::Low | Severity::Info => pf_core::rule::RuleSeverity::Advisory,
                    }),
                    locations: finding.locations.clone(),
                    has_callers: true,
                    is_public: true,
                },
            })
            .collect::<Vec<_>>(),
    );
    Ok(candidates)
}

/// 执行 `agent`（FR-044 … FR-049）。
///
/// # 诚实的边界
///
/// 当前 Runtime 的计划由**程序结构**驱动（符号索引 + 规则命中），模型接缝
/// （[`pf_agent::provider::LlmProvider`]）存在但没有内置任何外部调用。这不
/// 影响命令的可用性：结构与反馈已经能决定「跑哪些性质、先跑哪个、何时停」。
/// 轨迹逐条记录选择依据，因此同一份输入可以复现同一次会话（FR-043）。
fn run_agent(args: &AgentArgs) -> anyhow::Result<ExitCode> {
    let properties = match &args.properties {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|reason| anyhow::anyhow!("读不到性质规格 {}：{reason}", path.display()))?;
            pf_verify::PropertyFile::from_json(&text)
                .map_err(|reason| anyhow::anyhow!("{}：{reason}", path.display()))?
        }
        None => pf_verify::PropertyFile::empty(),
    };

    let candidates = agent_candidates(args, &properties)?;

    let config = pf_agent::AgentConfig {
        budget: pf_agent::ExecutionBudget {
            tool_calls: Some(u32::try_from(args.max_tool_calls).unwrap_or(u32::MAX)),
            // 显式放开模型费用上限：默认预算是 0（不许花钱），结构计划不调用模型，
            // 但接外部 Provider 时必须由使用者显式放宽，而不是默默开始计费。
            model_cost_micros: Some(1000),
            ..pf_agent::ExecutionBudget::default()
        },
        include_existing_tests: args.include_existing_tests,
        max_properties_per_target: args.max_properties_per_target.max(1),
        fuzz_high_priority_targets: args.fuzz,
        session_id: args.session_id.clone(),
        // 目标路径必须传进去：任务要真实执行（跑测试、编译驱动、模糊测试），
        // 没有目标就没有任何可执行的东西，那时每个任务都会记「证据不足」。
        target: Some(args.target.clone()),
    };

    let session = pf_agent::run(&candidates, &config)?;
    let gate = session.gate();
    if args.format == CliReportFormat::Json {
        println!("{}", serde_json::to_string_pretty(&session)?);
    } else {
        println!(
            "会话 {}：{}",
            session.session_id,
            session.final_state.as_str()
        );
        if let Some(reason) = &session.stop_reason {
            println!("停止原因：{reason:?}");
        }
        println!("完成任务：{}", session.completed_tasks.len());
        println!("预算用量：{:?}", session.budget_usage);
        // 逐条任务的结论必须出现在终端里：轨迹记录的是「做了什么」，
        // 这里回答「结论是什么」。两者缺一个，会话就不可审计（FR-049）。
        for (key, summary) in session.task_summaries() {
            println!("  {key}：{summary}");
        }
        println!("轨迹（{} 条）：", session.trace.entries().len());
        for entry in session.trace.entries() {
            println!("  [{}] {}", entry.kind.as_str(), entry.message);
        }
        println!("\n门禁结论：{gate}");
    }

    // 退出码就是门禁结论本身：复现了缺陷必须是 1，没跑完必须是 2。
    // 曾经这里只看「会话是否正常结束」，于是带着一条已复现缺陷的会话会返回 0
    // ——那正是红线 4 要防的「没做完当成通过」。
    Ok(ExitCode::from(gate.exit_code()))
}

/// 执行 `report`。
///
/// 三种视图共用同一个数据库：最近一次运行（默认）、质量趋势（`--trend`）与
/// 规则集版本历史（`--rulesets`）。趋势与版本历史需要按项目过滤，否则多个项目
/// 混在一张表里会得到一条没有意义的折线。
async fn run_report(args: &ReportArgs) -> anyhow::Result<ExitCode> {
    let store = Store::open(&args.store).await?;
    let project = args
        .project
        .clone()
        .unwrap_or_else(|| PathBuf::from("."))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));

    if args.trend {
        return run_trend(&store, &project).await;
    }
    if args.rulesets {
        return run_ruleset_history(&store, &project).await;
    }

    let runs = store.recent_runs(1).await?;
    let Some(latest) = runs.first() else {
        eprintln!(
            "proofforge report: {} 中还没有任何运行记录。\n  \
             先执行：proofforge inspect . --store {}",
            args.store.display(),
            args.store.display()
        );
        return Ok(ExitCode::from(GateStatus::InsufficientEvidence.exit_code()));
    };
    println!("最近一次运行");
    println!("  run_id : {}", latest.run_id);
    println!("  项目   : {}", latest.project_root);
    println!("  版本   : {}", latest.revision);
    println!("  门禁   : {}", latest.gate);
    println!("  发现数 : {}", latest.finding_count);
    Ok(ExitCode::from(GateStatus::Passed.exit_code()))
}

/// 打印质量趋势（FR-069）。
async fn run_trend(store: &Store, project: &Path) -> anyhow::Result<ExitCode> {
    let points = store.trend(&project.display().to_string(), 50).await?;
    if points.is_empty() {
        eprintln!(
            "proofforge report --trend: {} 下还没有任何运行记录。\n  \
             先执行：proofforge inspect {} --store <数据库>",
            project.display(),
            project.display()
        );
        return Ok(ExitCode::from(GateStatus::InsufficientEvidence.exit_code()));
    }
    println!("质量趋势：{}", project.display());
    println!("  时间                 门禁                发现  豁免  阻断  文件  规则集");
    for point in &points {
        println!(
            "  {}  {:<18} {:<5} {:<5} {:<5} {:<5} {}",
            point.created_at,
            point.gate.as_str(),
            point.finding_count,
            point.exempted_count,
            point.blocking_count,
            point.file_count,
            point.ruleset_version
        );
    }
    // 趋势必须能自证「下降是不是因为豁免变多」：只报首尾两个数会骗人。
    let first = &points[0];
    let last = &points[points.len() - 1];
    println!(
        "  变化：发现 {} → {}，豁免 {} → {}，阻断 {} → {}",
        first.finding_count,
        last.finding_count,
        first.exempted_count,
        last.exempted_count,
        first.blocking_count,
        last.blocking_count
    );
    Ok(ExitCode::from(GateStatus::Passed.exit_code()))
}

/// 打印规则集版本历史（FR-061）。
async fn run_ruleset_history(store: &Store, project: &Path) -> anyhow::Result<ExitCode> {
    let history = store
        .ruleset_history(&project.display().to_string())
        .await?;
    if history.is_empty() {
        eprintln!(
            "proofforge report --rulesets: {} 下还没有任何运行记录。",
            project.display()
        );
        return Ok(ExitCode::from(GateStatus::InsufficientEvidence.exit_code()));
    }
    println!("规则集版本历史：{}", project.display());
    println!("  版本        运行次数  首次使用            最近使用");
    for record in &history {
        println!(
            "  {:<10} {:<9} {:<20} {}",
            record.version, record.run_count, record.first_seen, record.last_seen
        );
    }
    Ok(ExitCode::from(GateStatus::Passed.exit_code()))
}

/// 执行 `rules`（FR-057 … FR-062）。
///
/// 全部子操作都是只读的：它们回答「规范是什么、导入了什么、哪里矛盾、豁免了
/// 什么」，不修改被审查仓库的任何文件。
fn run_rules(args: &RulesArgs) -> anyhow::Result<ExitCode> {
    match &args.command {
        RulesCommand::Standard => {
            // FR-057：规范要能一次性摊开给人看，否则「可解释」只是一句空话。
            print!(
                "{}",
                pf_rules::render_standard(&pf_rules::builtin_ruleset())
            );
            Ok(passed())
        }
        RulesCommand::Import { path } => {
            print_imported_standard(path)?;
            Ok(passed())
        }
        RulesCommand::Conflicts { paths } => print_conflicts(paths),
        RulesCommand::Exemptions {
            path,
            reference_date,
        } => print_exemptions(path, reference_date.as_deref()),
    }
}

/// 「命令跑完了」的退出码。
fn passed() -> ExitCode {
    ExitCode::from(GateStatus::Passed.exit_code())
}

/// 打印一次项目规范导入的结果（FR-058）。
fn print_imported_standard(path: &Path) -> anyhow::Result<()> {
    let imported = pf_rules::import_file(path)?;
    let automated = imported
        .ruleset
        .rules
        .iter()
        .filter(|rule| rule.is_automated())
        .count();
    println!("导入 {}", path.display());
    println!(
        "  规则集   : {}@{}",
        imported.ruleset.name, imported.ruleset.version
    );
    println!("  规则条数 : {}", imported.ruleset.rules.len());
    println!("  可自动检查 : {automated}");
    for note in &imported.notes {
        println!("  note: {note}");
    }
    println!();
    for rule in &imported.ruleset.rules {
        println!(
            "  [{}] {} — {}",
            if rule.is_automated() {
                "自动"
            } else {
                "人工"
            },
            rule.id.get(),
            rule.description
        );
    }
    Ok(())
}

/// 打印规范冲突（FR-060）。
///
/// 冲突不是错误：它意味着两个团队对同一件事有不同预期，答案取决于项目上下文。
/// 工具负责把冲突摆到桌面上并说明判断依据，裁决留给人（需求文档 1.3）。
fn print_conflicts(paths: &[PathBuf]) -> anyhow::Result<ExitCode> {
    let mut rules: Vec<pf_core::rule::Rule> = pf_rules::builtin_ruleset().rules;
    for path in paths {
        let imported = pf_rules::import_file(path)?;
        rules.extend(imported.ruleset.rules);
    }
    let conflicts = pf_rules::detect_conflicts(&rules);
    if conflicts.is_empty() {
        println!("未检测到规范冲突。");
        return Ok(passed());
    }
    println!("检测到 {} 处规范冲突：", conflicts.len());
    for conflict in &conflicts {
        let involved: Vec<String> = conflict
            .rules
            .iter()
            .map(|rule| rule.get().to_owned())
            .collect();
        println!("  [{}] {}", conflict.kind.as_str(), involved.join(" ↔ "));
        println!("    {}", conflict.explanation);
        if !conflict.kind.is_deterministic() {
            println!("    （启发式判断：依据是措辞而非语义分析，请人工确认）");
        }
    }
    Ok(passed())
}

/// 打印一个豁免文件里的全部豁免（FR-062）。
fn print_exemptions(path: &Path, reference_date: Option<&str>) -> anyhow::Result<ExitCode> {
    let set = pf_rules::exemptions_from_toml_file(path)
        .or_else(|_| pf_rules::exemptions_from_yaml_file(path))?;
    let today = reference_date.map_or_else(pf_core::exempt::today_iso, str::to_owned);
    println!("豁免：{}（参考日期 {today}）", path.display());
    if set.exemptions.is_empty() {
        println!("  这个文件里没有豁免。");
    }
    for exemption in &set.exemptions {
        let state = if exemption.is_active_on(&today) {
            "生效中"
        } else {
            "已过期"
        };
        println!(
            "  [{state}] {} — {}",
            exemption.rule_id.get(),
            exemption.scope.describe()
        );
        println!("    理由：{}", exemption.reason);
        println!("    批准人：{}", exemption.granted_by);
        if let Some(expiry) = &exemption.expires_on {
            println!("    过期日期：{expiry}");
        }
    }
    let expired = set.expired(&today);
    if !expired.is_empty() {
        // 过期豁免意味着它覆盖的问题已经悄悄回到报告里了，必须让人知道。
        eprintln!(
            "proofforge rules exemptions: {} 条豁免已过期，不再生效。",
            expired.len()
        );
    }
    Ok(passed())
}

/// 执行 `init`。
fn run_init(args: &InitArgs) -> anyhow::Result<ExitCode> {
    let text = Config::default().to_toml()?;
    std::fs::write(&args.output, text)?;
    println!("已生成配置模板 {}", args.output.display());
    Ok(ExitCode::from(GateStatus::Passed.exit_code()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // clap 在构建期校验参数定义；失败说明接口被破坏。
        Cli::command().debug_assert();
    }

    #[test]
    fn inspect_defaults_to_current_directory() {
        let cli = Cli::parse_from(["proofforge", "inspect"]);
        match cli.command {
            // 位置参数是可选项：不传时由配置文件或内置默认值决定目标，
            // 因此这里不能给出一个「看起来传了 `.`」的默认值。
            Command::Inspect(args) => assert_eq!(args.target, None),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn inspect_accepts_all_documented_options() {
        let cli = Cli::parse_from([
            "proofforge",
            "inspect",
            ".",
            "--format",
            "json",
            "--block-on",
            "medium",
            "--store",
            "/tmp/history.db",
            "--revision",
            "HEAD~1",
            "--baseline",
            "main",
            "--timeout",
            "60",
            "--max-files",
            "10",
            "--config",
            "proofforge.toml",
        ]);
        match cli.command {
            Command::Inspect(args) => {
                assert_eq!(args.format, Some(CliReportFormat::Json));
                assert_eq!(args.block_on, Some(CliSeverity::Medium));
                assert_eq!(args.store, Some(PathBuf::from("/tmp/history.db")));
                assert_eq!(args.revision.as_deref(), Some("HEAD~1"));
                assert_eq!(args.baseline.as_deref(), Some("main"));
                assert_eq!(args.timeout, Some(60));
                assert_eq!(args.max_files, Some(10));
                assert_eq!(args.config, Some(PathBuf::from("proofforge.toml")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn config_relative_paths_resolve_against_the_config_directory() {
        // 配置放在子目录里时，`target = ".."` 必须指向仓库根而不是那个子目录：
        // 否则 `--config configs/proofforge.toml` 会去审查 configs/ 自己。
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("configs");
        std::fs::create_dir_all(&nested).expect("mkdir");
        let config_path = nested.join("proofforge.toml");
        std::fs::write(&config_path, "target = \"..\"\nblock_on = \"medium\"\n")
            .expect("write config");

        let config = load_config(Some(&config_path)).expect("load");

        // 路径保持词法形式（`configs/..`）而不在这里规范化：规范化需要访问
        // 文件系统，那是 pf-review::canonical_target 的职责。
        assert_eq!(config.target, nested.join(".."));
        assert_eq!(config.block_on, Severity::Medium);
    }

    #[test]
    fn a_broken_config_is_an_error_not_a_silent_default() {
        // 坏配置必须让人知道审查没按预期跑：静默退回默认值会让「我以为设了
        // block_on = info」变成假话（AGENTS.md 安全红线 4 的同一精神）。
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("proofforge.toml");
        std::fs::write(&config_path, "this is not toml").expect("write config");

        let error = load_config(Some(&config_path)).expect_err("should fail");

        assert!(error.to_string().contains("proofforge.toml"), "{error}");
    }

    #[test]
    fn a_config_without_target_is_valid() {
        // `target` 有 serde 默认值，因此只写预算的配置文件也能加载。
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("proofforge.toml");
        std::fs::write(&config_path, "[budget]\nmax_files = 7\n").expect("write config");

        let config = load_config(Some(&config_path)).expect("load");

        assert_eq!(config.budget.max_files, 7);
        // 没写 target 时取默认值 `.`，再按配置文件所在目录拼接：配置放在仓库
        // 根就意味着「仓库根本身」，不会因为省略字段就指向别处。
        assert_eq!(config.target, dir.path().join("."));
    }

    #[test]
    fn global_flags_are_available_on_subcommands() {
        let cli = Cli::parse_from(["proofforge", "inspect", ".", "-vv", "--quiet"]);
        assert_eq!(cli.global.verbose, 2);
        assert!(cli.global.quiet);
    }

    #[test]
    fn verify_and_simplify_expose_target_interfaces() {
        let verify = Cli::parse_from([
            "proofforge",
            "verify",
            "--requirements",
            "req.md",
            "--target",
            "src/",
            "--properties",
            "props.json",
        ]);
        assert!(matches!(verify.command, Command::Verify(_)));
        match verify.command {
            Command::Verify(args) => {
                assert_eq!(args.properties, Some(PathBuf::from("props.json")));
                assert_eq!(args.target, PathBuf::from("src/"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let simplify = Cli::parse_from(["proofforge", "simplify", "--target", "src/a.rs"]);
        assert!(matches!(simplify.command, Command::Simplify(_)));
        match simplify.command {
            Command::Simplify(args) => assert_eq!(args.target, PathBuf::from("src/a.rs")),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn simplify_analyzes_without_touching_the_target() {
        // 不带 --yes 时 simplify 只产出报告：目标仓库一个字节都不改，
        // 退出码来自门禁而不是「应用成功」。
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "/// Wrapper.\npub fn wrapper(x: i32) -> i32 {\n    real(x)\n}\n\npub fn real(x: i32) -> i32 {\n    x + 1\n}\n",
        )
        .expect("write");
        let before = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");

        let code = run_simplify(&SimplifyArgs {
            target: dir.path().to_path_buf(),
            properties: None,
            max_candidates: 2,
            format: CliReportFormat::Json,
            yes: false,
            candidate: Vec::new(),
            timeout: 60,
        })
        .expect("run");

        let after = std::fs::read_to_string(dir.path().join("src/lib.rs")).expect("read");
        assert_eq!(before, after, "没有 --yes 时不得修改目标仓库");
        // fixture 里没有 Cargo.toml / pyproject，因此没有任何工具链能跑测试：
        // 门禁必须是「证据不足」（exit 2），而不是「通过」（exit 0）——红线 4。
        assert_eq!(code, ExitCode::from(2));
    }

    #[test]
    fn simplify_refuses_to_apply_without_explicit_authorization() {
        // FR-055：没有 --yes 就没有授权， apply 必须拒绝而不是静默跳过。
        let report = pf_simplify::SimplifyReport {
            target: PathBuf::from("."),
            candidates: Vec::new(),
            gate: GateStatus::Passed,
            reasons: Vec::new(),
            residual_risks: Vec::new(),
            ruleset_version: "test".to_owned(),
        };
        let error = pf_simplify::apply(&report, Path::new("."), &Authorization::None, &[])
            .expect_err("refused");
        assert!(matches!(error, pf_simplify::SimplifyError::NotAuthorized));
    }

    #[test]
    fn verify_without_properties_is_insufficient_evidence_not_a_pass() {
        // 没有性质规格就验证不了任何东西。此时返回 exit 0 等于宣布
        // 「没做检查」=「检查通过」，正是红线 4 禁止的。
        let code = run_verify(&VerifyArgs {
            requirements: None,
            target: PathBuf::from("."),
            properties: None,
            metamorphic: None,
            format: CliReportFormat::Terminal,
            output: None,
            timeout: 300,
        })
        .expect("run");
        assert_eq!(code, ExitCode::from(2));
    }

    #[test]
    fn verify_rejects_an_unreadable_properties_file() {
        // 规格文件路径写错必须报错，而不是静悄悄地什么都不验证。
        let error = run_verify(&VerifyArgs {
            requirements: None,
            target: PathBuf::from("."),
            properties: Some(PathBuf::from("/definitely/not/here.json")),
            metamorphic: None,
            format: CliReportFormat::Terminal,
            output: None,
            timeout: 300,
        })
        .expect_err("missing file must fail");
        assert!(error.to_string().contains("读不到性质规格"), "{error}");
    }

    #[test]
    fn enum_conversions_match_core_types() {
        assert_eq!(Severity::from(CliSeverity::High), Severity::High);
        assert_eq!(
            ReportFormat::from(CliReportFormat::Sarif),
            ReportFormat::Sarif
        );
    }
}
