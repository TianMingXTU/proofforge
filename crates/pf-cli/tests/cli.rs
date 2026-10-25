//! 命令行入口的端到端测试。
//!
//! 这些测试启动真正的 `proofforge` 可执行文件，覆盖单元测试碰不到的部分：
//! 参数解析、退出码约定、进程间输出。夹具来自仓库根的 `fixtures/` 目录，
//! 与 `pf-integration` 共用同一批样本项目。

use std::path::PathBuf;
use std::process::Command;

/// 仓库内 `fixtures/` 目录的绝对路径。
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
        .canonicalize()
        .expect("fixtures directory")
}

/// 以 JSON 模式审查一个夹具项目，返回解析后的报告。
///
/// 同时校验退出码与门禁结论一致（需求文档 14.3）：这是 CLI 对外的核心契约，
/// 比「进程是否成功」更能说明问题——「检查跑完但发现了阻断问题」必须退出 1，
/// 而「检查没跑完」必须退出 2，两者都不是 0。
fn inspect_json(target: &std::path::Path) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["inspect", "--format", "json"])
        .arg(target)
        .output()
        .expect("spawn proofforge");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout must be JSON ({error}); stderr: {stderr}"));
    let expected = match report["gate"].as_str() {
        Some("passed") => Some(0),
        Some("failed") => Some(1),
        Some("insufficient_evidence") => Some(2),
        Some("cancelled") => Some(3),
        other => panic!("unknown gate in report: {other:?}"),
    };
    assert_eq!(output.status.code(), expected, "stderr: {stderr}");
    report
}

/// 断言一次审查属于「跑完了」的两个退出码之一。
///
/// 通过是 0、发现阻断问题是 1；2 与 3 意味着检查没跑完，那必须在各自的测试里
/// 单独断言，不能混在「渲染格式是否正确」的测试里被悄悄接受。
fn assert_review_completed(output: &std::process::Output) {
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "stderr: {stderr}"
    );
}

#[test]
fn inspect_emits_a_machine_readable_report() {
    let report = inspect_json(&fixture("over-engineered"));
    assert_eq!(report["schema"], "proofforge/report/1");
    // 夹具里植入了五类以上问题，全部应当被定位到。
    assert!(report["findings"]
        .as_array()
        .is_some_and(|items| items.len() >= 5));
    assert!(report["file_count"].as_u64().is_some_and(|n| n >= 3));
    // 夹具里植入的问题包含强制性规则（公共 API 缺文档、空异常处理、循环依赖），
    // 因此门禁必须是「未通过」。它过去曾经因为所有发现都停留在 Suspected 而
    // 永远返回 passed——那条路径现在由 pf-core 的门禁测试守护。
    assert_eq!(report["gate"], "failed");
}

#[test]
fn every_seeded_rule_fires_on_the_over_engineered_fixture() {
    // 夹具的价值在于它守护的行为是明确的：每条植入都必须在报告里出现，
    // 否则说明对应规则退化了（曾经发生过 `redundant-comment` 永不触发）。
    let report = inspect_json(&fixture("over-engineered"));
    let findings = report["findings"].as_array().expect("findings array");
    let mut fired: Vec<&str> = findings
        .iter()
        .filter_map(|finding| finding["rule_id"].as_str())
        .collect();
    fired.sort_unstable();
    fired.dedup();
    for expected in [
        "builtin.public-api-missing-docs",
        "builtin.naming-convention",
        "builtin.single-implementation-trait",
        "builtin.forwarding-wrapper",
        "builtin.needless-factory",
        "builtin.too-many-parameters",
        "builtin.redundant-comment",
        "builtin.duplicate-code",
        "builtin.over-configuration",
        "builtin.over-modularization",
        "builtin.boilerplate-code",
        "builtin.circular-dependency",
    ] {
        assert!(
            fired.contains(&expected),
            "{expected} 未在夹具上触发：{fired:?}"
        );
    }
}

#[test]
fn boilerplate_findings_do_not_claim_authorship() {
    // FR-020 的硬约束：只能判断代码质量，不能判断代码作者。
    let report = inspect_json(&fixture("over-engineered"));
    let findings = report["findings"].as_array().expect("findings array");
    let boilerplate: Vec<_> = findings
        .iter()
        .filter(|finding| finding["rule_id"] == "builtin.boilerplate-code")
        .collect();
    assert_eq!(boilerplate.len(), 2, "夹具植入了两条样板代码");
    for finding in boilerplate {
        let text = format!(
            "{}{}",
            finding["title"].as_str().unwrap_or_default(),
            finding["detail"].as_str().unwrap_or_default()
        );
        assert!(
            !text.contains("AI") && !text.contains("生成"),
            "样板代码结论不得暗示作者身份：{text}"
        );
    }
}

