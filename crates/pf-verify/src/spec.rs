//! 性质规格文件：让「要验证什么」成为人可以写、机器能跑的输入。
//!
//! 覆盖需求文档 5.6 的 FR-033（把需求转化为可执行验证）的**可执行一半**。
//!
//! # 这一半是什么，另外一半在哪里
//!
//! 从自然语言需求里自动抽出性质需要语义理解，属于阶段二的「AI 语义分析」，
//! 依赖阶段四的 Agent Runtime。在那之前，本模块提供诚实的替代路径：性质由人
//! （或后续的 agent）写成一份显式规格文件，验证引擎负责把它跑成可复现的证据。
//!
//! 这样分工的好处是**证据链不断**：一旦 agent 能产出规格，它产出的东西立刻
//! 就能被同一套沙箱、同一套反例缩减、同一套门禁消费，不需要再搭一层。
//!
//! # 文件长什么样
//!
//! ```json
//! {
//!   "language": "rust",
//!   "imports": ["mycrate"],
//!   "samples": 32,
//!   "properties": [
//!     {
//!       "name": "解析结果非负",
//!       "call_template": "mycrate::parse({args})",
//!       "domain": { "kind": "int", "min": -100, "max": 100 },
//!       "invariant": "output >= 0"
//!     }
//!   ]
//! }
//! ```

use serde::{Deserialize, Serialize};

use crate::domain::InputDomain;
use crate::driver::{Language, PropertySpec};
use crate::metamorphic::MetamorphicSpec;

/// 顶层默认样本数。
fn default_samples() -> usize {
    16
}

/// 顶层默认随机种子。
fn default_seed() -> u64 {
    1
}

/// 顶层默认语言。
fn default_language() -> Language {
    Language::Rust
}

/// 一份性质规格文件。
///
/// 顶层字段是**默认值**：每条性质可以各自覆盖样本数与种子，但语言与引用列表
/// 共享——同一个文件里的性质几乎总是测同一个 crate，重复写十六遍没有意义。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyFile {
    /// 所有性质共用的目标语言。
    #[serde(default = "default_language")]
    pub language: Language,
    /// 所有性质共用的引用列表（每项原样写进驱动程序的 import 区）。
    #[serde(default)]
    pub imports: Vec<String>,
    /// 默认样本数。
    #[serde(default = "default_samples")]
    pub samples: usize,
    /// 默认随机种子。
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// 待验证的性质。至少一条：一份空文件没有任何可验证的东西。
    pub properties: Vec<PropertyEntry>,
}

/// 一条待验证的性质。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyEntry {
    /// 人类可读的名称，出现在报告里。
    pub name: String,
    /// 调用模板，`{args}` 会被替换成实参表达式。
    pub call_template: String,
    /// 输入域。
    pub domain: InputDomain,
    /// 对输出的断言表达式，其中 `output` 绑定到调用结果。
    pub invariant: String,
    /// 覆盖顶层样本数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples: Option<usize>,
    /// 覆盖顶层随机种子。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

impl PropertyFile {
    /// 构造一份空规格文件。
    ///
    /// 供「没有性质也要跑」的调用方使用（例如 `pf-cli agent` 只做结构计划）。
    /// 它刻意不走 [`Self::from_json`]：空文件在规格文件的语义里是**错误**，
    /// 但在编排层是合法的「本次不验证性质」。
    #[must_use]
    pub fn empty() -> Self {
        Self {
            language: Language::Rust,
            imports: Vec::new(),
            samples: 1,
            seed: 0,
            properties: Vec::new(),
        }
    }

    /// 从 JSON 文本解析规格文件。
    ///
    /// # Errors
    /// 文本不是合法 JSON、或语义上不可执行（空性质列表、域上下界颠倒）时返回
    /// [`SpecError`]。错误信息带上具体是哪一条性质，因为规格文件是人手写的。
    pub fn from_json(text: &str) -> Result<Self, SpecError> {
        let file: Self = serde_json::from_str(text)?;
        if file.properties.is_empty() {
            return Err(SpecError::Empty);
        }
        if file.samples == 0 {
            return Err(SpecError::NoSamples);
        }
        for entry in &file.properties {
            if entry.name.trim().is_empty() {
                return Err(SpecError::UnnamedProperty(entry.call_template.clone()));
            }
            if !entry.domain.is_well_formed() {
                return Err(SpecError::MalformedDomain(entry.name.clone()));
            }
            // 覆盖值同样要查：一条 `"samples": 0` 的性质会让驱动程序跑一个空循环，
            // 打印完成标记后以 0 退出——「一个输入都没测」于是变成「未观察到违反」。
            if entry.samples == Some(0) {
                return Err(SpecError::NoSamples);
            }
        }
        Ok(file)
    }

