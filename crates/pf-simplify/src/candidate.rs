//! 简化候选生成（FR-050）。
//!
//! # 一条不可谈判的边界
//!
//! 候选分成两类，报告里必须分开写：
//!
//! - **可自动编辑**（[`Automation::Automatic`]）：结构事实足以证明这个变换在文本
//!   层面是安全的，例如「项目内零调用者的公共 API」对应一次删除。即使如此，
//!   它仍然要过 [`crate::regression`] 与 [`crate::behavior`] 两道验证，并且要
//!   人工确认才落地（FR-055）。
//! - **仅建议**（[`Automation::Manual`]）：需要语义判断的变换，例如把重复代码
//!   提成共享函数、内联只有一种实现的 trait。工具只给出结构化说明，不生成
//!   补丁——把需要判断的改写自动化，正是「机械简化」被需求文档明确反对的
//!   （1.3「简单性优先，但不追求机械减少代码」）。
//!
//! 因此 `generate` 从不为了凑数而产出候选：找不到可自动编辑的变换时，返回的
//! 是带说明的手动候选，而不是一份看起来能用的补丁。

use pf_core::finding::{Finding, FindingKind};
use pf_core::ids::{FindingId, RuleId};
use pf_core::location::Location;
use pf_symbol::{Symbol, SymbolIndex};
use serde::{Deserialize, Serialize};

use crate::edit::Edit;

/// 候选对应的简化动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimplificationKind {
    /// 删除项目内没有调用者的对外 API（需人工确认它是否入口点）。
    RemoveUncalledPublicApi,
    /// 内联纯转发包装：删掉包装，让调用方直接调用被转发方。
    InlineForwardingWrapper,
    /// 只给建议，不生成补丁。
    ManualProposal,
}

impl SimplificationKind {
    /// 稳定标签，用于报告分组。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::RemoveUncalledPublicApi => "remove_uncalled_public_api",
            Self::InlineForwardingWrapper => "inline_forwarding_wrapper",
            Self::ManualProposal => "manual_proposal",
        }
    }
}

/// 候选是否能被自动编辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Automation {
    /// 工具生成具体编辑。
    Automatic,
    /// 工具只给结构化说明，补丁由人写。
    Manual,
}

/// 一个简化候选。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimplificationCandidate {
    /// 稳定标识，由规则 ID 与位置派生（重复运行得到同一 ID）。
    pub id: String,
    /// 触发该候选的发现。
    pub finding_id: FindingId,
    /// 触发该候选的规则。
    pub rule_id: RuleId,
    /// 简化动作类型。
    pub kind: SimplificationKind,
    /// 是否可自动编辑。
    pub automation: Automation,
    /// 一句话标题。
    pub title: String,
    /// 目标文件（仓库相对路径）。
    pub path: String,
    /// 具体编辑（仅 [`Automation::Automatic`] 非空）。
    pub edits: Vec<Edit>,
    /// 人工执行时的步骤说明（[`Automation::Manual`] 时为主要内容）。
    pub instructions: Vec<String>,
    /// 残余风险：应用后仍然成立的不确定因素。
    pub risks: Vec<String>,
    /// 关联的需求编号。
    pub requirement_refs: Vec<String>,
}

impl SimplificationCandidate {
    /// 该候选涉及的文件集合。
    ///
    /// 内联一个转发包装通常要改两个文件：定义所在文件与调用方所在文件。
    /// 应用补丁时必须逐个文件处理，否则行号会在错误的文件上失效。
    #[must_use]
    pub fn touched_files(&self) -> Vec<&str> {
        let mut files: Vec<&str> = self.edits.iter().map(|edit| edit.path.as_str()).collect();
        if !self.path.is_empty() {
            files.push(self.path.as_str());
        }
        files.sort_unstable();
        files.dedup();
        files
    }

    /// 是否需要人工确认才能应用。
    ///
    /// 全部候选都需要：FR-055 没有「免确认」分支。该方法存在是为了让调用方
    /// 显式表达这个判断，而不是靠默认值漏掉它。
    #[must_use]
    pub const fn needs_human_confirmation(&self) -> bool {
        true
    }

    /// 报告里用的自动化标签。
    #[must_use]
    pub const fn automation_label(&self) -> &'static str {
        match self.automation {
            Automation::Automatic => "自动编辑",
            Automation::Manual => "仅建议",
        }
    }
}

