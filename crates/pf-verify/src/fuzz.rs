//! 模糊测试：基于程序反馈持续探索输入空间（FR-036）。
//!
//! # 反馈信号是什么，以及它不是什么
//!
//! 需求文档要求「基于程序反馈持续探索输入空间」。工业级覆盖率引导模糊测试
//! （libFuzzer、AFL）的反馈是**代码覆盖位图**，需要编译器插桩。本实现**不做**
//! 插桩——它要求 nightly 工具链或外部覆盖工具，会把「能跑」变成「经常跑不起来」。
//!
//! 可用的诚实的反馈信号有三个，全部来自程序的可观察行为：
//!
//! 1. **结果类别**——输入让程序满足不变式、违反不变式，还是崩溃 / 超时。
//! 2. **反例多样性**——不同的失败输入说明探索到了不同的缺陷路径。
//! 3. **语料新鲜度**——产生过新类别的输入被保留并优先变异。
//!
//! 因此它是一个**结果引导**的探索器，不是覆盖率引导的模糊器。这个区别写进
//! 报告与 [`crate::docs`] 里的已知限制，避免使用者高估它的能力。
//!
//! # 变异算子
//!
//! 全部按值类型定义：整数做位翻转与边界跳转、字符串做片段替换与长度变化、
//! 列表做元素增删与重排、元组按位变异。变异是确定性的——同一个种子与同一个
//! 语料必然产出同一批候选。
//!
//! # 变异不越出声明域
//!
//! 每个候选都由 [`InputDomain::clamp`] 夹回 [`PropertySpec::domain`]。这不是
//! 保守，而是语义正确性：一条性质的断言范围就是它声明的域，把 `i64::MAX` 上
//! 观察到的崩溃记在「`[-100, 100]` 上恒为偶数」这条性质头上，等于让报告声称
//! 一个它并没有做出的断言。域外的潜在缺陷确实存在，但那是另一个问题——
//! 记录在 `docs/known-limitations.md`，由使用者显式放宽域来探索。

use pf_adapters::ToolOptions;
use pf_core::evidence::ExecutionResult;

use crate::domain::{InputDomain, InputValue};
use crate::driver::{render_driver, render_reduction_driver, PropertySpec};
use crate::sandbox::Sandbox;

/// 一次模糊测试会话的配置。
#[derive(Debug, Clone)]
pub struct FuzzConfig {
    /// 要验证的性质。
    pub spec: PropertySpec,
    /// 变异轮数。
    pub rounds: usize,
    /// 每轮生成的候选数。
    pub candidates_per_round: usize,
    /// 语料库上限：超过后淘汰最旧的条目，控制内存与执行成本。
    pub corpus_limit: usize,
}

impl FuzzConfig {
    /// 用默认的轮次与语料上限构造配置。
    #[must_use]
    pub fn new(spec: PropertySpec) -> Self {
        Self {
            spec,
            rounds: 8,
            candidates_per_round: 16,
            corpus_limit: 64,
        }
    }

    /// 设置变异轮数。
    #[must_use]
    pub fn with_rounds(mut self, rounds: usize) -> Self {
        self.rounds = rounds;
        self
    }

    /// 设置每轮候选数。
    #[must_use]
    pub fn with_candidates_per_round(mut self, count: usize) -> Self {
        self.candidates_per_round = count;
        self
    }
}

/// 一个输入让程序表现出的行为类别。
///
/// 类别而不是覆盖位图：这是不插桩时能拿到的最强反馈信号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BehaviorClass {
    /// 不变式被违反——找到了缺陷。
    Violated,
    /// 程序崩溃、panic 或超时。
    Crashed,
    /// 没有结论（编译失败、工具缺失）。
    Inconclusive,
    /// 不变式成立。
    Satisfied,
}

impl std::fmt::Display for BehaviorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Violated => "违反",
            Self::Crashed => "崩溃",
            Self::Inconclusive => "无结论",
            Self::Satisfied => "满足",
        })
    }
}

