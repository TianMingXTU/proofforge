//! 端到端验证：在真实项目上跑一次性质检查。
//!
//! 这些测试启动真正的 `cargo`，因此比单元测试慢，但它们守护的是单元测试
//! 覆盖不到的部分：沙箱隔离、驱动程序能否真的编译、反例能否真的回传。

use std::path::Path;
use std::sync::Mutex;

use pf_core::config::{Budget, Config};
use pf_verify::domain::{InputDomain, InputValue};
use pf_verify::driver::{Language, PropertySpec};
use pf_verify::property::{PropertyResult, PropertyRunner};
use pf_verify::sandbox::Sandbox;
use pf_verify::test_runner::{TestRunner, TestStatus};
use pf_verify::{FuzzConfig, FuzzRunner, VerificationEngine};

/// 串行化所有会调用 cargo 的测试。
///
/// 每个测试都自建一个临时 crate 并编译它，而 cargo 的**包缓存锁**是进程间
/// 共享的：并行跑会在 `Blocking waiting for file lock on package cache` 上互相
/// 挡住，测试于是随机失败。串行执行让它们确定，代价是总时长——对守护真实行为
/// 的端到端测试来说是值得的。
static CARGO_LOCK: Mutex<()> = Mutex::new(());

/// 一个带已知缺陷的 Rust 项目：`parse` 对某些输入会 panic。
fn project_with_bug(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).expect("mkdir");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"buggy\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write manifest");
    std::fs::write(
        dir.join("src/lib.rs"),
        "/// 把整数转成 usize，非正输入会因 `try_into` 失败而 panic。\n\
         ///\n\
         /// 参数故意收 `i64`：性质测试的输入域就是 `i64`，签名对不上会让生成的\n\
         /// 调用根本编译不过，那是一次「没跑起来」而不是「性质被违反」。\n\
         pub fn parse(value: i64) -> usize {\n\
         \x20   (value - 1).try_into().unwrap()\n\
         }\n\
         \n\
         /// 一个正常的函数。\n\
         pub fn double(value: i64) -> i64 {\n\
         \x20   value * 2\n\
         }\n",
    )
    .expect("write lib.rs");
}

/// 构造一条针对夹具项目的性质。
fn spec(call: &str, invariant: &str, domain: InputDomain) -> PropertySpec {
    PropertySpec {
        name: None,
        language: Language::Rust,
        imports: vec!["buggy".to_owned()],
        call_template: format!("buggy::{call}({{args}})"),
        domain,
        invariant: invariant.to_owned(),
        samples: 16,
        seed: 1,
    }
}

#[test]
fn sandbox_isolates_the_project_and_leaves_it_read_only() {
    let _guard = CARGO_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let source = tempfile::tempdir().expect("tempdir");
    project_with_bug(source.path());
    let before = std::fs::read_to_string(source.path().join("src/lib.rs")).expect("read");

    let sandbox = Sandbox::isolate(source.path(), &Budget::default()).expect("isolate");
    sandbox
        .write_file("tests/proofforge_driver.rs", "// generated\n")
        .expect("write driver");

    // 原仓库必须分毫未动：沙箱的一切写入都发生在副本上（AGENTS.md 安全红线 3）。
    assert_eq!(
        std::fs::read_to_string(source.path().join("src/lib.rs")).expect("read"),
        before,
        "原仓库不得被修改"
    );
    assert!(
        !source.path().join("tests").exists(),
        "驱动程序必须落在沙箱里，而不是用户项目里"
    );
    assert!(sandbox.root().join("tests/proofforge_driver.rs").is_file());
}