/// 候选生成器。
///
/// 无状态：同样的 [`pf_rules::Context`] 与发现列表必然给出同样的候选列表，
/// 因此候选本身可以进审计记录、可以跨版本 diff（FR-061、FR-070）。
#[derive(Debug, Default)]
pub struct CandidateGenerator;

impl CandidateGenerator {
    /// 构造生成器。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// 从一次审查的发现里生成简化候选。
    ///
    /// 只处理与「不必要的复杂性」相关的规则；命名、文档、错误处理类发现没有
    /// 对应的结构变换，直接跳过——把它们也算成简化候选只会稀释人工确认的
    /// 注意力。
    #[must_use]
    pub fn generate(
        context: &pf_rules::Context<'_>,
        findings: &[Finding],
    ) -> Vec<SimplificationCandidate> {
        let mut candidates = Vec::new();
        for finding in findings {
            let Some(candidate) = Self::candidate_for(context, finding) else {
                continue;
            };
            candidates.push(candidate);
        }
        // 顺序确定：按文件、再按行号，报告因此可复现。
        candidates.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.id.cmp(&right.id))
        });
        candidates
    }

    /// 为单条发现生成候选；不是简化目标的发现返回 `None`。
    fn candidate_for(
        context: &pf_rules::Context<'_>,
        finding: &Finding,
    ) -> Option<SimplificationCandidate> {
        match finding.rule_id.local_name() {
            "uncalled-public-api" => Self::uncalled_public_api(context, finding),
            "forwarding-wrapper" => Self::forwarding_wrapper(context, finding),
            "single-implementation-trait"
            | "needless-factory"
            | "duplicate-code"
            | "boilerplate-code"
            | "over-configuration"
            | "over-modularization" => Some(Self::manual_proposal(finding)),
            _ => None,
        }
    }

    /// 零调用者的公共 API：删除是可验证的文本变换，但可能删掉对外入口点。
    fn uncalled_public_api(
        context: &pf_rules::Context<'_>,
        finding: &Finding,
    ) -> Option<SimplificationCandidate> {
        let location = finding.primary_location()?;
        let symbol = symbol_at(context.index, location)?;
        let source = context.source(&symbol.file)?;
        let start = location.start_line?;
        let end = location.end_line.unwrap_or(start);
        let (start, end) = expand_deletion_range(&source.content, start, end);
        let edits = vec![Edit::delete(
            symbol.file.clone(),
            start,
            end,
            format!(
                "删除 `{}`：符号索引显示项目内没有任何调用者",
                symbol.qualified_name
            ),
        )];

        Some(SimplificationCandidate {
            id: candidate_id(&finding.rule_id, &symbol.file, start, end),
            finding_id: finding.id.clone(),
            rule_id: finding.rule_id.clone(),
            kind: SimplificationKind::RemoveUncalledPublicApi,
            automation: Automation::Automatic,
            title: finding.title.clone(),
            path: symbol.file.clone(),
            edits,
            instructions: vec![
                format!(
                    "确认 `{}` 是否是对外入口点（库的公共 API、二进制入口、测试夹具）。",
                    symbol.qualified_name
                ),
                "若是对外入口点：保留它，并补一条测试或文档说明它服务于谁。".to_owned(),
                "若是死代码：应用本补丁。".to_owned(),
            ],
            risks: vec![
                "项目外的调用方（下游 crate、脚本、反射式调用）不在符号索引里，删除会破坏它们。"
                    .to_owned(),
            ],
            requirement_refs: finding.requirement_refs.clone(),
        })
    }

    /// 纯转发包装：删除包装并把直接调用点改写为被转发方。
    ///
    /// 只在**每个**调用点都是 `name(...)` 形式的直接调用时才自动编辑；出现
    /// 取地址（`let f = name;`）、方法调用（`obj.name(...)`）或限定路径
    /// （`mod::name(...)`）时退化为手动候选——这些位置的改写需要语义判断。
    fn forwarding_wrapper(
        context: &pf_rules::Context<'_>,
        finding: &Finding,
    ) -> Option<SimplificationCandidate> {
        let location = finding.primary_location()?;
        let symbol = symbol_at(context.index, location)?;
        let source = context.source(&symbol.file)?;
        let forwarded_to = fact(finding, "forwarded_to")?;
        // 只接受裸标识符或被同一文件内的相对路径：跨模块的限定路径改写后
        // 可能编译不过，那不是「文本变换安全」的范畴。
        if forwarded_to.contains("::") || forwarded_to.contains('.') {
            return Some(Self::manual_proposal(finding));
        }
        let target = forwarded_to;

        let definition_start = location.start_line?;
        let definition_end = location.end_line.unwrap_or(definition_start);
        let (delete_start, delete_end) =
            expand_deletion_range(&source.content, definition_start, definition_end);
        let mut edits = vec![Edit::delete(
            symbol.file.clone(),
            delete_start,
            delete_end,
            format!("删除纯转发包装 `{}`", symbol.name),
        )];

        let mut manual_reasons = Vec::new();
        for caller_id in context.call_graph.callers_of(&symbol.id) {
            if *caller_id == symbol.id {
                continue;
            }
            let Some(caller) = context.index.symbols().iter().find(|s| s.id == *caller_id) else {
                continue;
            };
            let Some(caller_source) = context.source(&caller.file) else {
                continue;
            };
            let Some(call_start) = caller.location.start_line else {
                manual_reasons.push(format!("`{caller_id}` 的位置没有行号，无法定位调用点"));
                continue;
            };
            let call_end = caller.location.end_line.unwrap_or(call_start);
            let (call_start, call_end) = (call_start.max(1), call_end.max(call_start));
            let rewritten = rewrite_direct_calls(
                &caller_source.content,
                call_start,
                call_end,
                &symbol.name,
                &target,
            );
            match rewritten {
                Some(lines) => edits.push(Edit::replace(
                    caller.file.clone(),
                    call_start,
                    call_end,
                    lines,
                    format!("把对 `{}` 的直接调用改写为 `{target}`", symbol.name),
                )),
                None => manual_reasons.push(format!(
                    "`{}` 里的调用不是 `{}(...)` 形式的直接调用",
                    caller.file, symbol.name
                )),
            }
        }

        if !manual_reasons.is_empty() {
            let mut candidate = Self::manual_proposal(finding);
            candidate.instructions.extend(
                manual_reasons
                    .into_iter()
                    .map(|reason| format!("需人工处理：{reason}")),
            );
            return Some(candidate);
        }

        Some(SimplificationCandidate {
            id: candidate_id(&finding.rule_id, &symbol.file, delete_start, delete_end),
            finding_id: finding.id.clone(),
            rule_id: finding.rule_id.clone(),
            kind: SimplificationKind::InlineForwardingWrapper,
            automation: Automation::Automatic,
            title: finding.title.clone(),
            path: symbol.file.clone(),
            edits,
            instructions: vec![
                format!(
                    "把调用方对 `{}` 的调用改为直接调用 `{target}`。",
                    symbol.name
                ),
                "确认该包装没有承担未来扩展职责（错误处理、日志、校验）。".to_owned(),
            ],
            risks: vec![
                "改写只覆盖项目内的直接调用点；宏展开、动态派发或跨语言的调用不在符号索引里。"
                    .to_owned(),
            ],
            requirement_refs: finding.requirement_refs.clone(),
        })
    }

    /// 需要语义判断的简化：只产出说明，不产出补丁。
    fn manual_proposal(finding: &Finding) -> SimplificationCandidate {
        let mut instructions = Vec::new();
        if let Some(remediation) = &finding.remediation {
            instructions.push(remediation.summary.clone());
            if let Some(code) = &remediation.suggested_code {
                instructions.push(format!("参考写法：{code}"));
            }
        }
        instructions.push(format!("判断依据：{}", finding.detail));
        instructions.push("改写后必须重新运行测试与性质验证；不要只凭结构相似就合并。".to_owned());

        SimplificationCandidate {
            id: candidate_id(
                &finding.rule_id,
                finding
                    .primary_location()
                    .map_or("", |location| location.path.as_str()),
                finding
                    .primary_location()
                    .and_then(|location| location.start_line)
                    .unwrap_or(1),
                finding
                    .primary_location()
                    .and_then(|location| location.end_line)
                    .unwrap_or(1),
            ),
            finding_id: finding.id.clone(),
            rule_id: finding.rule_id.clone(),
            kind: SimplificationKind::ManualProposal,
            automation: Automation::Manual,
            title: finding.title.clone(),
            path: finding
                .primary_location()
                .map_or_else(String::new, |location| location.path.clone()),
            edits: Vec::new(),
            instructions,
            risks: vec![
                "该变换需要语义判断，工具不生成补丁；不要在没有测试保护的情况下手工套用。"
                    .to_owned(),
            ],
            requirement_refs: finding.requirement_refs.clone(),
        }
    }
}