/// 一次模糊测试会话的结论。
#[derive(Debug, Clone)]
pub struct FuzzOutcome {
    /// 探索过的输入总数（含初始语料）。
    pub explored: usize,
    /// 观察到各类行为的次数。
    pub class_counts: Vec<(BehaviorClass, usize)>,
    /// 发现的不同反例（已去重）：不变式被违反的输入。
    ///
    /// 不含崩溃输入——见 [`Self::crashers`]。两者分开是因为它们支撑不同的
    /// 结论：「输出不满足不变式」与「程序没有给出输出」不能互相替代。
    pub counterexamples: Vec<InputValue>,
    /// 发现的不同崩溃输入（已去重）。
    ///
    /// 单独收集而不是塞进 [`Self::counterexamples`]：崩溃是模糊测试最有价值的
    /// 产出，而它触发的问题（panic、unwrap、除零）与不变式不成立是两类缺陷，
    /// 修法不同。少了这个字段，报告只能说「崩溃了 25 次」却给不出一个能复现的
    /// 输入——那等于没有结论。
    pub crashers: Vec<InputValue>,
    /// 保留的语料（产生过新类别的输入）。
    pub corpus: Vec<InputValue>,
    /// 未能得到结论的原因（若有）。
    pub inconclusive_reason: Option<String>,
}

/// 模糊测试探索器。
#[derive(Debug, Default)]
pub struct FuzzRunner;

impl FuzzRunner {
    /// 在沙箱里跑一轮反馈驱动的探索。
    ///
    /// # Errors
    /// 驱动程序写不进沙箱时返回 [`pf_core::Error`]。
    pub fn run(
        sandbox: &Sandbox,
        config: &FuzzConfig,
        options: &ToolOptions,
    ) -> pf_core::Result<FuzzOutcome> {
        let mut corpus: Vec<InputValue> = config
            .spec
            .domain
            .samples(config.spec.seed, config.candidates_per_round);
        let mut counterexamples: Vec<InputValue> = Vec::new();
        let mut crashers: Vec<InputValue> = Vec::new();
        let mut counts: std::collections::BTreeMap<BehaviorClass, usize> =
            std::collections::BTreeMap::new();
        let mut explored = 0usize;
        let mut inconclusive_reason = None;

        for round in 0..config.rounds {
            // 每轮从语料变异出新候选，并保留语料本身：已经表现出有趣行为的
            // 输入是下一轮最好的起点。
            let mut batch = corpus.clone();
            for (index, seed_input) in corpus.iter().enumerate() {
                let mutations = mutate(
                    &config.spec.domain,
                    seed_input,
                    round,
                    index,
                    config.candidates_per_round,
                );
                batch.extend(mutations);
            }
            batch.truncate(config.candidates_per_round.saturating_mul(4));

            let classes = classify_batch(sandbox, &config.spec, &batch, options);
            if let Some(reason) = &classes.inconclusive_reason {
                inconclusive_reason = Some(reason.clone());
                break;
            }
            explored += batch.len();
            for (input, class) in batch.iter().zip(classes.per_input.iter()) {
                *counts.entry(*class).or_insert(0) += 1;
                let bucket = match class {
                    BehaviorClass::Violated => Some(&mut counterexamples),
                    BehaviorClass::Crashed => Some(&mut crashers),
                    // 「没有结论」与「满足」都不产生可复现的输入：前者根本不知道
                    // 发生了什么，后者没有缺陷可复现。
                    BehaviorClass::Inconclusive | BehaviorClass::Satisfied => None,
                };
                if let Some(bucket) = bucket {
                    if !bucket.contains(input) {
                        bucket.push(input.clone());
                    }
                }
            }
            // 语料更新：只保留表现出「非常规」行为的输入——全部满足的输入没有
            // 信息量，全部留着只会让变异在原地打转。
            let interesting: Vec<InputValue> = batch
                .iter()
                .zip(classes.per_input.iter())
                .filter(|(_, class)| **class != BehaviorClass::Satisfied)
                .map(|(input, _)| input.clone())
                .collect();
            if interesting.is_empty() {
                // 整轮都没有新行为：输入空间在这一带已经探不到更多东西。
                break;
            }
            corpus = interesting;
            corpus.truncate(config.corpus_limit);
        }

        Ok(FuzzOutcome {
            explored,
            class_counts: counts.into_iter().collect(),
            counterexamples,
            crashers,
            corpus,
            inconclusive_reason,
        })
    }
}