    /// 展开成验证引擎可消费的性质列表。
    #[must_use]
    pub fn specs(&self) -> Vec<PropertySpec> {
        self.properties
            .iter()
            .map(|entry| PropertySpec {
                name: Some(entry.name.clone()),
                language: self.language,
                imports: self.imports.clone(),
                call_template: entry.call_template.clone(),
                domain: entry.domain.clone(),
                invariant: entry.invariant.clone(),
                samples: entry.samples.unwrap_or(self.samples),
                seed: entry.seed.unwrap_or(self.seed),
            })
            .collect()
    }
}

/// 加载性质规格时的失败原因。
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    /// 文本不是合法的 JSON。
    #[error("性质规格不是合法的 JSON：{0}")]
    Malformed(#[from] serde_json::Error),
    /// 文件里一条性质都没有。
    #[error("性质规格里没有任何性质；没有可验证的内容就不是「验证通过」")]
    Empty,
    /// 某条性质没有名字。
    #[error("性质 `{0}` 缺少 name；报告需要能指认每一条性质")]
    UnnamedProperty(String),
    /// 某个域的上下界或长度约束不成立。
    #[error("性质 `{0}` 的输入域不可执行（例如 min > max）；请检查域声明")]
    MalformedDomain(String),

    /// 样本数为 0。
    ///
    /// 这不是洁癖：样本数为 0 时驱动程序的循环一次都不进，随后打印完成标记
    /// 并以 0 退出，于是「一个输入都没测」被记成「未观察到违反」并让门禁通过。
    #[error("样本数必须大于 0；一个输入都不测的性质会被记成「未观察到违反」并让门禁通过")]
    NoSamples,

    /// 蜕变关系的变换表达式缺少 `{input}` 占位符。
    ///
    /// 少了它，变换表达式与原输入无关——于是两次调用拿到同一个输入，关系退化成
    /// `f(x)` 与 `f(x)` 的比较。那是一条恒真的命题，却会被记成「未观察到违反」。
    #[error("蜕变关系 `{0}` 的 transform 缺少 `{{input}}` 占位符；没有它两次调用会拿到同一个输入")]
    TransformMissingPlaceholder(String),

    /// 蜕变关系的关系表达式没有引用两个输出。
    ///
    /// 只提到一个输出的关系（例如 `output_a > 0`）不是蜕变关系，它是一条普通性质，
    /// 放在这里会让使用者以为验证了「输入变换与输出的关系」。
    #[error(
        "蜕变关系 `{0}` 的 relation 必须同时引用 output_a 与 output_b；只提一个输出的不是蜕变关系"
    )]
    RelationMissingOutput(String),
}

/// 一份蜕变关系规格文件（FR-039）。
///
/// 顶层字段与 [`PropertyFile`] 同样只是默认值：语言、引用、样本数与种子共享，
/// 每条关系可以各自覆盖样本与种子。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetamorphicFile {
    /// 所有关系共用的目标语言。
    #[serde(default = "default_language")]
    pub language: Language,
    /// 所有关系共用的引用列表。
    #[serde(default)]
    pub imports: Vec<String>,
    /// 默认样本数。
    #[serde(default = "default_samples")]
    pub samples: usize,
    /// 默认随机种子。
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// 待验证的关系。至少一条。
    pub relations: Vec<MetamorphicEntry>,
}

/// 一条待验证的蜕变关系。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetamorphicEntry {
    /// 人类可读的名称，出现在报告里。
    pub name: String,
    /// 调用模板，`{args}` 会被替换成实参表达式。
    pub call_template: String,
    /// 源输入的域。
    pub domain: InputDomain,
    /// 输入变换表达式，必须包含 `{input}`。
    pub transform: String,
    /// 对两个输出的关系断言，其中 `output_a` 是原始输入的输出、
    /// `output_b` 是变换后输入的输出。
    pub relation: String,
    /// 覆盖顶层样本数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples: Option<usize>,
    /// 覆盖顶层随机种子。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

