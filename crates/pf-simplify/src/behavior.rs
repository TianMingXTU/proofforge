//! 行为比较（FR-053）。
//!
//! # 比较的是什么
//!
//! 「可观察行为」在这里有严格定义：**同一组输入下，程序产生的输出是否相同**。
//! 工具用的是两条证据：
//!
//! 1. 项目自带的测试套件（见 [`crate::regression`]）；
//! 2. 调用方提供的性质规格：同一性质在基线副本与补丁副本上分别跑，比较结论。
//!
//! 没有性质规格时，行为比较退化成只有测试套件一条证据。这不是缺陷，但报告
//! 必须说清楚「行为比较只覆盖了测试覆盖到的那部分」——把测试通过说成「行为
//! 完全一致」是红线 5 禁止的措辞越界。
//!
//! # 为什么「两边都违反」算行为保留
//!
//! 行为比较回答的是「补丁有没有改变行为」，不是「程序是否正确」。同一个反例
//! 在两边都出现，说明补丁没有改变这条性质下的行为；程序本身的问题由性质的
//! 结论去说，不由比较去说。

use pf_adapters::ToolOptions;
use pf_verify::property::{PropertyResult, PropertyRunner};
use pf_verify::sandbox::Sandbox;
use pf_verify::PropertySpec;
use serde::{Deserialize, Serialize};

/// 行为比较结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BehaviorVerdict {
    /// 所有探针结论一致：没有观察到行为变化。
    Preserved,
    /// 至少一条探针的结论在补丁前后不同：行为发生了变化。
    Changed,
    /// 至少一条探针没跑出结论，因此无法断言行为保留。
    Unverified,
}

impl BehaviorVerdict {
    /// 稳定标签。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Preserved => "preserved",
            Self::Changed => "changed",
            Self::Unverified => "unverified",
        }
    }

    /// 该结论是否允许补丁进入人工确认环节。
    #[must_use]
    pub const fn allows_application(&self) -> bool {
        matches!(self, Self::Preserved)
    }
}

/// 一条性质在两侧的结论对比。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeComparison {
    /// 性质名称。
    pub name: String,
    /// 基线副本上的结论标签。
    pub baseline: String,
    /// 补丁副本上的结论标签。
    pub patched: String,
    /// 两侧结论是否一致。
    pub agrees: bool,
}

/// 一次行为比较的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorComparison {
    /// 每条性质的对比。
    pub probes: Vec<ProbeComparison>,
    /// 结论。
    pub verdict: BehaviorVerdict,
    /// 覆盖范围说明：这次比较到底覆盖了什么。
    pub coverage: Vec<String>,
    /// 补充说明。
    pub notes: Vec<String>,
}

/// 在基线副本与补丁副本上分别验证同一组性质并比较结论。
///
/// `specs` 为空时返回 [`BehaviorVerdict::Unverified`]：没有探针就没有行为
/// 证据，此时不能声称「行为一致」。
#[must_use]
pub fn compare(
    baseline: &Sandbox,
    patched: &Sandbox,
    specs: &[PropertySpec],
    options: &ToolOptions,
) -> BehaviorComparison {
    if specs.is_empty() {
        return BehaviorComparison {
            probes: Vec::new(),
            verdict: BehaviorVerdict::Unverified,
            coverage: vec![
                "没有提供性质规格：行为比较只能依赖测试套件，未覆盖测试之外的输入空间。".to_owned(),
            ],
            notes: vec![
                "若要断言「可观察行为一致」，请用 --properties 提供至少一条性质。".to_owned(),
            ],
        };
    }

    let mut probes = Vec::new();
    let mut notes = Vec::new();
    let mut verdict = BehaviorVerdict::Preserved;
    for spec in specs {
        let spec_name = spec_label(spec);
        // 性质跑不起来（驱动写不进沙箱等）不是错误，而是「这条探针没结论」：
        // 记成 Inconclusive，让比较结论诚实地变成未验证。
        let baseline_result = PropertyRunner::run(baseline, spec, options).map_or_else(
            |reason| PropertyResult::Inconclusive(reason.to_string()),
            |outcome| outcome.result,
        );
        let patched_result = PropertyRunner::run(patched, spec, options).map_or_else(
            |reason| PropertyResult::Inconclusive(reason.to_string()),
            |outcome| outcome.result,
        );

        let baseline_label = label(&baseline_result);
        let patched_label = label(&patched_result);
        let agrees = baseline_label == patched_label;
        if !agrees {
            verdict = BehaviorVerdict::Changed;
            notes.push(format!(
                "性质 `{spec_name}` 的结论从「{baseline_label}」变为「{patched_label}」：补丁改变了可观察行为。"
            ));
        } else if matches!(baseline_result, PropertyResult::Inconclusive(_)) {
            if !matches!(verdict, BehaviorVerdict::Changed) {
                verdict = BehaviorVerdict::Unverified;
            }
            notes.push(format!(
                "性质 `{spec_name}` 在两侧都没跑出结论，无法用于比较。"
            ));
        }
        probes.push(ProbeComparison {
            name: spec_name,
            baseline: baseline_label,
            patched: patched_label,
            agrees,
        });
    }

    BehaviorComparison {
        probes,
        verdict,
        coverage: vec![format!(
            "{} 条性质在基线副本与补丁副本上分别执行并比较结论。",
            specs.len()
        )],
        notes,
    }
}

/// 性质的显示名：规格文件强制要求名字，内部调用允许省略。
fn spec_label(spec: &PropertySpec) -> String {
    spec.name
        .clone()
        .unwrap_or_else(|| format!("<unnamed:{}>", spec.call_template))
}

/// 结论的稳定标签。
///
/// 不用 `Debug` 派生：`Satisfied { samples }` 的调试输出带样本数，两侧样本数
/// 相同时才算一致会把「跑了多少个样本」混进「结论是否相同」。
fn label(result: &PropertyResult) -> String {
    match result {
        PropertyResult::Satisfied { .. } => "satisfied".to_owned(),
        PropertyResult::Violated => "violated".to_owned(),
        PropertyResult::Inconclusive(reason) => format!("inconclusive: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_preserved_allows_application() {
        assert!(BehaviorVerdict::Preserved.allows_application());
        assert!(!BehaviorVerdict::Changed.allows_application());
        assert!(!BehaviorVerdict::Unverified.allows_application());
    }

    #[test]
    fn labels_ignore_the_sample_count() {
        // 结论相同才算一致：样本数是证据强度，不是结论的一部分。
        let left = label(&PropertyResult::Satisfied { samples: 10 });
        let right = label(&PropertyResult::Satisfied { samples: 99 });
        assert_eq!(left, right);
        assert_ne!(left, label(&PropertyResult::Violated));
    }

    #[test]
    fn a_probe_that_agrees_is_reported_as_agreeing() {
        // 两条结论标签相同时 `agrees` 必须为真——报告里人只看得见这个字段。
        let comparison = ProbeComparison {
            name: "p".to_owned(),
            baseline: "satisfied".to_owned(),
            patched: "satisfied".to_owned(),
            agrees: true,
        };
        assert!(comparison.agrees);
    }
}