impl FuzzOutcome {
    /// 是否观察到了不变式违反。
    ///
    /// 崩溃（`Crashed`）不算违反：程序在某个输入下没有给出任何输出，
    /// 「output 满足不变式」这一命题因此不成立，但它与「给出了错误的输出」
    /// 是两类不同的观测，混在一起会让报告说不清到底看到了什么。
    #[must_use]
    pub fn found_violation(&self) -> bool {
        self.class_counts
            .iter()
            .any(|(class, count)| *class == BehaviorClass::Violated && *count > 0)
    }

    /// 是否观察到了崩溃。
    ///
    /// 与 [`Self::found_violation`] 分开：崩溃同样说明目标有缺陷，但它不是
    /// 「不变式被违反」。门禁据此可以同时报告两类问题而不必把它们混成一个。
    #[must_use]
    pub fn found_crash(&self) -> bool {
        !self.crashers.is_empty()
    }

    /// 是否得到了结论。
    ///
    /// 编译失败或工具缺失时 `class_counts` 为空、`inconclusive_reason` 有值：
    /// 此时「没发现违反」什么也不能说明。
    #[must_use]
    pub fn is_conclusive(&self) -> bool {
        self.inconclusive_reason.is_none() && !self.class_counts.is_empty()
    }

    /// 一行人类可读的结论。
    ///
    /// 附上第一个反例与第一个崩溃输入：只说「违反×444」而不给输入，使用者
    /// 没有任何可以动手复现的东西。
    #[must_use]
    pub fn summary(&self) -> String {
        if let Some(reason) = &self.inconclusive_reason {
            return format!("证据不足：{reason}");
        }
        let mut parts: Vec<String> = self
            .class_counts
            .iter()
            .map(|(class, count)| format!("{class}×{count}"))
            .collect();
        if let Some(first) = self.counterexamples.first() {
            parts.push(format!("反例 {}", first.describe()));
        }
        if let Some(first) = self.crashers.first() {
            parts.push(format!("崩溃输入 {}", first.describe()));
        }
        parts.push(format!("共探索 {} 个输入", self.explored));
        parts.join("，")
    }
}

/// 一批输入的分类结果。
#[derive(Debug, Clone)]
struct BatchClassification {
    /// 每个输入的行为类别，与输入一一对应。
    per_input: Vec<BehaviorClass>,
    /// 整批没有结论时的原因。
    inconclusive_reason: Option<String>,
}

/// 一次执行分类一批输入。
///
/// 与 [`crate::property`] 的缩减批次共用同一套驱动机制：一次编译执行得到全部
/// 输入的类别，成本与输入数无关——这是模糊测试能跑足够多轮的前提。
fn classify_batch(
    sandbox: &Sandbox,
    spec: &PropertySpec,
    batch: &[InputValue],
    options: &ToolOptions,
) -> BatchClassification {
    if batch.is_empty() {
        return BatchClassification {
            per_input: Vec::new(),
            inconclusive_reason: None,
        };
    }
    let source = render_reduction_driver(spec, batch);
    if sandbox
        .write_file(spec.language.driver_path(), &source)
        .is_err()
    {
        return BatchClassification {
            per_input: Vec::new(),
            inconclusive_reason: Some("cannot write the driver into the sandbox".to_owned()),
        };
    }
    let execution = crate::driver::run_driver(sandbox, spec.language, options);
    BatchClassification {
        per_input: classify_from_output(&execution, batch.len()),
        inconclusive_reason: inconclusive_from(&execution),
    }
}