/// 取发现上的结构事实。
fn fact(finding: &Finding, key: &str) -> Option<String> {
    finding
        .facts
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
}

/// 按位置找符号：先精确匹配字节范围，再退化为包含匹配。
fn symbol_at<'a>(index: &'a SymbolIndex, location: &Location) -> Option<&'a Symbol> {
    index
        .symbols()
        .iter()
        .find(|symbol| symbol.file == location.path && symbol.span == location.span)
        .or_else(|| {
            index.symbols().iter().find(|symbol| {
                symbol.file == location.path
                    && symbol.span.start <= location.span.start
                    && symbol.span.end >= location.span.end
            })
        })
}

/// 把删除范围向上扩展到连续的文档注释与属性行。
///
/// Rust 里 `/// doc` 与 `#[attr]` 是独立节点，只删 `fn` 那几行会留下悬空的
/// 文档注释，编译直接失败。向下再吞掉一个空行，避免删完留下双空行。
fn expand_deletion_range(content: &str, start: u32, end: u32) -> (u32, u32) {
    let lines: Vec<&str> = content.lines().collect();
    let mut start = start.max(1);
    let mut end = end.max(start);
    while start > 1 {
        let above = (start as usize).saturating_sub(2);
        let previous = lines.get(above).copied().unwrap_or("").trim_start();
        if previous.starts_with("///")
            || previous.starts_with("//!")
            || previous.starts_with('#')
            || previous.starts_with('@')
        {
            start -= 1;
        } else {
            break;
        }
    }
    let end_index = usize::try_from(end).unwrap_or(0);
    // 「没有下一行」与「下一行是空行」必须分开：`lines.get` 返回 `None` 时把空串
    // 当空行处理，会把删除范围推到文件外——函数正好在文件末尾结束时必然如此，
    // 于是候选应用失败，整个 simplify 运行以错误告终而不是跳过这一个候选。
    if lines
        .get(end_index)
        .is_some_and(|line| line.trim().is_empty())
    {
        end += 1;
    }
    (start, end)
}

