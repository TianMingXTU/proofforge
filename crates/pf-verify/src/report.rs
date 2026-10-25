//! 验证报告（FR-064、FR-065）。
//!
//! # 三条硬规则
//!
//! 1. **「没跑起来」不是「通过」**。工具缺失、编译失败、超时一律记
//!    [`pf_core::status::EvidenceStatus::Inconclusive`]，门禁据此返回
//!    `InsufficientEvidence`（AGENTS.md 安全红线 4）。
//! 2. **「未观察到违反」不是「程序正确」**。样本是有限的，报告措辞不能超过
//!    证据的强度（红线 5）。
//! 3. **反例必须附带复现配方**。没有复现途径的结论不可审计。

use pf_core::evidence::EvidenceArtifact;
use pf_core::status::{EvidenceStatus, GateStatus};
use serde::Serialize;

use crate::metamorphic::{MetamorphicOutcome, MetamorphicResult};
use crate::property::{PropertyOutcome, PropertyResult};

/// 一次验证运行的汇总。
#[derive(Debug, Clone, Default, Serialize)]
pub struct VerificationReport {
    /// 被验证的性质条目。
    pub entries: Vec<VerifiedProperty>,
    /// 被验证的蜕变关系条目（FR-039）。
    ///
    /// 与 `entries` 分开而不是混成一个列表：一条性质断言「输出应该是什么」，
    /// 一条蜕变关系断言「两个相关输入的输出之间有什么关系」。混在一起会让
    /// 读者分不清哪条结论的预言机是绝对值、哪条是相对关系。
    pub relations: Vec<VerifiedRelation>,
    /// 运行过程中产生的警告。
    pub warnings: Vec<String>,
}

/// 一条蜕变关系的验证结论。
#[derive(Debug, Clone, Serialize)]
pub struct VerifiedRelation {
    /// 关系名称（若有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 被验证的目标（调用模板）。
    pub target: String,
    /// 输入变换。
    pub transform: String,
    /// 输出关系。
    pub relation: String,
    /// 结论。
    pub verdict: Verdict,
    /// 证据状态。
    pub status: EvidenceStatus,
    /// 反例（已缩减）与复现配方。
    pub counterexample: Option<CounterexampleSummary>,
    /// 没有得到结论时的原因。
    pub reason: Option<String>,
}

/// 一条性质的验证结论。
#[derive(Debug, Clone, Serialize)]
pub struct VerifiedProperty {
    /// 性质名称（若有）；报告用它指认每一条性质。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 被验证的目标（调用模板）。
    pub target: String,
    /// 不变式。
    pub invariant: String,
    /// 结论。
    pub verdict: Verdict,
    /// 证据状态。
    pub status: EvidenceStatus,
    /// 反例（已缩减）与复现配方。
    pub counterexample: Option<CounterexampleSummary>,
    /// 没有得到结论时的原因。
    pub reason: Option<String>,
}

/// 一条性质的裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// 未观察到违反。
    NotViolated,
    /// 观察到违反。
    Violated,
    /// 没有得到结论。
    Inconclusive,
}

/// 反例摘要（完整内容在 [`crate::property::Counterexample`]）。
#[derive(Debug, Clone, Serialize)]
pub struct CounterexampleSummary {
    /// 缩减后的输入。
    pub input: String,
    /// 缩减前规模。
    pub original_size: usize,
    /// 缩减后规模。
    pub reduced_size: usize,
    /// 驱动程序在用户项目里的相对路径。
    pub driver_path: String,
    /// 可直接粘贴执行的命令。
    pub command: String,
    /// 驱动程序源码。
    ///
    /// 复现配方的设计目标是「不看 `ProofForge` 的文档也能重放」（见
    /// [`crate::property::ReproRecipe`）。少了源码，`command` 指向的是一个
    /// 沙箱里已被删除的文件，那条命令就不可执行——那等于没有配方。
    pub driver_source: String,
}

