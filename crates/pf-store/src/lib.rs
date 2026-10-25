//! 本地结果存储。
//!
//! 需求文档 FR-070、第十章：任务、规则、工具、模型、输入版本与关键证据的
//! 审计追踪；FR-061 的规则集版本历史与 FR-069 的质量趋势查询。
//!
//! 存储选型是 SQLite + SQLx（需求文档 6.2）：本地单机场景无需额外服务进程，
//! 同时保留迁移到 PostgreSQL 的可能。本 crate 只提供「建表 + 写入 + 查询」
//! 的最小集合，不为尚未出现的查询需求提前设计抽象层（需求文档 11.4）。

#![warn(missing_docs)]

use std::path::Path;

use pf_core::evidence::EvidenceArtifact;
use pf_core::ids::{Identifier, RunId};
use pf_core::review::ReviewReport;
use pf_core::status::GateStatus;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Executor;

/// 存储错误。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// 无法打开或创建数据库。
    #[error("failed to open database {path}: {reason}")]
    OpenFailed {
        /// 数据库路径。
        path: String,
        /// 底层原因。
        reason: String,
    },

    /// 迁移（建表）失败。
    #[error("database migration failed: {reason}")]
    MigrationFailed {
        /// 底层原因。
        reason: String,
    },

    /// 查询或写入失败。
    #[error("query failed: {reason}")]
    QueryFailed {
        /// 底层原因。
        reason: String,
    },
}

/// 结果别名。
pub type Result<T> = std::result::Result<T, StoreError>;

/// 一次运行的历史记录摘要。
#[derive(Debug, Clone)]
pub struct RunSummary {
    /// 运行 ID。
    pub run_id: RunId,
    /// 项目根目录。
    pub project_root: String,
    /// 代码版本。
    pub revision: String,
    /// 门禁结论。
    pub gate: GateStatus,
    /// 发现数量。
    pub finding_count: i64,
}

/// 质量趋势上的一个点（FR-069）。
///
/// 每个点对应一次运行，按时间正序返回。带 `exempted_count` 与 `blocking_count`
/// 而不是只有 `finding_count`：「发现数下降」可能只是因为豁免变多了——这两个
/// 数字放在一起才看得出质量到底在往哪边走。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrendPoint {
    /// 运行 ID。
    pub run_id: RunId,
    /// 写入时间（SQLite `datetime('now')`，UTC）。
    pub created_at: String,
    /// 门禁结论。
    pub gate: GateStatus,
    /// 发现数量。
    pub finding_count: i64,
    /// 其中已被豁免压制的数量。
    pub exempted_count: i64,
    /// 其中达到阻断阈值且未被豁免的数量。
    pub blocking_count: i64,
    /// 分析到的文件数。
    pub file_count: i64,
    /// 本次运行使用的规则集版本（FR-061）。
    pub ruleset_version: String,
}

/// 趋势查询的一行原始结果。
///
/// 提成别名而不是让八元组散在函数签名里：列的顺序变化时，只有这一处需要改。
type TrendRow = (String, String, String, i64, i64, i64, i64, String);

/// 一个规则集版本在历史里出现的情况（FR-061）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesetVersionRecord {
    /// 规则集版本。
    pub version: String,
    /// 使用该版本的运行次数。
    pub run_count: i64,
    /// 第一次使用的时间。
    pub first_seen: String,
    /// 最近一次使用的时间。
    pub last_seen: String,
}

