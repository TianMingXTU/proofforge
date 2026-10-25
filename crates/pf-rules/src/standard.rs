//! 内置工程规范的说明文档（FR-057）。
//!
//! # 为什么规范要能渲染成文档
//!
//! FR-057 要求规范「通用且可解释」。可解释不只是每条规则带一句说明，还要能
//! 把整套规范一次性摊开给人看：这条规则属于哪一层、违反时默认多严重、用什么
//! 方式检查。因此这里提供一个渲染器，把 [`builtin_ruleset`] 输出成 Markdown。
//!
//! 渲染是纯函数：同一份规则集必然得到同一份文档，因此它可以进审计记录，也
//! 可以在 CI 里 diff（FR-061 规则版本管理）。

use pf_core::rule::{Rule, RuleCheck, RuleSet, RuleSeverity};

/// 把规则集渲染成 Markdown 说明文档。
///
/// 无法自动化检查的规则会被单独分组并标注——读者因此不会以为每条规则都在
/// 每次审查中被执行（AGENTS.md 安全红线 4）。
#[must_use]
pub fn render_markdown(ruleset: &RuleSet) -> String {
    let mut out = String::new();
    out.push_str(&format!("# 工程规范：{}\n\n", ruleset.name));
    out.push_str(&format!("版本：`{}`\n\n", ruleset.version));
    if let Some(source) = &ruleset.source {
        out.push_str(&format!("来源：{source}\n\n"));
    }
    let automated: Vec<&Rule> = ruleset
        .rules
        .iter()
        .filter(|rule| rule.is_automated())
        .collect();
    let manual: Vec<&Rule> = ruleset
        .rules
        .iter()
        .filter(|rule| !rule.is_automated())
        .collect();

    out.push_str(&format!(
        "共 {} 条规则：{} 条自动检查，{} 条人工审查。\n\n",
        ruleset.rules.len(),
        automated.len(),
        manual.len()
    ));

    for (title, group) in [("自动检查的规则", &automated), ("人工审查的规则", &manual)]
    {
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("## {title}\n\n"));
        for rule in group {
            out.push_str(&format!("### `{}`\n\n", rule.id.get()));
            out.push_str(&format!("- **级别**：{}\n", severity_label(rule.severity)));
            out.push_str(&format!(
                "- **默认严重程度**：{}\n",
                rule.effective_severity().as_str()
            ));
            out.push_str(&format!("- **检查方式**：{}\n", check_label(&rule.check)));
            out.push_str(&format!("- **说明**：{}\n", rule.description));
            out.push_str(&format!(
                "- **是否参与自动执行**：{}\n",
                if rule.enabled { "是" } else { "否" }
            ));
            out.push('\n');
        }
    }
    out
}

/// 级别的中文标签。
const fn severity_label(severity: RuleSeverity) -> &'static str {
    match severity {
        RuleSeverity::Mandatory => "强制性要求",
        RuleSeverity::Project => "项目规则",
        RuleSeverity::Advisory => "风格建议",
    }
}

/// 检查方式的中文标签。
fn check_label(check: &RuleCheck) -> String {
    match check {
        RuleCheck::Builtin { checker } => format!("内置检查器 `{checker}`"),
        RuleCheck::ExternalTool { tool, args } => {
            format!("外部工具 `{tool}`（参数：{}）", args.join(" "))
        }
        RuleCheck::StructuralQuery { query } => format!("结构查询 `{query}`"),
        RuleCheck::Manual { guidance } => format!("人工审查（要点：{guidance}）"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_ruleset;

    #[test]
    fn the_builtin_standard_renders_as_a_readable_document() {
        let markdown = render_markdown(&builtin_ruleset());
        assert!(markdown.contains("# 工程规范：builtin"));
        assert!(markdown.contains("## 自动检查的规则"));
        assert!(markdown.contains("内置检查器"));
        // 每条规则都必须出现在文档里：漏一条就等于这条规则不可解释。
        for rule in builtin_ruleset().rules {
            assert!(
                markdown.contains(rule.id.get()),
                "规范文档缺少规则 {}",
                rule.id.get()
            );
        }
    }

    #[test]
    fn manual_rules_are_listed_separately() {
        // 人工规则必须与自动规则分开，否则读者会以为它每次都被执行（红线 4）。
        let mut ruleset = RuleSet::new("project", "1.0.0");
        ruleset.push(Rule::manual(
            pf_core::ids::RuleId::project("commit-message"),
            "提交信息必须说明为什么",
            RuleSeverity::Project,
            "写清触发条件与取舍",
        ));
        let markdown = render_markdown(&ruleset);
        assert!(markdown.contains("## 人工审查的规则"));
        assert!(markdown.contains("人工审查"));
        assert!(!markdown.contains("## 自动检查的规则"));
    }

    #[test]
    fn rendering_is_deterministic() {
        let ruleset = builtin_ruleset();
        assert_eq!(render_markdown(&ruleset), render_markdown(&ruleset));
    }
}
