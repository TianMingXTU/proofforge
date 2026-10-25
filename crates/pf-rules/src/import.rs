//! 项目规范导入（FR-058）。
//!
//! # 支持哪些来源
//!
//! | 格式 | 解析方式 | 产出的规则 |
//! |---|---|---|
//! | TOML | 直接反序列化成 [`RuleSet`] | 完整定义（含检查方式） |
//! | YAML | 直接反序列化成 [`RuleSet`] | 完整定义（含检查方式） |
//! | Markdown / AGENTS.md / CLAUDE.md | 按标题与列表的结构约定提取 | 只能人工审查的规则 |
//!
//! # 为什么 Markdown 导入只产出人工规则
//!
//! `AGENTS.md` 里写的是「提交信息说明为什么」这类**意图**，不是可执行的结构
//! 查询。把一句话意图猜成一条 `StructuralQuery`，再报告「该规则已检查并通过」，
//! 是在伪造证据（AGENTS.md 安全红线 4）。因此 Markdown 导入的规则一律是
//! [`pf_core::rule::RuleCheck::Manual`]，且默认不参与自动执行。
//!
//! # 提取约定（写进报告，不藏在代码里）
//!
//! - 二级/三级标题（`##`、`###`）视为一条规范的候选；
//! - 标题下的列表项与正文作为该规范的说明；
//! - 规则 ID 由标题文本派生（小写、非字母数字转 `-`），因此同一份文档重复
//!   导入得到同一批 ID——这是 FR-061 可审计性的前提。
//!
//! 提取不到任何规范时返回 [`ImportError::NoRules`]，而不是给一个空规则集：
//! 「导入成功但一条规范都没有」与「导入失败」对调用方的含义不同。

use std::path::Path;

use pf_core::exempt::{ExemptionError, ExemptionSet};
use pf_core::ids::RuleId;
use pf_core::rule::{Rule, RuleSet, RuleSeverity};
use serde::{Deserialize, Serialize};

/// 支持的规范来源格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecFormat {
    /// `AGENTS.md`：按 Markdown 结构约定提取。
    AgentsMd,
    /// `CLAUDE.md`：与 `AGENTS.md` 同一套约定。
    ClaudeMd,
    /// 通用 Markdown。
    Markdown,
    /// YAML 规则集。
    Yaml,
    /// TOML 规则集。
    Toml,
}

impl SpecFormat {
    /// 从文件名推断格式。
    ///
    /// 推断不了时返回 `None`：让调用方明确指定，而不是猜一个可能错的解析器。
    ///
    /// 匹配对大小写不敏感（`AGENTS.MD`、`Rules.YAML` 都要认），因此先归一化
    /// 再用 [`Path::extension`] 判断扩展名——手写 `ends_with` 在 Windows 与
    /// macOS 上会把 `FOO.MD` 漏掉。
    #[must_use]
    pub fn from_file_name(name: &str) -> Option<Self> {
        let lowered = name.to_ascii_lowercase();
        if lowered.ends_with("agents.md") {
            Some(Self::AgentsMd)
        } else if lowered.ends_with("claude.md") {
            Some(Self::ClaudeMd)
        } else {
            match Path::new(&lowered).extension().and_then(|ext| ext.to_str()) {
                Some("md" | "markdown") => Some(Self::Markdown),
                Some("yaml" | "yml") => Some(Self::Yaml),
                Some("toml") => Some(Self::Toml),
                _ => None,
            }
        }
    }

    /// 稳定标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::AgentsMd => "agents_md",
            Self::ClaudeMd => "claude_md",
            Self::Markdown => "markdown",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
        }
    }

    /// 该格式能否表达自动化检查。
    #[must_use]
    pub const fn supports_automation(&self) -> bool {
        matches!(self, Self::Yaml | Self::Toml)
    }
}

/// 导入失败的原因。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImportError {
    /// 无法从文件名推断格式。
    #[error("cannot infer the standard format from `{path}`; pass an explicit format")]
    UnknownFormat {
        /// 文件路径。
        path: String,
    },
    /// 结构化格式（TOML/YAML）解析失败。
    #[error("`{path}` is not a valid {format} rule set: {reason}")]
    Parse {
        /// 文件路径。
        path: String,
        /// 格式标签。
        format: String,
        /// 底层原因。
        reason: String,
    },
    /// 文档里提取不到任何规范。
    #[error("`{path}` contains no recognizable rules")]
    NoRules {
        /// 文件路径。
        path: String,
    },
    /// 读文件失败。
    #[error("cannot read `{path}`: {reason}")]
    Read {
        /// 文件路径。
        path: String,
        /// 底层原因。
        reason: String,
    },
}

