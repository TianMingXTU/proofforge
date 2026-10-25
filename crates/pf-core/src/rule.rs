//! 规则与规则集定义。
//!
//! 需求文档 FR-057 至 FR-062：内置规范、项目规范导入、优先级、冲突检测、
//! 版本管理与豁免。

use serde::{Deserialize, Serialize};

use crate::ids::RuleId;
use crate::status::Severity;

/// 规则的强制级别，对应 FR-059 的三层划分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSeverity {
    /// 强制性要求，违反即阻断合并。
    Mandatory,
    /// 项目自定义规则，违反记入报告。
    Project,
    /// 风格建议，仅提示。
    Advisory,
}

impl RuleSeverity {
    /// 短标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Mandatory => "mandatory",
            Self::Project => "project",
            Self::Advisory => "advisory",
        }
    }

    /// 该级别的规则在命中时使用的严重程度。
    #[must_use]
    pub const fn implied_severity(&self) -> Severity {
        match self {
            Self::Mandatory => Severity::High,
            Self::Project => Severity::Medium,
            Self::Advisory => Severity::Low,
        }
    }
}

/// 规则的执行方式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum RuleCheck {
    /// 由内置 Rust 代码实现的检查。
    Builtin {
        /// 内置检查器的名称，例如 `no_unwrap_in_lib`。
        checker: String,
    },
    /// 调用外部工具（cargo clippy、ruff 等）并解析输出。
    ExternalTool {
        /// 工具名。
        tool: String,
        /// 工具参数（不含可执行文件本身）。
        args: Vec<String>,
    },
    /// 对符号索引执行结构查询（Tree-sitter Query 风格）。
    StructuralQuery {
        /// 查询表达式。
        query: String,
    },
    /// 没有自动化检查：这条规则只能由人审查满足。
    ///
    /// 从 AGENTS.md / Markdown 导入的项目规范大多是这一类——「提交信息说明
    /// 为什么」无法用结构查询表达。把它标成 [`Self::Builtin`] 会让报告声称
    /// 一条从未执行的检查通过了，因此必须单列一个变体（AGENTS.md 安全红线 4）。
    Manual {
        /// 人工审查时要看的要点。
        guidance: String,
    },
}

/// 单条规则定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// 规则 ID（稳定标识，用于豁免与版本追踪）。
    pub id: RuleId,
    /// 规则的可读名称。
    pub name: String,
    /// 规则说明：它检查什么、为什么重要。
    pub description: String,
    /// 强制级别。
    pub severity: RuleSeverity,
    /// 适用的语言；`None` 表示全部语言。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub languages: Option<Vec<String>>,
    /// 执行方式。
    pub check: RuleCheck,
    /// 命中的默认严重程度，可被 `severity_override` 覆盖。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity_override: Option<Severity>,
    /// 是否默认启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 规则集版本，用于审计（FR-061）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Rule {
    /// 构造一条内置规则。
    ///
    /// 规则的可读名称由 [`RuleId::local_name`] 派生，因此 ID 是唯一需要
    /// 维护的稳定标识（FR-060 / FR-061）。
    pub fn builtin(
        id: RuleId,
        description: impl Into<String>,
        severity: RuleSeverity,
        checker: impl Into<String>,
    ) -> Self {
        Self {
            id: id.clone(),
            name: id.local_name().to_owned(),
            description: description.into(),
            severity,
            languages: None,
            check: RuleCheck::Builtin {
                checker: checker.into(),
            },
            severity_override: None,
            enabled: true,
            version: None,
        }
    }

    /// 构造一条只能人工审查的规则（FR-058 导入的项目规范）。
    ///
    /// `enabled` 为 `false`：没有检查器的规则不参与自动执行，只在报告里作为
    /// 「人工审查项」出现。把它设为启用会让规则引擎去找一个不存在的检查器。
    #[must_use]
    pub fn manual(
        id: RuleId,
        description: impl Into<String>,
        severity: RuleSeverity,
        guidance: impl Into<String>,
    ) -> Self {
        Self {
            id: id.clone(),
            name: id.local_name().to_owned(),
            description: description.into(),
            severity,
            languages: None,
            check: RuleCheck::Manual {
                guidance: guidance.into(),
            },
            severity_override: None,
            enabled: false,
            version: None,
        }
    }

    /// 该规则是否有自动化检查。
    #[must_use]
    pub const fn is_automated(&self) -> bool {
        !matches!(self.check, RuleCheck::Manual { .. })
    }

    /// 该规则命中时使用的严重程度。
    #[must_use]
    pub fn effective_severity(&self) -> Severity {
        self.severity_override
            .unwrap_or_else(|| self.severity.implied_severity())
    }

    /// 规则是否适用于给定语言。
    #[must_use]
    pub fn applies_to(&self, language: &str) -> bool {
        match &self.languages {
            None => true,
            Some(languages) => languages.iter().any(|lang| lang == language),
        }
    }
}