impl VerificationReport {
    /// 把一条性质验证结果收进报告。
    pub fn push(&mut self, outcome: &PropertyOutcome) {
        let (verdict, status, reason) = match &outcome.result {
            PropertyResult::Satisfied { .. } => {
                (Verdict::NotViolated, EvidenceStatus::NotReproduced, None)
            }
            PropertyResult::Violated => (Verdict::Violated, EvidenceStatus::Reproduced, None),
            PropertyResult::Inconclusive(reason) => (
                Verdict::Inconclusive,
                EvidenceStatus::Inconclusive,
                Some(reason.clone()),
            ),
        };
        self.entries.push(VerifiedProperty {
            name: outcome.spec.name.clone(),
            target: outcome.spec.call_template.clone(),
            invariant: outcome.spec.invariant.clone(),
            verdict,
            status,
            counterexample: outcome
                .counterexample
                .as_ref()
                .map(|example| CounterexampleSummary {
                    input: example.input.describe(),
                    original_size: example.reduction.original_size,
                    reduced_size: example.reduction.final_size,
                    driver_path: example.repro.driver_path.clone(),
                    command: example.repro.command.clone(),
                    driver_source: example.repro.driver_source.clone(),
                }),
            reason,
        });
    }

    /// 把一条蜕变关系验证结果收进报告。
    ///
    /// 结论映射与 [`Self::push`] 完全一致：同样的三种措辞、同样的证据状态。
    /// 两套验证共用一套判定顺序，报告才不会出现「性质说违反、蜕变说通过」这种
    /// 无法解释的分歧。
    pub fn push_metamorphic(&mut self, outcome: &MetamorphicOutcome) {
        let (verdict, status, reason) = match &outcome.result {
            MetamorphicResult::Satisfied { .. } => {
                (Verdict::NotViolated, EvidenceStatus::NotReproduced, None)
            }
            MetamorphicResult::Violated => (Verdict::Violated, EvidenceStatus::Reproduced, None),
            MetamorphicResult::Inconclusive(reason) => (
                Verdict::Inconclusive,
                EvidenceStatus::Inconclusive,
                Some(reason.clone()),
            ),
        };
        self.relations.push(VerifiedRelation {
            name: outcome.spec.name.clone(),
            target: outcome.spec.call_template.clone(),
            transform: outcome.spec.transform.clone(),
            relation: outcome.spec.relation.clone(),
            verdict,
            status,
            counterexample: outcome
                .counterexample
                .as_ref()
                .map(|example| CounterexampleSummary {
                    input: example.input.describe(),
                    original_size: example.reduction.original_size,
                    reduced_size: example.reduction.final_size,
                    driver_path: example.repro.driver_path.clone(),
                    command: example.repro.command.clone(),
                    driver_source: example.repro.driver_source.clone(),
                }),
            reason,
        });
    }