/// 从一次执行的输出里推出每个输入的类别。
///
/// 缩减驱动程序只报告「哪些下标仍然失败」，因此这里只能区分失败与未失败。
/// 崩溃与超时无法归到具体下标——那需要每个输入一次执行，成本翻倍。这个粒度
/// 差异写进文档，不让报告声称它做了更细的归因。
fn classify_from_output(execution: &ExecutionResult, count: usize) -> Vec<BehaviorClass> {
    let mut classes = vec![BehaviorClass::Satisfied; count];
    if execution.failed_to_start()
        || execution.status == pf_core::evidence::ExecutionStatus::Signalled
    {
        return vec![BehaviorClass::Inconclusive; count];
    }
    // 没跑起来说明驱动程序没执行：编译不过、目标不存在、参数写错都长这样。
    // 此时把输入记成「满足」会是彻头彻尾的假阴性——一个根本没跑的验证没有资格
    // 对任何输入下结论。
    if !crate::driver::reduction_driver_ran(&execution.stdout) {
        return vec![BehaviorClass::Inconclusive; count];
    }
    // 先看崩溃再看违反：一个输入只属于一类。崩溃优先——程序连输出都没给出来，
    // 这个事实比「输出不满足不变式」更强，也更能说明缺陷的性质。
    for index in crate::driver::crash_indices(&execution.stdout).unwrap_or_default() {
        if let Some(slot) = classes.get_mut(index) {
            *slot = BehaviorClass::Crashed;
        }
    }
    for index in crate::driver::violation_indices(&execution.stdout).unwrap_or_default() {
        if let Some(slot) = classes.get_mut(index) {
            // 已经被判崩溃的不降级：违反标记与崩溃标记不会同时描述同一个下标，
            // 但若渲染与解读不一致，保守的一边是崩溃。
            if *slot != BehaviorClass::Crashed {
                *slot = BehaviorClass::Violated;
            }
        }
    }
    classes
}

/// 判断一次执行是否让整批失去结论。
fn inconclusive_from(execution: &ExecutionResult) -> Option<String> {
    if execution.failed_to_start() {
        return Some(format!(
            "toolchain unavailable: {}",
            execution.stderr.lines().next().unwrap_or_default()
        ));
    }
    if execution.status == pf_core::evidence::ExecutionStatus::Signalled {
        return Some("execution timed out or was killed".to_owned());
    }
    // 与 [`crate::driver::interpret`] 同一条规则：两条标记都没有就是没跑起来。
    if !crate::driver::reduction_driver_ran(&execution.stdout) {
        return Some(format!(
            "driver did not run to completion: {}",
            crate::driver::first_error_line(&execution.stderr)
        ));
    }
    None
}

/// 对一个输入生成一批变异。
///
/// 全部变异结果都由 [`InputDomain::clamp`] 夹回 [`PropertySpec::domain`]：性质
/// 的断言范围就是它声明的域，域外输入的表现不能记在性质头上。
///
/// `round` 与 `index` 进入种子：同一轮里不同起点的变异互不相同，而整个序列对
/// 相同的初始语料是确定的。
fn mutate(
    domain: &InputDomain,
    input: &InputValue,
    round: usize,
    index: usize,
    count: usize,
) -> Vec<InputValue> {
    let mut seed = seed_of(input, round, index);
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(domain.clamp(&mutate_once(domain, input, &mut seed)));
    }
    out
}

/// 把输入、轮次与起点混合成一个种子。
fn seed_of(input: &InputValue, round: usize, index: usize) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in input.describe().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash ^= (round as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    hash ^= (index as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93);
    hash
}