#[test]
fn a_holding_property_reports_not_violated() {
    let _guard = CARGO_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let source = tempfile::tempdir().expect("tempdir");
    project_with_bug(source.path());
    let sandbox = Sandbox::isolate(source.path(), &Budget::default()).expect("isolate");

    // `double` 对任意输入都满足「偶数」这条不变式。
    let outcome = PropertyRunner::run(
        &sandbox,
        &spec(
            "double",
            "output % 2 == 0",
            InputDomain::Int { min: -50, max: 50 },
        ),
        &pf_adapters::ToolOptions::default(),
    )
    .expect("run");

    assert!(
        matches!(outcome.result, PropertyResult::Satisfied { samples: 16 }),
        "{:?}",
        outcome.result
    );
    assert!(outcome.counterexample.is_none());
    assert_eq!(outcome.probe.exit_code, Some(0));
}

#[test]
fn a_violated_property_yields_a_reduced_counterexample_and_a_repro() {
    let _guard = CARGO_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let source = tempfile::tempdir().expect("tempdir");
    project_with_bug(source.path());
    let sandbox = Sandbox::isolate(source.path(), &Budget::default()).expect("isolate");

    // `parse` 在输入为 0 时会 `(0 - 1).try_into()` 而 panic。
    //
    // 不变式里的字面量必须能对 `parse` 的返回类型 `usize` 定型：写成
    // `output < 100` 会让驱动程序编译不过，那是一次「没跑起来」而不是
    // 「性质被违反」——两者在报告里必须是不同的结论。
    let outcome = PropertyRunner::run(
        &sandbox,
        &spec(
            "parse",
            "output < 100usize",
            InputDomain::Int { min: -20, max: 20 },
        ),
        &pf_adapters::ToolOptions::default(),
    )
    .expect("run");

    assert!(
        matches!(outcome.result, PropertyResult::Violated),
        "{:?}",
        outcome.result
    );
    let example = outcome.counterexample.expect("必须有反例");
    // 反例必须被缩减过：从批量样本压到一个更小的输入。
    assert!(
        example.reduction.final_size < example.reduction.original_size,
        "{:?}",
        example.reduction
    );
    assert!(example.reduction.converged);
    // 复现配方必须自带源码与命令，不依赖 ProofForge 文档。
    assert_eq!(example.repro.driver_path, "tests/proofforge_driver.rs");
    assert!(example
        .repro
        .driver_source
        .contains("fn proofforge_property()"));
    assert!(example
        .repro
        .render()
        .contains("cargo test --test proofforge_driver"));
}

#[test]
fn an_uncompilable_invariant_is_inconclusive_not_a_pass() {
    let _guard = CARGO_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let source = tempfile::tempdir().expect("tempdir");
    project_with_bug(source.path());
    let sandbox = Sandbox::isolate(source.path(), &Budget::default()).expect("isolate");

    // 不变式引用了不存在的函数：驱动程序编译不过，必须得到「证据不足」。
    let outcome = PropertyRunner::run(
        &sandbox,
        &spec(
            "double",
            "does_not_exist(output)",
            InputDomain::Int { min: 0, max: 1 },
        ),
        &pf_adapters::ToolOptions::default(),
    )
    .expect("run");

    assert!(
        matches!(&outcome.result, PropertyResult::Inconclusive(reason) if !reason.is_empty()),
        "{:?}",
        outcome.result
    );
}

#[test]
fn a_missing_toolchain_is_inconclusive_not_a_pass() {
    // 空目录没有 Cargo.toml：没有适配器可用，绝不能报告成通过（红线 4）。
    let dir = tempfile::tempdir().expect("tempdir");
    let sandbox = Sandbox::isolate(dir.path(), &Budget::default()).expect("isolate");
    let outcome = PropertyRunner::run(
        &sandbox,
        &spec("double", "output > 0", InputDomain::Bool),
        &pf_adapters::ToolOptions::default(),
    )
    .expect("run");
    assert!(
        matches!(&outcome.result, PropertyResult::Inconclusive(reason) if reason.contains("toolchain")),
        "{:?}",
        outcome.result
    );
}

