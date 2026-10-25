//! 驱动程序：把性质渲染成可执行的测试，并解析执行结果。
//!
//! 覆盖需求文档 5.6 的 FR-034（测试生成）与 FR-043（结果复现）。
//!
//! # 输入以字面量嵌入
//!
//! 生成的输入不是驱动程序里的随机数，而是 `ProofForge` 事先算好、以字面量写进
//! 源码的值。理由见 [`crate::domain`]：可复现、可审计、可缩减。
//!
//! # 反例如何回传
//!
//! 驱动程序不靠解析标准输出找回反例——那要求把 Rust 的 `Debug` 格式反向解析
//! 回 [`InputValue`]，既脆弱又给目标类型增加 `Debug` 约束。改为让驱动程序
//! 打印一个**标签**，标签与输入的对应关系由 `ProofForge` 在渲染时记录：
//!
//! ```text
//! PROOFFORGE_VIOLATION 3
//! ```
//!
//! 标签就是输入在样本列表中的下标，因此回传不需要任何解析，也不会因为目标
//! 类型没有实现 `Debug` 而编译失败。

use pf_adapters::{Tool, ToolOptions};
use pf_core::evidence::{ExecutionResult, ExecutionStatus};
use serde::{Deserialize, Serialize};

use crate::domain::{InputDomain, InputValue};
use crate::metamorphic::MetamorphicSpec;
use crate::sandbox::Sandbox;

/// 驱动测试在沙箱中的固定文件名（不含扩展名）。
///
/// 固定名字让复现命令可以写死：`cargo test --test proofforge_driver`。
pub const DRIVER_STEM: &str = "proofforge_driver";

/// 驱动程序在失败时打印的前缀。
///
/// 用一个不太可能出现在普通测试输出里的前缀，避免把目标项目自己的测试失败
/// 误判成性质违反。
pub const VIOLATION_MARKER: &str = "PROOFFORGE_VIOLATION";

/// 驱动程序在**目标程序 panic** 时打印的前缀。
///
/// 与 [`VIOLATION_MARKER`] 分开是必要的：「不变式不成立」与「程序崩了」是两类
/// 不同的观测——前者说明逻辑错了，后者说明连输出都没有。合成一个标记会让报告
/// 无法区分，而 [`crate::fuzz::BehaviorClass::Crashed`] 也就永远是个空枚举。
pub const CRASH_MARKER: &str = "PROOFFORGE_CRASH";

/// 驱动程序跑完时打印的前缀。
///
/// 这个标记解决一个危险的歧义：编译失败、测试目标不存在、参数写错都会让
/// cargo 以非零退出码结束，而输出里没有任何测试执行记录。没有它，一次根本
/// 没跑起来的验证会被解读成「全部样本都满足」——彻头彻尾的假阴性。
pub const DONE_MARKER: &str = "PROOFFORGE_DONE";

/// 目标语言。
///
/// 可序列化：性质规格文件里要写 `"language": "rust"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// Rust（cargo）。
    Rust,
    /// Python（pytest）。
    Python,
}

impl Language {
    /// 机器可读标识。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
        }
    }

    /// 驱动程序文件的相对路径（沙箱内）。
    #[must_use]
    pub const fn driver_path(self) -> &'static str {
        match self {
            Self::Rust => "tests/proofforge_driver.rs",
            Self::Python => "test_proofforge_driver.py",
        }
    }
}

/// 一条待验证的性质。
///
/// 可直接从 JSON 反序列化；手写规格文件的默认值折叠见 [`crate::spec`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertySpec {
    /// 人类可读的名称；报告用它指认每一条性质。
    ///
    /// 可选是为了让测试与内部调用不必为每条性质编名字；但规格文件**强制**
    /// 要求它——一条进了报告却读不出是哪条的性质没有审计价值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 目标语言。
    pub language: Language,
    /// 需要引入的引用（每行一条，原样写入驱动程序的 import 区）。
    pub imports: Vec<String>,
    /// 调用模板，`{args}` 会被替换成实参表达式。
    ///
    /// 例如 `"fixture::parse({args})"`。只替换这一个占位符：模板里出现其他
    /// 花括号一律按字面量处理，避免与 Rust 的格式化语法混淆。
    pub call_template: String,
    /// 输入域。
    pub domain: InputDomain,
    /// 对输出的断言表达式，其中 `output` 绑定到调用结果。
    pub invariant: String,
    /// 样本数量。
    pub samples: usize,
    /// 随机种子。
    pub seed: u64,
}

/// 一次驱动执行观测到的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverOutcome {
    /// 全部样本都满足性质。
    Satisfied,
    /// 找到反例（附带样本下标）。
    Violated(usize),
    /// 驱动程序没能编译、没能启动或超时——没有得到任何结论。
    Inconclusive(String),
}

/// 一次驱动执行的完整记录。
#[derive(Debug, Clone)]
pub struct DriverRun {
    /// 观测到的结果。
    pub outcome: DriverOutcome,
    /// 触发违反的输入（若有）。
    pub counterexample: Option<InputValue>,
    /// 底层执行记录，作为可复现证据。
    pub execution: ExecutionResult,
}

/// 渲染并执行一次性质检查。
///
/// # Errors
/// 驱动程序写不进沙箱时返回 [`pf_core::Error`]。执行本身不返回错误——
/// 编译失败、超时、工具缺失都以 [`DriverOutcome::Inconclusive`] 表达，
/// 因为它们是「拿不到结论」而不是「调用方用错了参数」。
pub fn run_property(
    sandbox: &Sandbox,
    spec: &PropertySpec,
    options: &ToolOptions,
) -> pf_core::Result<DriverRun> {
    let inputs = spec.domain.samples(spec.seed, spec.samples);
    let source = render_driver(spec, &inputs);
    sandbox.write_file(spec.language.driver_path(), &source)?;

    let execution = run_driver(sandbox, spec.language, options);
    let outcome = interpret(&execution, &inputs);
    let counterexample = match &outcome {
        DriverOutcome::Violated(index) => inputs.get(*index).cloned(),
        _ => None,
    };
    Ok(DriverRun {
        outcome,
        counterexample,
        execution,
    })
}