/// 一组规则，附带版本信息，便于审计与冲突检测。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSet {
    /// 规则集名称，例如 `builtin`、`project`。
    pub name: String,
    /// 规则集版本（FR-061）。
    pub version: String,
    /// 规则列表。
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// 来源描述（内置、文件路径或 URL），用于报告展示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl RuleSet {
    /// 构造一个空规则集。
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            rules: Vec::new(),
            source: None,
        }
    }

    /// 加入一条规则。
    pub fn push(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    /// 仅返回启用的规则。
    pub fn enabled_rules(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter().filter(|rule| rule.enabled)
    }

    /// 查找指定 ID 的规则。
    #[must_use]
    pub fn find(&self, id: &RuleId) -> Option<&Rule> {
        self.rules.iter().find(|rule| &rule.id == id)
    }

    /// 检测重复 ID（FR-060 冲突检测的一部分）。
    ///
    /// 返回第一个重复的规则 ID。
    #[must_use]
    pub fn find_duplicate_id(&self) -> Option<RuleId> {
        let mut seen: Vec<&RuleId> = Vec::new();
        for rule in &self.rules {
            if seen.contains(&&rule.id) {
                return Some(rule.id.clone());
            }
            seen.push(&rule.id);
        }
        None
    }

    /// 合并另一个规则集；同名 ID 以 `other` 为准（项目规则覆盖内置规则）。
    pub fn merge(&mut self, other: &Self) {
        for rule in &other.rules {
            match self
                .rules
                .iter_mut()
                .find(|existing| existing.id == rule.id)
            {
                Some(existing) => *existing = rule.clone(),
                None => self.rules.push(rule.clone()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_name_is_derived_from_the_rule_id() {
        // 规则 ID 是唯一稳定标识：显示名由它派生，避免两处名字漂移。
        let rule = Rule::builtin(
            RuleId::builtin("no-unwrap"),
            "lib code must not unwrap",
            RuleSeverity::Mandatory,
            "no_unwrap",
        );
        assert_eq!(rule.name, "no-unwrap");
        assert_eq!(rule.effective_severity(), Severity::High);
        assert!(rule.applies_to("rust"));
    }

    #[test]
    fn rule_language_filter() {
        let mut rule = Rule::builtin(
            RuleId::builtin("test-rule"),
            "d",
            RuleSeverity::Project,
            "c",
        );
        rule.languages = Some(vec!["python".to_owned()]);
        assert!(rule.applies_to("python"));
        assert!(!rule.applies_to("rust"));
    }

    #[test]
    fn rule_set_detects_duplicate_ids() {
        let mut set = RuleSet::new("test", "1.0.0");
        let id = RuleId::builtin("test-rule");
        set.push(Rule::builtin(id.clone(), "a", RuleSeverity::Project, "c1"));
        set.push(Rule::builtin(id.clone(), "b", RuleSeverity::Project, "c2"));
        assert_eq!(set.find_duplicate_id(), Some(id));
    }

    #[test]
    fn merge_replaces_same_id() {
        let id = RuleId::builtin("test-rule");
        let mut base = RuleSet::new("builtin", "1.0.0");
        base.push(Rule::builtin(
            id.clone(),
            "old",
            RuleSeverity::Advisory,
            "c",
        ));
        let mut overlay = RuleSet::new("project", "2.0.0");
        overlay.push(Rule::builtin(
            id.clone(),
            "new",
            RuleSeverity::Mandatory,
            "c",
        ));

        base.merge(&overlay);
        assert_eq!(base.rules.len(), 1);
        assert_eq!(base.rules[0].description, "new");
        assert_eq!(base.rules[0].effective_severity(), Severity::High);
    }

    #[test]
    fn rule_set_serde_round_trip() {
        let mut set = RuleSet::new("project", "1.2.3");
        set.push(Rule::builtin(
            RuleId::builtin("test-rule"),
            "avoid unwrap in library code",
            RuleSeverity::Mandatory,
            "no_unwrap",
        ));
        let json = serde_json::to_string(&set).expect("serialize");
        let back: RuleSet = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(set, back);
    }
}
