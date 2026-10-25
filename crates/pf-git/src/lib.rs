//! Git 仓库读取。
//!
//! 需求文档 FR-001 / FR-002：导入本地 Git 仓库、比较两个 Commit、分支或
//! 工作区变更。
//!
//! Git 对象模型与 pack 解析全部复用 [`gix`]；本 crate 只提供「打开仓库 →
//! 解析引用 → 递归遍历树 → 比较两棵树」这一层薄封装，并对「不是 Git 仓库」
//! 这类常见情况返回可操作的错误。

#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gix::{ObjectId, Repository};

/// Git 操作错误。
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// 路径不是 Git 仓库（也不是其子目录）。
    #[error("not a git repository: {path}")]
    NotARepository {
        /// 尝试打开的路径。
        path: PathBuf,
    },

    /// 仓库已损坏或不被当前 gix 版本支持。
    #[error("failed to open git repository at {path}: {reason}")]
    OpenFailed {
        /// 仓库路径。
        path: PathBuf,
        /// 底层原因。
        reason: String,
    },

    /// 引用无法解析。
    #[error("cannot resolve git revision {revision:?}: {reason}")]
    RevisionNotFound {
        /// 用户提供的引用。
        revision: String,
        /// 底层原因。
        reason: String,
    },

    /// 仓库中没有任何提交。
    #[error("repository at {path} has no commits yet")]
    NoCommits {
        /// 仓库路径。
        path: PathBuf,
    },

    /// Git 对象不存在或类型不符。
    #[error("git object {id} not found or has unexpected type: {reason}")]
    ObjectNotFound {
        /// 对象 ID。
        id: String,
        /// 底层原因。
        reason: String,
    },

    /// Blob 内容不是合法 UTF-8。
    #[error("blob {id} is not valid UTF-8")]
    BlobNotUtf8 {
        /// 对象 ID。
        id: String,
    },
}

/// 结果别名。
pub type Result<T> = std::result::Result<T, GitError>;

/// 一个已打开的 Git 仓库。
#[derive(Debug)]
pub struct GitRepo {
    repo: Repository,
    path: PathBuf,
}