/// 渲染并执行一次蜕变检查（FR-039）。
///
/// 与 [`run_property`] 共用同一条执行与判定路径：同一个 [`run_driver`]、同一个
/// [`interpret`]、同一套标记。分两个函数而不在一个里面加分支，是因为两者的输入
/// 形状不同（一个输入 vs. 一对输入），共用的部分只有「怎么跑」与「怎么读」——
/// 而那两部分一旦分叉，就会出现「性质说跑过了、蜕变说没跑」这种无法解释的分歧。
///
/// # Errors
/// 驱动程序写不进沙箱时返回 [`pf_core::Error`]。
pub fn run_metamorphic(
    sandbox: &Sandbox,
    spec: &MetamorphicSpec,
    options: &ToolOptions,
) -> pf_core::Result<DriverRun> {
    let inputs = spec.domain.samples(spec.seed, spec.samples);
    let source = render_metamorphic_driver(spec, &inputs);
    sandbox.write_file(spec.language.driver_path(), &source)?;

    let execution = run_driver(sandbox, spec.language, options);
    let outcome = interpret(&execution, &inputs);
    let counterexample = match &outcome {
        DriverOutcome::Violated(index) => inputs.get(*index).cloned(),
        _ => None,
    };
    Ok(DriverRun {
        outcome,
        counterexample,
        execution,
    })
}

/// 渲染蜕变测试驱动程序。
///
/// 每个样本算两次调用：一次用原始输入，一次用经过 `transform` 变换的输入，然后
/// 断言 `relation` 对两个输出的关系成立。变换写在规格里而不是由工具挑选——工具
/// 不知道哪个变换对这个函数是有意义的，猜一个就等于伪造一条性质（红线 5）。
#[must_use]
pub fn render_metamorphic_driver(spec: &MetamorphicSpec, inputs: &[InputValue]) -> String {
    match spec.language {
        Language::Rust => render_rust_metamorphic_driver(spec, inputs),
        Language::Python => render_python_metamorphic_driver(spec, inputs),
    }
}

/// 渲染 Rust 蜕变驱动程序。
///
/// 变换表达式填的是 `call_template` 里的 `{args}` 槽位，因此它的形状必须与源调用
/// 一致：元组域要展开成**逗号分隔的实参列表**（`{input}.1, {input}.0`），而不是
/// 一个字面元组。写成 `({input}.1, {input}.0)` 会让第二次调用只收到一个参数，
/// 编译报「参数个数不符」——那是规格写错，报告会以证据不足如实呈现。
fn render_rust_metamorphic_driver(spec: &MetamorphicSpec, inputs: &[InputValue]) -> String {
    let source_args = rust_arg_exprs(&spec.domain).join(", ");
    let call_a = escape_braces(&spec.call_template.replace("{args}", &source_args));
    // 变换表达式拿到的是**整个输入值**：标量域是 `input.0`，元组域是 `input`。
    let value_expr = metamorphic_value_expr(&spec.domain, Language::Rust);
    let transformed = spec.transform.replace("{input}", &value_expr);
    let call_b = escape_braces(&spec.call_template.replace("{args}", &transformed));
    let imports: Vec<String> = spec
        .imports
        .iter()
        .map(|import| format!("use {import};"))
        .collect();
    let literals: Vec<String> = inputs
        .iter()
        .enumerate()
        .map(|(index, value)| format!("    ({index}, {}),", rust_case_literal(&spec.domain, value)))
        .collect();
    let cases = render_cases(&literals);
    let case_type = rust_case_type(&spec.domain);
    let relation = escape_braces(&spec.relation);
    format!(
        r#"//! 由 `ProofForge` 生成的蜕变测试驱动程序，请勿手改。
//! 目标：`{template}`；样本数：{count}；种子：{seed}。
#![allow(unused_imports, dead_code, clippy::all)]

{imports}

#[test]
fn proofforge_metamorphic() {{
    let cases: Vec<(usize, {case_type})> = {cases};
    for (label, input) in cases {{
        // 任一侧 panic 都算这条关系没有被满足：给不出输出，`R` 自然不成立。
        // 这与 Rust 性质驱动程序对 panic 的处置一致（见 `render_rust_driver`），
        // 也意味着报告层**不**区分「关系不成立」与「调用崩了」——两者同为失败
        // 观测，证据强度相同。catch_unwind 让后续样本继续跑。
        let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{
            let output_a = {call_a};
            let output_b = {call_b};
            {relation}
        }}));
        if !matches!(observed, Ok(true)) {{
            println!("{marker} {{label}}");
            panic!("{marker} {{label}}");
        }}
    }}
    println!("{done}");
}}
"#,
        template = spec.call_template,
        count = inputs.len(),
        seed = spec.seed,
        imports = imports.join("\n"),
        case_type = case_type,
        cases = cases,
        call_a = call_a,
        call_b = call_b,
        relation = relation,
        marker = VIOLATION_MARKER,
        done = DONE_MARKER,
    )
}