#[test]
fn inspect_reports_the_clean_fixture_as_passing() {
    let report = inspect_json(&fixture("clean"));
    assert_eq!(report["gate"], "passed");
    assert_eq!(report["findings"].as_array().map(Vec::len), Some(0));
}

#[test]
fn terminal_format_is_human_readable() {
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .arg("inspect")
        .arg(fixture("over-engineered"))
        .output()
        .expect("spawn proofforge");
    assert_review_completed(&output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("ProofForge 工程质量审查"));
    assert!(text.contains("质量分层"));
    assert!(text.contains("门禁结论"));
}

#[test]
fn sarif_format_is_written_to_the_requested_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sarif = dir.path().join("report.sarif");
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["inspect", "--format", "sarif"])
        .arg("--output")
        .arg(&sarif)
        .arg(fixture("over-engineered"))
        .output()
        .expect("spawn proofforge");
    assert_review_completed(&output);
    let text = std::fs::read_to_string(&sarif).expect("sarif written");
    let value: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    assert_eq!(value["version"], "2.1.0");
}

#[test]
fn simplify_without_a_toolchain_reports_insufficient_evidence() {
    // 目录里只有源码、没有任何项目清单，因此没有工具链能跑测试，候选无法
    // 验证。此时必须返回退出码 2，绝不能因为「没做检查」而返回 0
    // （AGENTS.md 安全红线 4）。
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "/// Wrapper.\npub fn wrapper(x: i32) -> i32 {\n    real(x)\n}\n",
    )
    .expect("write");
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["simplify", "--target"])
        .arg(dir.path())
        .output()
        .expect("spawn proofforge");
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn simplify_emits_a_report_without_touching_the_target() {
    // 不带 --yes 的 simplify 是只读的：报告可以反复生成，目标仓库不变。
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    let source = dir.path().join("src/lib.rs");
    std::fs::write(
        &source,
        "/// Wrapper.\npub fn wrapper(x: i32) -> i32 {\n    real(x)\n}\n\npub fn real(x: i32) -> i32 {\n    x + 1\n}\n",
    )
    .expect("write");
    let before = std::fs::read_to_string(&source).expect("read");

    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["simplify", "--format", "json", "--target"])
        .arg(dir.path())
        .output()
        .expect("spawn proofforge");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("stdout must be JSON");
    assert!(
        report["candidates"].is_array(),
        "报告必须含候选列表：{stdout}"
    );
    assert!(
        report["residual_risks"].is_array(),
        "报告必须列出残余风险（FR-066）：{stdout}"
    );
    // 门禁必须是一个明确状态，并且退出码与它一致。
    let gate = report["gate"].as_str().unwrap_or_default();
    assert!(
        matches!(gate, "passed" | "failed" | "insufficient_evidence"),
        "unexpected gate: {gate}"
    );
    let after = std::fs::read_to_string(&source).expect("read");
    assert_eq!(before, after, "没有 --yes 时不得修改目标仓库");
    assert_eq!(
        output.status.code(),
        Some(2),
        "没有工具链时门禁必须是「证据不足」（红线 4）"
    );
}

#[test]
fn agent_runs_a_session_and_reports_its_trace() {
    // Agent 循环必须给出可审计的轨迹与明确退出码（FR-049 / 红线 4）。
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["agent", "--target", ".", "--format", "json"])
        .output()
        .expect("spawn proofforge");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let session: serde_json::Value = serde_json::from_str(&stdout).expect("stdout must be JSON");
    assert_eq!(session["session_id"], "cli-session");
    assert!(
        session["trace"]["entries"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()),
        "轨迹不能为空：{stdout}"
    );
    assert!(
        session["final_state"].is_string(),
        "必须给出最终状态：{stdout}"
    );
}

#[test]
fn verify_without_a_properties_file_is_insufficient_evidence() {
    // `verify` 本身已实现，但「没有性质可验证」仍然不是「验证通过」。
    // 回归守护：以前这里传的是位置参数 `.`，clap 直接以用法错误退出 2，
    // 于是测试「通过」了，却从没验证过我们想验证的那条行为。
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["verify", "--target", "."])
        .output()
        .expect("spawn proofforge");
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--properties"), "{stderr}");
}