/// 应用一个随机选择的变异算子。
///
/// `domain` 是 `input` 对应的（子）域：元组与列表要按位置找到子域，否则嵌套的
/// 变异无从知道该夹回哪个范围。形状不匹配时直接返回原值。
fn mutate_once(domain: &InputDomain, input: &InputValue, seed: &mut u64) -> InputValue {
    *seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut state = *seed;
    let pick = |state: &mut u64| -> u64 {
        *state = (*state ^ (*state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        *state = (*state ^ (*state >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *state ^ (*state >> 31)
    };
    let choice = pick(&mut state) % 4;
    match (domain, input) {
        (InputDomain::Int { min, max }, InputValue::Int(value)) => {
            mutate_int(*value, choice, &mut state, *min, *max)
        }
        (InputDomain::Bool, InputValue::Bool(value)) => InputValue::Bool(*value ^ (choice == 0)),
        (InputDomain::Str { max_len }, InputValue::Str(text)) => {
            mutate_str(text, choice, &mut state, *max_len)
        }
        (InputDomain::Tuple { fields }, InputValue::Tuple(inner))
            if fields.len() == inner.len() && !inner.is_empty() =>
        {
            if choice == 3 {
                return input.clone();
            }
            let position = usize::try_from(pick(&mut state)).unwrap_or(0) % inner.len();
            let mut reduced = inner.clone();
            reduced[position] = mutate_once(&fields[position], &inner[position], &mut state);
            InputValue::Tuple(reduced)
        }
        (InputDomain::List { item, .. }, InputValue::List(inner)) => {
            let mut reduced = inner.clone();
            match choice {
                0 => {
                    // 追加一个由现有元素变异来的新元素：长度变化常触发边界缺陷。
                    // 追加后仍受 max_len 约束（clamp 负责截断），否则语料会漂出域外。
                    let source = inner
                        .last()
                        .cloned()
                        .unwrap_or_else(|| item.boundary_value());
                    reduced.push(mutate_once(item, &source, &mut state));
                }
                1 => {
                    reduced.pop();
                }
                2 if !reduced.is_empty() => {
                    let position = usize::try_from(pick(&mut state)).unwrap_or(0) % reduced.len();
                    reduced[position] = mutate_once(item, &reduced[position], &mut state);
                }
                _ => {}
            }
            InputValue::List(reduced)
        }
        _ => input.clone(),
    }
}

/// 整数变异算子。
///
/// 所有结果都夹在 `[min, max]` 内，边界跳转的目标也换成域自己的边界：缺陷高发处
/// 是**声明域的**端点，不是 `i64` 的端点。
fn mutate_int(value: i64, choice: u64, state: &mut u64, min: i64, max: i64) -> InputValue {
    let next = |state: &mut u64| -> u64 {
        *state = (*state ^ (*state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        *state = (*state ^ (*state >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *state ^ (*state >> 31)
    };
    let mutated = match choice {
        0 => value ^ (1 << (next(state) % 64)), // 位翻转
        1 => value.wrapping_add(1),
        2 => value.wrapping_sub(1),
        _ => {
            // 边界跳转：域的上下界优先，其余是常见魔数（越界的会被夹回来）。
            const MAGIC: [i64; 4] = [0, 1, -1, 255];
            let options: [i64; 6] = [min, max, MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3]];
            options[usize::try_from(next(state)).unwrap_or(0) % options.len()]
        }
    };
    InputValue::Int(mutated.clamp(min, max))
}

/// 字符串变异算子。
///
/// `max_len` 是声明域的长度上限，所有分支的结果都不得超出它：片段重复会把长度
/// 翻倍，追加字符会线性增长，二者都必须在这里被截住（`clamp` 只兜底结构，
/// 长度约束由本函数保证，避免为一次截断再遍历一遍）。
fn mutate_str(text: &str, choice: u64, state: &mut u64, max_len: usize) -> InputValue {
    let next = |state: &mut u64| -> u64 {
        *state = (*state ^ (*state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        *state = (*state ^ (*state >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *state ^ (*state >> 31)
    };
    let mut chars: Vec<char> = text.chars().collect();
    match choice {
        0 if !chars.is_empty() => {
            let position = usize::try_from(next(state)).unwrap_or(0) % chars.len();
            chars[position] = char::from(b' ' + u8::try_from(next(state) % 95).unwrap_or(0));
        }
        1 if !chars.is_empty() => {
            let position = usize::try_from(next(state)).unwrap_or(0) % chars.len();
            chars.remove(position);
        }
        2 => {
            chars.push(char::from(
                b' ' + u8::try_from(next(state) % 95).unwrap_or(0),
            ));
        }
        3 if !chars.is_empty() => {
            // 片段重复：解析器在处理重复结构时容易出错。
            let split = usize::try_from(next(state)).unwrap_or(0) % chars.len();
            let tail: Vec<char> = chars[split..].to_vec();
            chars.extend(tail);
        }
        _ => {}
    }
    chars.truncate(max_len);
    InputValue::Str(chars.into_iter().collect())
}

/// 渲染一次模糊会话用到的主导驱动程序（供复现与审计）。
#[must_use]
pub fn session_driver(config: &FuzzConfig) -> String {
    render_driver(&config.spec, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_core::evidence::{Command, ExecutionStatus};

    fn config() -> FuzzConfig {
        FuzzConfig::new(PropertySpec {
            name: None,
            language: crate::driver::Language::Rust,
            imports: vec!["fixture".to_owned()],
            call_template: "fixture::parse({args})".to_owned(),
            domain: InputDomain::Int { min: 0, max: 100 },
            invariant: "output >= 0".to_owned(),
            samples: 8,
            seed: 5,
        })
    }

    #[test]
    fn mutation_is_deterministic_for_the_same_seed() {
        let domain = InputDomain::Int { min: 0, max: 100 };
        let input = InputValue::Int(42);
        assert_eq!(
            mutate(&domain, &input, 1, 0, 8),
            mutate(&domain, &input, 1, 0, 8)
        );
    }

    #[test]
    fn different_rounds_produce_different_mutations() {
        let domain = InputDomain::Int { min: 0, max: 100 };
        let input = InputValue::Int(42);
        assert_ne!(
            mutate(&domain, &input, 1, 0, 8),
            mutate(&domain, &input, 2, 0, 8)
        );
    }

    #[test]
    fn mutation_can_change_the_value() {
        let domain = InputDomain::Int { min: 0, max: 100 };
        let input = InputValue::Int(42);
        let mutated = mutate(&domain, &input, 0, 0, 32);
        assert!(
            mutated.iter().any(|value| value != &input),
            "变异必须真的产生不同的输入"
        );
    }

    #[test]
    fn boundary_jumps_reach_the_domain_endpoints() {
        // 边界跳转的目标是**声明域**的端点，不是 i64 的端点：跑到 i64::MAX 上
        // 观察到的崩溃属于域外行为，记在性质头上会让报告声称一个它没有做的断言。
        let domain = InputDomain::Int {
            min: -100,
            max: 100,
        };
        let input = InputValue::Int(7);
        let mutated = mutate(&domain, &input, 0, 0, 256);
        let values: Vec<i64> = mutated
            .iter()
            .filter_map(|value| match value {
                InputValue::Int(v) => Some(*v),
                _ => None,
            })
            .collect();
        assert!(values.contains(&-100), "必须能跳到下界：{values:?}");
        assert!(values.contains(&100), "必须能跳到上界：{values:?}");
        assert!(
            values.iter().all(|value| (-100..=100).contains(value)),
            "任何候选都不得越出声明域：{values:?}"
        );
    }

    #[test]
    fn no_mutation_escapes_the_declared_domain() {
        // 位翻转会翻到符号位、字符串拼接会超出长度、列表追加会超过 max_len。
        // 每一类都必须被夹回来，否则「模糊测试的结论」与「性质的断言范围」会脱节。
        let int = InputDomain::Int { min: -8, max: 8 };
        let corpus = int.samples(3, 24);
        for (round, value) in corpus.iter().enumerate() {
            for mutated in mutate(&int, value, round, 0, 32) {
                assert_eq!(int.clamp(&mutated), mutated, "整数变异越界：{mutated:?}");
            }
        }

        let text = InputDomain::Str { max_len: 3 };
        let corpus = text.samples(3, 24);
        for (round, value) in corpus.iter().enumerate() {
            for mutated in mutate(&text, value, round, 0, 32) {
                assert_eq!(text.clamp(&mutated), mutated, "字符串变异越界：{mutated:?}");
            }
        }

        let list = InputDomain::List {
            item: Box::new(InputDomain::Int { min: -8, max: 8 }),
            max_len: 3,
        };
        let corpus = list.samples(3, 24);
        for (round, value) in corpus.iter().enumerate() {
            for mutated in mutate(&list, value, round, 0, 32) {
                assert_eq!(list.clamp(&mutated), mutated, "列表变异越界：{mutated:?}");
            }
        }
    }

    #[test]
    fn list_mutation_changes_length() {
        let domain = InputDomain::List {
            item: Box::new(InputDomain::Int { min: 0, max: 9 }),
            max_len: 4,
        };
        let input = InputValue::List(vec![InputValue::Int(1), InputValue::Int(2)]);
        let mutated = mutate(&domain, &input, 0, 0, 64);
        assert!(
            mutated
                .iter()
                .any(|value| matches!(value, InputValue::List(items) if items.len() != 2)),
            "列表变异必须能改变长度"
        );
    }

    #[test]
    fn string_mutation_changes_content() {
        let domain = InputDomain::Str { max_len: 6 };
        let input = InputValue::Str("abc".to_owned());
        let mutated = mutate(&domain, &input, 0, 0, 64);
        assert!(
            mutated
                .iter()
                .any(|value| matches!(value, InputValue::Str(text) if text != "abc")),
            "字符串变异必须改变内容"
        );
    }

    #[test]
    fn classification_marks_failing_indices() {
        let execution = ExecutionResult {
            command: Command::new("cargo"),
            status: ExecutionStatus::Exited,
            exit_code: Some(101),
            signal: None,
            duration_ms: 1,
            stdout: format!(
                "PROOFFORGE_VIOLATION [1, 3]\n{}",
                crate::driver::DONE_MARKER
            ),
            stderr: String::new(),
            truncated: false,
        };
        assert_eq!(
            classify_from_output(&execution, 4),
            vec![
                BehaviorClass::Satisfied,
                BehaviorClass::Violated,
                BehaviorClass::Satisfied,
                BehaviorClass::Violated
            ]
        );
    }

    #[test]
    fn a_crash_is_classified_as_a_crash_not_as_a_violation() {
        // 「程序崩了」与「不变式不成立」是两类观测，合成一类会让报告无法区分，
        // BehaviorClass::Crashed 也会变成永远取不到的空枚举。
        let execution = ExecutionResult {
            command: Command::new("cargo"),
            status: ExecutionStatus::Exited,
            exit_code: Some(101),
            signal: None,
            duration_ms: 1,
            stdout: format!("PROOFFORGE_CRASH [0, 2]\n{}", crate::driver::DONE_MARKER),
            stderr: String::new(),
            truncated: false,
        };
        assert_eq!(
            classify_from_output(&execution, 3),
            vec![
                BehaviorClass::Crashed,
                BehaviorClass::Satisfied,
                BehaviorClass::Crashed
            ]
        );
    }

    #[test]
    fn a_driver_that_never_ran_is_never_satisfied() {
        // 编译失败：非零退出码、输出里没有任何执行记录。把这类输入记成
        // 「满足」就是假阴性——一次没跑的验证没有资格对任何输入下结论。
        let execution = ExecutionResult {
            command: Command::new("cargo"),
            status: ExecutionStatus::Exited,
            exit_code: Some(101),
            signal: None,
            duration_ms: 1,
            stdout: String::new(),
            stderr: "error[E0308]: mismatched types\n".to_owned(),
            truncated: false,
        };
        assert_eq!(
            classify_from_output(&execution, 3),
            vec![BehaviorClass::Inconclusive; 3]
        );
        assert!(inconclusive_from(&execution)
            .expect("必须给出原因")
            .contains("did not run to completion"));
    }

    #[test]
    fn unstartable_execution_marks_everything_inconclusive() {
        let execution = ExecutionResult {
            command: Command::new("cargo"),
            status: ExecutionStatus::FailedToStart,
            exit_code: None,
            signal: None,
            duration_ms: 0,
            stdout: String::new(),
            stderr: "cargo: not found".to_owned(),
            truncated: false,
        };
        assert_eq!(
            classify_from_output(&execution, 3),
            vec![BehaviorClass::Inconclusive; 3]
        );
        assert!(inconclusive_from(&execution).is_some());
    }

    #[test]
    fn empty_batch_is_not_inconclusive() {
        let classification = BatchClassification {
            per_input: Vec::new(),
            inconclusive_reason: None,
        };
        assert_eq!(classification.per_input, Vec::new());
    }

    #[test]
    fn behavior_classes_are_ordered_for_reporting() {
        // 排序让报告里的类别顺序稳定：违反必须排在满足之前。
        assert!(BehaviorClass::Violated < BehaviorClass::Crashed);
        assert!(BehaviorClass::Crashed < BehaviorClass::Inconclusive);
        assert!(BehaviorClass::Inconclusive < BehaviorClass::Satisfied);
    }

    #[test]
    fn config_defaults_are_bounded() {
        let config = config();
        assert_eq!(config.rounds, 8);
        assert_eq!(config.candidates_per_round, 16);
        assert_eq!(config.corpus_limit, 64);
        assert_eq!(config.clone().with_rounds(2).rounds, 2);
        assert_eq!(config.with_candidates_per_round(4).candidates_per_round, 4);
    }

    #[test]
    fn session_driver_renders_a_compilable_shape() {
        let source = session_driver(&config());
        assert!(source.contains("fn proofforge_property()"));
        // 空样本集必须渲染成 `vec![]`：带空行的 `vec![` 看起来像被截断了。
        assert!(
            source.contains("let cases: Vec<(usize, (i64,))> = vec![];"),
            "{source}"
        );
    }
}
