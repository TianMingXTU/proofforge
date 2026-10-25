//! 规则豁免（FR-062）。
//!
//! # 一条豁免必须带什么
//!
//! 需求文档 FR-062 要求豁免「有理由、有范围」。因此解析时拒绝：
//!
//! - 没有 `reason` 的豁免——说不清为什么豁免的豁免只是把发现藏起来；
//! - 没有 `granted_by` 的豁免——豁免必须可归属到人，否则审计时查不到是谁放的
//!   （FR-070）；
//! - `expires_on` 早于参考日期的豁免——过期豁免自动失效，但会出现在
//!   [`ExemptionSet::expired`] 里，让人知道有条豁免悄悄过期了。
//!
//! # 为什么豁免不删除发现
//!
//! 豁免只把发现的呈现改成「已豁免」并附上理由，发现本身仍在报告里。静默删除
//! 会让「我们到底压掉了多少问题」无从审计。
//!
//! 类型放在 `pf-core` 而不是 `pf-rules`：豁免要能出现在 [`crate::config::Config`]
//! 里，而 `pf-core` 不能依赖任何内部 crate。

use crate::finding::Finding;
use crate::ids::RuleId;
use serde::{Deserialize, Serialize};

/// 豁免的适用范围。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExemptionScope {
    /// 整条规则在所有位置生效。
    Rule,
    /// 只在指定文件内生效。
    File {
        /// 仓库相对路径。
        path: String,
    },
    /// 只在指定文件的指定行范围内生效。
    Lines {
        /// 仓库相对路径。
        path: String,
        /// 起始行（一基，含）。
        start_line: u32,
        /// 结束行（一基，含）。
        end_line: u32,
    },
}

impl ExemptionScope {
    /// 该范围是否覆盖给定位置。
    #[must_use]
    pub fn covers(&self, path: &str, line: Option<u32>) -> bool {
        match self {
            Self::Rule => true,
            Self::File { path: scope } => scope == path,
            Self::Lines {
                path: scope,
                start_line,
                end_line,
            } => scope == path && line.is_some_and(|line| line >= *start_line && line <= *end_line),
        }
    }

    /// 稳定标签，用于报告展示。
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Rule => "整条规则".to_owned(),
            Self::File { path } => format!("文件 {path}"),
            Self::Lines {
                path,
                start_line,
                end_line,
            } => format!("文件 {path} 第 {start_line}-{end_line} 行"),
        }
    }
}

/// 当前日期的 ISO 形式（`YYYY-MM-DD`）。
///
/// 只有二进制层（CLI）应该调用它：库函数的过期判断一律走显式传入的参考日期，
/// 这样「同一份代码 + 同一个日期 ⇒ 同一份报告」才成立（FR-061）。
///
/// 实现不引入日期库：只需要精确到天的日历换算，自己算比为一个 `expires_on`
/// 字段拉进一整个时间 crate 划算。
#[must_use]
pub fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// 把「1970-01-01 起的天数」换算成公历日期（Howard Hinnant 的 `civil_from_days`）。
///
/// 返回 `(年, 月, 日)`。手写而不是用 `chrono`：这里只需要一个稳定的、无时区
/// 依赖的换算，而整个项目对日期的唯一需求就是比较 `YYYY-MM-DD` 字符串。
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let shifted = days_since_epoch + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * month_prime + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    })
    .unwrap_or(1);
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

/// 一条豁免。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exemption {
    /// 被豁免的规则。
    pub rule_id: RuleId,
    /// 适用范围。
    pub scope: ExemptionScope,
    /// 为什么豁免。不可为空——说不清理由的豁免只是把发现藏起来。
    pub reason: String,
    /// 谁批准的（开发者 ID、CI 作业名）。
    pub granted_by: String,
    /// 过期日期（ISO `YYYY-MM-DD`）；`None` 表示长期有效。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_on: Option<String>,
}

impl Exemption {
    /// 该豁免在给定参考日期下是否仍然有效。
    ///
    /// 参考日期由调用方传入而不是读系统时钟：库代码因此保持确定性与可测试性
    /// （AGENTS.md §5「测试必须确定性」）。
    #[must_use]
    pub fn is_active_on(&self, reference_date: &str) -> bool {
        match &self.expires_on {
            None => true,
            Some(expiry) => expiry.as_str() >= reference_date,
        }
    }

    /// 该豁免是否覆盖某条发现。
    #[must_use]
    pub fn covers(&self, finding: &Finding) -> bool {
        if finding.rule_id != self.rule_id {
            return false;
        }
        if self.reason.trim().is_empty() {
            return false;
        }
        // 没有位置的发现只能用规则级豁免覆盖：给它编一个位置等于伪造证据。
        if finding.locations.is_empty() {
            return matches!(self.scope, ExemptionScope::Rule);
        }
        finding
            .locations
            .iter()
            .any(|location| self.scope.covers(&location.path, location.start_line))
    }
}