/// 一次导入的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedStandard {
    /// 导入的规则集。
    pub ruleset: RuleSet,
    /// 来源描述（文件路径或格式名）。
    pub source: String,
    /// 导入过程中做出的判断，逐条写明。
    ///
    /// 不可省略：从自然语言提取规范本质上是启发式的，调用方必须能看到工具
    /// 「读了什么、猜成了什么」。
    pub notes: Vec<String>,
}

/// 从文件导入项目规范。
///
/// # Errors
/// 格式无法推断、内容解析失败、或提取不到任何规范时返回 [`ImportError`]。
pub fn import_file(path: &Path) -> Result<ImportedStandard, ImportError> {
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let Some(format) = SpecFormat::from_file_name(&name) else {
        return Err(ImportError::UnknownFormat {
            path: path.display().to_string(),
        });
    };
    let text = std::fs::read_to_string(path).map_err(|reason| ImportError::Read {
        path: path.display().to_string(),
        reason: reason.to_string(),
    })?;
    import_text(&text, format).map_err(|error| match error {
        ImportError::Parse { format, reason, .. } => ImportError::Parse {
            path: path.display().to_string(),
            format,
            reason,
        },
        ImportError::NoRules { .. } => ImportError::NoRules {
            path: path.display().to_string(),
        },
        other => other,
    })
}

/// 从文本导入项目规范。
///
/// # Errors
/// 结构化格式解析失败、或提取不到任何规范时返回 [`ImportError`]。
pub fn import_text(text: &str, format: SpecFormat) -> Result<ImportedStandard, ImportError> {
    match format {
        SpecFormat::Toml => import_structured(text, format, |text| {
            toml::from_str::<RuleSet>(text).map_err(|reason| reason.to_string())
        }),
        SpecFormat::Yaml => import_structured(text, format, |text| {
            serde_yaml::from_str::<RuleSet>(text).map_err(|reason| reason.to_string())
        }),
        SpecFormat::AgentsMd | SpecFormat::ClaudeMd | SpecFormat::Markdown => {
            import_markdown(text, format)
        }
    }
}

/// 解析结构化规则集（TOML / YAML）。
fn import_structured(
    text: &str,
    format: SpecFormat,
    parse: impl FnOnce(&str) -> Result<RuleSet, String>,
) -> Result<ImportedStandard, ImportError> {
    let ruleset = parse(text).map_err(|reason| ImportError::Parse {
        path: String::new(),
        format: format.as_str().to_owned(),
        reason,
    })?;
    let mut notes = vec![format!(
        "以 {} 规则集解析，得到 {} 条规则。",
        format.as_str(),
        ruleset.rules.len()
    )];
    let manual = ruleset
        .rules
        .iter()
        .filter(|rule| !rule.is_automated())
        .count();
    if manual > 0 {
        notes.push(format!(
            "{manual} 条规则没有自动化检查（RuleCheck::Manual），只作为人工审查项出现。"
        ));
    }
    Ok(ImportedStandard {
        ruleset,
        source: format.as_str().to_owned(),
        notes,
    })
}

/// 按 Markdown 结构约定提取规范。
fn import_markdown(text: &str, format: SpecFormat) -> Result<ImportedStandard, ImportError> {
    let mut rules = Vec::new();
    let mut notes = Vec::new();
    let mut current: Option<Section> = None;

    for line in text.lines() {
        if let Some(heading) = parse_heading(line) {
            if let Some(section) = current.take() {
                rules.push(section.into_rule());
            }
            // 只取二级及以下标题：一级标题是文档标题，不是一条规范。
            if heading.level >= 2 {
                current = Some(Section::new(&heading.text));
            }
            continue;
        }
        if let Some(section) = current.as_mut() {
            section.push_body(line);
        }
    }
    if let Some(section) = current.take() {
        rules.push(section.into_rule());
    }

    if rules.is_empty() {
        return Err(ImportError::NoRules {
            path: format.as_str().to_owned(),
        });
    }

    notes.push(format!(
        "按 Markdown 结构约定提取：二级及以下标题各算一条规范，共 {} 条。",
        rules.len()
    ));
    notes.push(
        "Markdown 里的规范是意图而不是可执行查询，因此全部登记为人工审查项（默认不参与自动执行）。"
            .to_owned(),
    );

    Ok(ImportedStandard {
        ruleset: RuleSet {
            name: format!("project-{format}", format = format.as_str()),
            version: "imported".to_owned(),
            rules,
            source: Some(format.as_str().to_owned()),
        },
        source: format.as_str().to_owned(),
        notes,
    })
}

/// 去掉一行的列表记号；不是列表项时原样返回。
fn strip_list_marker(line: &str) -> &str {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return rest;
        }
    }
    // 有序列表：`1. ` 或 `1) `。
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &line[digits..];
        return rest
            .strip_prefix(". ")
            .or_else(|| rest.strip_prefix(") "))
            .unwrap_or(line);
    }
    line
}