impl MetamorphicFile {
    /// 从 JSON 文本解析蜕变关系规格文件。
    ///
    /// # Errors
    /// 文本不是合法 JSON、关系列表为空、或某条关系在语义上不可能成立时返回
    /// [`SpecError`]。错误信息带上具体是哪一条关系，因为规格文件是人手写的。
    pub fn from_json(text: &str) -> Result<Self, SpecError> {
        let file: Self = serde_json::from_str(text)?;
        if file.relations.is_empty() {
            return Err(SpecError::Empty);
        }
        if file.samples == 0 {
            return Err(SpecError::NoSamples);
        }
        for entry in &file.relations {
            if entry.name.trim().is_empty() {
                return Err(SpecError::UnnamedProperty(entry.call_template.clone()));
            }
            if !entry.domain.is_well_formed() {
                return Err(SpecError::MalformedDomain(entry.name.clone()));
            }
            if entry.samples == Some(0) {
                return Err(SpecError::NoSamples);
            }
            if !entry.transform.contains("{input}") {
                return Err(SpecError::TransformMissingPlaceholder(entry.name.clone()));
            }
            if !entry.relation.contains("output_a") || !entry.relation.contains("output_b") {
                return Err(SpecError::RelationMissingOutput(entry.name.clone()));
            }
        }
        Ok(file)
    }

