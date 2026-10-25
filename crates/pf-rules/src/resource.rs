//! 资源管理检查（需求文档 FR-010）。
//!
//! # 检查什么
//!
//! 四个可以**在语法树上独立论证**的资源风险信号，全部限定 Rust：
//!
//! | 信号 | 为什么是风险 |
//! |---|---|
//! | `std::mem::forget` | 明确跳过析构，被忘记的值持有的资源不会释放 |
//! | `Box::leak` | 把堆内存泄漏成 `'static`，规避借用检查的常见手法 |
//! | `Box::into_raw` 而无配对的 `Box::from_raw` | 裸指针交出去后没人重建 `Box`，内存不会归还 |
//! | 库代码里的 `std::process::exit` | 直接终止进程，栈上所有值的析构全部跳过 |
//!
//! # 为什么只有这四个
//!
//! 「锁使用错误」与「生命周期风险」需要过程间分析与类型信息，当前拿不到：前者要
//! 知道哪个 guard 在哪个分支里被持有，后者要跨函数跟踪引用的存活期。用文本匹配
//! 去猜这两类问题会大量假阳性，而假阳性会让使用者学会忽略整条规则——那比漏报更
//! 糟。缺口记录在 `docs/known-limitations.md`。
//!
//! # 证据强度分两档
//!
//! `mem::forget`、`Box::leak`、`process::exit` 是**确定**的结构事实：那一行确实
//! 这么写了，因此记 `StaticConfirmed`。`Box::into_raw` 不是——它是否是泄漏取决于
//! 调用方有没有把指针还回来，而调用方在另一个文件里，因此记 `Suspected` 并要求
//! 人工确认（AGENTS.md 安全红线 5）。

use pf_core::finding::{Finding, FindingKind, Remediation};
use pf_core::ids::RuleId;
use pf_core::location::Location;
use pf_core::rule::{Rule, RuleSeverity};

use super::{is_rust_test_code, is_test_path, location_for, Checker, Context, RUST_EXTENSION};

/// 确定会跳过析构的调用（函数名 → 说明）。
///
/// 按名字最后一段匹配，因此 `std::mem::forget(x)` 与 `mem::forget(x)` 都命中。
const LEAKING_CALLS: &[(&str, &str)] = &[
    ("forget", "`std::mem::forget` 明确跳过析构函数"),
    ("leak", "`Box::leak` 把堆内存泄漏成 `'static`"),
];

/// FR-010：资源管理检查。
pub struct ResourceManagementChecker;

impl Checker for ResourceManagementChecker {
    fn rule(&self) -> Rule {
        Rule::builtin(
            RuleId::builtin("resource-management"),
            "库代码不得跳过析构或直接终止进程",
            RuleSeverity::Project,
            "resource_management",
        )
    }

