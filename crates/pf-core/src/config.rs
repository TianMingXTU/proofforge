//! 运行配置。
//!
//! 配置由 CLI 参数与 `proofforge.toml` 共同决定，优先级为
//! CLI > 配置文件 > 默认值。没有环境变量这一层：多一个来源就多一处
//! 「我以为设了其实没设」的地方，而这类错误在门禁结论上是隐形的。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 报告输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportFormat {
    /// 人类可读的终端摘要。
    #[default]
    Terminal,
    /// 机器可读 JSON。
    Json,
    /// GitHub Actions 注解格式。
    Sarif,
    /// 单文件 HTML（FR-068）：可离线打开、可随工件归档。
    Html,
}

/// 资源与成本预算（FR-046、FR-032）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// 单个外部命令的最长执行时间（秒）。
    #[serde(default = "default_command_timeout")]
    pub command_timeout_secs: u64,
    /// 整个任务的最长执行时间（秒）。
    #[serde(default = "default_task_timeout")]
    pub task_timeout_secs: u64,
    /// 最多索引的文件数，防止超大仓库拖垮分析。
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    /// 单文件最大字节数。
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: usize,
    /// 允许的并发外部进程数。
    #[serde(default = "default_parallelism")]
    pub parallelism: usize,
    /// LLM Token 上限（`None` 表示不调用模型）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

fn default_command_timeout() -> u64 {
    300
}

fn default_task_timeout() -> u64 {
    3600
}

fn default_max_files() -> usize {
    20_000
}

fn default_max_file_bytes() -> usize {
    2 * 1024 * 1024
}

fn default_parallelism() -> usize {
    4
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            command_timeout_secs: default_command_timeout(),
            task_timeout_secs: default_task_timeout(),
            max_files: default_max_files(),
            max_file_bytes: default_max_file_bytes(),
            parallelism: default_parallelism(),
            max_tokens: None,
        }
    }
}

/// 一次审查运行的完整配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// 被分析的项目根目录。
    #[serde(default = "default_target")]
    pub target: PathBuf,
    /// Git 引用或工作区标识；`None` 表示当前工作区。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// 基线引用，用于增量分析；`None` 表示全量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    /// 启用的语言（空表示全部识别到的语言）。
    #[serde(default)]
    pub languages: Vec<String>,
    /// 阻断合并的最低严重程度。
    #[serde(default = "default_block_on")]
    pub block_on: crate::status::Severity,
    /// 报告格式。
    #[serde(default)]
    pub report_format: ReportFormat,
    /// 报告输出路径；`None` 表示写入标准输出。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_path: Option<PathBuf>,
    /// 预算。
    #[serde(default)]
    pub budget: Budget,
    /// 额外规则集文件（AGENTS.md / proofforge.toml）。
    #[serde(default)]
    pub rule_files: Vec<PathBuf>,
    /// 豁免文件（TOML 或 YAML，FR-062）。
    ///
    /// 相对路径按 [`Self::target`] 解析：豁免是项目级决策，跟着项目走而不是
    /// 跟着当前工作目录走。
    #[serde(default)]
    pub exemption_files: Vec<PathBuf>,
    /// 架构依赖规则（FR-024）。
    ///
    /// 空表示不检查依赖方向——内置的循环依赖检测（FR-023）始终生效，但「谁允许
    /// 依赖谁」是项目自己的决定，工具不预设。
    #[serde(default)]
    pub architecture_rules: Vec<ArchitectureRule>,
    /// 详细输出。
    #[serde(default)]
    pub verbose: bool,
}

fn default_target() -> PathBuf {
    PathBuf::from(".")
}

pub(crate) fn default_block_on() -> crate::status::Severity {
    crate::status::Severity::High
}

impl Default for Config {
    fn default() -> Self {
        Self {
            target: PathBuf::from("."),
            revision: None,
            baseline: None,
            languages: Vec::new(),
            block_on: default_block_on(),
            report_format: ReportFormat::Terminal,
            report_path: None,
            budget: Budget::default(),
            rule_files: Vec::new(),
            exemption_files: Vec::new(),
            architecture_rules: Vec::new(),
            verbose: false,
        }
    }
}

impl Config {
    /// 使用默认配置并指定目标路径。
    #[must_use]
    pub fn for_target(target: impl Into<PathBuf>) -> Self {
        Self {
            target: target.into(),
            ..Self::default()
        }
    }