#[test]
fn fuzzing_explores_the_input_space_and_reports_classes() {
    let _guard = CARGO_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let source = tempfile::tempdir().expect("tempdir");
    project_with_bug(source.path());
    let sandbox = Sandbox::isolate(source.path(), &Budget::default()).expect("isolate");

    let config = FuzzConfig::new(spec(
        "parse",
        "output < 100usize",
        InputDomain::Int { min: -20, max: 20 },
    ))
    .with_rounds(2)
    .with_candidates_per_round(4);

    let outcome =
        FuzzRunner::run(&sandbox, &config, &pf_adapters::ToolOptions::default()).expect("fuzz");
    assert!(outcome.inconclusive_reason.is_none(), "{outcome:?}");
    assert!(outcome.explored > 0);
    // `parse` 对非正输入会 panic，因此域内观察到的失败全部是崩溃，而不是
    // 「输出不满足不变式」——`output < 100usize` 在能给出输出的输入上都成立。
    // 崩溃与违反是两类观测，测试必须按这个区分来断言（红线 5）。
    assert!(outcome.found_crash(), "应当观察到崩溃：{outcome:?}");
    assert!(
        !outcome.found_violation(),
        "这个夹具在域内不会产生不变式违反：{outcome:?}"
    );
    // 崩溃输入必须被交出来：只说「崩溃了 25 次」却不给输入等于没有结论。
    assert!(
        !outcome.crashers.is_empty(),
        "必须给出可复现的崩溃输入：{outcome:?}"
    );
    assert!(
        outcome
            .crashers
            .iter()
            .all(|value| matches!(value, InputValue::Int(v) if *v <= 0)),
        "崩溃输入应是非正数，且不得越出声明域 [-20, 20]：{outcome:?}"
    );
    assert!(
        outcome.summary().contains("崩溃输入"),
        "结论一行要带上崩溃输入：{}",
        outcome.summary()
    );
}

#[test]
fn test_runner_executes_the_project_suite_in_the_sandbox() {
    let _guard = CARGO_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let source = tempfile::tempdir().expect("tempdir");
    project_with_bug(source.path());
    let run = TestRunner::run(source.path(), &Budget::default()).expect("run");
    // 夹具项目没有测试，因此 cargo test 会以「0 passed」成功退出。
    assert_eq!(
        run.status,
        TestStatus::Passed,
        "stderr: {}",
        run.execution.stderr
    );
    assert!(run.execution.command.cwd.is_some(), "必须记录工作目录");
}

#[test]
fn verification_engine_aggregates_properties_into_a_gate() {
    let _guard = CARGO_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let source = tempfile::tempdir().expect("tempdir");
    project_with_bug(source.path());
    let specs = vec![
        spec(
            "double",
            "output % 2 == 0",
            InputDomain::Int { min: -10, max: 10 },
        ),
        spec(
            "parse",
            "output < 100usize",
            InputDomain::Int { min: -20, max: 20 },
        ),
    ];
    let report = VerificationEngine::verify(
        source.path(),
        &specs,
        &[],
        &Config::for_target(source.path()),
    )
    .expect("verify");

    assert_eq!(report.entries.len(), 2);
    assert!(
        report.has_violations(),
        "parse 的性质必须被违反：{report:?}"
    );
    // 已经复现的缺陷让门禁失败，而不是被另一条通过项抵消。
    assert_eq!(report.gate(), pf_core::status::GateStatus::Failed);
    assert_eq!(report.exit_code(), 1);
    let artifacts = report.evidence();
    assert_eq!(artifacts.len(), 2);
    assert!(
        artifacts
            .iter()
            .all(pf_core::evidence::EvidenceArtifact::verify),
        "每条性质都必须有一条可校验的证据产物"
    );
    let text = report.render_terminal();
    assert!(text.contains("反例"), "{text}");
    assert!(text.contains("复现"), "{text}");
}

#[test]
fn generated_samples_stay_inside_their_domain() {
    // 缩减后的输入必须仍然是域内的合法值：驱动程序靠它构造字面量。
    let domain = InputDomain::Tuple {
        fields: vec![
            InputDomain::Int { min: -5, max: 5 },
            InputDomain::Str { max_len: 4 },
        ],
    };
    for value in domain.samples(21, 12) {
        assert!(matches!(value, InputValue::Tuple(_)));
    }
}