    fn check(&self, context: &Context<'_>) -> Vec<Finding> {
        let rule = self.rule();
        let mut findings = Vec::new();
        for (path, tree) in context.trees() {
            if !path.ends_with(RUST_EXTENSION) || is_test_path(path) {
                continue;
            }
            let Some(file) = context.source(path) else {
                continue;
            };
            for node in tree.descendants() {
                if node.kind() != "call_expression" || is_rust_test_code(tree, &node) {
                    continue;
                }
                let Some(callee) = node.child_by_field_name("function") else {
                    continue;
                };
                let Ok(name) = tree.node_text(&callee) else {
                    continue;
                };
                let short = short_name(name);
                if let Some((_, why)) = LEAKING_CALLS.iter().find(|(call, _)| *call == short) {
                    findings.push(super::confirmed(
                        Finding::new(
                            rule.id.clone(),
                            FindingKind::ResourceLeak,
                            format!("`{path}` 里的 `{name}()` 会跳过析构"),
                            format!(
                                "{why}。被它处理的值所持有的资源——堆内存、文件描述符、\
                                     锁——不会在本作用域结束时释放。如果这是有意的\
                                     （例如把数据移交给 C 侧），请用注释说明所有权的去向。",
                            ),
                            vec![location_for(file, &callee)],
                        )
                        .with_severity(rule.effective_severity())
                        .with_fact("callee", name.to_owned())
                        .with_remediation(Remediation::new(
                            "让值正常离开作用域以触发析构；确需转移所有权时，\
                                 在注释里写明谁负责释放",
                        )),
                    ));
                    continue;
                }
                if short == "exit" {
                    findings.push(super::confirmed(
                        Finding::new(
                            rule.id.clone(),
                            FindingKind::ResourceLeak,
                            format!("`{path}` 里的 `{name}()` 会直接终止进程"),
                            "`std::process::exit` 不返回值，而是立即终止进程：栈上所有\
                                 局部变量的析构函数都不会运行，已打开的文件与持有的锁\
                                 不会被清理。库代码应当把「要退出」这个决定交回调用方。"
                                .to_owned(),
                            vec![location_for(file, &callee)],
                        )
                        .with_severity(rule.effective_severity())
                        .with_fact("callee", name.to_owned())
                        .with_remediation(Remediation::new(
                            "返回错误让调用方决定是否退出；确需终止时说明为什么\
                                 跳过析构是可接受的",
                        )),
                    ));
                }
            }
        }
        findings.extend(raw_box_findings(context, &rule));
        findings
    }
}

/// 检查 `Box::into_raw` 是否有配对的 `Box::from_raw`。
///
/// 判据刻意保守：只有**同一个文件里**两条都找不到时才报。跨文件看不到调用方，
/// 而「调用方会不会把指针还回来」正是这条发现唯一不确定的地方——因此证据状态是
/// `Suspected` 并要求人工确认，而不是 `StaticConfirmed`。
fn raw_box_findings(context: &Context<'_>, rule: &Rule) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (path, tree) in context.trees() {
        if !path.ends_with(RUST_EXTENSION) || is_test_path(path) {
            continue;
        }
        let Some(file) = context.source(path) else {
            continue;
        };
        let mut has_into_raw = false;
        let mut has_from_raw = false;
        let mut raw_locations: Vec<Location> = Vec::new();
        for node in tree.descendants() {
            if node.kind() != "call_expression" || is_rust_test_code(tree, &node) {
                continue;
            }
            let Some(callee) = node.child_by_field_name("function") else {
                continue;
            };
            let Ok(name) = tree.node_text(&callee) else {
                continue;
            };
            match short_name(name) {
                "into_raw" => {
                    has_into_raw = true;
                    raw_locations.push(location_for(file, &callee));
                }
                "from_raw" => has_from_raw = true,
                _ => {}
            }
        }
        if !has_into_raw || has_from_raw {
            continue;
        }
        for location in raw_locations {
            findings.push(
                Finding::new(
                    rule.id.clone(),
                    FindingKind::ResourceLeak,
                    format!("`{path}` 把 `Box` 转成了裸指针，但同一文件里没有 `Box::from_raw`"),
                    format!(
                        "`{path}` 调用了 `Box::into_raw`，却没有在同一个文件里看到配对的\
                         `Box::from_raw`。裸指针交出去后如果没有被重新构造成 `Box`，\
                         这块堆内存永远不会被释放——FFI 边界是正当用法，但那要求\
                         接收方确实负责归还，而这一点在单个文件里看不出来。",
                    ),
                    vec![location],
                )
                .with_severity(rule.effective_severity())
                .with_fact("callee", "Box::into_raw")
                .with_fact("needs_human_confirmation", "true")
                .with_remediation(Remediation::new(
                    "确认裸指针的接收方会调用 `Box::from_raw` 归还内存；若否，\
                     改用引用或显式的所有权转移类型",
                )),
            );
        }
    }
    findings
}