/// SQLite 存储。
#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// 打开（必要时创建）数据库并执行迁移。
    ///
    /// # Errors
    /// 数据库无法打开或建表失败时返回 [`StoreError`]。
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|reason| StoreError::OpenFailed {
                path: path.display().to_string(),
                reason: reason.to_string(),
            })?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// 创建内存数据库（测试用）。
    ///
    /// # Errors
    /// 与 [`Self::open`] 相同。
    pub async fn in_memory() -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .map_err(|reason| StoreError::OpenFailed {
                path: ":memory:".to_owned(),
                reason: reason.to_string(),
            })?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// 建表（幂等）。
    ///
    /// 升级路径：早期版本的 `runs` 表没有 `exempted_count` / `blocking_count`
    /// 两列。`CREATE TABLE IF NOT EXISTS` 不会给已存在的表加列，因此这里显式
    /// 检查并 `ALTER TABLE ADD COLUMN`。少了这一步，老数据库上的趋势查询会直接
    /// 报「no such column」——那会让用户以为历史数据丢了。
    async fn migrate(&self) -> Result<()> {
        self.pool
            .execute(
                r"
                CREATE TABLE IF NOT EXISTS runs (
                    run_id         TEXT PRIMARY KEY,
                    project_root   TEXT NOT NULL,
                    revision       TEXT NOT NULL,
                    gate           TEXT NOT NULL,
                    file_count     INTEGER NOT NULL,
                    symbol_count   INTEGER NOT NULL,
                    finding_count  INTEGER NOT NULL,
                    exempted_count INTEGER NOT NULL DEFAULT 0,
                    blocking_count INTEGER NOT NULL DEFAULT 0,
                    ruleset_version TEXT NOT NULL,
                    duration_ms    INTEGER NOT NULL,
                    created_at     TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS findings (
                    finding_id  TEXT PRIMARY KEY,
                    run_id      TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                    rule_id     TEXT NOT NULL,
                    kind        TEXT NOT NULL,
                    severity    TEXT NOT NULL,
                    status      TEXT NOT NULL,
                    exempted    INTEGER NOT NULL DEFAULT 0,
                    title       TEXT NOT NULL,
                    detail      TEXT NOT NULL,
                    locations   TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_findings_run ON findings(run_id);

                CREATE TABLE IF NOT EXISTS evidence (
                    sha256     TEXT PRIMARY KEY,
                    run_id     TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                    kind       TEXT NOT NULL,
                    content    TEXT NOT NULL,
                    source     TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_evidence_run ON evidence(run_id);
                CREATE INDEX IF NOT EXISTS idx_runs_project ON runs(project_root, created_at);
                ",
            )
            .await
            .map_err(|reason| StoreError::MigrationFailed {
                reason: reason.to_string(),
            })?;
        self.add_missing_columns().await
    }

    /// 为旧版数据库补上新增的列。
    async fn add_missing_columns(&self) -> Result<()> {
        for (table, column, definition) in [
            ("runs", "exempted_count", "INTEGER NOT NULL DEFAULT 0"),
            ("runs", "blocking_count", "INTEGER NOT NULL DEFAULT 0"),
            ("findings", "exempted", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            if self.has_column(table, column).await? {
                continue;
            }
            // 表名与列名来自上面的常量数组，不来自任何外部输入，因此不存在
            // SQL 注入面。
            let statement = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
            self.pool
                .execute(statement.as_str())
                .await
                .map_err(|reason| StoreError::MigrationFailed {
                    reason: format!("cannot add column {table}.{column}: {reason}"),
                })?;
        }
        Ok(())
    }

    /// 判断表里是否已有某一列。
    async fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info(?1)")
            .bind(table)
            .fetch_all(&self.pool)
            .await
            .map_err(|reason| StoreError::MigrationFailed {
                reason: format!("cannot inspect table {table}: {reason}"),
            })?;
        Ok(rows.iter().any(|(name,)| name == column))
    }

    /// 写入一次审查运行及其发现与证据。
    ///
    /// # Errors
    /// 写入失败时返回 [`StoreError::QueryFailed`]。
    pub async fn save_run(&self, report: &ReviewReport) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|reason| StoreError::QueryFailed {
                reason: reason.to_string(),
            })?;

        sqlx::query(
            r"
            INSERT OR REPLACE INTO runs
                (run_id, project_root, revision, gate, file_count, symbol_count,
                 finding_count, exempted_count, blocking_count, ruleset_version, duration_ms)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ",
        )
        .bind(report.run_id.to_string())
        .bind(report.project_root.display().to_string())
        .bind(report.revision.clone())
        .bind(report.gate.as_str())
        .bind(i64::try_from(report.file_count).unwrap_or(i64::MAX))
        .bind(i64::try_from(report.symbol_count).unwrap_or(i64::MAX))
        .bind(i64::try_from(report.findings.len()).unwrap_or(i64::MAX))
        .bind(i64::try_from(report.exempted_count()).unwrap_or(i64::MAX))
        .bind(i64::try_from(report.blocking_findings().count()).unwrap_or(i64::MAX))
        .bind(report.ruleset_version.clone())
        .bind(i64::try_from(report.duration.as_millis()).unwrap_or(i64::MAX))
        .execute(&mut *tx)
        .await
        .map_err(|reason| StoreError::QueryFailed {
            reason: reason.to_string(),
        })?;

        for finding in &report.findings {
            let locations = serde_json::to_string(&finding.locations).map_err(|reason| {
                StoreError::QueryFailed {
                    reason: reason.to_string(),
                }
            })?;
            sqlx::query(
                r"
                INSERT OR REPLACE INTO findings
                    (finding_id, run_id, rule_id, kind, severity, status, exempted,
                     title, detail, locations)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                ",
            )
            .bind(finding.id.to_string())
            .bind(report.run_id.to_string())
            .bind(finding.rule_id.to_string())
            .bind(finding.kind.label())
            .bind(finding.severity.as_str())
            .bind(finding.status.as_str())
            .bind(i64::from(finding.is_exempted()))
            .bind(finding.title.clone())
            .bind(finding.detail.clone())
            .bind(locations)
            .execute(&mut *tx)
            .await
            .map_err(|reason| StoreError::QueryFailed {
                reason: reason.to_string(),
            })?;
        }

        for artifact in &report.evidence {
            self.save_evidence_in_tx(&mut tx, &report.run_id, artifact)
                .await?;
        }

        tx.commit()
            .await
            .map_err(|reason| StoreError::QueryFailed {
                reason: reason.to_string(),
            })?;
        Ok(())
    }

    /// 在既有事务中写入一条证据（内容寻址，重复内容只存一份）。
    async fn save_evidence_in_tx(
        &self,
        tx: &mut sqlx::SqliteConnection,
        run_id: &RunId,
        artifact: &EvidenceArtifact,
    ) -> Result<()> {
        sqlx::query(
            r"
            INSERT OR IGNORE INTO evidence (sha256, run_id, kind, content, source)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ",
        )
        .bind(artifact.sha256.clone())
        .bind(run_id.to_string())
        .bind(artifact.kind.clone())
        .bind(artifact.content.clone())
        .bind(artifact.source.clone())
        .execute(&mut *tx)
        .await
        .map_err(|reason| StoreError::QueryFailed {
            reason: reason.to_string(),
        })?;
        Ok(())
    }

    /// 列出最近的运行记录。
    ///
    /// # Errors
    /// 查询失败时返回 [`StoreError::QueryFailed`]。
    pub async fn recent_runs(&self, limit: i64) -> Result<Vec<RunSummary>> {
        let rows: Vec<(String, String, String, String, i64)> = sqlx::query_as(
            r"
            SELECT run_id, project_root, revision, gate, finding_count
            FROM runs
            ORDER BY created_at DESC, run_id DESC
            LIMIT ?1
            ",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|reason| StoreError::QueryFailed {
            reason: reason.to_string(),
        })?;

        rows.into_iter()
            .map(|(run_id, project_root, revision, gate, finding_count)| {
                Ok(RunSummary {
                    run_id: RunId::parse(&run_id).map_err(|_| StoreError::QueryFailed {
                        reason: format!("corrupt run_id in database: {run_id}"),
                    })?,
                    project_root,
                    revision,
                    gate: parse_gate(&gate)?,
                    finding_count,
                })
            })
            .collect()
    }

    /// 查询某次运行的发现数量。
    ///
    /// # Errors
    /// 查询失败时返回 [`StoreError::QueryFailed`]。
    pub async fn finding_count(&self, run_id: &RunId) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM findings WHERE run_id = ?1")
            .bind(run_id.to_string())
            .fetch_one(&self.pool)
            .await
            .map_err(|reason| StoreError::QueryFailed {
                reason: reason.to_string(),
            })?;
        Ok(count)
    }

    /// 查询某个项目的质量趋势（FR-069）。
    ///
    /// 按时间正序返回最近 `limit` 次运行。趋势要能回答「质量在变好还是变坏」，
    /// 因此每个点同时带发现数、豁免数与阻断数——只看发现数会被「豁免变多」
    /// 误导（FR-062）。
    ///
    /// # Errors
    /// 查询失败或库里有损坏的门禁状态时返回 [`StoreError::QueryFailed`]。
    pub async fn trend(&self, project_root: &str, limit: i64) -> Result<Vec<TrendPoint>> {
        let rows: Vec<TrendRow> = sqlx::query_as(
            r"
            SELECT run_id, created_at, gate, finding_count, exempted_count,
                   blocking_count, file_count, ruleset_version
            FROM runs
            WHERE project_root = ?1
            ORDER BY created_at ASC, run_id ASC
            LIMIT ?2
            ",
        )
        .bind(project_root)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|reason| StoreError::QueryFailed {
            reason: reason.to_string(),
        })?;

        rows.into_iter()
            .map(
                |(
                    run_id,
                    created_at,
                    gate,
                    finding_count,
                    exempted_count,
                    blocking_count,
                    file_count,
                    ruleset_version,
                )| {
                    Ok(TrendPoint {
                        run_id: RunId::parse(&run_id).map_err(|_| StoreError::QueryFailed {
                            reason: format!("corrupt run_id in database: {run_id}"),
                        })?,
                        created_at,
                        gate: parse_gate(&gate)?,
                        finding_count,
                        exempted_count,
                        blocking_count,
                        file_count,
                        ruleset_version,
                    })
                },
            )
            .collect()
    }

    /// 查询规则集版本的使用历史（FR-061）。
    ///
    /// 每次审查记录规则集版本，目的是让「这条结论是哪一版规则给出的」可追溯。
    /// 版本变更与发现数变化放在一起看，才能判断一次下降是代码变好了，还是规则
    /// 被关掉了。
    ///
    /// # Errors
    /// 查询失败时返回 [`StoreError::QueryFailed`]。
    pub async fn ruleset_history(&self, project_root: &str) -> Result<Vec<RulesetVersionRecord>> {
        let rows: Vec<(String, i64, String, String)> = sqlx::query_as(
            r"
            SELECT ruleset_version, COUNT(*) AS runs, MIN(created_at), MAX(created_at)
            FROM runs
            WHERE project_root = ?1
            GROUP BY ruleset_version
            ORDER BY MIN(created_at) ASC, ruleset_version ASC
            ",
        )
        .bind(project_root)
        .fetch_all(&self.pool)
        .await
        .map_err(|reason| StoreError::QueryFailed {
            reason: reason.to_string(),
        })?;

        Ok(rows
            .into_iter()
            .map(
                |(version, run_count, first_seen, last_seen)| RulesetVersionRecord {
                    version,
                    run_count,
                    first_seen,
                    last_seen,
                },
            )
            .collect())
    }

    /// 统计某次运行里被豁免压制的发现数量。
    ///
    /// # Errors
    /// 查询失败时返回 [`StoreError::QueryFailed`]。
    pub async fn exempted_count(&self, run_id: &RunId) -> Result<i64> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM findings WHERE run_id = ?1 AND exempted = 1")
                .bind(run_id.to_string())
                .fetch_one(&self.pool)
                .await
                .map_err(|reason| StoreError::QueryFailed {
                    reason: reason.to_string(),
                })?;
        Ok(count)
    }

    /// 校验某条证据内容是否与数据库记录一致。
    ///
    /// # Errors
    /// 查询失败时返回 [`StoreError::QueryFailed`]。
    pub async fn verify_evidence(&self, sha256: &str) -> Result<bool> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT content FROM evidence WHERE sha256 = ?1")
                .bind(sha256)
                .fetch_optional(&self.pool)
                .await
                .map_err(|reason| StoreError::QueryFailed {
                    reason: reason.to_string(),
                })?;
        Ok(row.is_some_and(|(content,)| pf_core::sha256_hex(content.as_bytes()) == sha256))
    }

    /// 删除指定运行及其关联数据。
    ///
    /// # Errors
    /// 删除失败时返回 [`StoreError::QueryFailed`]。
    pub async fn delete_run(&self, run_id: &RunId) -> Result<()> {
        sqlx::query("DELETE FROM runs WHERE run_id = ?1")
            .bind(run_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(|reason| StoreError::QueryFailed {
                reason: reason.to_string(),
            })?;
        Ok(())
    }
}

