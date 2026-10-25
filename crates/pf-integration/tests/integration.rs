//! 跨 crate 集成测试。
//!
//! 这些测试覆盖需求文档 14.3 的 MVP 成功标准中可以用静态分析验证的部分：
//! 分析真实项目、输出可定位问题、识别过度设计案例、区分三种门禁状态。

use std::path::Path;

use pf_core::config::{Config, ReportFormat};
use pf_core::status::{EvidenceStatus, GateStatus};
use pf_review::ReviewEngine;

/// 仓库内 `fixtures/` 目录的绝对路径。
///
/// 夹具作为真实文件提交，而不是在测试里拼字符串：这样它们可以被
/// `proofforge inspect fixtures/over-engineered` 直接跑，也能在评审时当普通
/// 代码阅读。`CARGO_MANIFEST_DIR` 在编译期确定，因此测试不依赖工作目录。
fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .canonicalize()
        .expect("fixtures directory")
}

/// 带有已知问题的 Rust 项目（`fixtures/over-engineered`）。
fn fixture_project() -> std::path::PathBuf {
    fixtures_dir().join("over-engineered")
}

/// 没有已知问题的 Rust 项目（`fixtures/clean`）。
fn clean_project() -> std::path::PathBuf {
    fixtures_dir().join("clean")
}

#[test]
fn analyzes_real_project_and_locate_findings() {
    let dir = fixture_project();
    let report = ReviewEngine::review(&Config::for_target(&dir)).expect("review");

    assert!(report.file_count >= 3, "should index all fixture files");
    assert!(!report.findings.is_empty(), "should find the seeded issues");

    // 每条发现都必须能定位到具体文件与行号（FR-064）。
    for finding in &report.findings {
        assert!(
            !finding.locations.is_empty(),
            "finding without location: {}",
            finding.title
        );
        for location in &finding.locations {
            assert_ne!(location.path, "", "位置必须指向具体文件");
            assert!(
                location.start_line.is_some() && location.start_column.is_some(),
                "location without line info: {}",
                location.display()
            );
        }
        // 结论必须带证据状态，且不能把猜测说成已验证。
        assert!(matches!(
            finding.status,
            EvidenceStatus::Suspected | EvidenceStatus::StaticConfirmed
        ));
    }
}

#[test]
fn detects_seeded_over_engineering_cases() {
    let dir = fixture_project();
    let report = ReviewEngine::review(&Config::for_target(&dir)).expect("review");

    // 按规则 ID 而不是 FindingKind 断言：`function-too-long` 与
    // `too-many-parameters` 共用 `FunctionTooComplex` 这一种类型，只有规则 ID
    // 能把它们区分开。
    let rules: Vec<&str> = report.findings.iter().map(|f| f.rule_id.get()).collect();

    // 人工构造的过度设计案例必须被识别（需求文档 14.3）。
    for expected in [
        "builtin.single-implementation-trait",
        "builtin.forwarding-wrapper",
        "builtin.needless-factory",
        "builtin.circular-dependency",
        "builtin.public-api-missing-docs",
        "builtin.naming-convention",
        "builtin.too-many-parameters",
        "builtin.redundant-comment",
        "builtin.duplicate-code",
        "builtin.over-configuration",
        "builtin.over-modularization",
        "builtin.boilerplate-code",
    ] {
        assert!(rules.contains(&expected), "missing {expected}: {rules:?}");
    }

    // 过度设计类判定只能给结构事实，必须标记为需要人工确认（需求文档 7.3）。
    for finding in &report.findings {
        if finding.rule_id.get().starts_with("builtin.over-")
            || finding.rule_id.get() == "builtin.boilerplate-code"
            || finding.rule_id.get() == "builtin.single-implementation-trait"
        {
            assert!(
                finding
                    .facts
                    .iter()
                    .any(|(key, value)| key == "needs_human_confirmation" && value == "true"),
                "{} 缺少人工确认标记",
                finding.rule_id.get()
            );
        }
    }

    // FR-020 的硬约束：样板代码只能描述质量，不能推断作者身份。
    for finding in report
        .findings
        .iter()
        .filter(|finding| finding.rule_id.get() == "builtin.boilerplate-code")
    {
        let text = format!("{}{}", finding.title, finding.detail);
        assert!(
            !text.contains("AI") && !text.contains("生成"),
            "样板代码结论不得暗示作者身份：{text}"
        );
    }
}

#[test]
fn clean_project_passes_the_gate() {
    let dir = clean_project();
    let report = ReviewEngine::review(&Config::for_target(&dir)).expect("review");
    assert_eq!(
        report.gate,
        GateStatus::Passed,
        "findings: {:?}",
        report.findings
    );
}

#[test]
fn gate_distinguishes_pass_from_insufficient_evidence() {
    // 空目录：没有可分析文件，因此是「证据不足」而不是「通过」。
    let empty = tempfile::tempdir().expect("tempdir");
    let report = ReviewEngine::review(&Config::for_target(empty.path())).expect("review");
    assert_eq!(report.gate, GateStatus::InsufficientEvidence);
}

#[test]
fn missing_target_is_an_error_not_an_empty_pass() {
    let config = Config::for_target(Path::new("/definitely/not/a/real/path"));
    assert!(ReviewEngine::review(&config).is_err());
}

