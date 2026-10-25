//! 行级文本编辑与统一差异（unified diff）渲染。
//!
//! # 为什么是「行级」而不是 AST 级
//!
//! 简化补丁最终要落在文件上，而人是通过 diff 审查它的。行级编辑有三个好处：
//! 边界可验证（不允许重叠、不允许越界）、可渲染成人能读的 diff、可反向应用。
//! AST 级重写更「正确」，但它要求我们在没有类型信息的情况下判断语义等价——
//! 那正是本工具**不敢**替用户决定的事，因此编辑只做「能验证的文本变换」，
//! 语义等价交给 [`crate::behavior`] 的行为比较去证伪。
//!
//! # 为什么编辑必须不重叠
//!
//! 重叠编辑的应用顺序会改变结果，同一份补丁因此不再可复现（违反 FR-043 的
//! 精神：同样输入给同样输出）。这里直接拒绝重叠，让调用方先把编辑合并好。

use serde::{Deserialize, Serialize};

/// 一次行级编辑。
///
/// 行号一基、闭区间：`start_line=3, end_line=5` 表示替换第 3 到第 5 行。
/// `replacement` 为空表示纯删除。`path` 是编辑所属的文件，因为一次简化可能
/// 同时改定义文件与若干调用方文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    /// 编辑所属文件（仓库相对路径）。
    pub path: String,
    /// 起始行（一基，含）。
    pub start_line: u32,
    /// 结束行（一基，含）。
    pub end_line: u32,
    /// 替换后的新行内容（不含换行符）。
    pub replacement: Vec<String>,
    /// 为什么做这个编辑，进补丁说明与审计记录。
    pub rationale: String,
}

impl Edit {
    /// 构造一次替换编辑。
    #[must_use]
    pub fn replace(
        path: impl Into<String>,
        start_line: u32,
        end_line: u32,
        replacement: Vec<String>,
        rationale: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            start_line,
            end_line,
            replacement,
            rationale: rationale.into(),
        }
    }

    /// 构造一次删除编辑。
    #[must_use]
    pub fn delete(
        path: impl Into<String>,
        start_line: u32,
        end_line: u32,
        rationale: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            start_line,
            end_line,
            replacement: Vec::new(),
            rationale: rationale.into(),
        }
    }

    /// 被编辑覆盖的行数。
    #[must_use]
    pub const fn covered_lines(&self) -> u32 {
        self.end_line - self.start_line + 1
    }
}

/// 从一组编辑里选出属于某个文件的那些。
#[must_use]
pub fn edits_for<'a>(path: &str, edits: &'a [Edit]) -> Vec<&'a Edit> {
    edits.iter().filter(|edit| edit.path == path).collect()
}

/// 编辑应用失败的原因。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditError {
    /// 编辑范围越界（文件没有这么多行）。
    #[error("edit range {start_line}..{end_line} exceeds file length {total_lines} in `{path}`")]
    OutOfRange {
        /// 文件路径。
        path: String,
        /// 起始行。
        start_line: u32,
        /// 结束行。
        end_line: u32,
        /// 文件总行数。
        total_lines: usize,
    },
    /// 两个编辑范围重叠。
    #[error("edits overlap in `{path}`: {first_start}..{first_end} overlaps {second_start}..{second_end}")]
    Overlapping {
        /// 文件路径。
        path: String,
        /// 前一个编辑的起始行。
        first_start: u32,
        /// 前一个编辑的结束行。
        first_end: u32,
        /// 后一个编辑的起始行。
        second_start: u32,
        /// 后一个编辑的结束行。
        second_end: u32,
    },
}

/// 把属于 `path` 的编辑应用到该文件的文本上。
///
/// 只应用 [`edits_for`] 选出的编辑，因此调用方可以把一份跨文件的候选整体
/// 传进来而不用自己分组。编辑按起始行**从后往前**应用，所以前面的行号不会
/// 因后面的插入/删除而失效。
///
/// # Errors
/// 范围越界或范围重叠时返回 [`EditError`]，此时文本不会被部分修改——
/// 全部校验先于任何写入。
pub fn apply_edits(text: &str, path: &str, edits: &[Edit]) -> Result<String, EditError> {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();

    let mut ordered = edits_for(path, edits);
    ordered.sort_by_key(|edit| edit.start_line);

    for window in ordered.windows(2) {
        let first = window[0];
        let second = window[1];
        if second.start_line <= first.end_line {
            return Err(EditError::Overlapping {
                path: first.path.clone(),
                first_start: first.start_line,
                first_end: first.end_line,
                second_start: second.start_line,
                second_end: second.end_line,
            });
        }
    }
    for edit in &ordered {
        let last = usize::try_from(edit.end_line).unwrap_or(usize::MAX);
        if last > total || edit.start_line == 0 {
            return Err(EditError::OutOfRange {
                path: edit.path.clone(),
                start_line: edit.start_line,
                end_line: edit.end_line,
                total_lines: total,
            });
        }
    }

    // 从后往前应用：先处理行号大的编辑，前面的行号因此保持有效。
    let mut result: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    for edit in ordered.iter().rev() {
        let start = usize::try_from(edit.start_line).unwrap_or(usize::MAX);
        let end = usize::try_from(edit.end_line).unwrap_or(usize::MAX);
        let tail = result.split_off(end.min(result.len()));
        result.truncate(start.saturating_sub(1));
        result.extend(edit.replacement.iter().cloned());
        result.extend(tail);
    }

    let mut out = result.join("\n");
    if text.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// 渲染最小统一差异（三行上下文）。
///
/// 刻意不引入 `similar` 之类的差异 crate：这里的差异只需要「人能审查、能反向
/// 应用」，Myers 最优性没有价值，而一个依赖换来的是一段更难审计的代码。
/// 逐段对齐（公共前缀 + 公共后缀）足够，并且对相同输入给出相同输出。
#[must_use]
pub fn unified_diff(path: &str, before: &str, after: &str) -> String {
    const CONTEXT: usize = 3;
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();

    let prefix = old
        .iter()
        .zip(new.iter())
        .take_while(|(left, right)| left == right)
        .count();
    if prefix == old.len() && prefix == new.len() {
        return String::new();
    }
    let suffix = old
        .iter()
        .skip(prefix)
        .rev()
        .zip(new.iter().skip(prefix).rev())
        .take_while(|(left, right)| left == right)
        .count();

    let old_start = prefix;
    let old_end = old.len() - suffix;
    let new_start = prefix;
    let new_end = new.len() - suffix;
    let context_start = old_start.saturating_sub(CONTEXT);
    let context_old_end = (old_end + CONTEXT).min(old.len());

    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        context_start + 1,
        context_old_end - context_start,
        context_start + 1,
        (new_end + CONTEXT).min(new.len()) - context_start
    ));
    for line in &old[context_start..old_start] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in &old[old_start..old_end] {
        out.push_str(&format!("-{line}\n"));
    }
    for line in &new[new_start..new_end] {
        out.push_str(&format!("+{line}\n"));
    }
    for line in &old[old_end..context_old_end] {
        out.push_str(&format!(" {line}\n"));
    }
    out
}

