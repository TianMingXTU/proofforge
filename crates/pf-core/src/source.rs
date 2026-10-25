//! 源代码文件与语言识别。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `ProofForge` 支持的编程语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// Rust。
    Rust,
    /// Python。
    Python,
    /// 其他已知但未深度分析的语言。
    Other,
}

impl Language {
    /// 根据文件扩展名推断语言。
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("rs") => Self::Rust,
            Some("py" | "pyi") => Self::Python,
            _ => Self::Other,
        }
    }

    /// 根据文件名推断语言（用于无扩展名的入口文件）。
    #[must_use]
    pub fn from_file_name(name: &str) -> Self {
        match name {
            "Cargo.toml" => Self::Rust,
            "pyproject.toml" | "setup.py" | "requirements.txt" => Self::Python,
            _ => Self::Other,
        }
    }

    /// 是否具备结构化分析能力（当前仅 Rust 与 Python）。
    #[must_use]
    pub const fn is_analyzable(&self) -> bool {
        matches!(self, Self::Rust | Self::Python)
    }

    /// 语言标识，与 Tree-sitter grammar 名称一致。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Other => "other",
        }
    }

    /// 语言在报告中的展示名。
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::Other => "Other",
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 单个被索引的源文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// 仓库相对路径（`/` 分隔）。
    pub path: String,
    /// 语言。
    pub language: Language,
    /// 文件内容。非 UTF-8 文件不会被索引。
    pub content: String,
    /// 文件字节数（与 `content.len()` 相同，单独保存以避免重复计算）。
    pub size: usize,
    /// 行号索引：每行起始字节偏移，用于快速偏移→行列转换。
    line_starts: Vec<usize>,
}

impl SourceFile {
    /// 从路径与内容构造源文件。
    #[must_use]
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Self {
        let path = path.into();
        let language = Language::from_path(Path::new(&path));
        let content = content.into();
        let size = content.len();
        let line_starts = build_line_starts(&content);
        Self {
            path,
            language,
            content,
            size,
            line_starts,
        }
    }

    /// 文件中的逻辑行数（与 `wc -l` 语义一致：结尾换行符终止最后一行）。
    ///
    /// 空文件为 0 行；`"a"` 与 `"a\n"` 都是 1 行；`"a\nb"` 为 2 行。
    #[must_use]
    pub fn line_count(&self) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let newlines = self.content.bytes().filter(|byte| *byte == b'\n').count();
        if self.content.ends_with('\n') {
            newlines
        } else {
            newlines + 1
        }
    }

    /// 把字节偏移转换为一基的行列号；越界时返回 `None`。
    ///
    /// 行列号按 Unicode 字符而不是字节计，因此中文注释不会让列号错位。
    #[must_use]
    pub fn line_column(&self, offset: usize) -> Option<(u32, u32)> {
        if offset > self.content.len() {
            return None;
        }
        let line_index = match self.line_starts.binary_search(&offset) {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        };
        let line_start = self.line_starts.get(line_index).copied()?;
        let column = self.content[line_start..offset].chars().count();
        let line = u32::try_from(line_index).ok()?.checked_add(1)?;
        let column = u32::try_from(column).ok()?.checked_add(1)?;
        Some((line, column))
    }

    /// 截取一段内容，越界时返回空串。
    #[must_use]
    pub fn slice(&self, start: usize, end: usize) -> &str {
        let end = end.min(self.content.len());
        let start = start.min(end);
        self.content.get(start..end).unwrap_or_default()
    }
}