/// 渲染 Python 蜕变驱动程序。
fn render_python_metamorphic_driver(spec: &MetamorphicSpec, inputs: &[InputValue]) -> String {
    let call_a = spec.call_template.replace("{args}", "*input");
    let value_expr = metamorphic_value_expr(&spec.domain, Language::Python);
    let transformed = spec.transform.replace("{input}", &value_expr);
    // Python 侧的实参形状与 Rust 不同：源调用用 `*input` 展开，因此元组域的变换
    // 要产出**一个字面元组**，调用侧再 `*` 展开一次；标量域直接传值。
    let spread = if matches!(spec.domain, InputDomain::Tuple { .. }) {
        format!("*({transformed})")
    } else {
        transformed
    };
    let call_b = spec.call_template.replace("{args}", &spread);
    let imports: Vec<String> = spec
        .imports
        .iter()
        .map(|import| format!("import {import}"))
        .collect();
    let literals: Vec<String> = inputs
        .iter()
        .enumerate()
        .map(|(index, value)| {
            format!(
                "    ({index}, {}),",
                python_case_literal(&spec.domain, value)
            )
        })
        .collect();
    format!(
        r#"# 由 `ProofForge` 生成的蜕变测试驱动程序，请勿手改。
# 目标：`{template}`；样本数：{count}；种子：{seed}。
{imports}

CASES = [
{literals}
]


def test_proofforge_metamorphic():
    for label, input in CASES:
        try:
            output_a = {call_a}
            output_b = {call_b}
            satisfied = bool({relation})
        except Exception:
            print(f"{crash} {{label}}", flush=True)
            raise AssertionError(f"{crash} {{label}}")
        if not satisfied:
            print(f"{marker} {{label}}", flush=True)
            raise AssertionError(f"{marker} {{label}}")
    print("{done}", flush=True)
"#,
        template = spec.call_template,
        count = inputs.len(),
        seed = spec.seed,
        imports = imports.join("\n"),
        literals = literals.join("\n"),
        call_a = call_a,
        call_b = call_b,
        relation = spec.relation,
        marker = VIOLATION_MARKER,
        crash = CRASH_MARKER,
        done = DONE_MARKER,
    )
}

/// 变换模板里 `{input}` 被替换成的「整个输入值」表达式。
///
/// 与 [`rust_arg_exprs`] 的分工：后者给出**拆开后**的实参（`input.0, input.1`），
/// 前者给出**未拆开**的值，因为变换是对输入整体定义的（「把列表反转」对单个元素
/// 没有意义）。
fn metamorphic_value_expr(domain: &InputDomain, language: Language) -> String {
    match (domain, language) {
        (InputDomain::Tuple { .. }, _) => "input".to_owned(),
        (_, Language::Rust) => "input.0".to_owned(),
        (_, Language::Python) => "input[0]".to_owned(),
    }
}

/// 在沙箱里跑一次已经写好的驱动程序。
///
/// # 为什么要有这个函数
///
/// 「怎么调用一次驱动程序」有三处使用（性质、反例缩减、模糊测试会话），而它的
/// 三个细节——目标选择、输出捕获、`--` 分隔符——都必须与 [`interpret`] 的判定
/// 规则严格对齐。分散写三遍就一定会漂移，而漂移的表现是静默的假阴性。
pub(crate) fn run_driver(
    sandbox: &Sandbox,
    language: Language,
    options: &ToolOptions,
) -> ExecutionResult {
    let mut tool_options = options.clone();
    match language {
        Language::Rust => {
            // 只跑生成的这一个测试目标。目标项目自己的测试可能很慢，更要紧的是
            // 它们的失败会让「驱动程序明明没问题」变成非零退出 + 没有任何标记，
            // 于是被误读成证据不足——那是一次彻底的假阴性。
            tool_options.extra_args.push("--test".to_owned());
            tool_options.extra_args.push(DRIVER_STEM.to_owned());
            // `--nocapture` 是 libtest 的参数，必须放在 `--` 之后。没有它，
            // 通过时 cargo 会把 stdout 吞掉，`DONE_MARKER` 永远看不见，
            // 于是「跑完了」和「根本没跑」无法区分。
            tool_options.extra_args.push("--".to_owned());
            tool_options.extra_args.push("--nocapture".to_owned());
        }
        Language::Python => {
            tool_options.extra_args.push("-s".to_owned());
            tool_options
                .extra_args
                .push(language.driver_path().to_owned());
        }
    }
    sandbox.run(Tool::SingleTest, &tool_options)
}

/// 渲染驱动程序源码。
///
/// 输入以字面量写出，并与下标标签成对出现：失败时驱动程序打印标签，
/// `ProofForge` 用它找回对应的输入值。
#[must_use]
pub fn render_driver(spec: &PropertySpec, inputs: &[InputValue]) -> String {
    match spec.language {
        Language::Rust => render_rust_driver(spec, inputs),
        Language::Python => render_python_driver(spec, inputs),
    }
}

/// 渲染 Rust 驱动程序。
fn render_rust_driver(spec: &PropertySpec, inputs: &[InputValue]) -> String {
    let args = rust_arg_exprs(&spec.domain).join(", ");
    let call = escape_braces(&spec.call_template.replace("{args}", &args));
    let imports: Vec<String> = spec
        .imports
        .iter()
        .map(|import| format!("use {import};"))
        .collect();
    let literals: Vec<String> = inputs
        .iter()
        .enumerate()
        .map(|(index, value)| format!("    ({index}, {}),", rust_case_literal(&spec.domain, value)))
        .collect();
    let cases = render_cases(&literals);
    let case_type = rust_case_type(&spec.domain);
    let invariant = escape_braces(&spec.invariant);
    format!(
        r#"//! 由 `ProofForge` 生成的性质测试驱动程序，请勿手改。
//! 目标：`{template}`；样本数：{count}；种子：{seed}。
#![allow(unused_imports, dead_code, clippy::all)]

{imports}

#[test]
fn proofforge_property() {{
    let cases: Vec<(usize, {case_type})> = {cases};
    for (label, input) in cases {{
        // panic 也要算违反：程序在指定输入下没有给出任何输出，
        // 「output 满足不变式」这一命题因此不成立。catch_unwind 让后续样本
        // 继续跑——否则第一个 panic 会终止整批，剩下的输入连试的机会都没有。
        // AssertUnwindSafe 是必要的：闭包捕获了目标类型的值，而它不保证
        // UnwindSafe；我们只记录「发生过 panic」，不观察被破坏的不变量。
        let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{
            let output = {call};
            {invariant}
        }}));
        if !matches!(observed, Ok(true)) {{
            // 标记必须先打到 stdout，再 panic。
            //
            // panic 消息走的是 stderr（libtest 只在失败报告里转述它），而
            // [`violation_index`] 读的是 stdout。把标记只写进 assert 消息的
            // 版本在真实运行里找不到反例——那不是「没违反」，是「看见了却
            // 没读到」，比前者危险得多。
            println!("{marker} {{label}}");
            panic!("{marker} {{label}}");
        }}
    }}
    println!("{done}");
}}
"#,
        template = spec.call_template,
        count = inputs.len(),
        seed = spec.seed,
        imports = imports.join("\n"),
        cases = cases,
        case_type = case_type,
        call = call,
        invariant = invariant,
        marker = VIOLATION_MARKER,
        done = DONE_MARKER,
    )
}

