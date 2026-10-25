//! 代码定位信息。
//!
//! 定位使用「一字节基的起止偏移 + 行列号」双表示：偏移用于与 Tree-sitter
//! 直接对齐，行列号用于渲染人类可读的报告。

use serde::{Deserialize, Serialize};

/// 源码中的一段范围（字节偏移为左闭右开区间）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Span {
    /// 起始字节偏移（含）。
    pub start: usize,
    /// 结束字节偏移（不含）。
    pub end: usize,
}

impl Span {
    /// 创建新范围。
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// 范围覆盖的字节数。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// 范围是否为空。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// 是否包含给定字节偏移。
    #[must_use]
    pub const fn contains(&self, offset: usize) -> bool {
        offset >= self.start && offset < self.end
    }

    /// 与另一范围的交集；不相交时返回 `None`。
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Option<Self> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        (start < end).then_some(Self::new(start, end))
    }

    /// 覆盖自身与另一范围的最小范围。
    #[must_use]
    pub fn cover(&self, other: &Self) -> Self {
        Self::new(self.start.min(other.start), self.end.max(other.end))
    }

    /// 从指定文本中截取范围对应的子串，越界时尽量返回安全结果。
    #[must_use]
    pub fn slice_of<'a>(&self, text: &'a str) -> &'a str {
        let end = self.end.min(text.len());
        let start = self.start.min(end);
        text.get(start..end).unwrap_or_default()
    }
}

/// 带文件路径的代码位置，是报告与证据的最小可定位单元。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    /// 仓库相对路径，统一使用 `/` 作为分隔符。
    pub path: String,
    /// 范围内的字节偏移。
    pub span: Span,
    /// 起始行号（一基）。无法确定时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    /// 起始列号（一基，按字符计）。无法确定时为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_column: Option<u32>,
    /// 结束行号（一基）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    /// 结束列号（一基，按字符计）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
}

impl Location {
    /// 仅使用路径与字节范围构造位置。
    #[must_use]
    pub const fn new(path: String, span: Span) -> Self {
        Self {
            path,
            span,
            start_line: None,
            start_column: None,
            end_line: None,
            end_column: None,
        }
    }
    /// 补全行列号后返回新值（构建器风格，便于链式调用）。
    #[must_use]
    pub const fn with_lines(
        mut self,
        start_line: u32,
        start_column: u32,
        end_line: u32,
        end_column: u32,
    ) -> Self {
        self.start_line = Some(start_line);
        self.start_column = Some(start_column);
        self.end_line = Some(end_line);
        self.end_column = Some(end_column);
        self
    }

    /// 渲染为 `path:line:column` 形式，缺少行列时退化为 `path:start-end`。
    #[must_use]
    pub fn display(&self) -> String {
        match (self.start_line, self.start_column) {
            (Some(line), Some(column)) => format!("{}:{}:{}", self.path, line, column),
            _ => format!("{}:{}-{}", self.path, self.span.start, self.span.end),
        }
    }
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_len_and_emptiness() {
        let span = Span::new(3, 10);
        assert_eq!(span.len(), 7);
        assert!(!span.is_empty());
        assert!(Span::new(4, 4).is_empty());
    }

    #[test]
    fn span_contains_is_half_open() {
        let span = Span::new(0, 5);
        assert!(span.contains(0));
        assert!(span.contains(4));
        assert!(!span.contains(5));
    }

    #[test]
    fn span_intersection() {
        let a = Span::new(0, 10);
        let b = Span::new(5, 20);
        assert_eq!(a.intersect(&b), Some(Span::new(5, 10)));
        assert_eq!(a.intersect(&Span::new(10, 20)), None);
    }

    #[test]
    fn span_cover_takes_union() {
        assert_eq!(Span::new(5, 8).cover(&Span::new(1, 6)), Span::new(1, 8));
    }

    #[test]
    fn slice_of_is_bounds_safe() {
        assert_eq!(Span::new(0, 5).slice_of("hello world"), "hello");
        assert_eq!(Span::new(7, 100).slice_of("hello"), "");
    }

    #[test]
    fn location_display_prefers_line_column() {
        let loc = Location::new("src/main.rs".to_owned(), Span::new(0, 4)).with_lines(12, 5, 12, 9);
        assert_eq!(loc.display(), "src/main.rs:12:5");
        assert_eq!(loc.to_string(), "src/main.rs:12:5");
    }

    #[test]
    fn location_display_falls_back_to_bytes() {
        let loc = Location::new("src/lib.rs".to_owned(), Span::new(10, 42));
        assert_eq!(loc.display(), "src/lib.rs:10-42");
    }
}