/// 一个 Markdown 章节。
struct Section {
    /// 标题文本。
    title: String,
    /// 章节正文（已规整空白）。
    body: Vec<String>,
}

impl Section {
    /// 新章节。
    fn new(title: &str) -> Self {
        Self {
            title: title.to_owned(),
            body: Vec::new(),
        }
    }

    /// 追加一行正文。
    fn push_body(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        // 去掉列表记号：内容是规范，记号只是排版。
        let content = strip_list_marker(trimmed);
        // 去掉行内代码记号，让说明读起来是连贯的句子。
        let content = content.trim_matches('`').trim();
        if !content.is_empty() {
            self.body.push(content.to_owned());
        }
    }

    /// 转成一条人工审查规则。
    fn into_rule(self) -> Rule {
        let slug = slugify(&self.title);
        let id = RuleId::project(&slug);
        let guidance = if self.body.is_empty() {
            self.title.clone()
        } else {
            self.body.join("；")
        };
        Rule::manual(
            id,
            self.title.clone(),
            // 项目自己写下的规范按「项目规则」处理：违反记入报告，但不默认阻断。
            // 想要阻断的项目应在 TOML/YAML 里显式声明 severity。
            RuleSeverity::Project,
            guidance,
        )
    }
}

/// 一个 Markdown 标题。
struct Heading {
    /// 标题级别（`#` 的个数）。
    level: usize,
    /// 标题文本。
    text: String,
}

/// 解析一行 Markdown 标题；不是标题时返回 `None`。
fn parse_heading(line: &str) -> Option<Heading> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let text = trimmed[level..].trim();
    if text.is_empty() {
        return None;
    }
    Some(Heading {
        level,
        text: text.to_owned(),
    })
}