/// 渲染 Python 驱动程序。
fn render_python_driver(spec: &PropertySpec, inputs: &[InputValue]) -> String {
    // `import X` 而不是 `from X import *`：调用模板写的是 `X.f(...)` 这样的
    // 限定名，而 `from X import *` 只把 X 的成员搬进来，**不绑定 `X` 本身**，
    // 于是生成的驱动程序一定 NameError。Rust 侧对应的是 `use mycrate;`。
    let imports: Vec<String> = spec
        .imports
        .iter()
        .map(|import| format!("import {import}"))
        .collect();
    let literals: Vec<String> = inputs
        .iter()
        .enumerate()
        .map(|(index, value)| {
            format!(
                "    ({index}, {}),",
                python_case_literal(&spec.domain, value)
            )
        })
        .collect();
    let call = spec.call_template.replace("{args}", "*input");
    format!(
        r#"# 由 `ProofForge` 生成的性质测试驱动程序，请勿手改。
# 目标：`{template}`；样本数：{count}；种子：{seed}。
{imports}

CASES = [
{literals}
]


def test_proofforge_property():
    for label, input in CASES:
        try:
            output = {call}
            satisfied = bool({invariant})
        except Exception:
            # 目标抛异常：连输出都没有，与「输出不满足不变式」是两类观测。
            print(f"{crash} {{label}}", flush=True)
            raise AssertionError(f"{crash} {{label}}")
        if not satisfied:
            print(f"{marker} {{label}}", flush=True)
            raise AssertionError(f"{marker} {{label}}")
    print("{done}", flush=True)
"#,
        template = spec.call_template,
        count = inputs.len(),
        seed = spec.seed,
        imports = imports.join("\n"),
        literals = literals.join("\n"),
        call = call,
        invariant = spec.invariant,
        marker = VIOLATION_MARKER,
        crash = CRASH_MARKER,
        done = DONE_MARKER,
    )
}

/// 把单个花括号转义成 Rust 格式化字符串里的字面量。
///
/// 调用模板与不变式都是用户提供的表达式，可能包含闭包或结构体字面量；
/// 不转义就会让渲染出的驱动程序无法编译。
fn escape_braces(text: &str) -> String {
    text.replace('{', "{{").replace('}', "}}")
}

/// 渲染一个样本的 Rust 字面量（与 `cases` 的元素类型一致）。
fn rust_case_literal(domain: &InputDomain, value: &InputValue) -> String {
    match domain {
        InputDomain::Tuple { .. } => value.to_rust_literal(),
        // 标量域也要包成单元结构，才能与标签一起放进同一个 Vec。
        _ => format!("({},)", value.to_rust_literal()),
    }
}

/// 渲染一个样本的 Python 字面量。
fn python_case_literal(domain: &InputDomain, value: &InputValue) -> String {
    match domain {
        InputDomain::Tuple { .. } => value.to_python_literal(),
        _ => format!("({},)", value.to_python_literal()),
    }
}

/// `cases` 向量的元素类型（Rust）。
///
/// 标量域必须与 [`rust_case_literal`] 保持一致地包成单元结构：字面量写成
/// `(0_i64,)` 而类型写成 `i64` 会让驱动程序编译不过，而且报的是「类型不匹配」
/// 这种跟真实原因无关的错。
fn rust_case_type(domain: &InputDomain) -> String {
    match domain {
        InputDomain::Tuple { fields } => {
            let rendered: Vec<String> = fields.iter().map(rust_value_type).collect();
            // 必须带尾逗号：单字段元组写成 `(i64)` 在 Rust 里就是 `i64`，
            // 而 [`rust_case_literal`] 渲染出的字面量是 `(0_i64,)`。两者对不上
            // 会让驱动程序编译不过，于是每条单字段元组性质永远证据不足。
            format!("({},)", rendered.join(", "))
        }
        other => format!("({},)", rust_value_type(other)),
    }
}

/// 单个值的 Rust 类型名。
fn rust_value_type(domain: &InputDomain) -> String {
    match domain {
        InputDomain::Int { .. } => "i64".to_owned(),
        InputDomain::Bool => "bool".to_owned(),
        InputDomain::Str { .. } => "String".to_owned(),
        InputDomain::Tuple { fields } => {
            let rendered: Vec<String> = fields.iter().map(rust_value_type).collect();
            format!("({},)", rendered.join(", "))
        }
        InputDomain::List { item, .. } => format!("Vec<{}>", rust_value_type(item)),
    }
}

/// 调用目标时的实参表达式列表（Rust）。
///
/// 元组域按位置展开（`input.0, input.1`），标量域同样解包一次（`input.0`）——
/// 因为 [`rust_case_literal`] 把每个样本都包成了元组，好让标签和输入能放进同一个
/// `Vec`。这一条决定了调用模板写 `f({args})` 时对两种域都成立。
fn rust_arg_exprs(domain: &InputDomain) -> Vec<String> {
    match domain {
        InputDomain::Tuple { fields } => (0..fields.len())
            .map(|index| format!("input.{index}"))
            .collect(),
        _ => vec!["input.0".to_owned()],
    }
}

