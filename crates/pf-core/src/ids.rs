//! 强类型标识符。
//!
//! 除 [`FindingId`] 外的标识符都是 26 字符的 Crockford Base32 ULID，可字典序
//! 排序且包含生成时间，便于在报告与证据中按时间复盘。[`FindingId`] 则由发现
//! 内容派生，以保证重复运行之间的稳定性（见 [`FindingId::derive`]）。

use std::fmt;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// 标识符的通用行为：解析、生成与显示。
///
/// 由 [`define_id`] 宏为每个具体 ID 类型实现，避免手写重复样板代码。
pub trait Identifier: Sized {
    /// 从字符串解析标识符，输入非法时返回错误。
    fn parse(value: &str) -> Result<Self, ParseIdError>;

    /// 生成一个新的标识符。
    fn generate() -> Self;

    /// 返回标识符的字符串形式。
    fn as_str(&self) -> &str;
}

/// 标识符解析失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} identifier: {value:?}")]
pub struct ParseIdError {
    /// 标识符类型名称，例如 `RunId`。
    pub kind: &'static str,
    /// 解析失败的原始输入。
    pub value: String,
}

/// 为具体 ID 类型生成实现代码。
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// 创建一个新的随机标识符。
            #[must_use]
            pub fn new() -> Self {
                Self(ulid::Ulid::new().to_string().to_lowercase())
            }

            /// 借用内部字符串。
            #[must_use]
            pub fn get(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl Identifier for $name {
            fn parse(value: &str) -> Result<Self, ParseIdError> {
                let normalized = value.trim().to_lowercase();
                if normalized.len() == 26 && normalized.chars().all(|c| c.is_ascii_alphanumeric()) {
                    Ok(Self(normalized))
                } else {
                    Err(ParseIdError { kind: stringify!($name), value: value.to_owned() })
                }
            }

            fn generate() -> Self {
                Self::new()
            }

            fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ParseIdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(&value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ParseIdError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

define_id! {
    /// 被审查项目的唯一标识。
    ProjectId
}

define_id! {
    /// 一次仓库快照（代码版本）的唯一标识。
    SnapshotId
}

define_id! {
    /// 一次分析任务的唯一标识。
    TaskId
}

define_id! {
    /// 一条审查发现的唯一标识。
    FindingId
}

impl FindingId {
    /// 从发现内容派生一个确定性标识。
    ///
    /// 与随机生成的 ID 不同，相同内容总是得到相同结果。这样同一个问题在
    /// 重复运行中可以相互抵消、报告可以跨版本 diff、落库不会为同一个问题
    /// 累积多行（FR-061 可审计性）。
    ///
    /// `parts` 应包含足以区分两条发现的全部要素：规则 ID、问题类型、标题
    /// 与位置。调用方负责保证这一点；内容相同即视为同一条发现。
    #[must_use]
    pub fn derive(parts: &[&str]) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for part in parts {
            hasher.update(part.as_bytes());
            // 用不可能出现在内容里的分隔符防止字段拼接歧义。
            hasher.update([0x1f]);
        }
        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        // ULID 的 26 字符 Crockford Base32 编码正好容纳 128 位摘要。
        Self(Ulid::from_bytes(bytes).to_string().to_lowercase())
    }
}

define_id! {
    /// 一次完整审查运行的唯一标识，用于串联报告与证据。
    RunId
}

/// 规则标识。
///
/// 与其他实体 ID 不同，规则 ID 必须是**可读且稳定**的：它要出现在报告、
/// 豁免配置和审计记录里，因此采用 `builtin.no-unwrap` 这类短横线命名，
/// 而不是随机 ULID。`Checker::rule()` 每次调用都必须返回同一个 ID，
/// 否则按 ID 筛选规则会失效。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RuleId(String);

impl RuleId {
    /// 规则 ID 的最大长度。
    pub const MAX_LEN: usize = 64;

    /// 构造内置规则 ID（加上 `builtin.` 前缀以区分来源）。
    #[must_use]
    pub fn builtin(name: &str) -> Self {
        Self(format!("builtin.{name}"))
    }

    /// 构造项目自定义规则 ID。
    #[must_use]
    pub fn project(name: &str) -> Self {
        Self(format!("project.{name}"))
    }

    /// 借用内部字符串。
    #[must_use]
    pub fn get(&self) -> &str {
        &self.0
    }

    /// 是否为内置规则。
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        self.0.starts_with("builtin.")
    }

    /// 去掉来源前缀后的本地名称。
    ///
    /// 例如 `builtin.public-api-missing-docs` 得到 `public-api-missing-docs`。
    /// 规则的显示名直接由 ID 派生，避免同一规则在两处各写一份名字而漂移
    /// （规则 ID 会出现在报告、豁免配置与审计记录中，必须唯一且稳定）。
    #[must_use]
    pub fn local_name(&self) -> &str {
        self.0.rsplit('.').next().unwrap_or(&self.0)
    }
}

impl Identifier for RuleId {
    fn parse(value: &str) -> Result<Self, ParseIdError> {
        let normalized = value.trim().to_ascii_lowercase();
        let valid = !normalized.is_empty()
            && normalized.len() <= Self::MAX_LEN
            && normalized.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_')
            });
        if valid {
            Ok(Self(normalized))
        } else {
            Err(ParseIdError {
                kind: "RuleId",
                value: value.to_owned(),
            })
        }
    }

    fn generate() -> Self {
        Self::project(&Ulid::new().to_string().to_lowercase())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RuleId {
    type Error = ParseIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl TryFrom<&str> for RuleId {
    type Error = ParseIdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<RuleId> for String {
    fn from(value: RuleId) -> Self {
        value.0
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_unique() {
        assert_ne!(RunId::new(), RunId::new());
    }

    #[test]
    fn generated_ids_are_26_char_base32() {
        let id = RunId::new();
        assert_eq!(id.as_str().len(), 26);
        assert!(id.as_str().chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn parse_accepts_normalized_ulid() {
        let id = RunId::new();
        let parsed = RunId::parse(id.as_str()).expect("round-trip should succeed");
        assert_eq!(id, parsed);
    }

    #[test]
    fn parse_rejects_malformed_input() {
        let err = RunId::parse("not-an-id").expect_err("should reject");
        assert_eq!(err.kind, "RunId");
        assert_eq!(err.value, "not-an-id");
    }

    #[test]
    fn parse_is_case_insensitive() {
        let id = RunId::new();
        let upper = id.as_str().to_uppercase();
        assert_eq!(RunId::parse(&upper).expect("should parse"), id);
    }

    #[test]
    fn serde_round_trip_uses_plain_string() {
        let id = TaskId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{}\"", id.as_str()));
        let back: TaskId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn rule_id_is_readable_and_stable() {
        let id = RuleId::builtin("no-unwrap");
        assert_eq!(id.as_str(), "builtin.no-unwrap");
        assert!(id.is_builtin());
        // 每次构造得到同一个 ID，是按 ID 筛选规则的前提。
        assert_eq!(RuleId::builtin("no-unwrap"), id);
        assert!(!RuleId::project("custom").is_builtin());
        assert_eq!(id.local_name(), "no-unwrap");
    }

    #[test]
    fn finding_id_is_derived_from_content() {
        let parts = [
            "builtin.no-unwrap",
            "unwrap_in_library",
            "`x` 使用了 unwrap",
        ];
        let first = FindingId::derive(&parts);
        let second = FindingId::derive(&parts);
        // 相同内容必须得到相同 ID，否则报告无法跨运行 diff。
        assert_eq!(first, second);
        assert_eq!(first.as_str().len(), 26);
        assert!(first.as_str().chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn finding_id_separates_different_content() {
        let base = FindingId::derive(&["a", "b"]);
        // 分隔符必须防止字段拼接歧义：["ab", "c"] 不能等于 ["a", "bc"]。
        assert_ne!(base, FindingId::derive(&["ab", "c"]));
        assert_ne!(base, FindingId::derive(&["a", "b", "c"]));
        assert_ne!(base, FindingId::derive(&["a"]));
    }

    #[test]
    fn rule_id_rejects_malformed_values() {
        assert!(RuleId::parse("builtin.no-unwrap").is_ok());
        assert!(RuleId::parse("").is_err());
        assert!(RuleId::parse("has space").is_err());
        assert!(RuleId::parse(&"x".repeat(65)).is_err());
        assert!(
            RuleId::parse("Builtin.MixedCase").is_ok_and(|id| id.as_str() == "builtin.mixedcase")
        );
    }

    #[test]
    fn rule_id_serde_round_trip() {
        let id = RuleId::builtin("circular-dependency");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"builtin.circular-dependency\"");
        let back: RuleId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn serde_rejects_invalid_string() {
        let result: Result<TaskId, _> = serde_json::from_str("\"nope\"");
        assert!(result.is_err());
    }
}