#[test]
fn verify_runs_a_property_against_a_fixture_project() {
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join("src")).expect("mkdir");
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(
        project.path().join("src/lib.rs"),
        "/// 一个可被性质测试的纯函数。\npub fn double(value: i64) -> i64 { value * 2 }\n",
    )
    .expect("lib.rs");

    let properties = tempfile::Builder::new()
        .suffix(".json")
        .tempfile()
        .expect("tempfile");
    std::fs::write(
        properties.path(),
        serde_json::json!({
            "language": "rust",
            "imports": ["tiny"],
            "samples": 4,
            "properties": [{
                "name": "double 恒为偶数",
                "call_template": "tiny::double({args})",
                "domain": {"kind": "int", "min": -20, "max": 20},
                "invariant": "output % 2 == 0"
            }]
        })
        .to_string(),
    )
    .expect("write properties");

    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["verify", "--target"])
        .arg(project.path())
        .arg("--properties")
        .arg(properties.path())
        .output()
        .expect("spawn proofforge");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "未观察到违反应当放行，stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("double 恒为偶数"), "{stdout}");
    assert!(stdout.contains("未观察到违反"), "{stdout}");
    // 措辞不得超过证据：样本通过不等于程序正确（红线 5）。
    assert!(!stdout.contains("正确"), "{stdout}");
}

#[test]
fn verify_reports_a_reproduced_defect_as_a_failure() {
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join("src")).expect("mkdir");
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(
        project.path().join("src/lib.rs"),
        "/// 对 0 会 panic 的除法。\npub fn half(value: i64) -> i64 {\n  \
         if value == 0 { panic!(\"zero\"); }\n  value / 2\n}\n",
    )
    .expect("lib.rs");

    let properties = tempfile::Builder::new()
        .suffix(".json")
        .tempfile()
        .expect("tempfile");
    std::fs::write(
        properties.path(),
        serde_json::json!({
            "language": "rust",
            "imports": ["tiny"],
            "samples": 8,
            "properties": [{
                "name": "half 不 panic",
                "call_template": "tiny::half({args})",
                // 下界取 0：第一个样本固定是边界值，因此一定命中 panic 路径。
                // 用 -4..4 时能不能找到反例取决于 splitmix64 的序列，那会让这条
                // 回归测试悄悄失去守护能力。
                "domain": {"kind": "int", "min": 0, "max": 4},
                "invariant": "output.abs() < 100"
            }]
        })
        .to_string(),
    )
    .expect("write properties");

    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["verify", "--target"])
        .arg(project.path())
        .arg("--properties")
        .arg(properties.path())
        .output()
        .expect("spawn proofforge");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(1),
        "复现的缺陷必须让门禁失败：{stdout}"
    );
    assert!(stdout.contains("反例"), "{stdout}");
    assert!(stdout.contains("复现"), "{stdout}");
}

#[test]
fn verify_reproduces_the_seeded_defect_in_the_verifiable_fixture() {
    // 端到端守护「复现」这条路：`fixtures/verifiable` 的 `parse` 对非正输入
    // 必然 panic，因此门禁必须是 failed（exit 1），且报告要同时出现反例与
    // 复现命令。这条测试串起了规格加载、沙箱、驱动程序、缩减与报告。
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["verify", "--target"])
        .arg(fixture("verifiable"))
        .arg("--properties")
        .arg(fixture("verifiable").join("properties.json"))
        .output()
        .expect("spawn proofforge");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("parse 不 panic（已知缺陷）"), "{stdout}");
    assert!(stdout.contains("[reproduced]"), "{stdout}");
    // 反例必须被压到最小绝对值：`-20` → `-1`。它不会停在 0 上，因为
    // `InputValue::size()` 让每个在场的元素至少占 1（否则删除零值元素不改变
    // 规模，缩减就没有动力），于是 `-1` 与 `0` 同等大小，先问到的那个赢。
    assert!(
        stdout.contains("反例：-1（规模 20 → 1）"),
        "反例必须被缩减到 -1：{stdout}"
    );
    // 配方必须自带驱动源码与保存路径：只给 `cargo test --test ...` 是不够的，
    // 驱动文件在沙箱里已经被删除，不重新保存根本无法执行（红线：不可执行的配方
    // 等于没有配方）。
    assert!(
        stdout.contains("复现：把源码保存为 tests/proofforge_driver.rs"),
        "配方必须说明把驱动源码保存到哪里：{stdout}"
    );
    assert!(
        stdout.contains("cargo test --test proofforge_driver"),
        "{stdout}"
    );
    // 干净的那条性质必须仍然是「未观察到违反」，不能被缺陷项抵消掉。
    assert!(stdout.contains("double 恒为偶数（干净路径）"), "{stdout}");
    assert!(stdout.contains("未观察到违反"), "{stdout}");
    assert!(
        !stdout.contains("正确"),
        "措辞不得越过证据（红线 5）：{stdout}"
    );
}