/// 解读一次执行的结果。
///
/// # 判定顺序
///
/// 先看「有没有跑起来」，再看「有没有违反」。顺序不能反：一个编译失败的驱动
/// 程序同样有非零退出码，把它解读成「性质被违反」会产出彻头彻尾的假反例。
///
/// 「跑起来了」靠 [`DONE_MARKER`] 判定，而不是退出码。编译失败、测试目标不
/// 存在、参数写错都会让 cargo 非零退出，而输出里没有任何测试执行记录——只看
/// 退出码的话，这三种情况和「全部样本都满足」混在一起。
fn interpret(execution: &ExecutionResult, inputs: &[InputValue]) -> DriverOutcome {
    if execution.failed_to_start() {
        return DriverOutcome::Inconclusive(format!(
            "toolchain unavailable: {}",
            first_line(&execution.stderr)
        ));
    }
    if execution.status == ExecutionStatus::Signalled {
        return DriverOutcome::Inconclusive(format!(
            "timed out or was killed: {}",
            first_line(&execution.stderr)
        ));
    }
    if let Some(index) = violation_index(&execution.stdout) {
        return match inputs.get(index) {
            Some(_) => DriverOutcome::Violated(index),
            // 标签越界说明渲染与解读不一致，属于 `ProofForge` 的缺陷，不是目标
            // 项目的缺陷，因此按证据不足处理。
            None => {
                DriverOutcome::Inconclusive(format!("driver reported unknown sample index {index}"))
            }
        };
    }
    // 违反标记优先于完成标记：驱动程序在打印完成标记之前就中断了（第一个
    // 违反让 assert 失败），此时 stdout 里只有标记、没有完成行。
    if !execution.stdout.contains(DONE_MARKER) {
        return DriverOutcome::Inconclusive(format!(
            "driver did not run to completion: {}",
            first_line(&first_error_line(&execution.stderr))
        ));
    }
    if execution.succeeded() {
        return DriverOutcome::Satisfied;
    }
    DriverOutcome::Inconclusive(format!(
        "driver finished but the test harness failed: {}",
        first_line(&first_error_line(&execution.stderr))
    ))
}

/// 从标准输出里找出崩溃标记后的样本下标集合。
///
/// 与 [`violation_indices`] 同形：缩减驱动程序把两类下标分别打在两行里。
pub(crate) fn crash_indices(stdout: &str) -> Option<Vec<usize>> {
    let line = stdout.lines().find(|line| line.contains(CRASH_MARKER))?;
    let list = line
        .split_once(CRASH_MARKER)?
        .1
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let mut out = Vec::new();
    for token in list.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        out.push(token.parse().ok()?);
    }
    Some(out)
}

/// 从编译器输出里挑出**可操作**的那几行。
///
/// cargo 的第一行非空输出通常是 `Compiling foo v0.1.0` 这种进度信息，直接把它
/// 当原因呈现给使用者等于没给原因。而只取 `error` 行同样不够：`error[E0308]:
/// mismatched types` 不告诉你错在哪、也不说怎么改，`--> path:line:col` 与
/// `help:` 才是可操作的部分（AGENTS.md 4.3）。
///
/// 因此这里取第一个 `error` 行，连同它到下一个 `error` 行之间的定位行与建议行。
/// 指向驱动程序自身的定位行被丢掉：那个文件在沙箱里，运行结束时已经删除，
/// 把使用者指向一个不存在的路径比不给路径更糟。
pub(crate) fn first_error_line(text: &str) -> String {
    const LIMIT: usize = 400;
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let Some(start) = lines.iter().position(|line| line.starts_with("error")) else {
        return first_line(text);
    };
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.starts_with("error"))
        .map_or(lines.len(), |offset| start + 1 + offset);
    let mut picked = vec![lines[start].to_owned()];
    picked.extend(
        lines[start + 1..end]
            .iter()
            .filter(|line| {
                line.starts_with("-->") || line.starts_with("help:") || line.starts_with('=')
            })
            // 驱动程序的路径在沙箱里，跑完就没了。
            .filter(|line| !line.contains(DRIVER_STEM))
            .map(|line| (*line).to_owned()),
    );
    picked.join(" | ").chars().take(LIMIT).collect()
}

