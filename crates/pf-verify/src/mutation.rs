//! 变异测试（需求文档 FR-037）。
//!
//! # 它回答什么问题
//!
//! 「项目自带的测试套件，能不能发现行为变化？」做法是对源码做一处小的、可回退的
//! 修改（**变异体**），再跑一遍测试：
//!
//! - 测试**失败** → 这个变化被发现了，记 `Killed`；
//! - 测试**通过** → 测试套件没发现这个变化，记 `Survived`；
//! - 变异体**编译不过** → 记 `Invalid`，它不进入任何分母；
//! - 工具缺失或超时 → 记 `Inconclusive`，同样不进分母。
//!
//! # 三条不能越界的结论
//!
//! 1. **变异通过率度量的是测试套件，不是程序**。`Survived` 说的是「测试分不出原
//!    版与变异版」，不是「程序有缺陷」。因此幸存变异体在报告里是**测试覆盖缺口**
//!    这一侧的发现，严重程度为建议级，不让门禁失败。
//! 2. **编译不过的变异体不是幸存**。把它算进幸存会凭空放大覆盖缺口；把它算进已
//!    杀会伪造「测试发现了问题」。两类都不对，所以它单独成一类并明说不进分母。
//! 3. **基线失败时整个运行没有结论**。原版测试就红着的时候，任何变异体的失败都
//!    无法归因，此时返回证据不足而不是一个分数（AGENTS.md 安全红线 4）。
//!
//! # 「编译不过」是怎么判出来的
//!
//! 测试失败与编译失败在 `cargo test` 下都是非零退出码，只看来退出码会把后者记成
//! `Killed`——那是用一次没发生的实验支撑「测试发现了问题」的结论。因此测试一失败
//! 就再问一次编译器（[`pf_adapters::Tool::Build`]），由它给答案。
//!
//! 反过来，`pytest` 没有对应的构建步骤（Python 适配器的 `Tool::Build` 返回
//! `None`），**Python 项目产不出 `Invalid`**：一个改坏了语法的变异体会被记成
//! `Killed`。这是已知的偏差而不是隐含假设，报告与
//! [`docs/known-limitations.md`](../../docs/known-limitations.md) 都写明了这一点。
//!
//! # 为什么按行做词法变异
//!
//! `pf-verify` 只依赖 `pf-core` 与 `pf-adapters`（见 `docs/architecture.md`），
//! 因此拿不到语法树；变异算子按行文本匹配实现。代价是可能命中字符串字面量与泛型
//! 定界符里的记号——那种变异体要么改变了一个字符串常量的内容（记 `Survived`，
//! 由使用者判断是否值得覆盖），要么编译不过落入 `Invalid`，由计数器如实呈现，
//! 不会被说成幸存。要做语法级变异需要把生成逻辑挪到依赖解析器的 crate，届时
//! 只需替换 [`plan`]，执行与分类逻辑不变。

use pf_adapters::{Tool, ToolOptions};
use pf_core::config::Budget;
use pf_core::source::{collect_source_files, read_source_file, SourceFile};
use pf_core::status::EvidenceStatus;
use serde::Serialize;

use crate::sandbox::Sandbox;
use crate::test_runner::{TestRun, TestRunner, TestStatus};

/// 默认最多验证多少个变异体。
///
/// 每个变异体都要复制一份工作区并跑一遍完整测试套件，成本线性于变异体数量。
/// 12 是一个在小项目上约一分钟的量级；它是**抽样**而不是全量，报告里必须这么写。
pub const DEFAULT_MAX_MUTANTS: usize = 12;

/// 变异算子。
///
/// 顺序即应用顺序：同一行上有多个算子命中时，靠前的算子先产出一个变异体。
/// 固定顺序保证同一份源码永远得到同一批变异体。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationOperator {
    /// 算术运算符互换：`+` ↔ `-`、`*` ↔ `/`。
    ArithmeticSwap,
    /// 关系运算符互换：`==` ↔ `!=`、`<` ↔ `<=`、`>` ↔ `>=`。
    RelationalSwap,
    /// 逻辑运算符互换：`&&` ↔ `||`。
    LogicalSwap,
    /// 布尔字面量取反：`true` ↔ `false`。
    BooleanFlip,
    /// 整数字面量加一。
    LiteralBump,
}

impl MutationOperator {
    /// 算子的稳定标识，出现在报告与证据里。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ArithmeticSwap => "arithmetic-swap",
            Self::RelationalSwap => "relational-swap",
            Self::LogicalSwap => "logical-swap",
            Self::BooleanFlip => "boolean-flip",
            Self::LiteralBump => "literal-bump",
        }
    }

    /// 全部算子，按应用顺序。
    pub const ALL: [Self; 5] = [
        Self::ArithmeticSwap,
        Self::RelationalSwap,
        Self::LogicalSwap,
        Self::BooleanFlip,
        Self::LiteralBump,
    ];
}