/// 构建「每行起始偏移」表。
fn build_line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// 一次分析涉及的全部源文件集合。
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    /// 创建空的源文件集合。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 加入一个文件，返回其在集合中的序号。
    pub fn insert(&mut self, file: SourceFile) -> usize {
        self.files.push(file);
        self.files.len() - 1
    }

    /// 按路径查找文件。
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&SourceFile> {
        self.files.iter().find(|file| file.path == path)
    }

    /// 遍历所有文件。
    pub fn iter(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter()
    }

    /// 文件数量。
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 仅返回可结构化分析的文件。
    pub fn analyzable(&self) -> impl Iterator<Item = &SourceFile> {
        self.files
            .iter()
            .filter(|file| file.language.is_analyzable())
    }

    /// 统计每种语言的文件数，用于报告概览。
    #[must_use]
    pub fn language_histogram(&self) -> Vec<(Language, usize)> {
        let mut counts: Vec<(Language, usize)> = Vec::new();
        for file in &self.files {
            match counts.iter_mut().find(|(lang, _)| *lang == file.language) {
                Some((_, count)) => *count += 1,
                None => counts.push((file.language, 1)),
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        counts
    }

    /// 总代码行数。
    #[must_use]
    pub fn total_lines(&self) -> usize {
        self.files.iter().map(SourceFile::line_count).sum()
    }

    /// 总字节数。
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.files.iter().map(|file| file.size).sum()
    }
}

impl<'a> IntoIterator for &'a SourceMap {
    type IntoIter = std::slice::Iter<'a, SourceFile>;
    type Item = &'a SourceFile;

    fn into_iter(self) -> Self::IntoIter {
        self.files.iter()
    }
}

impl FromIterator<SourceFile> for SourceMap {
    fn from_iter<I: IntoIterator<Item = SourceFile>>(iter: I) -> Self {
        Self {
            files: iter.into_iter().collect(),
        }
    }
}

/// 判断路径是否应当被跳过（构建产物、依赖目录等）。
///
/// 该判断只依赖路径字符串，不访问文件系统，因此可被独立测试。
#[must_use]
pub fn is_ignored_path(relative_path: &str) -> bool {
    const IGNORED_DIRS: &[&str] = &[
        "target",
        "node_modules",
        "dist",
        "build",
        "out",
        ".git",
        ".venv",
        "venv",
        "__pycache__",
        ".mypy_cache",
        ".pytest_cache",
        ".ruff_cache",
        ".tox",
        "vendor",
        "third_party",
    ];
    const IGNORED_FILES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
    ];

    if IGNORED_FILES.contains(&relative_path) {
        return true;
    }
    relative_path
        .split('/')
        .any(|segment| IGNORED_DIRS.contains(&segment))
}

/// 把绝对路径转换为仓库相对路径的字符串（统一 `/` 分隔）。
#[must_use]
pub fn to_relative_posix(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut text = relative.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = text.strip_prefix("./") {
        text = stripped.to_owned();
    }
    text
}

/// 从磁盘读取一个源文件，非 UTF-8 时返回 `None`。
///
/// # Errors
/// 当文件不存在或读取失败时返回 [`crate::Error::SourceRead`]。
pub fn read_source_file(path: &Path, root: &Path) -> crate::Result<Option<SourceFile>> {
    let bytes = std::fs::read(path).map_err(|reason| crate::Error::SourceRead {
        path: path.to_owned(),
        reason: reason.to_string(),
    })?;
    let Ok(content) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    Ok(Some(SourceFile::new(
        to_relative_posix(root, path),
        content,
    )))
}

/// 判断是否应把该路径加入索引。
/// 构建清单的文件名。
///
/// 这些文件的语言是 [`Language::Other`]——没有 grammar，因此不会产生语法树，任何
/// 规则都不会在它们上面触发。但它们必须被索引：依赖管理（FR-012）要拿 `Cargo.toml`
/// 里声明的依赖与源码里实际 import 的对一遍，拿不到清单就一条发现都产不出来。
///
/// 只认**文件名**而不是扩展名：`Cargo.toml` 与项目自己的 `proofforge.toml` 扩展名
/// 相同，后者不是构建清单。
const BUILD_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "requirements.txt",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
];

/// 是否是构建清单文件。
#[must_use]
pub fn is_build_manifest(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| BUILD_MANIFESTS.contains(&name))
}

