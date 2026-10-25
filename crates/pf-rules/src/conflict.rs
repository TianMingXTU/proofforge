//! 规则冲突检测（FR-060）。
//!
//! # 检测哪几类冲突
//!
//! 1. **重复 ID**：同一个规则 ID 出现两次。这是确定性的结构冲突——报告与豁免
//!    都按 ID 索引，重复会让「豁免了哪一条」变得不确定。
//! 2. **同 ID 不同定义**：合并多个来源后，同名规则的严重程度或说明不一致。
//! 3. **同主题相反模态**：两条规则约束同一件事，一条说「不得」、另一条说
//!    「应当」。这是**启发式**判断（基于措辞，不是语义），因此解释里必须写明
//!    依据是措辞（AGENTS.md 安全红线 5：不把猜测说成结论）。
//! 4. **强制与建议并存**：同一检查方式既被标成强制性、又被标成建议性。
//!
//! # 为什么不自动解决冲突
//!
//! 冲突往往意味着两个团队对同一件事有不同预期，答案取决于项目上下文。工具
//! 负责把冲突摆到桌面上并说明判断依据，裁决留给人（需求文档 1.3「尊重工程
//! 上下文」）。

use pf_core::ids::RuleId;
use pf_core::rule::{Rule, RuleCheck, RuleSeverity};
use serde::{Deserialize, Serialize};

/// 冲突类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// 同一规则 ID 出现多次。
    DuplicateId,
    /// 同一规则 ID 的定义不一致。
    DivergentDefinition,
    /// 同一主题上出现相反的模态（启发式）。
    OppositeModality,
    /// 同一检查方式上强制与建议并存。
    MixedEnforcement,
}

impl ConflictKind {
    /// 稳定标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::DuplicateId => "duplicate_id",
            Self::DivergentDefinition => "divergent_definition",
            Self::OppositeModality => "opposite_modality",
            Self::MixedEnforcement => "mixed_enforcement",
        }
    }

    /// 该冲突是否是确定性判断（而非启发式）。
    #[must_use]
    pub const fn is_deterministic(&self) -> bool {
        matches!(self, Self::DuplicateId | Self::DivergentDefinition)
    }
}

/// 一条规则冲突。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleConflict {
    /// 冲突类型。
    pub kind: ConflictKind,
    /// 涉及的规则 ID（排序后存放，报告因此可复现）。
    pub rules: Vec<RuleId>,
    /// 人类可读的解释，含判断依据。
    pub explanation: String,
}

/// 检测一组规则里的冲突。
///
/// 结果按（类型，规则 ID）排序：同一份输入必然得到同一份冲突列表。
#[must_use]
pub fn detect(rules: &[Rule]) -> Vec<RuleConflict> {
    let mut conflicts = Vec::new();
    conflicts.extend(duplicate_ids(rules));
    conflicts.extend(divergent_definitions(rules));
    conflicts.extend(mixed_enforcement(rules));
    conflicts.extend(opposite_modality(rules));
    conflicts.sort_by(|left, right| {
        left.kind
            .as_str()
            .cmp(right.kind.as_str())
            .then_with(|| left.rules.cmp(&right.rules))
    });
    conflicts.dedup_by(|left, right| left.kind == right.kind && left.rules == right.rules);
    conflicts
}

/// 检测重复 ID。
fn duplicate_ids(rules: &[Rule]) -> Vec<RuleConflict> {
    let mut seen: Vec<&RuleId> = Vec::new();
    let mut conflicts = Vec::new();
    for rule in rules {
        if seen.contains(&&rule.id) {
            conflicts.push(RuleConflict {
                kind: ConflictKind::DuplicateId,
                rules: vec![rule.id.clone()],
                explanation: format!(
                    "规则 `{}` 出现了多次。报告与豁免都按 ID 索引，重复会让\
                     「豁免了哪一条」变得不确定。",
                    rule.id.get()
                ),
            });
        }
        seen.push(&rule.id);
    }
    conflicts
}

/// 检测同 ID 但定义不一致。
fn divergent_definitions(rules: &[Rule]) -> Vec<RuleConflict> {
    let mut conflicts = Vec::new();
    for (index, left) in rules.iter().enumerate() {
        for right in &rules[index + 1..] {
            if left.id != right.id {
                continue;
            }
            if left.severity != right.severity || left.description != right.description {
                conflicts.push(RuleConflict {
                    kind: ConflictKind::DivergentDefinition,
                    rules: vec![left.id.clone()],
                    explanation: format!(
                        "规则 `{}` 有两份不一致的定义：严重程度 {} / {}，\
                         说明「{}」/「{}」。后合并的一方会覆盖前者，\
                         实际生效的规则与代码里读到的不一定一致。",
                        left.id.get(),
                        left.severity.as_str(),
                        right.severity.as_str(),
                        left.description,
                        right.description
                    ),
                });
            }
        }
    }
    conflicts
}