    /// 展开成验证引擎可消费的关系列表。
    #[must_use]
    pub fn specs(&self) -> Vec<MetamorphicSpec> {
        self.relations
            .iter()
            .map(|entry| MetamorphicSpec {
                name: Some(entry.name.clone()),
                language: self.language,
                imports: self.imports.clone(),
                call_template: entry.call_template.clone(),
                domain: entry.domain.clone(),
                transform: entry.transform.clone(),
                relation: entry.relation.clone(),
                samples: entry.samples.unwrap_or(self.samples),
                seed: entry.seed.unwrap_or(self.seed),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一份最小可用的规格文件。
    fn minimal() -> &'static str {
        r#"{
            "language": "rust",
            "imports": ["mycrate"],
            "samples": 8,
            "properties": [
                {
                    "name": "解析结果非负",
                    "call_template": "mycrate::parse({args})",
                    "domain": { "kind": "int", "min": -100, "max": 100 },
                    "invariant": "output >= 0"
                }
            ]
        }"#
    }

    #[test]
    fn a_spec_file_loads_into_runnable_specs() {
        let file = PropertyFile::from_json(minimal()).expect("load");
        let specs = file.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].language, Language::Rust);
        assert_eq!(specs[0].imports, vec!["mycrate".to_owned()]);
        assert_eq!(specs[0].samples, 8);
        assert_eq!(specs[0].seed, 1, "未覆盖时用顶层默认种子");
        assert_eq!(specs[0].call_template, "mycrate::parse({args})");
        assert!(matches!(specs[0].domain, InputDomain::Int { .. }));
    }

    #[test]
    fn per_property_overrides_win_over_the_top_level_defaults() {
        let text = minimal().replace(
            "\"invariant\": \"output >= 0\"",
            "\"invariant\": \"output >= 0\", \"samples\": 3, \"seed\": 99",
        );
        let specs = PropertyFile::from_json(&text).expect("load").specs();
        assert_eq!(specs[0].samples, 3);
        assert_eq!(specs[0].seed, 99);
    }

    #[test]
    fn defaults_apply_when_the_top_level_fields_are_absent() {
        // 省掉 language / imports / samples：只靠顶层默认值也要能跑。
        let text = r#"{"properties": [{"name": "p", "call_template": "f({args})",
            "domain": {"kind": "bool"}, "invariant": "output"}]}"#;
        let file = PropertyFile::from_json(text).expect("load");
        assert_eq!(file.language, Language::Rust);
        assert_eq!(file.imports, Vec::<String>::new());
        assert_eq!(file.samples, 16);
        assert_eq!(file.specs()[0].seed, 1);
    }

    #[test]
    fn a_file_without_properties_is_rejected() {
        // 空文件没有可验证的内容，绝不能因为「什么都没测」而放行（红线 4）。
        let error = PropertyFile::from_json(r#"{"properties": []}"#).expect_err("empty");
        assert!(matches!(error, SpecError::Empty), "{error}");
    }

    #[test]
    fn an_inverted_domain_is_rejected_at_load_time() {
        let text = minimal().replace("\"min\": -100, \"max\": 100", "\"min\": 100, \"max\": -100");
        let error = PropertyFile::from_json(&text).expect_err("inverted domain");
        assert!(matches!(error, SpecError::MalformedDomain(_)), "{error}");
    }

    #[test]
    fn a_property_without_a_name_is_rejected() {
        // 报告必须能指认每一条性质；没有名字的性质进了报告也读不了。
        let text = minimal().replace("\"name\": \"解析结果非负\"", "\"name\": \"  \"");
        let error = PropertyFile::from_json(&text).expect_err("unnamed");
        assert!(matches!(error, SpecError::UnnamedProperty(_)), "{error}");
    }

    #[test]
    fn a_zero_sample_count_is_rejected() {
        // 回归：`"samples": 0` 让驱动程序的循环一次都不进，然后打印完成标记并以
        // 0 退出。判定规则因此给出 Satisfied——一个输入都没测的性质被记成
        // 「未观察到违反」，还让门禁通过。红线 4 禁止的正是这种假通过。
        let text = minimal().replace("\"samples\": 8", "\"samples\": 0");
        let error = PropertyFile::from_json(&text).expect_err("zero samples");
        assert!(matches!(error, SpecError::NoSamples), "{error}");
    }

    #[test]
    fn a_zero_per_property_override_is_rejected() {
        let text = minimal().replace(
            "\"invariant\": \"output >= 0\"",
            "\"invariant\": \"output >= 0\", \"samples\": 0",
        );
        let error = PropertyFile::from_json(&text).expect_err("zero override");
        assert!(matches!(error, SpecError::NoSamples), "{error}");
    }

    #[test]
    fn malformed_json_is_reported_as_such() {
        let error = PropertyFile::from_json("{not json").expect_err("malformed");
        assert!(matches!(error, SpecError::Malformed(_)), "{error}");
    }

    #[test]
    fn nested_domains_are_validated_recursively() {
        let text = minimal().replace(
            "\"domain\": { \"kind\": \"int\", \"min\": -100, \"max\": 100 }",
            "\"domain\": { \"kind\": \"tuple\", \"fields\": [{\"kind\": \"int\", \"min\": 5, \"max\": 1}] }",
        );
        let error = PropertyFile::from_json(&text).expect_err("nested inverted");
        assert!(matches!(error, SpecError::MalformedDomain(_)), "{error}");
    }

    /// 一份最小可用的蜕变关系规格文件。
    fn metamorphic() -> &'static str {
        r#"{
            "language": "rust",
            "imports": ["fixture"],
            "samples": 8,
            "relations": [
                {
                    "name": "排序与输入顺序无关",
                    "call_template": "fixture::sort({args})",
                    "domain": { "kind": "list", "item": { "kind": "int", "min": -20, "max": 20 }, "max_len": 6 },
                    "transform": "{input}.iter().rev().cloned().collect::<Vec<_>>()",
                    "relation": "output_a == output_b"
                }
            ]
        }"#
    }

    #[test]
    fn a_metamorphic_file_loads_into_runnable_specs() {
        let file = MetamorphicFile::from_json(metamorphic()).expect("load");
        let specs = file.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].language, Language::Rust);
        assert_eq!(specs[0].samples, 8);
        assert_eq!(specs[0].seed, 1, "未覆盖时用顶层默认种子");
        assert_eq!(
            specs[0].transform,
            "{input}.iter().rev().cloned().collect::<Vec<_>>()"
        );
        assert_eq!(specs[0].relation, "output_a == output_b");
    }

    #[test]
    fn a_transform_without_the_placeholder_is_rejected() {
        // 没有 `{input}` 的变换与原输入无关，两次调用会拿到同一个输入，关系于是
        // 退化成恒真命题——那会被记成「未观察到违反」，是一次假通过。
        let text = metamorphic().replace(
            "\"transform\": \"{input}.iter().rev().cloned().collect::<Vec<_>>()\"",
            "\"transform\": \"vec![1, 2, 3]\"",
        );
        let error = MetamorphicFile::from_json(&text).expect_err("no placeholder");
        assert!(
            matches!(error, SpecError::TransformMissingPlaceholder(_)),
            "{error}"
        );
    }

    #[test]
    fn a_relation_that_mentions_only_one_output_is_rejected() {
        // 只提一个输出的关系不是蜕变关系：它是一条普通性质，放在这里会让使用者
        // 以为验证了「输入变换与输出的关系」。
        let text = metamorphic().replace(
            "\"relation\": \"output_a == output_b\"",
            "\"relation\": \"output_a.len() >= 0\"",
        );
        let error = MetamorphicFile::from_json(&text).expect_err("one output");
        assert!(
            matches!(error, SpecError::RelationMissingOutput(_)),
            "{error}"
        );
    }

    #[test]
    fn an_empty_relation_list_is_rejected() {
        let error = MetamorphicFile::from_json(r#"{"relations": []}"#).expect_err("empty");
        assert!(matches!(error, SpecError::Empty), "{error}");
    }

    #[test]
    fn a_metamorphic_domain_is_validated_like_a_property_domain() {
        let text = metamorphic().replace(
            "\"domain\": { \"kind\": \"list\", \"item\": { \"kind\": \"int\", \"min\": -20, \"max\": 20 }, \"max_len\": 6 }",
            "\"domain\": { \"kind\": \"int\", \"min\": 9, \"max\": 1 }",
        );
        let error = MetamorphicFile::from_json(&text).expect_err("inverted");
        assert!(matches!(error, SpecError::MalformedDomain(_)), "{error}");
    }
}