impl GitRepo {
    /// 打开路径处的仓库（自动向上查找 `.git`）。
    ///
    /// # Errors
    /// 路径不是 Git 仓库时返回 [`GitError::NotARepository`] 或
    /// [`GitError::OpenFailed`]。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        match gix::open(&path) {
            Ok(repo) => Ok(Self { repo, path }),
            Err(gix::open::Error::NotARepository { .. }) => Err(GitError::NotARepository { path }),
            Err(reason) => Err(GitError::OpenFailed {
                path,
                reason: reason.to_string(),
            }),
        }
    }

    /// 判断路径是否可以按 Git 仓库打开。
    #[must_use]
    pub fn is_repository(path: impl AsRef<Path>) -> bool {
        gix::open(path.as_ref()).is_ok()
    }

    /// 仓库根目录（包含 `.git` 的目录）。
    #[must_use]
    pub fn workdir(&self) -> Option<&Path> {
        self.repo.workdir()
    }

    /// 仓库路径（打开时传入的路径）。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// HEAD 指向的提交 ID。
    ///
    /// # Errors
    /// 仓库为空或 HEAD 悬空时返回错误。
    pub fn head_id(&self) -> Result<ObjectId> {
        match self.repo.head_id() {
            Ok(id) => Ok(id.detach()),
            Err(reason) => {
                // `git init` 之后 HEAD 已存在但尚未指向任何提交，此时 gix 报的是
                // 「引用不存在」；用 head_name() 区分「空仓库」与「HEAD 损坏」。
                if self.repo.head_name().ok().flatten().is_some() {
                    Err(GitError::NoCommits {
                        path: self.path.clone(),
                    })
                } else {
                    Err(GitError::RevisionNotFound {
                        revision: "HEAD".to_owned(),
                        reason: reason.to_string(),
                    })
                }
            }
        }
    }

    /// 解析任意 Git 引用（分支、tag、`HEAD~2`、完整 SHA 等）。
    ///
    /// # Errors
    /// 引用不存在时返回 [`GitError::RevisionNotFound`]。
    pub fn resolve(&self, revision: &str) -> Result<ObjectId> {
        self.repo
            .rev_parse_single(revision)
            .map(gix::Id::detach)
            .map_err(|reason| GitError::RevisionNotFound {
                revision: revision.to_owned(),
                reason: reason.to_string(),
            })
    }

    /// 取某个提交的树 ID。
    ///
    /// # Errors
    /// 提交不存在时返回 [`GitError::ObjectNotFound`]。
    pub fn tree_of(&self, revision: &ObjectId) -> Result<ObjectId> {
        let commit =
            self.repo
                .find_commit(*revision)
                .map_err(|reason| GitError::ObjectNotFound {
                    id: revision.to_string(),
                    reason: reason.to_string(),
                })?;
        commit
            .tree_id()
            .map(gix::Id::detach)
            .map_err(|reason| GitError::ObjectNotFound {
                id: revision.to_string(),
                reason: reason.to_string(),
            })
    }

    /// 递归列出树中全部文件（路径 → blob ID）。
    ///
    /// 只返回 blob（文件）条目，子目录会被递归展开。目录数量极多时该调用
    /// 的时间与树规模成正比。
    ///
    /// # Errors
    /// 树对象损坏时返回 [`GitError::ObjectNotFound`]。
    pub fn tree_files(&self, tree_id: &ObjectId) -> Result<BTreeMap<String, ObjectId>> {
        let mut files = BTreeMap::new();
        let mut stack = vec![(String::new(), *tree_id)];
        while let Some((prefix, current)) = stack.pop() {
            let tree = self
                .repo
                .find_tree(current)
                .map_err(|reason| GitError::ObjectNotFound {
                    id: current.to_string(),
                    reason: reason.to_string(),
                })?;
            for entry in tree.iter().flatten() {
                let name = entry.filename().to_string();
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };
                let oid = entry.oid().to_owned();
                if entry.mode().is_tree() {
                    stack.push((path, oid));
                } else {
                    files.insert(path, oid);
                }
            }
        }
        Ok(files)
    }

    /// 读取 blob 内容。
    ///
    /// # Errors
    /// 对象不存在时返回 [`GitError::ObjectNotFound`]；内容不是 UTF-8 时返回
    /// [`GitError::BlobNotUtf8`]。
    pub fn read_blob(&self, id: &ObjectId) -> Result<String> {
        let blob = self
            .repo
            .find_blob(*id)
            .map_err(|reason| GitError::ObjectNotFound {
                id: id.to_string(),
                reason: reason.to_string(),
            })?;
        String::from_utf8(blob.data.clone())
            .map_err(|_| GitError::BlobNotUtf8 { id: id.to_string() })
    }

    /// 比较两个引用，返回文件级变更列表。
    ///
    /// 不做重命名检测，重命名表现为一次删除加一次新增；调用方如需识别重命名，
    /// 可按 blob ID 相同来配对。
    ///
    /// # Errors
    /// 引用无法解析或树无法读取时返回相应错误。
    pub fn changed_files(&self, from: &str, to: &str) -> Result<Vec<PathChange>> {
        let from_tree = self.tree_of(&self.resolve(from)?)?;
        let to_tree = self.tree_of(&self.resolve(to)?)?;
        Ok(Self::diff_trees(
            &self.tree_files(&from_tree)?,
            &self.tree_files(&to_tree)?,
        ))
    }

    /// 比较两棵已展开的树，得到变更列表。
    #[must_use]
    pub fn diff_trees(
        from: &BTreeMap<String, ObjectId>,
        to: &BTreeMap<String, ObjectId>,
    ) -> Vec<PathChange> {
        let mut changes = Vec::new();
        for (path, new_id) in to {
            match from.get(path) {
                Some(old_id) if old_id == new_id => {}
                Some(old_id) => changes.push(PathChange {
                    path: path.clone(),
                    old_id: Some(*old_id),
                    new_id: Some(*new_id),
                    kind: ChangeKind::Modified,
                }),
                None => changes.push(PathChange {
                    path: path.clone(),
                    old_id: None,
                    new_id: Some(*new_id),
                    kind: ChangeKind::Added,
                }),
            }
        }
        for (path, old_id) in from {
            if !to.contains_key(path) {
                changes.push(PathChange {
                    path: path.clone(),
                    old_id: Some(*old_id),
                    new_id: None,
                    kind: ChangeKind::Deleted,
                });
            }
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }

    /// 读取某个引用下指定路径的文件内容。
    ///
    /// # Errors
    /// 路径在该引用中不存在时返回 [`GitError::ObjectNotFound`]。
    pub fn read_file_at(&self, revision: &str, path: &str) -> Result<String> {
        let tree_id = self.tree_of(&self.resolve(revision)?)?;
        let files = self.tree_files(&tree_id)?;
        let id = files
            .get(path)
            .copied()
            .ok_or_else(|| GitError::ObjectNotFound {
                id: path.to_owned(),
                reason: "path not present in revision".to_owned(),
            })?;
        self.read_blob(&id)
    }
}