/// 重写一段行范围内的直接调用。
///
/// 返回 `None` 表示这段范围里没有找到 `name(` 形式的直接调用——调用方据此把
/// 候选降级为手动，而不是猜一个改写结果。
fn rewrite_direct_calls(
    content: &str,
    start_line: u32,
    end_line: u32,
    name: &str,
    target: &str,
) -> Option<Vec<String>> {
    let lines: Vec<&str> = content.lines().collect();
    let first = usize::try_from(start_line).ok()?.checked_sub(1)?;
    let last = usize::try_from(end_line).ok()?.min(lines.len());
    if first >= last {
        return None;
    }
    let mut rewritten = Vec::new();
    let mut found = false;
    for line in &lines[first..last] {
        match rewrite_line(line, name, target) {
            Some(new_line) => {
                found = true;
                rewritten.push(new_line);
            }
            None => rewritten.push((*line).to_owned()),
        }
    }
    found.then_some(rewritten)
}

/// 在单行内把 `name(` 改写为 `target(`。
///
/// 只在标识符边界处替换，且要求 `name` 后面紧跟 `(`：`obj.name(x)`、
/// `mod::name(x)`、`let f = name;` 都不会被改写。
fn rewrite_line(line: &str, name: &str, target: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut rest = line;
    let mut changed = false;
    while let Some(index) = rest.find(name) {
        let (before, after) = rest.split_at(index);
        let after_name = &after[name.len()..];
        let boundary_before = before
            .chars()
            .last()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '.'));
        let boundary_after = after_name.trim_start().starts_with('(');
        if boundary_before && boundary_after {
            out.push_str(before);
            out.push_str(target);
            rest = after_name;
            changed = true;
        } else {
            out.push_str(before);
            out.push_str(name);
            rest = after_name;
        }
    }
    out.push_str(rest);
    changed.then_some(out)
}