/// 解析数据库中存储的门禁状态。
fn parse_gate(value: &str) -> Result<GateStatus> {
    match value {
        "passed" => Ok(GateStatus::Passed),
        "failed" => Ok(GateStatus::Failed),
        "insufficient_evidence" => Ok(GateStatus::InsufficientEvidence),
        "cancelled" => Ok(GateStatus::Cancelled),
        other => Err(StoreError::QueryFailed {
            reason: format!("unknown gate value in database: {other}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::ids::{ProjectId, RuleId};
    use pf_core::location::{Location, Span};
    use pf_core::review::{LayerSummary, QualityLayers};
    use std::time::Duration;

    fn sample_report() -> ReviewReport {
        ReviewReport {
            impact: None,
            run_id: RunId::new(),
            project_id: ProjectId::new(),
            project_root: std::path::PathBuf::from("/tmp/demo"),
            revision: "workdir".to_owned(),
            file_count: 2,
            symbol_count: 5,
            findings: vec![],
            layers: QualityLayers {
                correctness: LayerSummary::default(),
                engineering: LayerSummary::default(),
                maintainability: LayerSummary::default(),
                verification: LayerSummary::default(),
                block_on: pf_core::status::Severity::High,
            },
            gate: GateStatus::Passed,
            ruleset_version: "0.1.0".to_owned(),
            imported_standards: Vec::new(),
            duration: Duration::from_millis(7),
            evidence: vec![EvidenceArtifact::from_text("log", "hello evidence")],
            warnings: vec![],
        }
    }

    async fn store() -> Store {
        Store::in_memory().await.expect("in-memory store")
    }

    #[tokio::test]
    async fn saves_and_lists_runs() {
        let store = store().await;
        let report = sample_report();
        store.save_run(&report).await.expect("save");

        let runs = store.recent_runs(10).await.expect("list");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, report.run_id);
        assert_eq!(runs[0].gate, GateStatus::Passed);
    }

    #[tokio::test]
    async fn counts_findings_for_run() {
        let store = store().await;
        let mut report = sample_report();
        report.findings = vec![
            pf_core::finding::Finding::new(
                RuleId::builtin("test"),
                pf_core::finding::FindingKind::NamingViolation,
                "t",
                "d",
                vec![Location::new("a.rs".to_owned(), Span::new(0, 1))],
            ),
            pf_core::finding::Finding::new(
                RuleId::builtin("test"),
                pf_core::finding::FindingKind::CommentQuality,
                "t2",
                "d2",
                vec![],
            ),
        ];
        store.save_run(&report).await.expect("save");
        assert_eq!(store.finding_count(&report.run_id).await.expect("count"), 2);
    }

    #[tokio::test]
    async fn evidence_is_content_addressed_and_verifiable() {
        let store = store().await;
        let report = sample_report();
        let artifact = &report.evidence[0];
        store.save_run(&report).await.expect("save");
        assert!(store
            .verify_evidence(&artifact.sha256)
            .await
            .expect("verify"));
        // 篡改后的摘要不应通过校验。
        assert!(!store
            .verify_evidence(&"0".repeat(64))
            .await
            .expect("verify"));
    }

    #[tokio::test]
    async fn saving_same_run_twice_is_idempotent() {
        let store = store().await;
        let report = sample_report();
        store.save_run(&report).await.expect("first");
        store.save_run(&report).await.expect("second");
        assert_eq!(store.recent_runs(10).await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn delete_run_removes_findings_too() {
        let store = store().await;
        let mut report = sample_report();
        report.findings = vec![pf_core::finding::Finding::new(
            RuleId::builtin("test"),
            pf_core::finding::FindingKind::NamingViolation,
            "t",
            "d",
            vec![],
        )];
        store.save_run(&report).await.expect("save");
        store.delete_run(&report.run_id).await.expect("delete");
        assert_eq!(store.finding_count(&report.run_id).await.expect("count"), 0);
        assert!(store.recent_runs(10).await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn recent_runs_respects_limit() {
        let store = store().await;
        for _ in 0..5 {
            store.save_run(&sample_report()).await.expect("save");
        }
        assert_eq!(store.recent_runs(3).await.expect("list").len(), 3);
    }

    #[test]
    fn gate_parsing_round_trips() {
        assert_eq!(parse_gate("passed").expect("ok"), GateStatus::Passed);
        assert_eq!(
            parse_gate("insufficient_evidence").expect("ok"),
            GateStatus::InsufficientEvidence
        );
        assert!(parse_gate("bogus").is_err());
    }

    /// 造一次带阻断级发现与豁免的运行。
    fn report_with_findings(root: &str, run_id: RunId) -> ReviewReport {
        let mut finding = pf_core::finding::Finding::new(
            RuleId::builtin("naming-convention"),
            pf_core::finding::FindingKind::NamingViolation,
            "t",
            "d",
            vec![Location::new("a.rs".to_owned(), Span::new(0, 1))],
        )
        .with_severity(pf_core::status::Severity::High)
        .with_status(pf_core::status::EvidenceStatus::StaticConfirmed);
        finding.exemption = Some(pf_core::exempt::Exemption {
            rule_id: RuleId::builtin("naming-convention"),
            scope: pf_core::exempt::ExemptionScope::Rule,
            reason: "排期在 #42".to_owned(),
            granted_by: "alice".to_owned(),
            expires_on: None,
        });
        ReviewReport {
            impact: None,
            run_id,
            project_id: ProjectId::new(),
            project_root: std::path::PathBuf::from(root),
            revision: "workdir".to_owned(),
            file_count: 4,
            symbol_count: 9,
            findings: vec![finding],
            layers: QualityLayers {
                correctness: LayerSummary::default(),
                engineering: LayerSummary {
                    count: 1,
                    confirmed: 0,
                    exempted: 1,
                    blocking: 0,
                },
                maintainability: LayerSummary::default(),
                verification: LayerSummary::default(),
                block_on: pf_core::status::Severity::High,
            },
            gate: GateStatus::Passed,
            ruleset_version: "0.1.0".to_owned(),
            imported_standards: Vec::new(),
            duration: Duration::from_millis(7),
            evidence: vec![],
            warnings: vec![],
        }
    }

    #[tokio::test]
    async fn exempted_and_blocking_counts_survive_a_round_trip() {
        // 趋势查询依赖这两列：漏存就等于「压掉了多少问题」在历史里不可见。
        let store = store().await;
        let report = report_with_findings("/tmp/demo", RunId::new());
        store.save_run(&report).await.expect("save");

        assert_eq!(
            store.exempted_count(&report.run_id).await.expect("count"),
            1
        );
        let trend = store.trend("/tmp/demo", 10).await.expect("trend");
        assert_eq!(trend.len(), 1);
        assert_eq!(trend[0].exempted_count, 1);
        assert_eq!(trend[0].blocking_count, 0, "已豁免的发现不阻断");
        assert_eq!(trend[0].finding_count, 1);
    }

    #[tokio::test]
    async fn trend_is_ordered_and_scoped_to_one_project() {
        let store = store().await;
        store
            .save_run(&report_with_findings("/tmp/a", RunId::new()))
            .await
            .expect("a1");
        store
            .save_run(&report_with_findings("/tmp/a", RunId::new()))
            .await
            .expect("a2");
        store
            .save_run(&report_with_findings("/tmp/b", RunId::new()))
            .await
            .expect("b1");

        let trend = store.trend("/tmp/a", 10).await.expect("trend");
        assert_eq!(trend.len(), 2, "趋势必须只覆盖被查询的项目");
        let times: Vec<&str> = trend
            .iter()
            .map(|point| point.created_at.as_str())
            .collect();
        let mut sorted = times.clone();
        sorted.sort_unstable();
        assert_eq!(times, sorted, "趋势必须按时间正序");
        assert_eq!(store.trend("/tmp/a", 1).await.expect("limit").len(), 1);
    }

    #[tokio::test]
    async fn ruleset_history_groups_by_version() {
        // FR-061：同一版本出现多次要合并成一条记录，版本变更要能看出时间顺序。
        let store = store().await;
        let mut first = report_with_findings("/tmp/a", RunId::new());
        first.ruleset_version = "0.1.0".to_owned();
        store.save_run(&first).await.expect("first");
        let mut second = report_with_findings("/tmp/a", RunId::new());
        second.ruleset_version = "0.2.0".to_owned();
        store.save_run(&second).await.expect("second");

        let history = store.ruleset_history("/tmp/a").await.expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].version, "0.1.0");
        assert_eq!(history[0].run_count, 1);
        assert_eq!(history[1].version, "0.2.0");
        assert!(history[0].first_seen <= history[1].first_seen);
    }

    #[tokio::test]
    async fn an_old_database_without_the_new_columns_is_upgraded() {
        // 老数据库没有 exempted_count / blocking_count 两列。迁移必须补列，
        // 否则趋势查询直接报错，用户会以为历史数据丢了。
        let store = store().await;
        store
            .pool
            .execute("ALTER TABLE runs DROP COLUMN blocking_count")
            .await
            .expect("drop column");
        store
            .pool
            .execute("ALTER TABLE findings DROP COLUMN exempted")
            .await
            .expect("drop column");
        assert!(!store
            .has_column("runs", "blocking_count")
            .await
            .expect("check"));

        store.add_missing_columns().await.expect("upgrade");

        assert!(store
            .has_column("runs", "blocking_count")
            .await
            .expect("check"));
        assert!(store
            .has_column("findings", "exempted")
            .await
            .expect("check"));
        let report = report_with_findings("/tmp/a", RunId::new());
        store.save_run(&report).await.expect("save after upgrade");
        let trend = store
            .trend("/tmp/a", 10)
            .await
            .expect("trend after upgrade");
        assert_eq!(trend[0].blocking_count, 0);
    }
}