    /// 记录一条警告。
    pub fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    /// 是否观察到了任何违反。
    ///
    /// 性质与蜕变关系一起看：后者同样是一次真实执行得到的观测，漏掉它会让门禁
    /// 对一个已经复现的问题放行。
    #[must_use]
    pub fn has_violations(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.verdict == Verdict::Violated)
            || self
                .relations
                .iter()
                .any(|entry| entry.verdict == Verdict::Violated)
    }

    /// 是否存在没有得到结论的条目。
    #[must_use]
    pub fn has_inconclusive(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.verdict == Verdict::Inconclusive)
            || self
                .relations
                .iter()
                .any(|entry| entry.verdict == Verdict::Inconclusive)
    }

    /// 给出门禁结论。
    ///
    /// 判定顺序与 [`pf_core::review::decide_gate`] 一致：**已经复现的缺陷优先**。
    /// 一个真实观测到的违反不是「不可靠的证据」，它是直接观察；此时再宣称
    /// 「证据不足」会把已知的缺陷藏起来。证据不足只在「没有任何结论、也没有
    /// 任何违反」时出现——那意味着这次验证白跑了。
    ///
    /// 没能跑出结论的条目不会因此消失：它们仍然逐条列在 [`Self::entries`] 里，
    /// 终端输出与证据产物都会带上原因。
    #[must_use]
    pub fn gate(&self) -> GateStatus {
        if self.has_violations() {
            return GateStatus::Failed;
        }
        if self.has_inconclusive() {
            return GateStatus::InsufficientEvidence;
        }
        GateStatus::Passed
    }

    /// 对应的进程退出码。
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.gate().exit_code()
    }

    /// 生成证据产物：每条性质一条，内容是人类可读的结论摘要。
    ///
    /// 反例连同复现命令一起落盘，因此审计时不需要重新跑一遍验证也能知道
    /// 「当时看到的是什么」。
    #[must_use]
    pub fn evidence(&self) -> Vec<EvidenceArtifact> {
        self.entries
            .iter()
            .map(|entry| {
                let mut text = format!(
                    "target={} invariant={} verdict={:?} status={}\n",
                    entry.target, entry.invariant, entry.verdict, entry.status
                );
                if let Some(example) = &entry.counterexample {
                    text.push_str(&format!(
                        "counterexample={} (reduced from {} to {})\n\
                         repro={} {}\n--- {} ---\n{}\n--- end ---\n",
                        example.input,
                        example.original_size,
                        example.reduced_size,
                        example.driver_path,
                        example.command,
                        example.driver_path,
                        example.driver_source
                    ));
                }
                if let Some(reason) = &entry.reason {
                    text.push_str(&format!("reason={reason}\n"));
                }
                EvidenceArtifact::from_text("verification-property", text)
            })
            .chain(self.relations.iter().map(|entry| {
                let mut text = format!(
                    "metamorphic target={} transform={} relation={} verdict={:?} status={}\n",
                    entry.target, entry.transform, entry.relation, entry.verdict, entry.status
                );
                if let Some(example) = &entry.counterexample {
                    text.push_str(&format!(
                        "counterexample={} (reduced from {} to {})\n\
                         repro={} {}\n--- {} ---\n{}\n--- end ---\n",
                        example.input,
                        example.original_size,
                        example.reduced_size,
                        example.driver_path,
                        example.command,
                        example.driver_path,
                        example.driver_source
                    ));
                }
                if let Some(reason) = &entry.reason {
                    text.push_str(&format!("reason={reason}\n"));
                }
                EvidenceArtifact::from_text("verification-relation", text)
            }))
            .collect()
    }

    /// 导出成机器可读的 JSON。
    ///
    /// 与 [`Self::render_terminal`] 必须给出同一套结论：JSON 里多出来的只有
    /// `gate` 与 `exit_code`——它们是派生量，写进去是为了让消费方不必重新实现
    /// 一遍判定顺序（违反优先于证据不足）。
    ///
    /// # Errors
    /// 序列化失败时返回 [`pf_core::Error`]。报告里只有自有字段与字符串，
    /// 正常情况下不会失败。
    pub fn to_json(&self) -> Result<String, pf_core::Error> {
        #[derive(Serialize)]
        struct Snapshot<'a> {
            properties: &'a [VerifiedProperty],
            relations: &'a [VerifiedRelation],
            warnings: &'a [String],
            gate: GateStatus,
            exit_code: u8,
        }
        let snapshot = Snapshot {
            properties: &self.entries,
            relations: &self.relations,
            warnings: &self.warnings,
            gate: self.gate(),
            exit_code: self.exit_code(),
        };
        serde_json::to_string_pretty(&snapshot).map_err(|reason| pf_core::Error::Serialization {
            reason: reason.to_string(),
        })
    }

    /// 渲染成终端可读的文本。
    #[must_use]
    pub fn render_terminal(&self) -> String {
        let mut out = String::from("`ProofForge` 行为验证\n\n");
        if self.entries.is_empty() {
            out.push_str("没有验证任何性质。\n");
        }
        for entry in &self.entries {
            let label = match &entry.name {
                Some(name) => format!("{name}（{}）", entry.target),
                None => entry.target.clone(),
            };
            out.push_str(&format!(
                "[{}] {}\n    不变式：{}\n",
                entry.status, label, entry.invariant
            ));
            match entry.verdict {
                Verdict::NotViolated => out.push_str("    未观察到违反。\n"),
                Verdict::Violated => {
                    if let Some(example) = &entry.counterexample {
                        out.push_str(&format!(
                            "    反例：{}（规模 {} → {}）\n    复现：把源码保存为 {}，\
                             然后执行 `{}`\n    完整源码见 JSON 报告（\
                             counterexample.driver_source）或 --store 里的证据产物\n",
                            example.input,
                            example.original_size,
                            example.reduced_size,
                            example.driver_path,
                            example.command
                        ));
                    }
                }
                Verdict::Inconclusive => {
                    out.push_str(&format!(
                        "    证据不足：{}\n",
                        entry.reason.as_deref().unwrap_or("未知原因")
                    ));
                }
            }
        }
        for entry in &self.relations {
            let label = match &entry.name {
                Some(name) => format!("{name}（{}）", entry.target),
                None => entry.target.clone(),
            };
            out.push_str(&format!(
                "[蜕变][{}] {}\n    变换：{}\n    关系：{}\n",
                entry.status, label, entry.transform, entry.relation
            ));
            match entry.verdict {
                Verdict::NotViolated => out.push_str("    未观察到违反。\n"),
                Verdict::Violated => {
                    if let Some(example) = &entry.counterexample {
                        out.push_str(&format!(
                            "    反例：{}（规模 {} → {}）\n    复现：把源码保存为 {}，\
                             然后执行 `{}`\n    完整源码见 JSON 报告（\
                             relations[].counterexample.driver_source）或 --store 里的证据产物\n",
                            example.input,
                            example.original_size,
                            example.reduced_size,
                            example.driver_path,
                            example.command
                        ));
                    }
                }
                Verdict::Inconclusive => {
                    out.push_str(&format!(
                        "    证据不足：{}\n",
                        entry.reason.as_deref().unwrap_or("未知原因")
                    ));
                }
            }
        }
        out.push_str(&format!("\n门禁结论：{}\n", self.gate()));
        for warning in &self.warnings {
            out.push_str(&format!("警告：{warning}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{InputDomain, InputValue};
    use crate::property::{Counterexample, ReproRecipe};
    use crate::reduce::ReductionOutcome;

    fn outcome(result: PropertyResult) -> PropertyOutcome {
        PropertyOutcome {
            spec: crate::driver::PropertySpec {
                name: None,
                language: crate::driver::Language::Rust,
                imports: vec!["fixture".to_owned()],
                call_template: "fixture::parse({args})".to_owned(),
                domain: InputDomain::Int { min: 0, max: 9 },
                invariant: "output >= 0".to_owned(),
                samples: 4,
                seed: 1,
            },
            result,
            counterexample: None,
            probe: pf_core::evidence::ExecutionResult {
                command: pf_core::evidence::Command::new("cargo"),
                status: pf_core::evidence::ExecutionStatus::Exited,
                exit_code: Some(0),
                signal: None,
                duration_ms: 1,
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
            },
            reduction_rounds: Vec::new(),
        }
    }

    fn with_counterexample(mut base: PropertyOutcome) -> PropertyOutcome {
        base.counterexample = Some(Counterexample {
            input: InputValue::Int(-3),
            reduction: ReductionOutcome {
                value: InputValue::Int(-3),
                original_size: 12,
                final_size: 3,
                rounds: 4,
                converged: true,
            },
            repro: ReproRecipe {
                language: crate::driver::Language::Rust,
                driver_path: "tests/proofforge_driver.rs".to_owned(),
                driver_source: "// driver".to_owned(),
                command: "cargo test --test proofforge_driver".to_owned(),
            },
        });
        base
    }

    #[test]
    fn satisfied_property_is_not_reproduced_but_passes_the_gate() {
        let mut report = VerificationReport::default();
        report.push(&outcome(PropertyResult::Satisfied { samples: 4 }));
        assert!(!report.has_violations());
        assert_eq!(report.gate(), GateStatus::Passed);
        assert_eq!(report.entries[0].status, EvidenceStatus::NotReproduced);
    }

    #[test]
    fn violated_property_fails_the_gate_and_is_reproduced() {
        let mut report = VerificationReport::default();
        report.push(&with_counterexample(outcome(PropertyResult::Violated)));
        assert!(report.has_violations());
        assert_eq!(report.gate(), GateStatus::Failed);
        assert_eq!(report.entries[0].status, EvidenceStatus::Reproduced);
        let example = report.entries[0]
            .counterexample
            .as_ref()
            .expect("counterexample");
        assert_eq!(example.input, "-3");
        assert_eq!(example.original_size, 12);
        assert_eq!(example.reduced_size, 3);
        assert!(example.command.contains("cargo test"));
    }

    #[test]
    fn inconclusive_property_outranks_a_pass() {
        // 一条没跑出结论的性质必须让门禁变成「证据不足」，而不是被其他通过项
        // 抵消（AGENTS.md 安全红线 4）。
        let mut report = VerificationReport::default();
        report.push(&outcome(PropertyResult::Satisfied { samples: 4 }));
        report.push(&outcome(PropertyResult::Inconclusive(
            "toolchain unavailable".to_owned(),
        )));
        assert!(report.has_inconclusive());
        assert_eq!(report.gate(), GateStatus::InsufficientEvidence);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.entries[1].status, EvidenceStatus::Inconclusive);
        assert_eq!(
            report.entries[1].reason.as_deref(),
            Some("toolchain unavailable")
        );
    }

    #[test]
    fn violation_outranks_inconclusive() {
        // 已经复现的缺陷必须让门禁失败：此时「证据不足」只是附注，不是结论。
        let mut report = VerificationReport::default();
        report.push(&with_counterexample(outcome(PropertyResult::Violated)));
        report.push(&outcome(PropertyResult::Inconclusive("timeout".to_owned())));
        assert_eq!(report.gate(), GateStatus::Failed);
    }

    #[test]
    fn empty_report_passes() {
        // 没有性质可验证时不给「证据不足」：调用方没有要求验证任何事情。
        let report = VerificationReport::default();
        assert_eq!(report.gate(), GateStatus::Passed);
        assert!(report.render_terminal().contains("没有验证任何性质"));
    }

    #[test]
    fn evidence_carries_the_counterexample_and_repro_command() {
        let mut report = VerificationReport::default();
        report.push(&with_counterexample(outcome(PropertyResult::Violated)));
        let artifacts = report.evidence();
        assert_eq!(artifacts.len(), 1);
        assert!(artifacts[0].verify(), "证据摘要必须与内容一致");
        assert!(artifacts[0].content.contains("counterexample=-3"));
        assert!(artifacts[0]
            .content
            .contains("cargo test --test proofforge_driver"));
    }

    #[test]
    fn terminal_output_mentions_the_inconclusive_reason() {
        let mut report = VerificationReport::default();
        report.push(&outcome(PropertyResult::Inconclusive(
            "driver failed to compile".to_owned(),
        )));
        report.warn("sandbox skipped 2 files");
        let text = report.render_terminal();
        assert!(text.contains("证据不足"));
        assert!(text.contains("driver failed to compile"));
        assert!(text.contains("警告：sandbox skipped 2 files"));
    }

    #[test]
    fn terminal_output_uses_cautious_wording_for_satisfied_properties() {
        let mut report = VerificationReport::default();
        report.push(&outcome(PropertyResult::Satisfied { samples: 4 }));
        let text = report.render_terminal();
        assert!(text.contains("未观察到违反"));
        assert!(!text.contains("正确"), "不得把样本通过说成程序正确");
    }
}