/// 检测同一检查方式上强制与建议并存。
fn mixed_enforcement(rules: &[Rule]) -> Vec<RuleConflict> {
    let mut conflicts = Vec::new();
    for (index, left) in rules.iter().enumerate() {
        if !left.is_automated() {
            continue;
        }
        for right in &rules[index + 1..] {
            if left.id == right.id || !right.is_automated() {
                continue;
            }
            if !same_check(&left.check, &right.check) {
                continue;
            }
            let opposite = matches!(
                (left.severity, right.severity),
                (RuleSeverity::Mandatory, RuleSeverity::Advisory)
                    | (RuleSeverity::Advisory, RuleSeverity::Mandatory)
            );
            if !opposite {
                continue;
            }
            let mut pair = vec![left.id.clone(), right.id.clone()];
            pair.sort();
            conflicts.push(RuleConflict {
                kind: ConflictKind::MixedEnforcement,
                rules: pair,
                explanation: format!(
                    "`{}`（{}）与 `{}`（{}）使用同一个检查，却被分别标成\
                     强制性与建议性：同一次命中既会阻断合并、又只会提示。",
                    left.id.get(),
                    left.severity.as_str(),
                    right.id.get(),
                    right.severity.as_str()
                ),
            });
        }
    }
    conflicts
}

/// 检测同一主题上相反的模态（启发式）。
fn opposite_modality(rules: &[Rule]) -> Vec<RuleConflict> {
    let mut conflicts = Vec::new();
    for (index, left) in rules.iter().enumerate() {
        for right in &rules[index + 1..] {
            if left.id == right.id {
                continue;
            }
            let (Some((left_modality, left_subject)), Some((right_modality, right_subject))) = (
                modality_and_subject(&left.description),
                modality_and_subject(&right.description),
            ) else {
                continue;
            };
            if left_modality == right_modality || left_subject != right_subject {
                continue;
            }
            let mut pair = vec![left.id.clone(), right.id.clone()];
            pair.sort();
            conflicts.push(RuleConflict {
                kind: ConflictKind::OppositeModality,
                rules: pair,
                explanation: format!(
                    "`{}` 与 `{}` 都在说「{}」，但一条用{}措辞、另一条用{}措辞。\
                     该判断基于措辞而非语义分析，请人工确认是否真的矛盾。",
                    left.id.get(),
                    right.id.get(),
                    left_subject,
                    left_modality,
                    right_modality
                ),
            });
        }
    }
    conflicts
}

/// 两个检查方式是否指向同一次执行。
///
/// 三种「由一个字符串标识的检查」合并成一个分支：它们判等的方式本来就一样，
/// 拆开写只会让读者以为各有特殊规则。`ExternalTool` 单独一个分支——工具相同
/// 但参数不同时执行的仍是同一个工具，因此参数不参与比较。
fn same_check(left: &RuleCheck, right: &RuleCheck) -> bool {
    match (left, right) {
        (
            RuleCheck::Builtin { checker: a }
            | RuleCheck::StructuralQuery { query: a }
            | RuleCheck::Manual { guidance: a },
            RuleCheck::Builtin { checker: b }
            | RuleCheck::StructuralQuery { query: b }
            | RuleCheck::Manual { guidance: b },
        ) => a == b,
        (RuleCheck::ExternalTool { tool: a, .. }, RuleCheck::ExternalTool { tool: b, .. }) => {
            a == b
        }
        _ => false,
    }
}

/// 禁止性模态词表。
const FORBIDDEN_WORDS: [&str; 8] = [
    "不得",
    "禁止",
    "不准",
    "不允许",
    "must not",
    "never",
    "do not",
    "forbid",
];

/// 要求性模态词表。
const REQUIRED_WORDS: [&str; 5] = ["必须", "应当", "应该", "must", "shall"];

/// 从说明文本里读出模态与主题。
///
/// 主题取「模态词之前的那段文字」：两条规则只有在对同一个对象下达相反指令时
/// 才算冲突。模态之后的限定条件本来就可以不同——「不得 unwrap」与「必须记录
/// 错误」并不矛盾，因此不能拿整句比。
///
/// 词表刻意不做同义词扩展：误报一条「矛盾」比漏报更糟，它会让人去改一条本来
/// 没有问题的规则。
fn modality_and_subject(description: &str) -> Option<(&'static str, String)> {
    let forbidden = earliest_match(description, &FORBIDDEN_WORDS);
    let required = earliest_match(description, &REQUIRED_WORDS);
    match (forbidden, required) {
        (Some(forbidden_at), Some(required_at)) => {
            // 两种模态都出现时以先出现的那个为准：句子的语气由第一个模态词定。
            if forbidden_at <= required_at {
                Some(("禁止性", subject_at(description, forbidden_at)))
            } else {
                Some(("要求性", subject_at(description, required_at)))
            }
        }
        (Some(at), None) => Some(("禁止性", subject_at(description, at))),
        (None, Some(at)) => Some(("要求性", subject_at(description, at))),
        (None, None) => None,
    }
}