#[test]
fn missing_target_is_an_error_not_a_pass() {
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["inspect", "/definitely/not/a/real/path"])
        .output()
        .expect("spawn proofforge");
    assert!(!output.status.success());
    assert_ne!(output.status.code(), Some(0));
}

/// 造一个**测试本来就不过**的 Rust 项目。
///
/// 变异测试在跑任何变异体之前先跑一遍项目自己的测试；基线是红灯时通过率没有
/// 意义，因此需要这个夹具来守护「没有结论」这条路。测试写在 `tests/` 而不是
/// `#[cfg(test)] mod tests`：两者对 `cargo test` 等价，但前者不需要在 `lib.rs`
/// 里再开一个模块。
fn broken_project() -> (tempfile::TempDir, PathBuf) {
    let project = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(project.path().join("src")).expect("mkdir");
    std::fs::create_dir_all(project.path().join("tests")).expect("mkdir");
    std::fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname = \"broken\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    std::fs::write(
        project.path().join("src/lib.rs"),
        "/// 一个恒为真的加法。\npub fn add(left: i64, right: i64) -> i64 { left + right }\n",
    )
    .expect("lib.rs");
    std::fs::write(
        project.path().join("tests/behavior.rs"),
        "#[test]\nfn add_is_wrong() {\n    assert_eq!(broken::add(1, 1), 3);\n}\n",
    )
    .expect("test");
    let target = project.path().to_path_buf();
    (project, target)
}

/// 运行一次子命令并返回（退出码, stdout, stderr）。
fn run(args: &[&str]) -> (Option<i32>, String, String) {
    run_owned(
        &args
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>(),
    )
}

/// 与 [`run`] 相同，但接受运行期拼出来的参数（路径来自临时目录）。
fn run_owned(args: &[String]) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(args)
        .output()
        .expect("spawn proofforge");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn rules_standard_renders_the_builtin_standard_as_markdown() {
    // FR-057：规范必须能一次性摊开给人看，否则「可解释」只是空话。
    let (code, stdout, stderr) = run(&["rules", "standard"]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("# 工程规范：builtin"), "{stdout}");
    assert!(stdout.contains("## 自动检查的规则"), "{stdout}");
    assert!(
        stdout.contains("builtin.public-api-missing-docs"),
        "{stdout}"
    );
}

#[test]
fn rules_import_marks_markdown_rules_as_manual_only() {
    // 红线 4：Markdown 里的意图不能被当成「已执行的检查」。
    let dir = tempfile::tempdir().expect("tempdir");
    let standard = dir.path().join("AGENTS.md");
    std::fs::write(
        &standard,
        "# 团队规范\n\n## 提交信息必须说明为什么\n\n只说「修复 bug」不够。\n",
    )
    .expect("write standard");
    let (code, stdout, stderr) = run(&["rules", "import", standard.to_str().expect("utf8")]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("人工"), "{stdout}");
    assert!(stdout.contains("人工审查项"), "{stdout}");
    assert!(
        stdout.contains("提交信息必须说明为什么"),
        "导入的规范必须出现在输出里：{stdout}"
    );
}

#[test]
fn rules_conflicts_reports_opposite_modality_as_a_heuristic() {
    // FR-060 + 红线 5：启发式判断必须自证是启发式。
    let dir = tempfile::tempdir().expect("tempdir");
    let standard = dir.path().join("rules.toml");
    std::fs::write(
        &standard,
        "name = \"project\"\nversion = \"1.0.0\"\n\n[[rules]]\n\
         id = \"project.forbid-unwrap\"\nname = \"forbid-unwrap\"\n\
         description = \"unwrap 不得出现在库代码中\"\nseverity = \"mandatory\"\n\
         check = { type = \"builtin\", checker = \"no_unwrap_in_library\" }\n\n\
         [[rules]]\nid = \"project.require-unwrap\"\nname = \"require-unwrap\"\n\
         description = \"unwrap 必须出现在错误路径上\"\nseverity = \"project\"\n\
         check = { type = \"builtin\", checker = \"no_unwrap_in_library\" }\n",
    )
    .expect("write rules");
    let (code, stdout, stderr) = run(&["rules", "conflicts", standard.to_str().expect("utf8")]);
    assert_eq!(
        code,
        Some(0),
        "冲突不是错误，不该让命令失败。stderr: {stderr}"
    );
    assert!(stdout.contains("opposite_modality"), "{stdout}");
    assert!(stdout.contains("启发式"), "{stdout}");
}