/// 取限定名的最后一段（`std::mem::forget` → `forget`）。
fn short_name(name: &str) -> &str {
    name.rsplit("::")
        .next()
        .unwrap_or(name)
        .rsplit('.')
        .next()
        .unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::testing::with_context;

    #[test]
    fn mem_forget_is_confirmed_as_a_resource_risk() {
        with_context(
            &[(
                "src/lib.rs",
                "use std::mem;\n\npub fn drop_it(value: String) {\n    mem::forget(value);\n}\n",
            )],
            |context| {
                let findings = ResourceManagementChecker.check(context);
                assert_eq!(findings.len(), 1, "{findings:?}");
                assert_eq!(findings[0].kind, FindingKind::ResourceLeak);
                // 语法树上确实有这一次调用：可复算的事实，因此是 StaticConfirmed。
                assert_eq!(
                    findings[0].status,
                    pf_core::status::EvidenceStatus::StaticConfirmed
                );
            },
        );
    }

    #[test]
    fn box_leak_is_reported() {
        with_context(
            &[(
                "src/lib.rs",
                "pub fn escape() -> &'static str {\n    Box::leak(String::from(\"x\").into_boxed_str())\n}\n",
            )],
            |context| {
                let findings = ResourceManagementChecker.check(context);
                assert_eq!(findings.len(), 1, "{findings:?}");
                assert!(findings[0].title.contains("leak"), "{}", findings[0].title);
            },
        );
    }

    #[test]
    fn process_exit_in_library_code_is_reported() {
        with_context(
            &[(
                "src/lib.rs",
                "pub fn bail() {\n    std::process::exit(1);\n}\n",
            )],
            |context| {
                let findings = ResourceManagementChecker.check(context);
                assert_eq!(findings.len(), 1, "{findings:?}");
                assert!(
                    findings[0].detail.contains("析构"),
                    "{}",
                    findings[0].detail
                );
            },
        );
    }

    #[test]
    fn into_raw_without_from_raw_is_only_suspected() {
        // 调用方在另一个文件里，看不到它是否归还内存，因此不能声称已确认。
        with_context(
            &[(
                "src/lib.rs",
                "pub fn hand_over(value: Box<i32>) -> *mut i32 {\n    Box::into_raw(value)\n}\n",
            )],
            |context| {
                let findings = ResourceManagementChecker.check(context);
                assert_eq!(findings.len(), 1, "{findings:?}");
                assert_eq!(
                    findings[0].status,
                    pf_core::status::EvidenceStatus::Suspected
                );
                assert_eq!(
                    findings[0]
                        .facts
                        .iter()
                        .find(|(k, _)| k == "needs_human_confirmation")
                        .map(|(_, v)| v.as_str()),
                    Some("true")
                );
            },
        );
    }

    #[test]
    fn into_raw_with_a_paired_from_raw_is_not_reported() {
        with_context(
            &[(
                "src/lib.rs",
                "pub fn hand_over(value: Box<i32>) -> *mut i32 {\n    Box::into_raw(value)\n}\n\npub unsafe fn take_back(ptr: *mut i32) -> Box<i32> {\n    Box::from_raw(ptr)\n}\n",
            )],
            |context| {
                assert_eq!(ResourceManagementChecker.check(context), []);
            },
        );
    }

    #[test]
    fn test_code_is_exempt() {
        // 测试里主动泄漏或退出是常见写法，报出来只会淹没真正的发现。
        with_context(
            &[(
                "src/lib.rs",
                "#[test]\nfn t() {\n    std::mem::forget(String::new());\n}\n",
            )],
            |context| {
                assert_eq!(ResourceManagementChecker.check(context), []);
            },
        );
    }

    #[test]
    fn unrelated_calls_are_not_reported() {
        with_context(
            &[("src/lib.rs", "pub fn f(v: Vec<i32>) -> usize { v.len() }\n")],
            |context| {
                assert_eq!(ResourceManagementChecker.check(context), []);
            },
        );
    }
}