/// 找出文本中最早出现的模态词位置。
fn earliest_match(text: &str, words: &[&str]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for word in words {
        for variant in [(*word).to_owned(), capitalize(word)] {
            if let Some(index) = text.find(&variant) {
                if best.is_none_or(|current| index < current) {
                    best = Some(index);
                }
            }
        }
    }
    best
}

/// 取模态词之前的主题，并归一化空白与大小写。
fn subject_at(text: &str, index: usize) -> String {
    text[..index]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// 首字母大写（用于匹配句首的英文模态词）。
fn capitalize(word: &str) -> String {
    let mut characters = word.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, description: &str, severity: RuleSeverity) -> Rule {
        Rule::builtin(
            RuleId::builtin(id),
            description,
            severity,
            format!("checker-{id}"),
        )
    }

    #[test]
    fn a_duplicated_id_is_a_conflict() {
        let rules = vec![
            rule("no-unwrap", "库代码不得 unwrap", RuleSeverity::Mandatory),
            rule("no-unwrap", "库代码不得 unwrap", RuleSeverity::Mandatory),
        ];
        let conflicts = detect(&rules);
        assert!(conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::DuplicateId));
    }

    #[test]
    fn divergent_definitions_of_one_id_are_a_conflict() {
        let rules = vec![
            rule("no-unwrap", "库代码不得 unwrap", RuleSeverity::Mandatory),
            rule("no-unwrap", "库代码不得 unwrap", RuleSeverity::Advisory),
        ];
        let conflicts = detect(&rules);
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.kind == ConflictKind::DivergentDefinition),
            "{conflicts:?}"
        );
    }

    #[test]
    fn mandatory_and_advisory_on_the_same_check_is_a_conflict() {
        let mut rules = vec![
            rule("strict-no-unwrap", "不得 unwrap", RuleSeverity::Mandatory),
            rule("loose-no-unwrap", "不得 unwrap", RuleSeverity::Advisory),
        ];
        // 两条规则的检查器由 ID 派生而不同；这里显式改成同一个检查，
        // 否则测到的是「ID 不同所以没冲突」。
        rules[1].check = rules[0].check.clone();
        let conflicts = detect(&rules);
        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.kind == ConflictKind::MixedEnforcement),
            "{conflicts:?}"
        );
    }

    #[test]
    fn opposite_modality_is_reported_as_a_heuristic() {
        // 该冲突必须自证是启发式：不把猜测说成结论（红线 5）。
        let rules = vec![
            rule(
                "forbid-unwrap",
                "unwrap 不得出现在库代码中",
                RuleSeverity::Mandatory,
            ),
            rule(
                "require-unwrap",
                "unwrap 必须出现在错误路径上",
                RuleSeverity::Project,
            ),
        ];
        let conflicts = detect(&rules);
        let conflict = conflicts
            .iter()
            .find(|conflict| conflict.kind == ConflictKind::OppositeModality)
            .expect("opposite modality");
        assert!(conflict.explanation.contains("基于措辞"));
        assert!(!ConflictKind::OppositeModality.is_deterministic());
    }

    #[test]
    fn the_same_modality_on_the_same_subject_is_not_a_conflict() {
        // 两条规则都在禁止同一件事：这是重复，不是矛盾。
        let rules = vec![
            rule("a", "unwrap 不得出现在库代码中", RuleSeverity::Mandatory),
            rule("b", "unwrap 不得出现在错误路径上", RuleSeverity::Project),
        ];
        assert_eq!(detect(&rules).len(), 0);
    }

    #[test]
    fn compatible_rules_produce_no_conflicts() {
        let rules = vec![
            rule("no-unwrap", "库代码不得 unwrap", RuleSeverity::Mandatory),
            rule("no-todo", "代码中不得留下 TODO", RuleSeverity::Project),
        ];
        assert_eq!(detect(&rules).len(), 0);
    }

    #[test]
    fn detection_is_deterministic_for_the_same_input() {
        let rules = vec![
            rule("a", "unwrap 不得出现在库代码中", RuleSeverity::Mandatory),
            rule("b", "unwrap 必须出现在错误路径上", RuleSeverity::Project),
        ];
        assert_eq!(detect(&rules), detect(&rules));
    }
}