#[test]
fn rules_exemptions_lists_each_exemption_with_its_grantor() {
    // FR-062 / FR-070：豁免必须可归属到人，且过期状态要看得见。
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("exemptions.toml");
    std::fs::write(
        &file,
        "[[exemptions]]\nrule_id = \"builtin.naming-convention\"\n\
         scope = { kind = \"rule\" }\nreason = \"改名排期在 #42\"\ngranted_by = \"alice\"\n\n\
         [[exemptions]]\nrule_id = \"builtin.duplicate-code\"\n\
         scope = { kind = \"file\", path = \"src/a.rs\" }\nreason = \"夹具\"\n\
         granted_by = \"bob\"\nexpires_on = \"2020-01-01\"\n",
    )
    .expect("write exemptions");
    let (code, stdout, stderr) = run(&[
        "rules",
        "exemptions",
        file.to_str().expect("utf8"),
        "--reference-date",
        "2026-10-25",
    ]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("alice"), "{stdout}");
    assert!(stdout.contains("改名排期在 #42"), "{stdout}");
    assert!(stdout.contains("[生效中]"), "{stdout}");
    assert!(stdout.contains("[已过期]"), "{stdout}");
    assert!(
        stderr.contains("过期"),
        "过期豁免必须在 stderr 出声：{stderr}"
    );
}

#[test]
fn inspect_with_an_exemption_suppresses_the_gate_but_keeps_the_finding() {
    // FR-062 的端到端路径：豁免让门禁从「未通过」变回「通过」，但发现仍在
    // JSON 里，并带上理由与批准人。
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    std::fs::write(
        src.join("lib.rs"),
        "//! fixture\n\n/// Documented.\npub fn documented() {}\n\n/// Documented.\npub fn badName() {}\n",
    )
    .expect("write lib.rs");
    let exemption = dir.path().join("exemptions.toml");
    std::fs::write(
        &exemption,
        "[[exemptions]]\nrule_id = \"builtin.naming-convention\"\n\
         scope = { kind = \"rule\" }\nreason = \"历史 API 名，排期在 #42\"\n\
         granted_by = \"alice\"\n",
    )
    .expect("write exemptions");

    // inspect 的目标是位置参数，不是 --target。
    let args = |extra: &[&str]| -> Vec<String> {
        let mut base = vec![
            "inspect".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
            "--block-on".to_owned(),
            "medium".to_owned(),
            dir.path().to_str().expect("utf8").to_owned(),
        ];
        base.extend(extra.iter().map(|value| (*value).to_owned()));
        base
    };

    let before = run_owned(&args(&[]));
    let before_report: serde_json::Value =
        serde_json::from_str(&before.1).expect("stdout must be JSON");
    assert_eq!(before_report["gate"], "failed", "没有豁免时必须未通过");
    assert_eq!(before.0, Some(1));
    assert_eq!(before_report["findings"][0]["exempted"], false);

    let extra = [
        "--exemption",
        exemption.to_str().expect("utf8"),
        "--reference-date",
        "2026-10-25",
    ];
    let after = run_owned(&args(&extra));
    let after_report: serde_json::Value =
        serde_json::from_str(&after.1).expect("stdout must be JSON");
    assert_eq!(after_report["gate"], "passed", "豁免后应当通过");
    assert_eq!(after.0, Some(0));
    let exempted: Vec<&serde_json::Value> = after_report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter(|finding| finding["exempted"] == true)
        .collect();
    assert_eq!(exempted.len(), 1, "豁免不能删除发现：{after_report}");
    assert_eq!(exempted[0]["exemption"]["granted_by"], "alice");
    assert_eq!(
        exempted[0]["exemption"]["reason"],
        "历史 API 名，排期在 #42"
    );
}