/// 一个变异体：对目标源码的一处可回退替换。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mutant {
    /// 仓库相对路径（`/` 分隔）。
    pub path: String,
    /// 1 起始的行号。
    pub line: usize,
    /// 产生它的算子。
    pub operator: MutationOperator,
    /// 替换前的整行内容。
    pub original: String,
    /// 替换后的整行内容。
    pub mutated: String,
    /// 该行所属函数的名称；文件级代码（常量、use）为 `None`。
    ///
    /// 只作为给人看的标签：行号才是定位依据。有了它，「幸存的变异体在
    /// `src/lib.rs:35`」才变成「哪个行为的变化没人发现」，否则报告指了一个
    /// 位置却没说是哪段逻辑。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
}

impl Mutant {
    /// 人类可读的指认，例如 `src/lib.rs:12（arithmetic-swap）`。
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}:{}（{}）", self.path, self.line, self.operator.as_str())
    }

    /// 带上函数名的指认，例如 `unchecked_discount（src/lib.rs:35，arithmetic-swap）`。
    ///
    /// 没有函数名时退化为 [`Self::label`]：文件级代码本来就没有可归属的逻辑。
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.function {
            Some(name) => format!(
                "{name}（{}:{}，{}）",
                self.path,
                self.line,
                self.operator.as_str()
            ),
            None => self.label(),
        }
    }
}

/// 一个变异体的验证结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutantStatus {
    /// 测试失败：这个行为变化被测试套件发现了。
    Killed,
    /// 测试通过：测试套件没有发现这个变化。
    Survived,
    /// 变异体编译不过，无法判断。
    ///
    /// 单独一类而不是算进 [`Self::Survived`]：编译不过说明这次比较根本没发生，
    /// 把它算成「测试没发现问题」是在用一个没做的实验支撑结论。也不算进
    /// [`Self::Killed`]——那更是伪造「测试发现了问题」。
    ///
    /// **Python 项目产不出这一类**：没有构建步骤可问，见模块文档。
    Invalid,
    /// 工具缺失或执行超时，没有得到结论。
    Inconclusive,
}

impl MutantStatus {
    /// 是否计入了变异通过率的分母。
    ///
    /// 只有 `Killed` 与 `Survived` 代表一次真实发生的比较。
    #[must_use]
    pub const fn counts_toward_score(&self) -> bool {
        matches!(self, Self::Killed | Self::Survived)
    }

    /// 结论对应的证据状态。
    #[must_use]
    pub const fn evidence_status(self) -> EvidenceStatus {
        match self {
            Self::Killed => EvidenceStatus::NotReproduced,
            Self::Survived => EvidenceStatus::Suspected,
            Self::Invalid | Self::Inconclusive => EvidenceStatus::Inconclusive,
        }
    }
}

/// 单个变异体的验证记录。
#[derive(Debug, Clone, Serialize)]
pub struct MutantResult {
    /// 被验证的变异体。
    pub mutant: Mutant,
    /// 结论。
    pub status: MutantStatus,
    /// 失败测试的名字（`Killed` 时）。
    pub failed_tests: Vec<String>,
    /// 没有得到结论时的原因。
    pub reason: Option<String>,
}

/// 一次变异测试的汇总。
#[derive(Debug, Clone)]
pub struct MutationOutcome {
    /// 基线（未变异）测试结论。
    pub baseline: TestStatus,
    /// 逐变异体的记录。
    pub results: Vec<MutantResult>,
    /// 生成的变异体总数中被验证的个数。
    pub attempted: usize,
}

impl MutationOutcome {
    /// 已杀死的变异体数。
    #[must_use]
    pub fn killed(&self) -> usize {
        self.count(MutantStatus::Killed)
    }

    /// 幸存的变异体数。
    #[must_use]
    pub fn survived(&self) -> usize {
        self.count(MutantStatus::Survived)
    }

    /// 编译不过的变异体数。
    #[must_use]
    pub fn invalid(&self) -> usize {
        self.count(MutantStatus::Invalid)
    }

    /// 没有得到结论的变异体数。
    #[must_use]
    pub fn inconclusive(&self) -> usize {
        self.count(MutantStatus::Inconclusive)
    }

    fn count(&self, status: MutantStatus) -> usize {
        self.results
            .iter()
            .filter(|result| result.status == status)
            .count()
    }

    /// 变异通过率：已杀死 /（已杀死 + 幸存）。
    ///
    /// `None` 表示分母为 0——一次真实比较都没发生。此时返回 0% 会让一个空跑看起来
    /// 像「测试完全无效」，返回 100% 更糟；没有分数就是没有分数。
    #[must_use]
    pub fn score(&self) -> Option<f64> {
        let killed = self.killed();
        let survived = self.survived();
        let total = killed + survived;
        if total == 0 {
            return None;
        }
        Some(
            f64::from(u32::try_from(killed).unwrap_or(u32::MAX))
                / f64::from(u32::try_from(total).unwrap_or(u32::MAX)),
        )
    }

    /// 幸存的变异体：测试套件没能发现的变化。
    pub fn survivors(&self) -> impl Iterator<Item = &MutantResult> {
        self.results
            .iter()
            .filter(|result| result.status == MutantStatus::Survived)
    }