/// 反向应用一份差异中的删除/插入，用于生成回滚补丁。
///
/// 简化管线的回滚不需要通用 patch 引擎：候选补丁一定来自 [`apply_edits`]，
/// 因此反向补丁就是「交换 before/after」。这里显式提供该函数，避免调用方
/// 自己去拼字符串而漏掉路径前缀。
#[must_use]
pub fn reverse_diff(path: &str, before: &str, after: &str) -> String {
    unified_diff(path, after, before)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(start: u32, end: u32, replacement: Vec<String>) -> Edit {
        Edit {
            path: "f.rs".to_owned(),
            start_line: start,
            end_line: end,
            replacement,
            rationale: "test".to_owned(),
        }
    }

    #[test]
    fn deleting_a_line_range_removes_exactly_those_lines() {
        let text = "one\ntwo\nthree\nfour\n";
        let out = apply_edits(text, "f.rs", &[edit(2, 3, Vec::new())]).expect("apply");
        assert_eq!(out, "one\nfour\n");
    }

    #[test]
    fn replacing_a_line_range_inserts_the_replacement() {
        let text = "a\nb\nc\n";
        let out = apply_edits(
            text,
            "f.rs",
            &[edit(2, 2, vec!["B1".to_owned(), "B2".to_owned()])],
        )
        .expect("apply");
        assert_eq!(out, "a\nB1\nB2\nc\n");
    }

    #[test]
    fn edits_are_applied_back_to_front_so_line_numbers_stay_valid() {
        let text = "l1\nl2\nl3\nl4\nl5\n";
        let out = apply_edits(
            text,
            "f.rs",
            &[
                edit(1, 1, vec!["L1".to_owned()]),
                edit(5, 5, vec!["L5".to_owned()]),
            ],
        )
        .expect("apply");
        assert_eq!(out, "L1\nl2\nl3\nl4\nL5\n");
    }

    #[test]
    fn only_edits_for_the_given_file_are_applied() {
        let text = "a\nb\n";
        let other = Edit::delete("other.rs", 1, 1, "not this file");
        let out =
            apply_edits(text, "f.rs", &[other, edit(2, 2, vec!["B".to_owned()])]).expect("apply");
        assert_eq!(out, "a\nB\n");
    }

    #[test]
    fn overlapping_edits_are_rejected_before_any_change() {
        let text = "a\nb\nc\n";
        let error = apply_edits(
            text,
            "f.rs",
            &[edit(1, 2, Vec::new()), edit(2, 3, Vec::new())],
        )
        .expect_err("overlap");
        assert!(matches!(error, EditError::Overlapping { .. }));
    }

    #[test]
    fn out_of_range_edits_are_rejected() {
        let text = "a\nb\n";
        let error = apply_edits(text, "f.rs", &[edit(9, 10, Vec::new())]).expect_err("range");
        assert!(matches!(error, EditError::OutOfRange { .. }));
    }

    #[test]
    fn identical_text_produces_an_empty_diff() {
        assert_eq!(unified_diff("f.rs", "a\nb\n", "a\nb\n"), "");
    }

    #[test]
    fn a_diff_shows_the_changed_lines_with_context() {
        let diff = unified_diff("f.rs", "a\nb\nc\nd\ne\n", "a\nb\nC\nd\ne\n");
        assert!(diff.contains("--- a/f.rs"));
        assert!(diff.contains("+++ b/f.rs"));
        assert!(diff.contains("-c"));
        assert!(diff.contains("+C"));
        assert!(diff.contains(" a"), "差异应带上下文行：{diff}");
    }

    #[test]
    fn a_reverse_diff_restores_the_original() {
        // 反向补丁的语义：把 after 变回 before。
        let reverse = reverse_diff("f.rs", "a\nb\n", "a\nB\n");
        assert!(reverse.contains("-B"));
        assert!(reverse.contains("+b"));
    }
}