/// 一组豁免。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExemptionSet {
    /// 全部豁免。
    #[serde(default)]
    pub exemptions: Vec<Exemption>,
}

/// 豁免解析失败的原因。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExemptionError {
    /// 文档解析失败。
    #[error("cannot parse the exemption file: {reason}")]
    Parse {
        /// 底层原因。
        reason: String,
    },
    /// 豁免缺少理由。
    #[error("exemption for rule `{rule_id}` has no reason; an exemption without a reason only hides the finding")]
    MissingReason {
        /// 规则 ID。
        rule_id: String,
    },
    /// 豁免缺少批准人。
    #[error("exemption for rule `{rule_id}` has no `granted_by`; exemptions must be attributable (FR-070)")]
    MissingGrantor {
        /// 规则 ID。
        rule_id: String,
    },
}

impl ExemptionSet {
    /// 构造空集合。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            exemptions: Vec::new(),
        }
    }

    /// 加入一条豁免。
    pub fn push(&mut self, exemption: Exemption) {
        self.exemptions.push(exemption);
    }

    /// 从 TOML 文本解析豁免集合。
    ///
    /// # Errors
    /// 文本不是合法 TOML、或某条豁免缺少理由/批准人时返回 [`ExemptionError`]。
    pub fn from_toml(text: &str) -> Result<Self, ExemptionError> {
        let set: Self = toml::from_str(text).map_err(|reason| ExemptionError::Parse {
            reason: reason.to_string(),
        })?;
        set.validate()?;
        Ok(set)
    }

    /// 序列化为 TOML。
    ///
    /// # Errors
    /// 序列化失败时返回 [`ExemptionError::Parse`]。
    pub fn to_toml(&self) -> Result<String, ExemptionError> {
        toml::to_string(self).map_err(|reason| ExemptionError::Parse {
            reason: reason.to_string(),
        })
    }

    /// 校验每一条豁免都带理由与批准人。
    ///
    /// # Errors
    /// 存在缺少理由或批准人的豁免时返回错误，并指出是哪一条规则。
    pub fn validate(&self) -> Result<(), ExemptionError> {
        for exemption in &self.exemptions {
            if exemption.reason.trim().is_empty() {
                return Err(ExemptionError::MissingReason {
                    rule_id: exemption.rule_id.get().to_owned(),
                });
            }
            if exemption.granted_by.trim().is_empty() {
                return Err(ExemptionError::MissingGrantor {
                    rule_id: exemption.rule_id.get().to_owned(),
                });
            }
        }
        Ok(())
    }

    /// 找出覆盖某条发现的、在参考日期下仍然有效的豁免。
    #[must_use]
    pub fn matching<'a>(
        &'a self,
        finding: &'a Finding,
        reference_date: &str,
    ) -> Vec<&'a Exemption> {
        self.exemptions
            .iter()
            .filter(|exemption| exemption.covers(finding) && exemption.is_active_on(reference_date))
            .collect()
    }

    /// 找出在参考日期下已经过期的豁免。
    ///
    /// 过期豁免不生效，但必须让人看见：一条悄悄过期的豁免意味着它覆盖的问题
    /// 已经悄悄回到报告里了。
    #[must_use]
    pub fn expired(&self, reference_date: &str) -> Vec<&Exemption> {
        self.exemptions
            .iter()
            .filter(|exemption| !exemption.is_active_on(reference_date))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{Finding, FindingKind};
    use crate::location::{Location, Span};

    fn finding(path: &str, line: u32, rule: &str) -> Finding {
        Finding::new(
            RuleId::builtin(rule),
            FindingKind::UselessAbstraction,
            "trait `Store` 只有一种实现",
            "detail",
            vec![Location {
                path: path.to_owned(),
                span: Span::new(0, 10),
                start_line: Some(line),
                start_column: Some(1),
                end_line: Some(line),
                end_column: Some(1),
            }],
        )
    }

    fn exemption(scope: ExemptionScope, reason: &str) -> Exemption {
        Exemption {
            rule_id: RuleId::builtin("single-implementation-trait"),
            scope,
            reason: reason.to_owned(),
            granted_by: "alice".to_owned(),
            expires_on: None,
        }
    }

    #[test]
    fn an_exemption_without_a_reason_is_rejected() {
        // 说不清理由的豁免只是把发现藏起来（FR-062）。
        let mut set = ExemptionSet::new();
        set.push(exemption(ExemptionScope::Rule, "   "));
        let error = set.validate().expect_err("missing reason");
        assert!(matches!(error, ExemptionError::MissingReason { .. }));
    }

    #[test]
    fn an_exemption_without_a_grantor_is_rejected() {
        // 豁免必须可归属到人（FR-070 审计追踪）。
        let mut set = ExemptionSet::new();
        set.push(Exemption {
            granted_by: String::new(),
            ..exemption(ExemptionScope::Rule, "测试替身需要这一层")
        });
        let error = set.validate().expect_err("missing grantor");
        assert!(matches!(error, ExemptionError::MissingGrantor { .. }));
    }

    #[test]
    fn a_rule_scoped_exemption_covers_every_location() {
        let set = ExemptionSet {
            exemptions: vec![exemption(ExemptionScope::Rule, "对外契约，暂时保留")],
        };
        let target = finding("src/a.rs", 12, "single-implementation-trait");
        assert_eq!(set.matching(&target, "2026-01-01").len(), 1);
    }

    #[test]
    fn a_line_scoped_exemption_only_covers_its_range() {
        let set = ExemptionSet {
            exemptions: vec![exemption(
                ExemptionScope::Lines {
                    path: "src/a.rs".to_owned(),
                    start_line: 10,
                    end_line: 20,
                },
                "这一段是测试替身",
            )],
        };
        assert_eq!(
            set.matching(
                &finding("src/a.rs", 15, "single-implementation-trait"),
                "2026-01-01"
            )
            .len(),
            1
        );
        assert_eq!(
            set.matching(
                &finding("src/a.rs", 25, "single-implementation-trait"),
                "2026-01-01"
            )
            .len(),
            0,
            "超出 end_line 的位置不被覆盖"
        );
        assert_eq!(
            set.matching(
                &finding("src/b.rs", 15, "single-implementation-trait"),
                "2026-01-01"
            )
            .len(),
            0,
            "别的文件里的同一行不被覆盖"
        );
    }

    #[test]
    fn an_exemption_for_another_rule_does_not_apply() {
        let set = ExemptionSet {
            exemptions: vec![exemption(ExemptionScope::Rule, "理由")],
        };
        assert_eq!(
            set.matching(&finding("src/a.rs", 1, "forwarding-wrapper"), "2026-01-01")
                .len(),
            0,
            "豁免只对它写明的规则生效"
        );
    }

    #[test]
    fn an_expired_exemption_stops_applying_but_stays_visible() {
        let mut entry = exemption(ExemptionScope::Rule, "临时豁免");
        entry.expires_on = Some("2026-06-30".to_owned());
        let set = ExemptionSet {
            exemptions: vec![entry],
        };
        let target = finding("src/a.rs", 1, "single-implementation-trait");
        assert!(
            set.matching(&target, "2026-06-29").len() == 1,
            "到期当天仍有效"
        );
        assert!(set.matching(&target, "2026-07-01").is_empty(), "过期后失效");
        assert_eq!(set.expired("2026-07-01").len(), 1, "过期豁免必须仍可见");
    }

    #[test]
    fn civil_from_days_maps_known_dates() {
        // 锚点：Unix 纪元与一个跨闰年/月末的日期。手写日历换算必须逐点验证，
        // 错一天就会让「当天仍有效」的豁免判断整体偏移。
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_999), (2024, 10, 3));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
        assert_eq!(civil_from_days(20_089), (2025, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn today_iso_is_a_plausible_calendar_date() {
        // 不锁定具体日期（那会让测试随时间失败），只校验形状与量级。
        let today = today_iso();
        let parts: Vec<&str> = today.split('-').collect();
        assert_eq!(parts.len(), 3, "{today}");
        assert_eq!(today.len(), 10, "{today}");
        let year: i64 = parts[0].parse().expect("year");
        let month: u32 = parts[1].parse().expect("month");
        let day: u32 = parts[2].parse().expect("day");
        assert!((2020..2100).contains(&year), "{today}");
        assert!((1..=12).contains(&month), "{today}");
        assert!((1..=31).contains(&day), "{today}");
    }

    #[test]
    fn exemptions_round_trip_through_toml() {
        let toml = r#"
[[exemptions]]
rule_id = "builtin.single-implementation-trait"
scope = { kind = "lines", path = "src/store.rs", start_line = 10, end_line = 40 }
reason = "测试替身需要这一层抽象"
granted_by = "alice"
expires_on = "2026-12-31"
"#;
        let set = ExemptionSet::from_toml(toml).expect("parse");
        assert_eq!(set.exemptions.len(), 1);
        let rendered = set.to_toml().expect("render");
        let back = ExemptionSet::from_toml(&rendered).expect("reparse");
        assert_eq!(set, back);
    }
}