    /// 基线是否可用。
    ///
    /// 基线失败时任何变异体的失败都无法归因于变异本身，因此整个运行没有结论。
    #[must_use]
    pub const fn baseline_usable(&self) -> bool {
        matches!(self.baseline, TestStatus::Passed)
    }

    /// 导出成机器可读的 JSON。
    ///
    /// 与 [`Self::summary`] 必须给出同一套数字。JSON 里多出 `baseline_usable`：
    /// 它是派生量，但消费方不该自己重一遍「基线失败时整个运行没有结论」的判定。
    ///
    /// # Errors
    /// 序列化失败时返回 [`pf_core::Error`]。报告里只有自有字段与字符串，
    /// 正常情况下不会失败。
    pub fn to_json(&self) -> Result<String, pf_core::Error> {
        #[derive(serde::Serialize)]
        struct Snapshot<'a> {
            baseline: &'static str,
            baseline_usable: bool,
            attempted: usize,
            killed: usize,
            survived: usize,
            invalid: usize,
            inconclusive: usize,
            score: Option<f64>,
            results: &'a [MutantResult],
        }
        let snapshot = Snapshot {
            baseline: match self.baseline {
                TestStatus::Passed => "passed",
                TestStatus::Failed => "failed",
                TestStatus::Inconclusive => "inconclusive",
            },
            baseline_usable: self.baseline_usable(),
            attempted: self.attempted,
            killed: self.killed(),
            survived: self.survived(),
            invalid: self.invalid(),
            inconclusive: self.inconclusive(),
            score: self.score(),
            results: &self.results,
        };
        serde_json::to_string_pretty(&snapshot).map_err(|reason| pf_core::Error::Serialization {
            reason: reason.to_string(),
        })
    }

    /// 人类可读的一行汇总。
    #[must_use]
    pub fn summary(&self) -> String {
        let score = self.score().map_or_else(
            || "无分数（没有可比较的变异体）".to_owned(),
            |s| format!("{:.0}%", s * 100.0),
        );
        format!(
            "变异体 {} 个：已杀死 {}、幸存 {}、无效 {}、无结论 {}；通过率 {}",
            self.attempted,
            self.killed(),
            self.survived(),
            self.invalid(),
            self.inconclusive(),
            score
        )
    }
}

/// 变异测试执行器。
#[derive(Debug, Default)]
pub struct MutationRunner;

impl MutationRunner {
    /// 在目标项目上做一次变异测试。
    ///
    /// # 步骤
    ///
    /// 1. 跑一遍未变异的测试套件作为基线。基线不通过就直接返回——此时任何结论
    ///    都无法归因。
    /// 2. 生成变异体（受 `config.max_mutants` 限制）。
    /// 3. 每个变异体复制一份独立工作区、应用修改、跑测试、按结论分类。
    ///
    /// # Errors
    /// 目标不可读、或沙箱创建失败时返回 [`pf_core::Error`]。单个变异体「没跑出
    /// 结论」不是错误，它进入 [`MutantStatus::Inconclusive`]。
    pub fn run(
        target: &std::path::Path,
        config: &MutationConfig,
        budget: &Budget,
        options: &ToolOptions,
    ) -> pf_core::Result<MutationOutcome> {
        let baseline = TestRunner::run(target, budget)?;
        if !matches!(baseline.status, TestStatus::Passed) {
            return Ok(MutationOutcome {
                baseline: baseline.status,
                results: Vec::new(),
                attempted: 0,
            });
        }

        let files = read_analyzable_sources(target, budget)?;
        let mut mutants = plan(&files);
        mutants.truncate(config.max_mutants.max(1));

        let mut results = Vec::with_capacity(mutants.len());
        for mutant in &mutants {
            results.push(run_one(target, mutant, budget, options));
        }
        Ok(MutationOutcome {
            baseline: baseline.status,
            results,
            attempted: mutants.len(),
        })
    }
}

/// 变异测试配置。
#[derive(Debug, Clone)]
pub struct MutationConfig {
    /// 最多验证多少个变异体。
    pub max_mutants: usize,
}

impl Default for MutationConfig {
    fn default() -> Self {
        Self {
            max_mutants: DEFAULT_MAX_MUTANTS,
        }
    }
}

/// 读取目标里可做变异分析的源文件。
fn read_analyzable_sources(
    target: &std::path::Path,
    budget: &Budget,
) -> pf_core::Result<Vec<SourceFile>> {
    let root = target
        .canonicalize()
        .map_err(|_| pf_core::Error::TargetNotFound {
            path: target.to_path_buf(),
        })?;
    let mut files = Vec::new();
    for path in collect_source_files(&root, budget.max_file_bytes)? {
        let Some(file) = read_source_file(&path, &root)? else {
            continue;
        };
        // 只变异有结构化分析能力的语言：规则文件、配置文件里的记号不是程序逻辑，
        // 变异它们只会得到一批必然编译失败的变异体，把通过率稀释掉。
        if file.language.is_analyzable() {
            files.push(file);
        }
    }
    Ok(files)
}