#[test]
fn inspect_with_a_project_standard_records_it_in_the_report() {
    // FR-058：导入的项目规范必须出现在报告里，并说明它是人工审查项。
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    std::fs::write(src.join("lib.rs"), "//! fixture\n\n/// D.\npub fn d() {}\n")
        .expect("write lib.rs");
    let standard = dir.path().join("AGENTS.md");
    std::fs::write(
        &standard,
        "# 规范\n\n## 提交信息必须说明为什么\n\n写清触发条件。\n",
    )
    .expect("write standard");

    let (code, stdout, stderr) = run_owned(&[
        "inspect".to_owned(),
        "--format".to_owned(),
        "json".to_owned(),
        dir.path().to_str().expect("utf8").to_owned(),
        "--standard".to_owned(),
        standard.to_str().expect("utf8").to_owned(),
    ]);
    assert_eq!(code, Some(0), "夹具本身是干净的。stderr: {stderr}");
    let report: serde_json::Value = serde_json::from_slice(stdout.as_bytes()).expect("json");
    let standards = report["imported_standards"].as_array().expect("array");
    assert_eq!(standards.len(), 1, "{report}");
    assert_eq!(standards[0]["rule_count"], 1);
    assert_eq!(
        standards[0]["automated_count"], 0,
        "Markdown 规范只能人工审查"
    );
    assert!(report["warnings"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item
            .as_str()
            .is_some_and(|text| text.contains("人工审查项")))));
}

#[test]
fn html_report_is_a_self_contained_document() {
    // FR-068：HTML 必须能离线打开、随 CI 工件归档。
    let dir = tempfile::tempdir().expect("tempdir");
    let html = dir.path().join("report.html");
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["inspect", "--format", "html", "--output"])
        .arg(&html)
        .arg(fixture("over-engineered"))
        .output()
        .expect("spawn proofforge");
    assert_review_completed(&output);
    let text = std::fs::read_to_string(&html).expect("html written");
    assert!(text.starts_with("<!doctype html>"), "{text}");
    assert!(text.contains("</html>"));
    assert!(text.contains("门禁结论"));
    assert!(text.contains("质量分层"));
    assert!(!text.contains("<link"), "不得引用外部样式表");
}

#[test]
fn verify_refuses_html_instead_of_emitting_a_misleading_report() {
    // 验证报告的证据结构与审查报告不同；给一份看起来完整、其实丢掉反例的
    // HTML 比直接说不支持更糟。
    let dir = tempfile::tempdir().expect("tempdir");
    let properties = dir.path().join("properties.json");
    std::fs::write(
        &properties,
        serde_json::json!({
            "language": "rust",
            "samples": 1,
            "properties": [{
                "name": "占位性质",
                "call_template": "tiny::double({args})",
                "domain": {"kind": "int", "min": 0, "max": 1},
                "invariant": "output >= 0"
            }]
        })
        .to_string(),
    )
    .expect("write properties");
    let (code, _, stderr) = run(&[
        "verify",
        "--format",
        "html",
        "--target",
        dir.path().to_str().expect("utf8"),
        "--properties",
        properties.to_str().expect("utf8"),
    ]);
    assert_eq!(code, Some(2), "stderr: {stderr}");
    assert!(stderr.contains("暂不支持 --format html"), "{stderr}");
}

#[test]
fn report_trend_reads_back_what_inspect_stored() {
    // FR-069：趋势查询必须把 inspect 落库的数字原样读回来。
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    // 必须有可分析的文件：空目录的结论是「证据不足」（exit 2），不是「通过」。
    std::fs::write(
        src.join("lib.rs"),
        "//! fixture\n\n/// Documented.\npub fn documented() {}\n",
    )
    .expect("write lib.rs");
    let db = dir.path().join("history.db");
    let target = dir.path().to_str().expect("utf8");
    for _ in 0..2 {
        let (code, _, stderr) = run_owned(&[
            "inspect".to_owned(),
            target.to_owned(),
            "--store".to_owned(),
            db.to_str().expect("utf8").to_owned(),
        ]);
        assert_eq!(code, Some(0), "夹具是干净的项目。stderr: {stderr}");
    }
    let (code, stdout, stderr) = run(&[
        "report",
        "--trend",
        "--project",
        target,
        "--store",
        db.to_str().expect("utf8"),
    ]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("质量趋势"), "{stdout}");
    assert!(
        stdout.contains("变化：发现"),
        "趋势必须给出首尾对比：{stdout}"
    );
    assert_eq!(
        stdout.matches("passed").count(),
        2,
        "两次运行都应入账：{stdout}"
    );
}