    /// 是否分析给定语言。
    #[must_use]
    pub fn wants_language(&self, language: &str) -> bool {
        self.languages.is_empty() || self.languages.iter().any(|lang| lang == language)
    }

    /// 从 TOML 文本解析配置。
    ///
    /// # Errors
    /// 文本不是合法 TOML 或字段类型不匹配时返回 [`crate::Error::ConfigParse`]。
    pub fn from_toml(text: &str, source: &str) -> crate::Result<Self> {
        toml::from_str(text).map_err(|reason| crate::Error::ConfigParse {
            origin: source.to_owned(),
            reason: reason.to_string(),
        })
    }

    /// 序列化为 TOML 文本（用于 `proofforge init` 生成模板）。
    ///
    /// # Errors
    /// 序列化失败时返回 [`crate::Error::Serialization`]。
    pub fn to_toml(&self) -> crate::Result<String> {
        toml::to_string_pretty(self).map_err(|reason| crate::Error::Serialization {
            reason: reason.to_string(),
        })
    }
}

/// 架构依赖规则的方向约束（FR-024）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchitectureMode {
    /// 禁止 `from` 指向 `to`。
    #[default]
    Forbidden,
    /// 只允许 `from` 指向 `to`，指向其他位置一律禁止。
    Allowed,
}

/// 一条架构依赖规则（FR-024）。
///
/// 模式是路径通配符而不是 crate 名：模块依赖图按文件建边（FR-022），规则要能落到
/// 文件上才可定位。`**` 匹配任意层级的目录，`*` 匹配单个路径段内的任意字符。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchitectureRule {
    /// 规则名称，出现在发现里。
    pub name: String,
    /// 源路径模式。
    pub from: String,
    /// 目标路径模式。
    pub to: String,
    /// 方向约束。
    #[serde(default)]
    pub mode: ArchitectureMode,
    /// 这条规则存在的理由，出现在发现里。
    ///
    /// 选填而非必填：一条写错方向的架构规则需要能说清「为什么这么定」，否则它只是
    /// 把一次错误决定固化下来。理由缺失时报告会自己标注这一点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ArchitectureRule {
    /// 这条边是否落在规则的**模式范围**内。
    ///
    /// 只判断模式，不判断方向约束：方向是 [`Self::permits`] 的事。把两件事合成
    /// 一个函数会让 `forbidden` 与 `allowed` 两种模式共用一套含糊的语义，而它们
    /// 要报的恰好是互补的边集。
    ///
    /// 自环（源与目标是同一个文件）永远不适用：文件依赖自己不是架构问题。
    #[must_use]
    pub fn applies_to(&self, from: &str, to: &str) -> bool {
        from != to && matches_pattern(&self.from, from) && matches_pattern(&self.to, to)
    }

    /// `to` 是否被这条规则允许作为 `from` 的目标。
    ///
    /// 与 [`Self::applies_to`] 的分工：前者回答「这条规则管不管这条边」，后者在
    /// `Allowed` 模式下回答「这个目标在不在白名单里」。
    #[must_use]
    pub fn permits(&self, from: &str, to: &str) -> bool {
        from == to || (matches_pattern(&self.from, from) && matches_pattern(&self.to, to))
    }
}

/// 匹配一个路径通配符。
///
/// 自己实现而不引 `glob`：只需要 `*` 与 `**` 两个语义，为一个 30 行的函数增加一
/// 个依赖不划算（AGENTS.md 4.5）。语义：
///
/// - `**` 匹配任意字符（含 `/`），因此 `crates/**` 覆盖 `crates/pf-core/src/lib.rs`；
/// - `*` 匹配单个路径段内的任意字符（不含 `/`），因此 `src/*.rs` 不覆盖 `src/a/b.rs`；
/// - 其余字符按字面量匹配。
#[must_use]
pub fn matches_pattern(pattern: &str, path: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let path: Vec<char> = path.chars().collect();
    matches_from(&pattern, &path)
}

