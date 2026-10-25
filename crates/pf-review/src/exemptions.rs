//! 豁免的加载与应用（FR-062）。
//!
//! # 为什么单独一个模块
//!
//! 豁免横跨三件事：从文件里读出来（格式判断、错误上报）、判断哪一条仍然有效
//! （参考日期）、以及把它贴到发现上（报告与门禁）。三件事的失败模式不同——
//! 读不出来要让人知道「你的豁免没生效」，过期了也要让人知道——因此集中在一处
//! 处理，而不是散在编排流程里。
//!
//! # 诚实的边界
//!
//! 豁免只改变发现的**呈现与门禁权重**，不删除发现，也不改标题、位置或严重
//! 程度。报告里仍然看得到这条发现，只是多一行「已豁免：<理由>（批准人：…）」。
//! 静默删除会让「我们到底压掉了多少问题」无从审计。

use std::path::{Path, PathBuf};

use pf_core::exempt::{Exemption, ExemptionSet};

/// 从配置列出的文件里加载全部豁免。
///
/// 相对路径按项目根目录解析：豁免是项目级决策，跟着项目走而不是跟着当前工作
/// 目录走。
///
/// 加载失败不会让整体审查失败——一份写错的豁免文件不应该让整个仓库失去审查
/// 结果。失败会以警告的形式进入报告，调用方因此知道「本该生效的豁免没有生效」。
/// 这正是红线 4 的要求：不把「没做到」说成「做到了」。
pub(crate) fn load(root: &Path, files: &[PathBuf], warnings: &mut Vec<String>) -> ExemptionSet {
    let mut set = ExemptionSet::new();
    for file in files {
        let path = if file.is_absolute() {
            file.clone()
        } else {
            root.join(file)
        };
        match load_one(&path) {
            Ok(loaded) => {
                for exemption in loaded {
                    set.push(exemption);
                }
            }
            Err(reason) => warnings.push(format!(
                "豁免文件 {} 加载失败（{reason}），其中的豁免没有生效。\
                 请修正该文件后重跑，不要依赖未生效的豁免。",
                path.display()
            )),
        }
    }
    set
}

/// 按扩展名选择解析器加载一个豁免文件。
///
/// # Errors
/// 读文件失败、格式不受支持、或某条豁免缺少理由/批准人时返回错误原因。
fn load_one(path: &Path) -> Result<Vec<Exemption>, String> {
    let extension = path
        .extension()
        .map_or_else(String::new, |ext| ext.to_string_lossy().to_lowercase());
    let set = match extension.as_str() {
        "toml" => pf_rules::exemptions_from_toml_file(path).map_err(|reason| reason.to_string())?,
        "yaml" | "yml" => {
            pf_rules::exemptions_from_yaml_file(path).map_err(|reason| reason.to_string())?
        }
        other => {
            return Err(format!(
                "不支持的豁免文件格式 `.{other}`；只支持 .toml 与 .yaml/.yml"
            ));
        }
    };
    Ok(set.exemptions)
}

/// 把生效的豁免贴到发现上，并返回被压制的发现数量。
///
/// `reference_date` 是判断过期用的「今天」（ISO `YYYY-MM-DD`），由调用方传入
/// 而不是读系统时钟：库代码因此保持确定性与可测试性。
pub(crate) fn apply(
    set: &ExemptionSet,
    findings: &mut [pf_core::finding::Finding],
    reference_date: &str,
    warnings: &mut Vec<String>,
) -> usize {
    // 过期豁免先出声：它意味着「之前被压住的问题已经悄悄回到报告里了」。
    for exemption in set.expired(reference_date) {
        warnings.push(format!(
            "规则 `{}` 的豁免已于 {} 过期（批准人：{}）。该豁免不再生效，\
             它覆盖的问题会重新出现在报告里。",
            exemption.rule_id.get(),
            exemption.expires_on.as_deref().unwrap_or("未知日期"),
            exemption.granted_by
        ));
    }

    let mut suppressed = 0;
    for finding in findings.iter_mut() {
        let matching: Vec<&Exemption> = set.matching(finding, reference_date);
        if let Some(exemption) = matching.into_iter().next() {
            finding.exemption = Some(exemption.clone());
            suppressed += 1;
        }
    }
    suppressed
}