/// 一个文件是否应当被索引进 [`SourceMap`]。
///
/// 可结构化分析的语言（Rust、Python）与构建清单都索引进来：前者产出符号与语法树，
/// 后者只提供文本供依赖管理（FR-012）比对——它的 [`Language`] 是
/// [`Language::Other`]，因此不会生成语法树，任何规则都不会在它上面触发。
#[must_use]
pub fn should_index(path: &Path, root: &Path, max_bytes: usize) -> bool {
    if !path.is_file() {
        return false;
    }
    if !Language::from_path(path).is_analyzable() && !is_build_manifest(path) {
        return false;
    }
    let relative = to_relative_posix(root, path);
    if is_ignored_path(&relative) {
        return false;
    }
    match std::fs::metadata(path) {
        // 文件大小不可能为负，usize 转换在 64 位平台上是恒等的。
        Ok(metadata) => usize::try_from(metadata.len()).is_ok_and(|len| len <= max_bytes),
        Err(_) => false,
    }
}

/// 便捷函数：收集目录下所有可索引的源文件。
///
/// # Errors
/// 目录遍历失败时返回 [`crate::Error::SourceRead`]。
pub fn collect_source_files(root: &Path, max_bytes: usize) -> crate::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|reason| crate::Error::SourceRead {
            path: dir.clone(),
            reason: reason.to_string(),
        })?;
        for entry in entries {
            let path = entry
                .map_err(|reason| crate::Error::SourceRead {
                    path: dir.clone(),
                    reason: reason.to_string(),
                })?
                .path();
            if path.is_dir() {
                stack.push(path);
            } else if should_index(&path, root, max_bytes) {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_from_extension() {
        assert_eq!(
            Language::from_path(Path::new("src/main.rs")),
            Language::Rust
        );
        assert_eq!(Language::from_path(Path::new("a/b.py")), Language::Python);
        assert_eq!(Language::from_path(Path::new("README.md")), Language::Other);
        assert_eq!(Language::from_file_name("Cargo.toml"), Language::Rust);
    }

    #[test]
    fn line_column_mapping() {
        // "fn main() {\n" 占 12 字节，因此第二行起始偏移为 12。
        let file = SourceFile::new("a.rs", "fn main() {\n    let x = 1;\n}\n");
        assert_eq!(file.line_count(), 3);
        assert_eq!(file.line_column(0), Some((1, 1)));
        assert_eq!(file.line_column(3), Some((1, 4)));
        assert_eq!(file.line_column(12), Some((2, 1)));
        // 第二行内容为 "    let x = 1;"，偏移 12+4 落在第一个 'l' 上。
        assert_eq!(file.line_column(16), Some((2, 5)));
        assert_eq!(file.line_column(9999), None);
    }

    #[test]
    fn line_count_follows_wc_semantics() {
        assert_eq!(SourceFile::new("a", "").line_count(), 0);
        assert_eq!(SourceFile::new("a", "x").line_count(), 1);
        assert_eq!(SourceFile::new("a", "x\n").line_count(), 1);
        assert_eq!(SourceFile::new("a", "x\ny").line_count(), 2);
        assert_eq!(SourceFile::new("a", "x\ny\n").line_count(), 2);
    }

    #[test]
    fn ignored_paths_cover_build_output() {
        assert!(is_ignored_path("target/debug/build"));
        assert!(is_ignored_path("node_modules/left-pad/index.js"));
        assert!(is_ignored_path("Cargo.lock"));
        assert!(is_ignored_path("src/__pycache__/x.pyc"));
        assert!(!is_ignored_path("src/main.rs"));
        assert!(!is_ignored_path("crates/pf-core/src/lib.rs"));
    }

    #[test]
    fn source_map_aggregates() {
        let mut map = SourceMap::new();
        map.insert(SourceFile::new("a.rs", "fn a() {}\n"));
        map.insert(SourceFile::new("b.py", "def b():\n    pass\n"));
        map.insert(SourceFile::new("c.md", "# docs\n"));
        assert_eq!(map.len(), 3);
        assert_eq!(map.analyzable().count(), 2);
        assert_eq!(map.language_histogram()[0], (Language::Rust, 1));
        assert_eq!(map.total_lines(), 4);
    }

    #[test]
    fn relative_posix_normalizes() {
        let root = Path::new("/repo");
        assert_eq!(
            to_relative_posix(root, Path::new("/repo/src/main.rs")),
            "src/main.rs"
        );
        assert_eq!(
            to_relative_posix(root, Path::new("/other/x.rs")),
            "/other/x.rs"
        );
    }
}