/// 从标准输出里找出违反标记后的样本下标。
///
/// 公开给报告层：它要区分「性质被违反」与「没跑起来」，而两者的退出码
/// 同样是非零。
pub(crate) fn violation_index(stdout: &str) -> Option<usize> {
    stdout.lines().find_map(|line| {
        let (_, rest) = line.split_once(VIOLATION_MARKER)?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// 从标准输出里找出**全部**仍然失败的候选下标。
///
/// 缩减驱动程序一次跑一批候选并把失败的下标集合打进 panic 消息，因此这里要
/// 解析形如 `PROOFFORGE_VIOLATION [0, 2]` 的一段。空列表表示这一批候选全都
/// 不再失败——那同样是一条有效的结论。
pub(crate) fn violation_indices(stdout: &str) -> Option<Vec<usize>> {
    let line = stdout
        .lines()
        .find(|line| line.contains(VIOLATION_MARKER))?;
    let list = line
        .split_once(VIOLATION_MARKER)?
        .1
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let mut out = Vec::new();
    for token in list.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        out.push(token.parse().ok()?);
    }
    Some(out)
}

/// 一次执行是否报告了违反标记。
///
/// 与 [`reports_violation`] 的分工：后者同时接受带下标与不带下标两种形状，面向
/// 报告层；这里只回答「有没有标记」，面向缩减循环——它按候选逐个执行，不需要
/// 下标。
pub(crate) fn reports_violation_marker(stdout: &str) -> bool {
    violation_index(stdout).is_some()
}

/// 一次执行是否真的跑完了。
///
/// 判定读标记而不是退出码，理由见 [`interpret`]：编译失败、测试目标不存在、参数
/// 写错都会让 cargo 非零退出，而输出里没有任何测试执行记录。
pub(crate) fn driver_ran(stdout: &str) -> bool {
    stdout.contains(DONE_MARKER) || reports_violation_marker(stdout)
}

/// 缩减驱动程序是否真的跑完了。
///
/// 它跑完时会**三选一**地打印标记：有候选违反不变式、有候选让程序崩溃、或全部
/// 通过（完成标记）。三条输出都没有，就说明它根本没执行（编译不过、目标不存在、
/// 参数写错）。这个区分是必需的——把「没跑」读成「没有候选失败」会让缩减误以为
/// 已经收敛，于是反例保持原样还被标上 `converged: true`。
pub(crate) fn reduction_driver_ran(stdout: &str) -> bool {
    violation_indices(stdout).is_some()
        || crash_indices(stdout).is_some()
        || stdout.contains(DONE_MARKER)
}

/// 空样本集时渲染出的 `vec!` 必须是 `vec![]` 而不是带空行的 `vec![`。
///
/// 生成的代码会被人读（它要随反例一起交出去），空行让它看起来像被截断了。
fn render_cases(literals: &[String]) -> String {
    if literals.is_empty() {
        return "vec![]".to_owned();
    }
    format!("vec![\n{}\n    ]", literals.join("\n"))
}

/// 渲染「一次测一批候选」的缩减驱动程序。
///
/// 与 [`render_driver`] 的差别：不满足不变式时不立即 panic，而是把下标收集起来
/// 最后统一报告。这样一次编译执行就能淘汰整批候选——缩减的执行成本直接决定
/// 反例能被压到多小（需求文档 15.3：自动验证成本过高是真实风险）。
#[must_use]
pub fn render_reduction_driver(spec: &PropertySpec, candidates: &[InputValue]) -> String {
    match spec.language {
        Language::Rust => render_rust_reduction_driver(spec, candidates),
        Language::Python => render_python_reduction_driver(spec, candidates),
    }
}

/// 渲染 Rust 缩减驱动程序。
fn render_rust_reduction_driver(spec: &PropertySpec, candidates: &[InputValue]) -> String {
    let args = rust_arg_exprs(&spec.domain).join(", ");
    let call = escape_braces(&spec.call_template.replace("{args}", &args));
    let imports: Vec<String> = spec
        .imports
        .iter()
        .map(|import| format!("use {import};"))
        .collect();
    let literals: Vec<String> = candidates
        .iter()
        .enumerate()
        .map(|(index, value)| format!("    ({index}, {}),", rust_case_literal(&spec.domain, value)))
        .collect();
    let cases = render_cases(&literals);
    let case_type = rust_case_type(&spec.domain);
    let invariant = escape_braces(&spec.invariant);
    format!(
        r#"//! 由 `ProofForge` 生成的反例缩减驱动程序，请勿手改。
#![allow(unused_imports, dead_code, clippy::all)]

{imports}

#[test]
fn proofforge_reduction() {{
    let cases: Vec<(usize, {case_type})> = {cases};
    let mut failing: Vec<usize> = Vec::new();
    let mut crashing: Vec<usize> = Vec::new();
    for (label, input) in cases {{
        // 与性质驱动程序同样把 panic 记为失败：否则缩减会在第一个 panic 上
        // 中断，剩下的候选连「是否仍然失败」都问不出来。
        let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{
            let output = {call};
            {invariant}
        }}));
        match observed {{
            Ok(false) => failing.push(label),
            Err(_) => crashing.push(label),
            Ok(true) => {{}}
        }}
    }}
    // 与性质驱动程序同理：标记走 stdout，panic 只负责让 cargo 非零退出。
    if !failing.is_empty() {{
        println!("{marker} {{failing:?}}");
        panic!("{marker} {{failing:?}}");
    }}
    if !crashing.is_empty() {{
        println!("{crash} {{crashing:?}}");
        panic!("{crash} {{crashing:?}}");
    }}
    println!("{done}");
}}
"#,
        imports = imports.join("\n"),
        cases = cases,
        case_type = case_type,
        call = call,
        invariant = invariant,
        marker = VIOLATION_MARKER,
        crash = CRASH_MARKER,
        done = DONE_MARKER,
    )
}

/// 渲染 Python 缩减驱动程序。
fn render_python_reduction_driver(spec: &PropertySpec, candidates: &[InputValue]) -> String {
    // 与 [`render_python_driver`] 同理由 `import X`，不用 `from X import *`。
    let imports: Vec<String> = spec
        .imports
        .iter()
        .map(|import| format!("import {import}"))
        .collect();
    let literals: Vec<String> = candidates
        .iter()
        .enumerate()
        .map(|(index, value)| {
            format!(
                "    ({index}, {}),",
                python_case_literal(&spec.domain, value)
            )
        })
        .collect();
    let call = spec.call_template.replace("{args}", "*input");
    format!(
        r#"# 由 `ProofForge` 生成的反例缩减驱动程序，请勿手改。
{imports}

CASES = [
{literals}
]


def test_proofforge_reduction():
    failing = []
    crashing = []
    for label, input in CASES:
        try:
            output = {call}
            if not ({invariant}):
                failing.append(label)
        except Exception:
            crashing.append(label)
    if failing:
        print(f"{marker} {{failing}}", flush=True)
        raise AssertionError(f"{marker} {{failing}}")
    if crashing:
        print(f"{crash} {{crashing}}", flush=True)
        raise AssertionError(f"{crash} {{crashing}}")
    print("{done}", flush=True)
"#,
        imports = imports.join("\n"),
        literals = literals.join("\n"),
        call = call,
        invariant = spec.invariant,
        marker = VIOLATION_MARKER,
        crash = CRASH_MARKER,
        done = DONE_MARKER,
    )
}