/// 把豁免集合的规模写入证据产物的一句话摘要。
///
/// 进证据而不只进警告：审计时需要回答「这次运行加载了多少条豁免、压制了多少
/// 发现」（FR-070）。
pub(crate) fn describe(set: &ExemptionSet, suppressed: usize) -> String {
    format!(
        "exemptions={} suppressed={}",
        set.exemptions.len(),
        suppressed
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::exempt::{ExemptionError, ExemptionScope};
    use pf_core::finding::{Finding, FindingKind};
    use pf_core::ids::RuleId;
    use pf_core::location::{Location, Span};

    fn finding(rule: &str, path: &str, line: u32) -> Finding {
        Finding::new(
            RuleId::builtin(rule),
            FindingKind::UselessAbstraction,
            "t",
            "d",
            vec![Location::new(path.to_owned(), Span::new(0, 1)).with_lines(line, 1, line, 2)],
        )
    }

    fn exemption(scope: ExemptionScope, expires_on: Option<&str>) -> Exemption {
        Exemption {
            rule_id: RuleId::builtin("single-implementation-trait"),
            scope,
            reason: "对外契约".to_owned(),
            granted_by: "alice".to_owned(),
            expires_on: expires_on.map(str::to_owned),
        }
    }

    #[test]
    fn an_unsupported_extension_is_reported_not_silently_ignored() {
        // 写错扩展名的豁免文件必须让人知道它没生效（红线 4）。
        let mut warnings = Vec::new();
        let set = load(
            Path::new("."),
            &[PathBuf::from("exemptions.json")],
            &mut warnings,
        );
        assert_eq!(set.exemptions.len(), 0);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("不支持的豁免文件格式"));
    }

    #[test]
    fn an_expired_exemption_is_reported_and_stops_applying() {
        let mut set = ExemptionSet::new();
        set.push(exemption(ExemptionScope::Rule, Some("2026-01-01")));
        let mut findings = vec![finding("single-implementation-trait", "src/a.rs", 3)];
        let mut warnings = Vec::new();

        let suppressed = apply(&set, &mut findings, "2026-06-01", &mut warnings);

        assert_eq!(suppressed, 0, "过期豁免不得压制发现");
        assert!(!findings[0].is_exempted());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("过期"), "{}", warnings[0]);
        assert!(warnings[0].contains("2026-01-01"), "{}", warnings[0]);
    }

    #[test]
    fn a_live_exemption_suppresses_without_hiding_the_finding() {
        let mut set = ExemptionSet::new();
        set.push(exemption(ExemptionScope::Rule, None));
        let mut findings = vec![finding("single-implementation-trait", "src/a.rs", 3)];
        let mut warnings = Vec::new();

        let suppressed = apply(&set, &mut findings, "2026-06-01", &mut warnings);

        assert_eq!(suppressed, 1);
        assert!(warnings.is_empty(), "生效中的豁免不应产生警告");
        let exemption = findings[0].exemption.as_ref().expect("exemption");
        assert_eq!(exemption.granted_by, "alice");
        // 发现本身没有被改写：标题、位置、严重程度都还在。
        assert_eq!(findings[0].title, "t");
        assert_eq!(findings[0].locations[0].path, "src/a.rs");
    }

    #[test]
    fn describe_reports_both_totals() {
        let mut set = ExemptionSet::new();
        set.push(exemption(ExemptionScope::Rule, None));
        assert_eq!(describe(&set, 1), "exemptions=1 suppressed=1");
    }

    #[test]
    fn reason_of_keeps_the_user_facing_message() {
        let error = ExemptionError::MissingReason {
            rule_id: "builtin.x".to_owned(),
        };
        assert!(error.to_string().contains("builtin.x"));
    }
}