/// 通配符匹配的递归实现。
fn matches_from(pattern: &[char], path: &[char]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some('*') => {
            let doubles = pattern.len() > 1 && pattern[1] == '*';
            if doubles {
                // `**` 可以吃掉零到任意多个字符，从最长可能开始试以保证确定性。
                for taken in (0..=path.len()).rev() {
                    if matches_from(&pattern[2..], &path[taken..]) {
                        return true;
                    }
                }
                false
            } else {
                // `*` 不吃 `/`。
                let mut taken = 0;
                while taken < path.len() && path[taken] != '/' {
                    taken += 1;
                    if matches_from(&pattern[1..], &path[taken..]) {
                        return true;
                    }
                }
                matches_from(&pattern[1..], &path[taken..])
            }
        }
        Some(expected) => path.first() == Some(expected) && matches_from(&pattern[1..], &path[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_budget_has_sane_values() {
        let budget = Budget::default();
        assert!(budget.command_timeout_secs > 0);
        assert!(budget.task_timeout_secs >= budget.command_timeout_secs);
        assert!(budget.max_files > 0);
        assert!(budget.parallelism > 0);
        assert!(budget.max_tokens.is_none());
    }

    #[test]
    fn config_toml_round_trip() {
        let config = Config::for_target("/tmp/project");
        let text = config.to_toml().expect("serialize");
        let back = Config::from_toml(&text, "test").expect("deserialize");
        assert_eq!(config, back);
    }

    #[test]
    fn invalid_toml_reports_source() {
        let err =
            Config::from_toml("this is not toml", "proofforge.toml").expect_err("should fail");
        match err {
            crate::Error::ConfigParse { origin, .. } => assert_eq!(origin, "proofforge.toml"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn wants_language_when_list_is_empty() {
        let config = Config::default();
        assert!(config.wants_language("rust"));
        assert!(config.wants_language("python"));
    }

    #[test]
    fn wants_language_respects_filter() {
        let config = Config {
            languages: vec!["rust".to_owned()],
            ..Config::default()
        };
        assert!(config.wants_language("rust"));
        assert!(!config.wants_language("python"));
    }

    #[test]
    fn a_double_star_crosses_directory_boundaries() {
        assert!(matches_pattern("crates/**", "crates/pf-core/src/lib.rs"));
        assert!(matches_pattern("crates/**", "crates/a.rs"));
        assert!(matches_pattern("**/lib.rs", "crates/pf-core/src/lib.rs"));
    }

    #[test]
    fn a_single_star_stays_inside_one_segment() {
        assert!(matches_pattern("src/*.rs", "src/lib.rs"));
        assert!(!matches_pattern("src/*.rs", "src/a/b.rs"));
    }

    #[test]
    fn literal_segments_match_exactly() {
        assert!(matches_pattern("src/lib.rs", "src/lib.rs"));
        assert!(!matches_pattern("src/lib.rs", "src/main.rs"));
    }

    #[test]
    fn a_forbidden_rule_applies_to_a_matching_edge() {
        let rule = ArchitectureRule {
            name: "报告层不得依赖规则层".to_owned(),
            from: "crates/pf-report/**".to_owned(),
            to: "crates/pf-rules/**".to_owned(),
            mode: ArchitectureMode::Forbidden,
            reason: None,
        };
        assert!(rule.applies_to("crates/pf-report/src/lib.rs", "crates/pf-rules/src/lib.rs"));
        assert!(!rule.applies_to("crates/pf-cli/src/lib.rs", "crates/pf-rules/src/lib.rs"));
        assert!(!rule.applies_to(
            "crates/pf-report/src/lib.rs",
            "crates/pf-report/src/other.rs"
        ));
    }

    #[test]
    fn a_self_edge_is_never_an_architecture_problem() {
        let rule = ArchitectureRule {
            name: "x".to_owned(),
            from: "**".to_owned(),
            to: "**".to_owned(),
            mode: ArchitectureMode::Forbidden,
            reason: None,
        };
        assert!(!rule.applies_to("src/a.rs", "src/a.rs"));
    }

    #[test]
    fn an_allowed_rule_permits_only_its_own_target() {
        let rule = ArchitectureRule {
            name: "CLI 只能依赖编排层".to_owned(),
            from: "crates/pf-cli/**".to_owned(),
            to: "crates/pf-agent/**".to_owned(),
            mode: ArchitectureMode::Allowed,
            reason: None,
        };
        assert!(rule.permits("crates/pf-cli/src/lib.rs", "crates/pf-agent/src/lib.rs"));
        assert!(!rule.permits("crates/pf-cli/src/lib.rs", "crates/pf-core/src/lib.rs"));
    }
}