/// 取第一行非空文本，用于把冗长的编译器输出压成一行原因。
fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::evidence::Command;

    fn spec() -> PropertySpec {
        PropertySpec {
            name: None,
            language: Language::Rust,
            imports: vec!["fixture".to_owned()],
            call_template: "fixture::parse({args})".to_owned(),
            domain: InputDomain::Int { min: 0, max: 10 },
            invariant: "output >= 0".to_owned(),
            samples: 3,
            seed: 1,
        }
    }

    fn execution(stdout: &str, exit_code: Option<i32>) -> ExecutionResult {
        full_execution(stdout, "", exit_code)
    }

    /// 分别指定两条输出流的执行记录。
    ///
    /// 编译错误走 stderr、测试输出走 stdout，判定规则要用到两条流，因此不能
    /// 把它们塞进同一个参数。
    fn full_execution(stdout: &str, stderr: &str, exit_code: Option<i32>) -> ExecutionResult {
        ExecutionResult {
            command: Command::new("cargo"),
            status: ExecutionStatus::Exited,
            exit_code,
            signal: None,
            duration_ms: 1,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            truncated: false,
        }
    }

    #[test]
    fn rust_driver_embeds_inputs_as_literals_with_labels() {
        let spec = spec();
        let inputs = spec.domain.samples(spec.seed, spec.samples);
        let source = render_driver(&spec, &inputs);
        assert!(source.contains("#![allow(unused_imports, dead_code, clippy::all)]"));
        assert!(source.contains("use fixture;"));
        assert!(source.contains("let output = fixture::parse(input.0);"));
        assert!(
            source.contains("if !matches!(observed, Ok(true))"),
            "不变式必须真的判定 observed：{source}"
        );
        assert!(source.contains("output >= 0"));
        assert!(source.contains(&format!("{VIOLATION_MARKER} {{label}}")));
        // 每个样本都要带下标标签。
        assert!(source.contains("(0, (0_i64,))"), "{source}");
        assert_eq!(source.matches("    (").count(), inputs.len());
    }

    #[test]
    fn tuple_domain_expands_arguments_positionally() {
        let spec = PropertySpec {
            name: None,
            call_template: "fixture::blend({args})".to_owned(),
            domain: InputDomain::Tuple {
                fields: vec![
                    InputDomain::Int { min: 0, max: 1 },
                    InputDomain::Str { max_len: 2 },
                ],
            },
            ..spec()
        };
        let inputs = spec.domain.samples(spec.seed, 2);
        let source = render_driver(&spec, &inputs);
        assert!(
            source.contains("let output = fixture::blend(input.0, input.1);"),
            "{source}"
        );
        // 元组类型必须带尾逗号：`(i64, String)` 与字面量 `(0_i64, "".to_owned(),)`
        // 对不上时驱动程序编译不过。单字段元组写成 `(i64)` 更是直接被 Rust 解析成
        // `i64`，那是同一类 bug 的更隐蔽形态。
        assert!(source.contains("Vec<(usize, (i64, String,))>"), "{source}");
    }

    #[test]
    fn a_single_field_tuple_keeps_its_trailing_comma() {
        // 回归：`(i64)` 在 Rust 里就是 `i64`，而字面量是 `(0_i64,)`。
        // 对不上让驱动程序编译不过，于是每条单字段元组性质永远证据不足。
        let spec = PropertySpec {
            name: None,
            call_template: "fixture::wrap({args})".to_owned(),
            domain: InputDomain::Tuple {
                fields: vec![InputDomain::Int { min: 0, max: 1 }],
            },
            ..spec()
        };
        let inputs = spec.domain.samples(spec.seed, 1);
        let source = render_driver(&spec, &inputs);
        assert!(source.contains("Vec<(usize, (i64,))>"), "{source}");
        assert!(source.contains("(0, (0_i64,))"), "{source}");
        assert!(
            source.contains("let output = fixture::wrap(input.0);"),
            "{source}"
        );
    }

    #[test]
    fn python_driver_uses_star_args_and_python_spelling() {
        let spec = PropertySpec {
            name: None,
            language: Language::Python,
            imports: vec!["fixture".to_owned()],
            call_template: "parse({args})".to_owned(),
            domain: InputDomain::Bool,
            ..spec()
        };
        let inputs = spec.domain.samples(spec.seed, 2);
        let source = render_driver(&spec, &inputs);
        assert!(source.contains("output = parse(*input)"));
        assert!(source.contains("satisfied = bool(output >= 0)"));
        assert!(source.contains("(0, (False,))"), "{source}");
    }

    #[test]
    fn python_imports_bind_the_module_name_itself() {
        // `from X import *` 只搬成员，不绑定 `X`：调用模板写 `X.f(...)` 时
        // 生成的驱动程序一定 NameError。必须用 `import X`。
        let spec = PropertySpec {
            name: None,
            language: Language::Python,
            imports: vec!["fixture".to_owned()],
            call_template: "fixture::parse({args})".to_owned(),
            domain: InputDomain::Bool,
            ..spec()
        };
        let source = render_driver(&spec, &spec.domain.samples(1, 1));
        assert!(source.contains("import fixture"), "{source}");
        assert!(
            !source.contains("from fixture import"),
            "星号导入不绑定模块名，限定调用会 NameError：{source}"
        );
    }

    #[test]
    fn the_python_reduction_driver_also_uses_plain_imports() {
        let spec = PropertySpec {
            name: None,
            language: Language::Python,
            imports: vec!["fixture".to_owned()],
            call_template: "fixture::parse({args})".to_owned(),
            domain: InputDomain::Bool,
            ..spec()
        };
        let source = render_reduction_driver(&spec, &[InputValue::Bool(true)]);
        assert!(source.contains("import fixture"), "{source}");
        assert!(!source.contains("from fixture import"), "{source}");
    }

    #[test]
    fn braces_in_user_expressions_are_escaped() {
        // 调用模板里的字面花括号不能被当成 Rust 的格式化占位符。
        let base = spec();
        let with_braces = PropertySpec {
            name: None,
            call_template: "fixture::wrap({args}, |v| v + 1)".to_owned(),
            ..base.clone()
        };
        let source = render_driver(&with_braces, &[InputValue::Int(1)]);
        assert!(
            source.contains("fixture::wrap(input.0, |v| v + 1)"),
            "{source}"
        );
        // 不变式里的结构体字面量同样要转义。
        let with_literal = PropertySpec {
            name: None,
            invariant: "output == Point { x: 0, y: 0 }".to_owned(),
            ..base
        };
        let source = render_driver(&with_literal, &[InputValue::Int(1)]);
        assert!(
            source.contains("output == Point {{ x: 0, y: 0 }}"),
            "{source}"
        );
    }

    #[test]
    fn passing_run_is_satisfied() {
        let outcome = interpret(
            &execution(
                &format!("{DONE_MARKER}\ntest result: ok. 1 passed\n"),
                Some(0),
            ),
            &[],
        );
        assert_eq!(outcome, DriverOutcome::Satisfied);
    }

    #[test]
    fn a_clean_exit_without_the_done_marker_is_inconclusive() {
        // 关键的一条：退出码 0 但驱动程序没打印完成标记，说明它压根没跑。
        // 编译失败常常就是这样收场的。把这种情况记成「满足」就是假阴性。
        let outcome = interpret(&execution("test result: ok. 0 passed\n", Some(0)), &[]);
        assert!(
            matches!(&outcome, DriverOutcome::Inconclusive(reason) if reason.contains("did not run to completion")),
            "{outcome:?}"
        );
    }

    #[test]
    fn the_done_marker_survives_being_interleaved_with_test_output() {
        // `--nocapture` 下驱动程序的输出与 libtest 自己的输出混在一起，
        // 完成标记仍然必须被认出来。
        let stdout = format!(
            "running 1 test\n{DONE_MARKER}\ntest proofforge_property ... ok\n\ntest result: ok.\n"
        );
        assert_eq!(
            interpret(&execution(&stdout, Some(0)), &[]),
            DriverOutcome::Satisfied
        );
    }

    #[test]
    fn violation_marker_yields_the_matching_input() {
        let inputs = vec![InputValue::Int(0), InputValue::Int(4), InputValue::Int(9)];
        let outcome = interpret(
            &execution(&format!("panicked: {VIOLATION_MARKER} 1\n"), Some(101)),
            &inputs,
        );
        assert_eq!(outcome, DriverOutcome::Violated(1));
    }

    #[test]
    fn compile_failure_is_inconclusive_not_a_violation() {
        // 编译失败同样有非零退出码；把它解读成「性质被违反」会产出假反例。
        let outcome = interpret(
            &full_execution(
                "",
                "   Compiling fixture v0.1.0\nerror[E0425]: cannot find value `x`\n",
                Some(101),
            ),
            &[InputValue::Int(1)],
        );
        assert!(
            matches!(&outcome, DriverOutcome::Inconclusive(reason) if reason.contains("did not run to completion") && reason.contains("E0425")),
            "{outcome:?}"
        );
    }

    #[test]
    fn failed_to_start_is_inconclusive() {
        let mut execution = execution("", None);
        execution.status = ExecutionStatus::FailedToStart;
        execution.stderr = "cargo: not found".to_owned();
        let outcome = interpret(&execution, &[InputValue::Int(1)]);
        assert!(
            matches!(&outcome, DriverOutcome::Inconclusive(reason) if reason.contains("toolchain unavailable")),
            "{outcome:?}"
        );
    }

    #[test]
    fn timeout_is_inconclusive() {
        let mut execution = execution("", None);
        execution.status = ExecutionStatus::Signalled;
        execution.stderr = "timed out after 30s".to_owned();
        let outcome = interpret(&execution, &[InputValue::Int(1)]);
        assert!(
            matches!(&outcome, DriverOutcome::Inconclusive(reason) if reason.contains("timed out")),
            "{outcome:?}"
        );
    }

    #[test]
    fn out_of_range_label_is_inconclusive() {
        let outcome = interpret(
            &execution(&format!("{VIOLATION_MARKER} 99\n"), Some(101)),
            &[InputValue::Int(1)],
        );
        assert!(
            matches!(&outcome, DriverOutcome::Inconclusive(reason) if reason.contains("unknown sample index")),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_violation_outranks_a_missing_done_marker() {
        // 驱动程序在打印完成标记之前就被 assert 打断了：stdout 里只有违反标记。
        // 判定顺序必须让违反赢，否则真反例会被说成「证据不足」。
        let inputs = vec![InputValue::Int(0), InputValue::Int(4)];
        let outcome = interpret(
            &execution(&format!("panicked: {VIOLATION_MARKER} 1\n"), Some(101)),
            &inputs,
        );
        assert_eq!(outcome, DriverOutcome::Violated(1));
    }

    #[test]
    fn violation_index_ignores_unrelated_output() {
        assert_eq!(violation_index("running 1 test\nPROOF X\n"), None);
        assert_eq!(violation_index("PROOFFORGE_VIOLATION 7\n"), Some(7));
        assert_eq!(
            violation_index("test PROOFFORGE_VIOLATION 2 -- ok\n"),
            Some(2)
        );
    }

    #[test]
    fn language_driver_paths_are_distinct() {
        assert_eq!(Language::Rust.driver_path(), "tests/proofforge_driver.rs");
        assert_eq!(Language::Python.driver_path(), "test_proofforge_driver.py");
        assert_ne!(Language::Rust.as_str(), Language::Python.as_str());
    }
}