#[test]
fn report_rulesets_lists_the_ruleset_version_history() {
    // FR-061：每次审查记录规则集版本，版本历史要能查出来。
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    std::fs::write(
        src.join("lib.rs"),
        "//! fixture\n\n/// Documented.\npub fn documented() {}\n",
    )
    .expect("write lib.rs");
    let db = dir.path().join("history.db");
    let target = dir.path().to_str().expect("utf8");
    let (code, _, stderr) = run_owned(&[
        "inspect".to_owned(),
        target.to_owned(),
        "--store".to_owned(),
        db.to_str().expect("utf8").to_owned(),
    ]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let (code, stdout, stderr) = run(&[
        "report",
        "--rulesets",
        "--project",
        target,
        "--store",
        db.to_str().expect("utf8"),
    ]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stdout.contains("规则集版本历史"), "{stdout}");
    assert!(stdout.contains("0.1.0"), "{stdout}");
}

#[test]
fn config_file_supplies_defaults_and_cli_flags_win() {
    // `--config` 的优先级：显式给出的 CLI 参数覆盖配置文件，没给的用配置里的值。
    // 这条测试守护的是「不传 --block-on 时用的是配置里的 medium，而不是默认的
    // high」——否则配置文件里的阈值会被静默忽略，而报告看起来完全正常。
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    // 一条 medium 的发现：block_on=medium 时未通过，block_on=high 时通过。
    std::fs::write(src.join("lib.rs"), "//! fixture\n\npub fn badName() {}\n")
        .expect("write lib.rs");
    let nested = dir.path().join("configs");
    std::fs::create_dir_all(&nested).expect("mkdir");
    let config = nested.join("proofforge.toml");
    // target 按配置文件所在目录解析：`..` 才是这个夹具，`.` 会指向 configs/。
    std::fs::write(&config, "target = \"..\"\nblock_on = \"medium\"\n").expect("write config");

    let with_config = |extra: &[&str]| -> Vec<String> {
        let mut base = vec![
            "inspect".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
            "--config".to_owned(),
            config.to_str().expect("utf8").to_owned(),
        ];
        base.extend(extra.iter().map(|value| (*value).to_owned()));
        base
    };

    let (code, stdout, stderr) = run_owned(&with_config(&[]));
    assert_eq!(code, Some(1), "配置里的 medium 必须生效。stderr: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(report["gate"], "failed");

    // 命令行显式给出 high，压过配置文件。
    let (code, stdout, stderr) = run_owned(&with_config(&["--block-on", "high"]));
    assert_eq!(code, Some(0), "CLI 必须优先于配置。stderr: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(report["gate"], "passed");
}

#[test]
fn a_broken_config_fails_instead_of_falling_back_to_defaults() {
    // 坏配置必须让人知道审查没按预期跑。静默退回默认值会让「我以为设了
    // 阈值」变成假话，那正是 AGENTS.md 安全红线 4 要防的。
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    std::fs::write(src.join("lib.rs"), "//! fixture\n\n/// D.\npub fn d() {}\n")
        .expect("write lib.rs");
    let config = dir.path().join("proofforge.toml");
    std::fs::write(&config, "this is not toml").expect("write config");

    let (code, _, stderr) = run_owned(&[
        "inspect".to_owned(),
        "--format".to_owned(),
        "json".to_owned(),
        dir.path().to_str().expect("utf8").to_owned(),
        "--config".to_owned(),
        config.to_str().expect("utf8").to_owned(),
    ]);
    assert_eq!(
        code,
        Some(2),
        "坏配置是「检查没跑完」，不是通过。stderr: {stderr}"
    );
    assert!(stderr.contains("proofforge.toml"), "{stderr}");
}

#[test]
fn config_exemption_files_are_applied_to_the_gate() {
    // 配置里的 `exemption_files` 必须真的参与门禁：这是「豁免写进配置文件」
    // 这条路线的端到端守护，走的是 --config 而不是 --exemption。
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    std::fs::write(
        src.join("lib.rs"),
        "//! fixture\n\n/// Documented.\npub fn badName() {}\n",
    )
    .expect("write lib.rs");
    std::fs::write(
        dir.path().join("exemptions.toml"),
        "[[exemptions]]\nrule_id = \"builtin.naming-convention\"\n\
         scope = { kind = \"rule\" }\nreason = \"历史 API 名，排期在 #42\"\n\
         granted_by = \"alice\"\n",
    )
    .expect("write exemptions");
    // exemption_files 按目标根目录解析，因此配置里的 target 也指向夹具本身。
    let config = dir.path().join("proofforge.toml");
    std::fs::write(
        &config,
        "target = \".\"\nblock_on = \"medium\"\nexemption_files = [\"exemptions.toml\"]\n",
    )
    .expect("write config");

    let (code, stdout, stderr) = run_owned(&[
        "inspect".to_owned(),
        "--format".to_owned(),
        "json".to_owned(),
        "--config".to_owned(),
        config.to_str().expect("utf8").to_owned(),
    ]);
    assert_eq!(code, Some(0), "豁免必须让门禁回到通过。stderr: {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(report["gate"], "passed");
    let exempted = report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter(|finding| finding["exempted"] == true)
        .count();
    assert_eq!(exempted, 1, "豁免不能删除发现：{report}");
}

#[test]
fn metamorphic_relations_run_end_to_end() {
    // FR-039 端到端：`fixtures/verifiable/relations.json` 里两条干净关系与一条
    // 踩到 `parse` 已知缺陷的关系，因此门禁必须是 failed（exit 1），且三种结论
    // 都要出现。它同时守护元组域的实参形状——写成字面元组会让第二次调用少收一个
    // 参数，编译失败后整条关系只剩「证据不足」，看不出是规格写错了。
    let target = fixture("verifiable");
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["verify", "--target"])
        .arg(&target)
        .arg("--metamorphic")
        .arg(target.join("relations.json"))
        .output()
        .expect("spawn proofforge");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("翻倍与输入符号无关（干净路径）"),
        "{stdout}"
    );
    assert!(stdout.contains("加法满足交换律（干净路径）"), "{stdout}");
    assert!(
        stdout.contains("parse 的平移量恒为 1（已知缺陷：非正输入 panic）"),
        "{stdout}"
    );
    assert!(stdout.contains("[reproduced]"), "{stdout}");
    assert!(stdout.contains("反例：-1"), "{stdout}");
    assert!(
        stdout.contains("cargo test --test proofforge_driver"),
        "复现配方必须可执行：{stdout}"
    );
}

#[test]
fn metamorphic_json_reports_relations_separately_from_properties() {
    // 性质与关系是两套结论，JSON 里必须分开：合成一个数组会让「哪条性质被违反」
    // 与「哪条关系被违反」混在一起。
    let target = fixture("verifiable");
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["verify", "--format", "json", "--target"])
        .arg(&target)
        .arg("--metamorphic")
        .arg(target.join("relations.json"))
        .output()
        .expect("spawn proofforge");

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    let relations = report["relations"].as_array().expect("relations array");
    assert_eq!(relations.len(), 3, "{report}");
    assert_eq!(relations[0]["status"], "not_reproduced", "{report}");
    assert_eq!(relations[1]["status"], "not_reproduced", "{report}");
    assert_eq!(relations[2]["status"], "reproduced", "{report}");
    let counterexample = &relations[2]["counterexample"];
    assert_eq!(counterexample["original_size"], 40, "缩减前规模：{report}");
    assert_eq!(counterexample["reduced_size"], 1, "缩减后规模：{report}");
    assert_eq!(relations[2]["counterexample"]["input"], "-1", "{report}");
}

#[test]
fn mutation_testing_scores_the_test_suite_not_the_program() {
    // FR-037 端到端：`fixtures/mutable` 里 `add` / `is_even` / `clamp` 有测试，
    // `unchecked_discount` / `odd_offset` 没有。因此通过率必须低于 100%，且幸存
    // 条目要落在没有测试的那两个函数上——这条断言守的是「通过率测量的是测试」。
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["mutate", "--target"])
        .arg(fixture("mutable"))
        .arg("--max-mutants")
        .arg("40")
        .output()
        .expect("spawn proofforge");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("幸存"),
        "夹具里植入了没有测试的函数，必须有变异体幸存：{stdout}"
    );
    assert!(
        stdout.contains("unchecked_discount") || stdout.contains("odd_offset"),
        "幸存的变异体必须能指认到没有被测试覆盖的代码：{stdout}"
    );
    assert!(
        !stdout.contains("通过率 100%"),
        "有未测函数就不可能满分：{stdout}"
    );
}

#[test]
fn mutation_testing_without_a_passing_baseline_has_no_conclusion() {
    // 基线测试本来就失败时，通过率没有意义——红灯上算出来的数字是假的。因此
    // 必须是退出码 2（证据不足），且报告不得给出通过率（红线 4）。
    let (_dir, target) = broken_project();
    let output = Command::new(env!("CARGO_BIN_EXE_proofforge"))
        .args(["mutate", "--target"])
        .arg(&target)
        .output()
        .expect("spawn proofforge");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(2), "{stdout}");
    assert!(stdout.contains("基线测试没有通过"), "{stdout}");
    assert!(
        !stdout.contains("通过率"),
        "没有结论时不得给出通过率：{stdout}"
    );
}