/// 候选 ID：由规则、文件与行范围派生，因此重复运行得到同一 ID。
fn candidate_id(rule_id: &RuleId, path: &str, start: u32, end: u32) -> String {
    format!("{}:{}:{start}-{end}", rule_id.get(), path)
}

/// 该发现是否属于「不必要的复杂性」这一类（可简化）。
#[must_use]
pub const fn is_simplifiable_kind(kind: &FindingKind) -> bool {
    matches!(
        kind,
        FindingKind::UselessAbstraction
            | FindingKind::ForwardingChain
            | FindingKind::NeedlessPattern
            | FindingKind::DuplicateCode
            | FindingKind::BoilerplateCode
            | FindingKind::MissingTestCoverage
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::location::Span;

    fn finding(
        rule: &str,
        kind: FindingKind,
        title: &str,
        path: &str,
        start: u32,
        end: u32,
    ) -> Finding {
        Finding::new(
            RuleId::builtin(rule),
            kind,
            title,
            "detail",
            vec![Location {
                path: path.to_owned(),
                span: Span::new(0, 10),
                start_line: Some(start),
                start_column: Some(1),
                end_line: Some(end),
                end_column: Some(1),
            }],
        )
    }

    #[test]
    fn a_rewrite_line_only_touches_direct_calls() {
        assert_eq!(
            rewrite_line("    wrapper(a, b)", "wrapper", "real_work").as_deref(),
            Some("    real_work(a, b)")
        );
        assert!(rewrite_line("    let f = wrapper;", "wrapper", "real_work").is_none());
        assert!(rewrite_line("    obj.wrapper(a)", "wrapper", "real_work").is_none());
        assert!(rewrite_line("    mod::wrapper(a)", "wrapper", "real_work").is_none());
        assert!(rewrite_line("    wrapper_x(a)", "wrapper", "real_work").is_none());
    }

    #[test]
    fn the_deletion_range_absorbs_doc_comments_and_attributes() {
        let content = "/// Doc.\n#[inline]\npub fn f() {}\n\nfn next() {}\n";
        let (start, end) = expand_deletion_range(content, 3, 3);
        assert_eq!((start, end), (1, 4));
    }

    #[test]
    fn a_function_ending_on_the_last_line_does_not_extend_past_the_file() {
        // 回归：`lines.get(end)` 在「没有下一行」时返回 `None`，旧代码把空串当空行，
        // 于是删除范围被推到文件外。函数正好在文件末尾结束时必然如此，结果是候选
        // 应用失败、整个 simplify 运行以错误告终，而不是跳过这一个候选。
        let content = "/// Doc.\npub fn alpha() {\n    work();\n}\n";
        assert_eq!(content.lines().count(), 4);
        let (start, end) = expand_deletion_range(content, 2, 4);
        assert_eq!((start, end), (1, 4), "范围不能越过最后一行");
    }

    #[test]
    fn findings_without_a_simplification_shape_are_skipped() {
        // 命名类发现没有结构变换，不能变成候选。
        assert!(!is_simplifiable_kind(&FindingKind::NamingViolation));
        assert!(is_simplifiable_kind(&FindingKind::ForwardingChain));
    }

    #[test]
    fn a_manual_proposal_never_carries_edits() {
        let candidate = CandidateGenerator::manual_proposal(&finding(
            "single-implementation-trait",
            FindingKind::UselessAbstraction,
            "trait `Store` 只有一种实现",
            "src/a.rs",
            3,
            9,
        ));
        assert_eq!(candidate.automation, Automation::Manual);
        assert!(
            candidate.edits.is_empty(),
            "手动候选不得带编辑：{:?}",
            candidate.edits
        );
        assert!(
            !candidate.instructions.is_empty(),
            "手动候选必须给出执行说明"
        );
        assert!(candidate.needs_human_confirmation());
    }

    #[test]
    fn candidate_ids_are_stable_for_the_same_location() {
        let left = candidate_id(&RuleId::builtin("forwarding-wrapper"), "src/a.rs", 3, 5);
        let right = candidate_id(&RuleId::builtin("forwarding-wrapper"), "src/a.rs", 3, 5);
        assert_eq!(left, right);
    }
}