/// 文件变更类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// 新增文件。
    Added,
    /// 内容被修改。
    Modified,
    /// 文件被删除。
    Deleted,
}

impl ChangeKind {
    /// 短标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

impl std::fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 单个文件的变更记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChange {
    /// 仓库相对路径。
    pub path: String,
    /// 变更前的 blob ID（新增时为 `None`）。
    pub old_id: Option<ObjectId>,
    /// 变更后的 blob ID（删除时为 `None`）。
    pub new_id: Option<ObjectId>,
    /// 变更类型。
    pub kind: ChangeKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as ProcessCommand;

    /// 在临时目录中创建一个带两次提交的仓库。
    fn fixture_repo() -> (tempfile::TempDir, GitRepo) {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            let status = ProcessCommand::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .expect("git runs");
            assert!(status.status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").expect("write");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "first"]);
        std::fs::write(dir.path().join("src/b.rs"), "fn b() {}\n").expect("write");
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}\nfn a2() {}\n").expect("write");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "second"]);
        let repo = GitRepo::open(dir.path()).expect("open");
        (dir, repo)
    }

    #[test]
    fn opens_repository_and_finds_head() {
        let (_dir, repo) = fixture_repo();
        let head = repo.head_id().expect("head");
        assert_eq!(repo.resolve("HEAD").expect("resolve"), head);
        assert_ne!(repo.resolve("HEAD~1").expect("resolve~1"), head);
    }

    #[test]
    fn lists_files_in_tree() {
        let (_dir, repo) = fixture_repo();
        let tree = repo.tree_of(&repo.head_id().expect("head")).expect("tree");
        let files = repo.tree_files(&tree).expect("files");
        assert!(files.contains_key("src/a.rs"));
        assert!(files.contains_key("src/b.rs"));
    }

    #[test]
    fn diffs_two_revisions() {
        let (_dir, repo) = fixture_repo();
        let changes = repo.changed_files("HEAD~1", "HEAD").expect("diff");
        let kinds: Vec<(&str, ChangeKind)> = changes
            .iter()
            .map(|change| (change.path.as_str(), change.kind))
            .collect();
        assert!(kinds.contains(&("src/b.rs", ChangeKind::Added)));
        assert!(kinds.contains(&("src/a.rs", ChangeKind::Modified)));
    }

    #[test]
    fn reads_file_at_revision() {
        let (_dir, repo) = fixture_repo();
        let old = repo.read_file_at("HEAD~1", "src/a.rs").expect("read old");
        assert_eq!(old, "fn a() {}\n");
        let new = repo.read_file_at("HEAD", "src/a.rs").expect("read new");
        assert_eq!(new, "fn a() {}\nfn a2() {}\n");
    }

    #[test]
    fn rejects_missing_revision() {
        let (_dir, repo) = fixture_repo();
        let err = repo.resolve("does-not-exist").expect_err("should fail");
        assert!(matches!(err, GitError::RevisionNotFound { .. }));
    }

    #[test]
    fn non_repository_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = GitRepo::open(dir.path()).expect_err("should fail");
        assert!(matches!(err, GitError::NotARepository { .. }));
        assert!(!GitRepo::is_repository(dir.path()));
    }

    #[test]
    fn empty_repository_reports_no_commits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = ProcessCommand::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .output()
            .expect("git init");
        assert!(status.status.success());
        let repo = GitRepo::open(dir.path()).expect("open");
        let err = repo.head_id().expect_err("no commits yet");
        assert!(matches!(err, GitError::NoCommits { .. }));
    }

    #[test]
    fn diff_trees_reports_deletions() {
        let mut from = BTreeMap::new();
        from.insert("a.rs".to_owned(), ObjectId::null(gix::hash::Kind::Sha1));
        let to = BTreeMap::new();
        let changes = GitRepo::diff_trees(&from, &to);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Deleted);
    }
}