/// 为一组源文件生成变异体。
///
/// 顺序确定：按路径排序，再按行号，再按 [`MutationOperator::ALL`] 的顺序。同一份
/// 输入因此永远得到同一批变异体，报告可复现（FR-061）。
#[must_use]
pub fn plan(files: &[SourceFile]) -> Vec<Mutant> {
    let mut sorted: Vec<&SourceFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut mutants = Vec::new();
    for file in sorted {
        let mut enclosing: Option<String> = None;
        for (index, line) in file.content.lines().enumerate() {
            if let Some(name) = declared_function(line) {
                enclosing = Some(name.to_owned());
            }
            for operator in MutationOperator::ALL {
                if let Some(mutated) = apply_operator(operator, line) {
                    mutants.push(Mutant {
                        path: file.path.clone(),
                        line: index + 1,
                        operator,
                        original: line.to_owned(),
                        mutated,
                        function: enclosing.clone(),
                    });
                }
            }
        }
    }
    mutants
}

/// 取一行里声明的函数名（若有）。
///
/// 词法判据：跳过注释行，找第一个作为独立词出现的 `fn`，取其后的标识符。这不是
/// 作用域分析——宏定义体、字符串字面量里的 `fn` 都会让它认错——但它只作为给人看
/// 的标签，定位靠行号；认错的代价是把幸存条目归到隔壁函数，不会改变任何结论。
fn declared_function(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('#') {
        return None;
    }
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(offset) = line[from..].find("fn") {
        let start = from + offset;
        let before = start.checked_sub(1).map(|index| bytes[index]);
        // 前一个字符必须是标识符边界：`ifn`、`myfn` 里的 `fn` 不是函数声明。
        if before.is_none_or(|byte| !byte.is_ascii_alphanumeric() && byte != b'_') {
            let rest = line[start + 2..].trim_start();
            let name: &str = rest
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default();
            if !name.is_empty() {
                return Some(name);
            }
        }
        from = start + 2;
    }
    None
}

/// 在一行上应用一个算子，返回替换后的整行。
///
/// 每个算子在**每行**只取第一处命中：一个变异体只改一处，否则两个同时生效的修改
/// 互相掩盖，杀死了也说不清是哪一个变化被发现的。
fn apply_operator(operator: MutationOperator, line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    // 注释行与文档行不参与变异：改注释不会改变程序行为，得到的变异体必然「幸存」，
    // 而那说明的是注释无害，不是测试有缺口。
    if trimmed.starts_with("//") || trimmed.starts_with('#') {
        return None;
    }
    match operator {
        MutationOperator::ArithmeticSwap => swap_first(
            line,
            &[
                // 复合赋值必须排在单字符之前：`x += 1` 是一条真实的行为变化，
                // 而 `is_standalone_operator` 会替单字符那条路挡掉它。
                ("+=", "-="),
                ("-=", "+="),
                ("*=", "/="),
                ("/=", "*="),
                ("+", "-"),
                ("*", "/"),
                ("-", "+"),
                ("/", "*"),
            ],
        ),
        MutationOperator::RelationalSwap => swap_first(
            line,
            &[
                ("==", "!="),
                ("!=", "=="),
                ("<=", ">="),
                (">=", "<="),
                ("<", ">"),
                (">", "<"),
            ],
        ),
        MutationOperator::LogicalSwap => swap_first(line, &[("&&", "||"), ("||", "&&")]),
        MutationOperator::BooleanFlip => {
            swap_word(line, "true", "false").or_else(|| swap_word(line, "false", "true"))
        }
        MutationOperator::LiteralBump => bump_first_integer(line),
    }
}

/// 按给定顺序替换第一处命中的子串。
///
/// 顺序重要的场合：`<=` 必须在 `<` 之前尝试，否则 `a <= b` 会先被 `<` 匹配，
/// 产出 `a => b` 这种语法错误而不是一个有效的变异体。
///
/// 命中的位置还必须**独立成符**：紧邻的前后字符不能再参与构成更长的运算符。
/// 少了这条判据，`-> i64` 里的 `-` 会被换成 `+` 得到 `+> i64`，`x += 1` 里的 `+`
/// 会被换成 `-` 得到 `-= 1` 之外的第三种形状——这些变异体编译不过，只会把
/// 「无效」计数撑高，让通过率的分母变小、结论变噪声。
fn swap_first(line: &str, pairs: &[(&str, &str)]) -> Option<String> {
    for (from, to) in pairs {
        let mut search_from = 0;
        while let Some(offset) = line[search_from..].find(from) {
            let start = search_from + offset;
            let end = start + from.len();
            if is_standalone_operator(line, start, end) {
                let mut out = String::with_capacity(line.len());
                out.push_str(&line[..start]);
                out.push_str(to);
                out.push_str(&line[end..]);
                return Some(out);
            }
            search_from = end;
        }
    }
    None
}