/// 把标题文本变成稳定的规则 ID 片段。
///
/// 同一个标题必然得到同一个 ID，因此重复导入同一份文档不会累积重复规则
/// （FR-061）。非 ASCII 字符按字符保留——中文标题是可读且稳定的。
fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for character in text.chars() {
        if character.is_alphanumeric() {
            slug.extend(character.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_owned()
}

/// 从 YAML 文本解析豁免集合（FR-062）。
///
/// YAML 加载放在 `pf-rules` 而不是 `pf-core`：核心领域模型只依赖 `toml`，
/// 多一个序列化格式就多一个需要维护的依赖面。
///
/// # Errors
/// 文本不是合法 YAML、或某条豁免缺少理由/批准人时返回 [`ExemptionError`]。
pub fn exemptions_from_yaml(text: &str) -> Result<ExemptionSet, ExemptionError> {
    let set: ExemptionSet = serde_yaml::from_str(text).map_err(|reason| ExemptionError::Parse {
        reason: reason.to_string(),
    })?;
    set.validate()?;
    Ok(set)
}

/// 从 YAML 文件解析豁免集合。
///
/// # Errors
/// 读文件失败、内容不是合法 YAML、或豁免缺少理由/批准人时返回 [`ExemptionError`]。
pub fn exemptions_from_yaml_file(path: &Path) -> Result<ExemptionSet, ExemptionError> {
    let text = std::fs::read_to_string(path).map_err(|reason| ExemptionError::Parse {
        reason: format!("{}: {reason}", path.display()),
    })?;
    exemptions_from_yaml(&text)
}

/// 从 TOML 文件解析豁免集合。
///
/// # Errors
/// 读文件失败、内容不是合法 TOML、或豁免缺少理由/批准人时返回 [`ExemptionError`]。
pub fn exemptions_from_toml_file(path: &Path) -> Result<ExemptionSet, ExemptionError> {
    let text = std::fs::read_to_string(path).map_err(|reason| ExemptionError::Parse {
        reason: format!("{}: {reason}", path.display()),
    })?;
    ExemptionSet::from_toml(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::rule::RuleCheck;

    #[test]
    fn the_format_is_inferred_from_the_file_name() {
        assert_eq!(
            SpecFormat::from_file_name("AGENTS.md"),
            Some(SpecFormat::AgentsMd)
        );
        assert_eq!(
            SpecFormat::from_file_name("CLAUDE.md"),
            Some(SpecFormat::ClaudeMd)
        );
        assert_eq!(
            SpecFormat::from_file_name("standards.yaml"),
            Some(SpecFormat::Yaml)
        );
        assert_eq!(
            SpecFormat::from_file_name("standards.yml"),
            Some(SpecFormat::Yaml)
        );
        assert_eq!(
            SpecFormat::from_file_name("proofforge.toml"),
            Some(SpecFormat::Toml)
        );
        assert_eq!(SpecFormat::from_file_name("notes.txt"), None);
    }

    #[test]
    fn a_toml_rule_set_round_trips() {
        let toml = r#"
name = "project"
version = "1.0.0"

[[rules]]
id = "project.no-unwrap"
name = "no-unwrap"
description = "库代码不得 unwrap"
severity = "mandatory"
check = { type = "builtin", checker = "no_unwrap" }
"#;
        let imported = import_text(toml, SpecFormat::Toml).expect("import");
        assert_eq!(imported.ruleset.rules.len(), 1);
        assert!(imported.ruleset.rules[0].is_automated());
        assert!(imported.ruleset.rules[0].enabled);
    }

    #[test]
    fn a_yaml_rule_set_round_trips() {
        let yaml = r#"
name: project
version: 1.0.0
rules:
  - id: project.max-fan-out
    name: max-fan-out
    description: 单个模块的依赖数量不得超过 5
    severity: project
    check:
      type: structural_query
      query: "(use_declaration) @use"
"#;
        let imported = import_text(yaml, SpecFormat::Yaml).expect("import");
        assert_eq!(imported.ruleset.rules.len(), 1);
        assert!(imported.ruleset.rules[0].is_automated());
    }

    #[test]
    fn a_markdown_document_yields_manual_rules_only() {
        // 红线 4：Markdown 里的意图不能变成「已执行的检查」。
        let markdown = "# 团队规范\n\n## 提交信息必须说明为什么\n\n- 只说「修复 bug」不够\n- 要写清触发条件\n\n### 禁止在库代码里 unwrap\n\n统一用 expect 并附带上下文。\n";
        let imported = import_text(markdown, SpecFormat::Markdown).expect("import");
        assert_eq!(imported.ruleset.rules.len(), 2);
        for rule in &imported.ruleset.rules {
            assert!(!rule.is_automated(), "{} 不应有自动化检查", rule.id.get());
            assert!(!rule.enabled, "{} 不应参与自动执行", rule.id.get());
            assert!(matches!(rule.check, RuleCheck::Manual { .. }));
        }
        assert!(imported
            .notes
            .iter()
            .any(|note| note.contains("人工审查项")));
    }

    #[test]
    fn the_rule_id_is_derived_from_the_heading_and_is_stable() {
        let left =
            import_text("## 提交信息必须说明为什么\n", SpecFormat::Markdown).expect("import");
        let right =
            import_text("## 提交信息必须说明为什么\n", SpecFormat::Markdown).expect("import");
        assert_eq!(left.ruleset.rules[0].id, right.ruleset.rules[0].id);
        assert_ne!(left.ruleset.rules[0].id.get(), "", "规则 ID 不能为空");
    }

    #[test]
    fn a_document_without_headings_is_rejected() {
        let error = import_text("只是一段正文，没有任何标题。\n", SpecFormat::Markdown)
            .expect_err("no rules");
        assert!(matches!(error, ImportError::NoRules { .. }));
    }

    #[test]
    fn a_top_level_heading_is_not_a_rule() {
        // 文档标题不是规范。
        let imported =
            import_text("# ProofForge\n\n## 一条规范\n", SpecFormat::Markdown).expect("import");
        assert_eq!(imported.ruleset.rules.len(), 1);
    }

    #[test]
    fn exemptions_can_be_loaded_from_yaml() {
        let yaml = r"
exemptions:
  - rule_id: builtin.single-implementation-trait
    scope:
      kind: rule
    reason: 对外契约
    granted_by: bob
";
        let set = exemptions_from_yaml(yaml).expect("parse");
        assert_eq!(set.exemptions.len(), 1);
        assert_eq!(set.exemptions[0].scope.describe(), "整条规则");
    }

    #[test]
    fn an_exemption_without_a_reason_is_rejected_from_yaml() {
        let yaml = r#"
exemptions:
  - rule_id: builtin.single-implementation-trait
    scope:
      kind: rule
    reason: "   "
    granted_by: bob
"#;
        let error = exemptions_from_yaml(yaml).expect_err("missing reason");
        assert!(matches!(error, ExemptionError::MissingReason { .. }));
    }

    #[test]
    fn a_slug_never_starts_or_ends_with_a_separator() {
        assert_eq!(slugify("  提交信息！！！  "), "提交信息");
        assert_eq!(slugify("No Unwrap In Library"), "no-unwrap-in-library");
    }

    #[test]
    fn list_markers_are_stripped_from_the_guidance() {
        let imported = import_text(
            "## 规范\n\n- 第一点\n- 第二点\n1. 第三点\n",
            SpecFormat::Markdown,
        )
        .expect("import");
        let guidance = match &imported.ruleset.rules[0].check {
            RuleCheck::Manual { guidance } => guidance.clone(),
            other => panic!("expected manual, got {other:?}"),
        };
        assert_eq!(guidance, "第一点；第二点；第三点");
    }
}