#[test]
fn report_renders_in_every_supported_format() {
    let dir = fixture_project();
    let report = ReviewEngine::review(&Config::for_target(&dir)).expect("review");

    for format in [
        ReportFormat::Terminal,
        ReportFormat::Json,
        ReportFormat::Sarif,
    ] {
        let rendered = pf_report::render(&report, format).expect("render");
        assert!(!rendered.is_empty(), "{format:?} produced empty output");
    }

    // JSON 必须可被解析。
    let json = pf_report::render(&report, ReportFormat::Json).expect("json");
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
    assert_eq!(value["schema"], "proofforge/report/1");
    assert!(value["findings"]
        .as_array()
        .is_some_and(|items| !items.is_empty()));

    // SARIF 必须是 2.1.0。
    let sarif = pf_report::render(&report, ReportFormat::Sarif).expect("sarif");
    let value: serde_json::Value = serde_json::from_str(&sarif).expect("parse");
    assert_eq!(value["version"], "2.1.0");
}

#[test]
fn quality_layers_are_populated_independently() {
    let dir = fixture_project();
    let report = ReviewEngine::review(&Config::for_target(&dir)).expect("review");

    // 分层统计之和必须等于发现总数（第七章 7.7：各层独立，不互相抵消）。
    let total = report.layers.correctness.count
        + report.layers.engineering.count
        + report.layers.maintainability.count
        + report.layers.verification.count;
    assert_eq!(total, report.findings.len());

    // 架构类问题计入正确性与安全层。
    assert!(
        report.layers.correctness.count > 0,
        "cycle detection should land in layer 1"
    );
}

#[test]
fn evidence_artifacts_are_hash_verifiable() {
    let dir = fixture_project();
    let report = ReviewEngine::review(&Config::for_target(&dir)).expect("review");
    assert_ne!(report.evidence.len(), 0, "审查必须留下可复现证据");
    for artifact in &report.evidence {
        assert!(
            artifact.verify(),
            "evidence digest mismatch: {}",
            artifact.kind
        );
    }
}

#[test]
fn report_is_deterministic_for_the_same_input() {
    let dir = fixture_project();
    let config = Config::for_target(&dir);
    let first = pf_report::render(
        &ReviewEngine::review(&config).expect("a"),
        ReportFormat::Json,
    )
    .expect("render a");
    let second = pf_report::render(
        &ReviewEngine::review(&config).expect("b"),
        ReportFormat::Json,
    )
    .expect("render b");
    // 发现内容与顺序必须稳定（运行 ID 与耗时除外）。
    let strip = |text: &str| {
        text.lines()
            .filter(|line| !line.contains("\"run_id\"") && !line.contains("duration"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(strip(&first), strip(&second));
}

/// 在临时目录里造一个带两次提交的 Git 仓库，第二次只改动 `base.rs`。
///
/// `top.rs` 从第一次提交起就依赖 `base`，因此它必须出现在影响集里。仓库用
/// `git init` 现造：ProofForge 自己的工作目录不是 Git 仓库，而这个路径只能
/// 在真仓库上验证。
fn git_project_with_one_changed_file() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    for (relative, content) in [
        (
            "Cargo.toml",
            "[package]\nname = \"impact_demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        ),
        (
            "src/lib.rs",
            "//! 演示变更影响分析。\npub mod base;\npub mod top;\n",
        ),
        ("src/base.rs", "/// 基础函数。\npub fn base() -> i64 { 1 }\n"),
        (
            "src/top.rs",
            "//! 顶层，依赖 base。\nuse crate::base::base;\npub fn top() -> i64 { base() + 1 }\n",
        ),
    ] {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, content).expect("write");
    }
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "test"],
        vec!["add", "-A"],
        vec!["commit", "-qm", "init"],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(root)
            .output()
            .expect("run git");
        assert!(status.status.success(), "git {args:?} failed");
    }
    // 只改 base.rs：top.rs 不变，但它依赖 base，因此必须被算作受影响。
    std::fs::write(
        root.join("src/base.rs"),
        "/// 基础函数（已修改）。\npub fn base() -> i64 { 2 }\n",
    )
    .expect("rewrite base.rs");
    dir
}

#[test]
fn impact_analysis_reports_the_files_that_depend_on_the_change() {
    // FR-025：影响集是模块依赖图上的反向可达集，而图必须建在**全部**文件上。
    // 只用变更文件建图时，依赖它们的文件不在图里，影响集恒为空——报告会写
    // 「可能波及 0 个文件」，一句永远为真的废话。
    let dir = git_project_with_one_changed_file();
    let mut config = Config::for_target(dir.path());
    config.baseline = Some("HEAD".to_owned());
    let report = ReviewEngine::review(&config).expect("review");

    let impact = report.impact.as_ref().expect("配了基线就必须有影响分析");
    assert_eq!(impact.changed, vec!["src/base.rs".to_owned()], "{impact:?}");
    assert_eq!(
        impact.impacted,
        vec!["src/top.rs".to_owned()],
        "依赖 base 的 top.rs 必须出现在影响集里：{impact:?}"
    );
    assert!(
        !impact.impacted.contains(&"src/base.rs".to_owned()),
        "变更文件本身不算受影响：{impact:?}"
    );
}

#[test]
fn baseline_review_keeps_rules_scoped_to_the_changed_files() {
    // 影响分析与规则执行是两件事：图按全项目建，规则只碰变更文件。混在一起会让
    // 使用者以为未变更的文件也被审查过了。
    let dir = git_project_with_one_changed_file();
    let mut config = Config::for_target(dir.path());
    config.baseline = Some("HEAD".to_owned());
    let report = ReviewEngine::review(&config).expect("review");

    assert!(report.impact.is_some());
    assert_eq!(report.file_count, 1, "本轮只分析变更文件：{report:?}");
}