/// `line[start..end]` 是否是一个独立的运算符。
///
/// 判据是两侧字符都不在 Rust 运算符字符集里。因此 `a + b` 命中，`a -> b` 与
/// `a => b` 不命中，`x += 1` 也不命中——复合赋值由 [`MutationOperator`] 的成对
/// 条目负责，不走单字符那条路。
fn is_standalone_operator(line: &str, start: usize, end: usize) -> bool {
    const OPERATOR_CHARS: &str = "+-*/%<>=!&|^~:.?";
    let before_ok = line[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !OPERATOR_CHARS.contains(c));
    let after_ok = line[end..]
        .chars()
        .next()
        .is_none_or(|c| !OPERATOR_CHARS.contains(c));
    before_ok && after_ok
}

/// 按词边界替换第一个命中的标识符。
///
/// 用词边界而不是裸 `replace`：`true` 会出现在 `truest`、`state`、`untrue` 里，
/// 裸替换会把它们一并改掉，产出与算子语义无关的变异体。
fn swap_word(line: &str, from: &str, to: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(offset) = line[search_from..].find(from) {
        let start = search_from + offset;
        let end = start + from.len();
        let before_ok = line[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let after_ok = line[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if before_ok && after_ok {
            let mut out = String::with_capacity(line.len());
            out.push_str(&line[..start]);
            out.push_str(to);
            out.push_str(&line[end..]);
            return Some(out);
        }
        search_from = end;
    }
    None
}

/// 把行内第一个整数字面量加一。
///
/// 只认独立的十进制整数，且整个数字 token 里不能有小数点：`1.5` 里的 `1` 加一会
/// 变成 `2.5`，那是另一个值而不是「边界挪动一档」，与算子语义不符。跳过时连小数
/// 部分一起跳过——否则扫描会落到 `.5` 的 `5` 上，把它当成一个独立整数改掉。
fn bump_first_integer(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let prev_ok = line[..index]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if !prev_ok {
            index += 1;
            continue;
        }
        // 吃掉整个数字 token：`1.5`、`1_000` 都要整块跳过。
        let start = index;
        let mut has_fraction = false;
        while index < bytes.len()
            && (bytes[index].is_ascii_digit() || bytes[index] == b'.' || bytes[index] == b'_')
        {
            if bytes[index] == b'.' {
                has_fraction = true;
            }
            index += 1;
        }
        let next_ok = line[index..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if has_fraction || !next_ok {
            continue;
        }
        let digits = &line[start..index];
        let value: i64 = digits.parse().ok()?;
        let mut out = String::with_capacity(line.len() + 2);
        out.push_str(&line[..start]);
        out.push_str(&(value + 1).to_string());
        out.push_str(&line[index..]);
        return Some(out);
    }
    None
}

/// 在独立沙箱里验证一个变异体。
fn run_one(
    target: &std::path::Path,
    mutant: &Mutant,
    budget: &Budget,
    options: &ToolOptions,
) -> MutantResult {
    // 每个变异体一份新沙箱：复用同一份工作区会让上一个变异体的产物（测试写进的
    // 文件、 cargo 的增量缓存）影响下一个的结论，而那种影响是静默的。
    let Ok(sandbox) = Sandbox::isolate(target, budget) else {
        return inconclusive(mutant, "无法创建隔离工作区");
    };
    let Ok(original) = std::fs::read_to_string(sandbox.root().join(&mutant.path)) else {
        return inconclusive(mutant, format!("沙箱里读不到 {}", mutant.path));
    };
    let patched = patch_line(&original, mutant.line, &mutant.mutated);
    let Some(patched) = patched else {
        // 行号越界只可能是生成逻辑与文件内容不一致，属于 ProofForge 的缺陷，
        // 不是目标项目的问题，因此按「无结论」而不是「幸存」处理。
        return inconclusive(mutant, "变异行号越界");
    };
    if sandbox.write_file(&mutant.path, &patched).is_err() {
        return inconclusive(mutant, "无法把变异体写进沙箱");
    }

    let run: TestRun = TestRunner::in_sandbox(&sandbox, options);
    match run.status {
        TestStatus::Failed => {
            // 测试失败不等于「测试发现了变异」：变异体改坏了语法时 cargo test 同样
            // 非零退出。此时再问一次编译器，由它区分这两种情况——只读退出码会把
            // 「根本没编译出来」记成 `Killed`，凭空抬高通过率（红线 4）。
            match compile_failure(&sandbox, options) {
                Some(reason) => MutantResult {
                    mutant: mutant.clone(),
                    status: MutantStatus::Invalid,
                    failed_tests: Vec::new(),
                    reason: Some(reason),
                },
                None => MutantResult {
                    mutant: mutant.clone(),
                    status: MutantStatus::Killed,
                    failed_tests: run.failed_tests,
                    reason: None,
                },
            }
        }
        TestStatus::Passed => MutantResult {
            mutant: mutant.clone(),
            status: MutantStatus::Survived,
            failed_tests: Vec::new(),
            reason: None,
        },
        TestStatus::Inconclusive => inconclusive(mutant, "测试套件没有得到结论"),
    }
}

/// 变异体是否编译不过；是则给出编译器的第一行错误。
///
/// 只在测试已经失败时调用，因此多出来的这一次执行只花在「需要分类的那批」上。
///
/// `None` 有两种含义完全不同的来路，调用方必须分开理解：
///
/// - **编译通过了**——这个变异体是有效的，测试失败说明它被发现了；
/// - **这个语言没有构建步骤**（Python 适配器的 [`pf_adapters::Tool::Build`] 返回
///   `None`）——此时无从判断，按「编译通过」处理。Python 项目因此产不出
///   [`MutantStatus::Invalid`]，这是已知偏差，已在模块文档里写明。
fn compile_failure(sandbox: &Sandbox, options: &ToolOptions) -> Option<String> {
    let execution = sandbox.run(Tool::Build, options);
    if execution.succeeded() {
        return None;
    }
    // 构建工具本身缺失时不能推断「编译不过」：那只是环境里没有 cargo。
    if execution.failed_to_start() {
        return None;
    }
    Some(format!(
        "变异体编译不过：{}",
        crate::driver::first_error_line(&execution.stderr)
    ))
}

/// 用变异行的内容替换原文件里的指定行。
fn patch_line(original: &str, line: usize, replacement: &str) -> Option<String> {
    let mut lines: Vec<&str> = original.lines().collect();
    let slot = lines.get_mut(line.checked_sub(1)?)?;
    *slot = replacement;
    let mut out = lines.join("\n");
    if original.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

/// 构造一条「无结论」记录。
fn inconclusive(mutant: &Mutant, reason: impl Into<String>) -> MutantResult {
    MutantResult {
        mutant: mutant.clone(),
        status: MutantStatus::Inconclusive,
        failed_tests: Vec::new(),
        reason: Some(reason.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &str) -> SourceFile {
        SourceFile::new(path, content)
    }

    #[test]
    fn arithmetic_operator_is_swapped_in_both_directions() {
        assert_eq!(
            apply_operator(MutationOperator::ArithmeticSwap, "let x = a + b;"),
            Some("let x = a - b;".to_owned())
        );
        assert_eq!(
            apply_operator(MutationOperator::ArithmeticSwap, "let x = a - b;"),
            Some("let x = a + b;".to_owned())
        );
    }

    #[test]
    fn relational_operators_do_not_match_their_own_prefix() {
        // `<=` 必须整体换成 `>=`。先试 `<` 会得到 `a => b`，那是语法错误而不是
        // 一个有效的变异体。
        assert_eq!(
            apply_operator(MutationOperator::RelationalSwap, "if a <= b {"),
            Some("if a >= b {".to_owned())
        );
        assert_eq!(
            apply_operator(MutationOperator::RelationalSwap, "if a == b {"),
            Some("if a != b {".to_owned())
        );
    }

    #[test]
    fn comment_lines_are_never_mutated() {
        // 改注释不改变行为，得到的变异体必然「幸存」，而那说明的是注释无害。
        assert_eq!(
            apply_operator(MutationOperator::ArithmeticSwap, "// a + b"),
            None
        );
        assert_eq!(
            apply_operator(MutationOperator::LiteralBump, "/// 42"),
            None
        );
    }

    #[test]
    fn boolean_flip_respects_word_boundaries() {
        assert_eq!(
            apply_operator(MutationOperator::BooleanFlip, "let f = true;"),
            Some("let f = false;".to_owned())
        );
        assert_eq!(
            apply_operator(MutationOperator::BooleanFlip, "let state = truest;"),
            None,
            "`truest` 里的 `true` 不是布尔字面量"
        );
    }

    #[test]
    fn integer_bump_skips_float_fractions_and_identifiers() {
        assert_eq!(
            apply_operator(MutationOperator::LiteralBump, "let n = 41;"),
            Some("let n = 42;".to_owned())
        );
        assert_eq!(
            apply_operator(MutationOperator::LiteralBump, "let x = 1.5;"),
            None,
            "`1.5` 里的 `1` 加一会得到另一个值，不是边界挪动一档"
        );
        assert_eq!(
            apply_operator(MutationOperator::LiteralBump, "let v2 = 1;"),
            Some("let v2 = 2;".to_owned()),
            "前缀是标识符不影响后面的独立字面量"
        );
    }

    #[test]
    fn one_mutant_per_operator_per_line() {
        // 一个变异体只改一处：两处同时改会互相掩盖，杀死了也说不清是哪一处。
        let line = "let x = a + b - c;";
        let mutant = apply_operator(MutationOperator::ArithmeticSwap, line).expect("mutant");
        assert_eq!(mutant, "let x = a - b - c;");
    }

    #[test]
    fn plan_is_ordered_by_path_then_line_then_operator() {
        let files = vec![
            file("src/b.rs", "let x = 1;\n"),
            file("src/a.rs", "let y = 2;\nlet z = 3;\n"),
        ];
        let mutants = plan(&files);
        assert_eq!(mutants[0].path, "src/a.rs");
        assert_eq!(mutants[0].line, 1);
        assert_eq!(mutants[1].path, "src/a.rs");
        assert_eq!(mutants[1].line, 2);
        assert_eq!(mutants[2].path, "src/b.rs");
    }

    #[test]
    fn plan_is_deterministic() {
        let files = vec![file(
            "src/a.rs",
            "let x = a + b;\nif c == d { let t = true; }\n",
        )];
        assert_eq!(plan(&files), plan(&files));
    }

    #[test]
    fn a_line_without_any_operator_yields_nothing() {
        let files = vec![file("src/a.rs", "pub struct Foo;\n")];
        assert_eq!(plan(&files), Vec::new());
    }

    #[test]
    fn invalid_and_inconclusive_do_not_count_toward_the_score() {
        assert!(!MutantStatus::Invalid.counts_toward_score());
        assert!(!MutantStatus::Inconclusive.counts_toward_score());
        assert!(MutantStatus::Killed.counts_toward_score());
        assert!(MutantStatus::Survived.counts_toward_score());
    }

    #[test]
    fn score_is_none_when_nothing_was_comparable() {
        let outcome = MutationOutcome {
            baseline: TestStatus::Passed,
            results: vec![MutantResult {
                mutant: Mutant {
                    path: "src/a.rs".to_owned(),
                    line: 1,
                    operator: MutationOperator::ArithmeticSwap,
                    original: "let x = a + b;".to_owned(),
                    mutated: "let x = a - b;".to_owned(),
                    function: None,
                },
                status: MutantStatus::Invalid,
                failed_tests: Vec::new(),
                reason: Some("does not compile".to_owned()),
            }],
            attempted: 1,
        };
        assert_eq!(outcome.score(), None, "一次真实比较都没发生，就没有分数");
        assert!(outcome.baseline_usable());
        assert!(outcome.summary().contains("无分数"));
    }

    #[test]
    fn score_counts_only_killed_and_survived() {
        let mut outcome = MutationOutcome {
            baseline: TestStatus::Passed,
            results: Vec::new(),
            attempted: 4,
        };
        for status in [
            MutantStatus::Killed,
            MutantStatus::Killed,
            MutantStatus::Survived,
            MutantStatus::Invalid,
        ] {
            outcome.results.push(MutantResult {
                mutant: Mutant {
                    path: "src/a.rs".to_owned(),
                    line: 1,
                    operator: MutationOperator::ArithmeticSwap,
                    original: "a + b".to_owned(),
                    mutated: "a - b".to_owned(),
                    function: None,
                },
                status,
                failed_tests: Vec::new(),
                reason: None,
            });
        }
        assert_eq!(outcome.score(), Some(2.0 / 3.0));
        assert_eq!(outcome.survivors().count(), 1);
        assert_eq!(outcome.invalid(), 1);
        assert!(outcome.summary().contains("67%"));
    }

    #[test]
    fn a_failing_baseline_makes_the_whole_run_unusable() {
        // 基线就红着时，任何变异体的失败都无法归因于变异本身。
        let outcome = MutationOutcome {
            baseline: TestStatus::Failed,
            results: Vec::new(),
            attempted: 0,
        };
        assert!(!outcome.baseline_usable());
    }

    #[test]
    fn patch_line_replaces_only_the_target_line() {
        let original = "one\ntwo\nthree\n";
        assert_eq!(
            patch_line(original, 2, "TWO"),
            Some("one\nTWO\nthree\n".to_owned())
        );
    }

    #[test]
    fn patch_line_rejects_an_out_of_range_line() {
        assert_eq!(patch_line("one\n", 9, "x"), None);
    }

    #[test]
    fn survivor_status_is_suspected_not_a_defect_claim() {
        // 幸存说明的是「测试分不出两个版本」，不是「程序有缺陷」。
        assert_eq!(
            MutantStatus::Survived.evidence_status(),
            EvidenceStatus::Suspected
        );
        assert_eq!(
            MutantStatus::Invalid.evidence_status(),
            EvidenceStatus::Inconclusive
        );
    }

    #[test]
    fn a_mutant_carries_the_name_of_the_function_it_lives_in() {
        // 幸存的变异体要能指认「哪个行为没人守」，只给行号的话读者还得自己去数。
        let mutants = plan(&[file(
            "src/lib.rs",
            "const TAX: i64 = 7;\n\npub fn total(price: i64) -> i64 {\n    price + TAX\n}\n",
        )]);
        assert_eq!(mutants.len(), 2, "{mutants:?}");
        assert_eq!(mutants[0].function, None, "常量行不属于任何函数");
        assert_eq!(mutants[1].function.as_deref(), Some("total"));
        assert_eq!(
            mutants[1].describe(),
            "total（src/lib.rs:4，arithmetic-swap）"
        );
    }

    #[test]
    fn declared_function_reads_a_declaration_and_skips_what_is_not_one() {
        assert_eq!(
            declared_function("pub fn add(a: i64) -> i64 {"),
            Some("add")
        );
        assert_eq!(declared_function("    fn helper() {}"), Some("helper"));
        assert_eq!(
            declared_function("let infix = 1;"),
            None,
            "`infix` 里含 fn 但不是声明"
        );
        assert_eq!(
            declared_function("// fn commented_out() {}"),
            None,
            "注释行不算"
        );
    }

    #[test]
    fn a_fn_inside_a_string_literal_is_a_known_mislabelling() {
        // 已知的误判方向：词法判据看不出字符串边界，于是 `let t = "fn f()";` 会让
        // 之后的幸存条目被归到 `f` 名下。这条断言把代价固定下来——标签会错，行号
        // 不会，因此结论不受影响；真的需要精确归属时得先有语法树。
        assert_eq!(declared_function("let t = \"fn f()\";"), Some("f"));
    }

    #[test]
    fn an_arrow_is_not_mutated_into_a_broken_operator() {
        // 回归：`-> i64` 里的 `-` 曾被 ArithmeticSwap 换成 `+`，`>=` 之前的 `<`
        // 也曾被 RelationalSwap 单独命中。两者都产出编译不过的变异体，把「无效」
        // 计数撑高、让通过率的分母变小——噪声而不是信号。
        assert_eq!(
            apply_operator(
                MutationOperator::ArithmeticSwap,
                "pub fn f(x: i64) -> i64 {"
            ),
            None
        );
        assert_eq!(
            apply_operator(
                MutationOperator::RelationalSwap,
                "pub fn f(x: i64) -> i64 {"
            ),
            None
        );
        assert_eq!(
            apply_operator(MutationOperator::ArithmeticSwap, "let x = a + b;"),
            Some("let x = a - b;".to_owned()),
            "独立的 `+` 仍然要能被换掉"
        );
    }

    #[test]
    fn compound_assignment_is_mutated_as_one_operator() {
        // `x += 1` → `x -= 1` 是一条真实的行为变化，值得被测到；而 `x = 1` 不能被
        // 拆成 `x = 1` 之外的形状。
        assert_eq!(
            apply_operator(MutationOperator::ArithmeticSwap, "        total += tax;"),
            Some("        total -= tax;".to_owned())
        );
    }

    /// 造一个带测试的 Rust 项目，返回其根目录（`TempDir` 需保持存活）。
    fn rust_project(body: &str, test: &str) -> tempfile::TempDir {
        let project = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(project.path().join("src")).expect("mkdir");
        std::fs::create_dir_all(project.path().join("tests")).expect("mkdir");
        std::fs::write(
            project.path().join("Cargo.toml"),
            "[package]\nname = \"mutant_target\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .expect("manifest");
        std::fs::write(project.path().join("src/lib.rs"), body).expect("lib.rs");
        std::fs::write(project.path().join("tests/behavior.rs"), test).expect("test");
        project
    }

    #[test]
    fn a_mutant_that_does_not_compile_is_invalid_not_killed() {
        // 回归：`Vec<i64>` 里的 `<` 被 RelationalSwap 换成 `>` 后编译不过，而
        // `cargo test` 同样非零退出。只读退出码会把它记成 `Killed`——那是用一次
        // 没发生的实验支撑「测试发现了问题」，凭空抬高通过率（红线 4）。
        let project = rust_project(
            "/// 求和。\npub fn total(values: &[i64]) -> i64 {\n                 let collected: Vec<i64> = values.to_vec();\n    collected.iter().sum()\n}\n",
            "use mutant_target::total;\n\n#[test]\nfn sums() {\n    assert_eq!(total(&[1, 2]), 3);\n}\n",
        );
        let outcome = MutationRunner::run(
            project.path(),
            &MutationConfig { max_mutants: 40 },
            &Budget::default(),
            &ToolOptions::default(),
        )
        .expect("run");

        assert!(outcome.baseline_usable(), "{outcome:?}");
        let broken = outcome
            .results
            .iter()
            .find(|result| result.mutant.operator == MutationOperator::RelationalSwap)
            .expect("泛型定界符上必然有一个关系变异体");
        assert_eq!(
            broken.status,
            MutantStatus::Invalid,
            "编译不过的变异体不能算已杀死：{:?}",
            broken.reason
        );
        assert!(
            broken
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("编译不过")),
            "无效条目必须带上编译器的原因：{:?}",
            broken.reason
        );
    }

    #[test]
    fn a_mutant_the_tests_catch_is_killed() {
        // 与上一条配对：有效的变异体被测试发现时仍然要记 `Killed`。两条一起守，
        // 才能证明「再问一次编译器」没有把本该杀掉的也推成无效。
        let project = rust_project(
            "/// 翻倍。\npub fn twice(value: i64) -> i64 {\n    value * 2\n}\n",
            "use mutant_target::twice;\n\n#[test]\nfn doubles() {\n    assert_eq!(twice(3), 6);\n}\n",
        );
        let outcome = MutationRunner::run(
            project.path(),
            &MutationConfig { max_mutants: 40 },
            &Budget::default(),
            &ToolOptions::default(),
        )
        .expect("run");

        assert!(outcome.killed() > 0, "{outcome:?}");
        assert_eq!(outcome.invalid(), 0, "{outcome:?}");
        assert_eq!(outcome.survived(), 0, "两个变异体都有测试守着：{outcome:?}");
    }
}
