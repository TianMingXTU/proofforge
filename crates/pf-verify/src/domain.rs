//! 输入域与输入值：属性测试与模糊测试的生成、序列化与缩减基础。
//!
//! 覆盖需求文档 5.6 的 FR-034（测试输入生成）与 FR-042（反例缩减）。
//!
//! # 为什么生成发生在 `ProofForge` 而不是目标程序里
//!
//! 生成的输入以**字面量**形式嵌入驱动程序，而不是在驱动程序里调用随机数
//! 发生器。这样换来三个性质：
//!
//! 1. **可复现**：同一个种子永远得到同一批输入，反例因此可以跨机器重放
//!    （FR-043 结果复现）。
//! 2. **可审计**：驱动程序是普通源码，人可以读懂「到底测了哪些值」。
//! 3. **可缩减**：输入是结构化值而不是字节串，缩减可以按类型语义进行——
//!    整数朝零收缩、列表成块删除，而不是盲目切字节。
//!
//! 随机数发生器不引入 `rand`：这里需要的是「同一种子 → 同一序列」的确定性，
//! 而不是密码学强度。splitmix64 用十五行给出这个保证，且不给一个以「不可信
//! 代码执行风险」为核心关注点的工具增加传递依赖。

use serde::{Deserialize, Serialize};

/// 输入域：描述一类输入的形状与取值范围。
///
/// 域是**声明式**的：它只说明「要生成什么」，不包含生成逻辑。生成由
/// [`InputDomain::samples`] 完成，缩减由 [`crate::reduce::Reducer`] 完成，
/// 三者因此可以独立测试。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum InputDomain {
    /// 闭区间 `[min, max]` 内的整数。
    Int {
        /// 下界（含）。
        min: i64,
        /// 上界（含）。
        max: i64,
    },
    /// 布尔值。
    Bool,
    /// 长度不超过 `max_len` 的字符串。
    Str {
        /// 最大字符数。
        max_len: usize,
    },
    /// 定长元组：每个位置各有自己的域。
    ///
    /// 用结构体变体而不是元组变体：`InputValue` 那一侧要保持紧凑，但域是要被
    /// 人写进规格文件的，`{"kind": "tuple", "fields": [...]}` 比一个裸数组
    ///  self-describing 得多。
    Tuple {
        /// 各位置的域。
        fields: Vec<InputDomain>,
    },
    /// 变长列表：元素共用一个域。
    List {
        /// 元素域。
        item: Box<InputDomain>,
        /// 最大元素个数。
        max_len: usize,
    },
}

/// 一个具体的输入值。
///
/// 与 [`InputDomain`] 一一对应：任何由 [`InputDomain::samples`] 产出的值都
/// 满足其域的约束，这是缩减算法能依赖的不变量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum InputValue {
    /// 整数值。
    Int(i64),
    /// 布尔值。
    Bool(bool),
    /// 字符串值。
    Str(String),
    /// 元组值。
    Tuple(Vec<InputValue>),
    /// 列表值。
    List(Vec<InputValue>),
}

impl InputDomain {
    /// 生成 `count` 个确定性样本。
    ///
    /// 第一个样本固定为域的「边界代表值」：最小值、空串、全 false。边界值是
    /// 缺陷最常出现的地方（FR-034 明确要求边界输入），把它们固定放在最前面
    /// 意味着即使样本数被预算压到 1，也仍然测到了边界。
    #[must_use]
    pub fn samples(&self, seed: u64, count: usize) -> Vec<InputValue> {
        let mut out = Vec::with_capacity(count);
        if count == 0 {
            return out;
        }
        out.push(self.boundary_value());
        let mut rng = SplitMix64::new(seed);
        while out.len() < count {
            out.push(self.generate(&mut rng));
        }
        out
    }

    /// 域的「边界代表值」。
    ///
    /// 递归定义：元组取各位置的边界值，列表取空列表。空列表与空串一样是边界
    /// —— 大量真实缺陷就藏在「没有元素」这条路径上。
    #[must_use]
    pub fn boundary_value(&self) -> InputValue {
        match self {
            Self::Int { min, .. } => InputValue::Int(*min),
            Self::Bool => InputValue::Bool(false),
            Self::Str { .. } => InputValue::Str(String::new()),
            Self::Tuple { fields } => {
                InputValue::Tuple(fields.iter().map(Self::boundary_value).collect())
            }
            Self::List { .. } => InputValue::List(Vec::new()),
        }
    }

    /// 这个域是否能真正生成样本。
    ///
    /// 声明式域的代价是**可以写出矛盾的声明**：`min > max`、长度为负这类域
    /// 让 `samples` 退化成只返回边界值，看起来像「测过了」其实只测了一个数。
    /// 加载规格文件时先问这一句，比让使用者在报告里发现「怎么只有 1 个样本」
    /// 要诚实得多。
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        match self {
            Self::Int { min, max } => min <= max,
            Self::Bool => true,
            Self::Str { max_len } => *max_len > 0,
            Self::Tuple { fields } => fields.iter().all(Self::is_well_formed),
            Self::List { item, max_len } => *max_len > 0 && item.is_well_formed(),
        }
    }

    /// 用给定的随机源生成一个值。
    fn generate(&self, rng: &mut SplitMix64) -> InputValue {
        match self {
            Self::Int { min, max } => InputValue::Int(rng.between(*min, *max)),
            Self::Bool => InputValue::Bool(rng.next_bit()),
            Self::Str { max_len } => {
                let len = rng.below(*max_len + 1);
                InputValue::Str((0..len).map(|_| rng.ascii_char()).collect())
            }
            Self::Tuple { fields } => {
                InputValue::Tuple(fields.iter().map(|field| field.generate(rng)).collect())
            }
            Self::List { item, max_len } => {
                let len = rng.below(*max_len + 1);
                InputValue::List((0..len).map(|_| item.generate(rng)).collect())
            }
        }
    }

    /// 把一个值夹回这个域。
    ///
    /// 变异算子天生会跑出域外——位翻转翻到符号位、边界跳转跳到 `i64::MAX`、字符串
    /// 拼接超出长度上限。夹回是**语义要求**而不是防御式编程：一条性质的断言范围
    /// 就是它声明的域，把域外输入的表现记在性质头上，等于让报告声称一个它并没有
    /// 做出的断言。`double` 在 `i64::MAX` 上溢出崩溃，与「`double` 在 `[-100, 100]`
    /// 上恒为偶数」这个命题无关。
    ///
    /// 值的形状与域不匹配时原样返回：语料本来就产自这个域，形状不一致说明调用方
    /// 传错了参数，此时夹一个错值比不夹更糟。
    #[must_use]
    pub fn clamp(&self, value: &InputValue) -> InputValue {
        match (self, value) {
            (Self::Int { min, max }, InputValue::Int(inner)) => {
                InputValue::Int((*inner).clamp(*min, *max))
            }
            (Self::Bool, InputValue::Bool(inner)) => InputValue::Bool(*inner),
            (Self::Str { max_len }, InputValue::Str(inner)) => {
                InputValue::Str(inner.chars().take(*max_len).collect())
            }
            (Self::Tuple { fields }, InputValue::Tuple(inner)) if fields.len() == inner.len() => {
                InputValue::Tuple(
                    fields
                        .iter()
                        .zip(inner.iter())
                        .map(|(field, value)| field.clamp(value))
                        .collect(),
                )
            }
            (Self::List { item, max_len }, InputValue::List(inner)) => InputValue::List(
                inner
                    .iter()
                    .take(*max_len)
                    .map(|value| item.clamp(value))
                    .collect(),
            ),
            _ => value.clone(),
        }
    }
}

impl InputValue {
    /// 值的「大小」：用于衡量缩减进展。
    ///
    /// 定义成结构规模而不是字节长度：整数朝零收缩、列表删除元素都会让这个值
    /// 下降，因此它可以作为缩减是否收敛的判据。
    ///
    /// # 不变量：每个在场的元素至少占 1
    ///
    /// `Int(0)` 的大小是 1，不是 0。这不是为了让数字好看，而是缩减算法的前提：
    /// [`crate::reduce::Reducer`] 用「规模严格下降」筛选候选，如果零值元素不占
    /// 规模，删掉它就不会让规模变化，于是 `[0, 0, 7, 0]` 与 `[7]` 同等大小——
    /// 缩减于是停在满是零的四元列表上，而它本来能压到单个元素。
    #[must_use]
    pub fn size(&self) -> usize {
        match self {
            Self::Int(value) => {
                usize::try_from(value.unsigned_abs())
                    .unwrap_or(usize::MAX)
                    // 0 也要占一份：见方法文档里的不变量说明。
                    .max(1)
            }
            Self::Bool(_) => 1,
            Self::Str(text) => text.chars().count(),
            Self::Tuple(fields) => fields.iter().map(Self::size).sum(),
            Self::List(items) => items.iter().map(Self::size).sum(),
        }
    }

    /// 渲染成 Rust 字面量。
    ///
    /// 字符串走 `Debug` 格式而不是手写引号：`Debug` 会正确处理转义，
    /// 手写拼接会让包含引号或反斜杠的输入生成无法编译的驱动程序。
    #[must_use]
    pub fn to_rust_literal(&self) -> String {
        match self {
            Self::Int(value) => format!("{value}_i64"),
            Self::Bool(value) => format!("{value}"),
            Self::Str(text) => format!("{text:?}.to_owned()"),
            Self::Tuple(fields) => {
                let rendered: Vec<String> = fields.iter().map(Self::to_rust_literal).collect();
                format!("({},)", rendered.join(", "))
            }
            Self::List(items) => {
                let rendered: Vec<String> = items.iter().map(Self::to_rust_literal).collect();
                format!("vec![{}]", rendered.join(", "))
            }
        }
    }

    /// 渲染成 Python 字面量。
    #[must_use]
    pub fn to_python_literal(&self) -> String {
        match self {
            Self::Int(value) => format!("{value}"),
            Self::Bool(value) => {
                // Python 的字面量是 `True`/`False`，与 Rust 的 `true`/`false` 不同。
                if *value { "True" } else { "False" }.to_owned()
            }
            Self::Str(text) => format!("{text:?}"),
            Self::Tuple(fields) => {
                let rendered: Vec<String> = fields.iter().map(Self::to_python_literal).collect();
                format!("({},)", rendered.join(", "))
            }
            Self::List(items) => {
                let rendered: Vec<String> = items.iter().map(Self::to_python_literal).collect();
                format!("[{}]", rendered.join(", "))
            }
        }
    }

    /// 渲染成人类可读的短描述，用于反例标题。
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Int(value) => value.to_string(),
            Self::Bool(value) => value.to_string(),
            Self::Str(text) => format!("{text:?}"),
            Self::Tuple(fields) => {
                let rendered: Vec<String> = fields.iter().map(Self::describe).collect();
                format!("({})", rendered.join(", "))
            }
            Self::List(items) => {
                let rendered: Vec<String> = items.iter().map(Self::describe).collect();
                format!("[{}]", rendered.join(", "))
            }
        }
    }
}

/// splitmix64：一个小型、快速、确定性的伪随机数发生器。
///
/// 选择它而不是 `rand::StdRng` 的理由见模块文档。实现是公共领域的标准形式，
/// 这里只保留用到的两个操作：取指定位数的随机位、取区间内的整数。
#[derive(Debug, Clone)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// 用种子构造。种子 0 是合法的：splitmix64 的输出不依赖于状态非零。
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// 推进状态并返回下一个 64 位输出。
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// 返回一个随机位。
    fn next_bit(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    /// 返回 `[0, bound)` 内的整数。
    ///
    /// `bound` 为 0 时返回 0：调用方用它生成「长度不超过 N」的序列，长度为 0
    /// 是合法且常见的取值，不应 panic。
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        usize::try_from(self.next_u64() % u64::try_from(bound).unwrap_or(u64::MAX)).unwrap_or(0)
    }

    /// 返回 `[min, max]` 内的整数（闭区间）。
    ///
    /// 区间宽度用 `i128` 算，不是 `i64`。`max - min` 在 `i64` 里对宽区间会溢出：
    /// 溢出后的负值让 `u64::try_from` 失败从而回退到 `u64::MAX`，`width + 1` 再绕回
    /// 0，最后 `% 0` 直接 panic。而域的上下界来自规格文件——不可信输入上的 panic
    /// 违反 AGENTS.md 4.3，也让整个工具以 101 退出而不是给出报告。
    fn between(&mut self, min: i64, max: i64) -> i64 {
        if min >= max {
            return min;
        }
        let width = i128::from(max) - i128::from(min);
        let offset = i128::from(self.next_u64()) % (width + 1);
        // offset 最大可以到 2^64 - 1，超出 i64；此时 clamp 到上界。
        min.saturating_add(i64::try_from(offset).unwrap_or(i64::MAX))
    }

    /// 返回一个可打印 ASCII 字符。
    ///
    /// 限定在可打印范围内：控制字符会让驱动程序与反例日志变得不可读，
    /// 而属性测试要找的是逻辑缺陷，不是终端兼容性问题。
    fn ascii_char(&mut self) -> char {
        const PRINTABLE: u8 = 95;
        let offset = u8::try_from(self.below(usize::from(PRINTABLE))).unwrap_or(0);
        char::from(b' ' + offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_are_deterministic_for_the_same_seed() {
        let domain = InputDomain::Tuple {
            fields: vec![
                InputDomain::Int { min: -5, max: 5 },
                InputDomain::Str { max_len: 8 },
            ],
        };
        assert_eq!(domain.samples(7, 20), domain.samples(7, 20));
    }

    #[test]
    fn the_full_i64_range_does_not_overflow() {
        // 回归：`max - min` 在 i64 里溢出后，`u64::try_from` 失败回退到
        // `u64::MAX`，`width + 1` 绕回 0，`% 0` 直接 panic。域的上下界来自
        // 规格文件，因此这是不可信输入上的 panic。
        let domain = InputDomain::Int {
            min: i64::MIN,
            max: i64::MAX,
        };
        let samples = domain.samples(1, 64);
        assert_eq!(samples.len(), 64);
        for value in &samples {
            let InputValue::Int(number) = value else {
                panic!("expected an int, got {value:?}");
            };
            // 只需确认生成没有 panic、且样本数对得上：区间宽度本身已经超过 u64
            // 的表示范围，逐个验证边界没有额外信息。
            let _ = number;
        }
    }

    #[test]
    fn a_range_wider_than_i64_stays_in_bounds() {
        // 同样宽但非对称的区间：宽度超过 u64 的表示范围。
        let domain = InputDomain::Int {
            min: i64::MIN,
            max: 0,
        };
        for value in domain.samples(7, 128) {
            let InputValue::Int(number) = value else {
                panic!("expected an int, got {value:?}");
            };
            assert!(number <= 0, "{number} should not exceed the upper bound");
        }
    }

    #[test]
    fn a_narrow_range_only_produces_in_range_values() {
        let domain = InputDomain::Int { min: -3, max: 3 };
        for value in domain.samples(11, 200) {
            let InputValue::Int(number) = value else {
                panic!("expected an int, got {value:?}");
            };
            assert!((-3..=3).contains(&number), "{number} outside [-3, 3]");
        }
    }

    #[test]
    fn different_seeds_produce_different_sequences() {
        let domain = InputDomain::Int {
            min: 0,
            max: 1_000_000,
        };
        assert_ne!(domain.samples(1, 10), domain.samples(2, 10));
    }

    #[test]
    fn first_sample_is_the_boundary_value() {
        let domain = InputDomain::Tuple {
            fields: vec![
                InputDomain::Int { min: -3, max: 9 },
                InputDomain::Str { max_len: 4 },
            ],
        };
        let samples = domain.samples(0, 3);
        assert_eq!(
            samples[0],
            InputValue::Tuple(vec![InputValue::Int(-3), InputValue::Str(String::new())]),
            "第一个样本必须是边界值"
        );
    }

    #[test]
    fn zero_count_yields_no_samples() {
        let domain = InputDomain::Bool;
        assert_eq!(domain.samples(1, 0), Vec::new());
    }

    #[test]
    fn generated_values_respect_their_domain() {
        let domain = InputDomain::Tuple {
            fields: vec![
                InputDomain::Int { min: -2, max: 2 },
                InputDomain::Str { max_len: 3 },
                InputDomain::Bool,
                InputDomain::List {
                    item: Box::new(InputDomain::Bool),
                    max_len: 2,
                },
            ],
        };
        for value in domain.samples(42, 200) {
            let InputValue::Tuple(fields) = &value else {
                panic!("元组域必须产出元组");
            };
            assert!(matches!(fields[0], InputValue::Int(v) if (-2..=2).contains(&v)));
            match &fields[1] {
                InputValue::Str(text) => assert!(text.chars().count() <= 3),
                other => panic!("expected string, got {other:?}"),
            }
            assert!(matches!(fields[2], InputValue::Bool(_)));
            match &fields[3] {
                InputValue::List(items) => {
                    assert!(items.len() <= 2);
                    assert!(items.iter().all(|item| matches!(item, InputValue::Bool(_))));
                }
                other => panic!("expected list, got {other:?}"),
            }
        }
    }

    #[test]
    fn degenerate_int_domain_yields_its_lower_bound() {
        // min >= max 时不能死循环或 panic：区间没有可取的值，只能退回下界。
        let domain = InputDomain::Int { min: 5, max: 5 };
        assert_eq!(domain.boundary_value(), InputValue::Int(5));
        let domain = InputDomain::Int { min: 9, max: 1 };
        assert_eq!(domain.samples(3, 5).last(), Some(&InputValue::Int(9)));
    }

    #[test]
    fn size_counts_structure_not_byte_length() {
        assert_eq!(InputValue::Int(-7).size(), 7);
        assert_eq!(InputValue::Str("abc".to_owned()).size(), 3);
        assert_eq!(
            InputValue::Tuple(vec![InputValue::Int(2), InputValue::Bool(true)]).size(),
            3
        );
        assert_eq!(InputValue::List(Vec::new()).size(), 0);
    }

    #[test]
    fn every_present_element_costs_at_least_one() {
        // 缩减算法用「规模严格下降」筛候选。零值元素若占 0，删掉它就不改变规模，
        // 于是 [0, 0, 7, 0] 与 [7] 同等大小，缩减会停在前者上。
        assert_eq!(InputValue::Int(0).size(), 1);
        assert_eq!(
            InputValue::List(vec![InputValue::Int(0), InputValue::Int(0)]).size(),
            2
        );
        // 删除一个元素必须让规模下降——这是 Reducer 依赖的不变量。
        let before = InputValue::List(vec![InputValue::Int(0), InputValue::Int(7)]);
        let after = InputValue::List(vec![InputValue::Int(7)]);
        assert!(after.size() < before.size());
    }

    #[test]
    fn rust_literals_are_typed_and_escaped() {
        assert_eq!(InputValue::Int(-1).to_rust_literal(), "-1_i64");
        assert_eq!(InputValue::Bool(true).to_rust_literal(), "true");
        assert_eq!(
            InputValue::Str("a\"b".to_owned()).to_rust_literal(),
            "\"a\\\"b\".to_owned()",
            "引号必须被转义，否则生成的驱动程序无法编译"
        );
        assert_eq!(
            InputValue::Tuple(vec![InputValue::Int(1)]).to_rust_literal(),
            "(1_i64,)"
        );
        assert_eq!(
            InputValue::List(vec![InputValue::Bool(false)]).to_rust_literal(),
            "vec![false]"
        );
    }

    #[test]
    fn python_literals_use_python_spelling() {
        assert_eq!(InputValue::Bool(true).to_python_literal(), "True");
        assert_eq!(
            InputValue::Tuple(vec![InputValue::Int(1)]).to_python_literal(),
            "(1,)"
        );
        assert_eq!(
            InputValue::List(vec![InputValue::Int(1)]).to_python_literal(),
            "[1]"
        );
    }

    #[test]
    fn generated_strings_are_printable_ascii() {
        let domain = InputDomain::Str { max_len: 16 };
        for value in domain.samples(9, 100) {
            let InputValue::Str(text) = &value else {
                panic!("expected string");
            };
            assert!(
                text.chars().all(|c| c.is_ascii_graphic() || c == ' '),
                "控制字符会让反例日志不可读：{text:?}"
            );
        }
    }
}

#[cfg(test)]
mod spec_domain_tests {
    use super::*;

    #[test]
    fn well_formed_domains_are_accepted() {
        assert!(InputDomain::Int { min: -1, max: 1 }.is_well_formed());
        assert!(InputDomain::Bool.is_well_formed());
        assert!(InputDomain::Str { max_len: 3 }.is_well_formed());
        assert!(InputDomain::Tuple {
            fields: vec![InputDomain::Bool; 2]
        }
        .is_well_formed());
        assert!(InputDomain::List {
            item: Box::new(InputDomain::Bool),
            max_len: 2
        }
        .is_well_formed());
    }

    #[test]
    fn contradictory_domains_are_rejected() {
        assert!(!InputDomain::Int { min: 2, max: 1 }.is_well_formed());
        assert!(!InputDomain::Str { max_len: 0 }.is_well_formed());
        assert!(!InputDomain::List {
            item: Box::new(InputDomain::Str { max_len: 0 }),
            max_len: 4
        }
        .is_well_formed());
        assert!(!InputDomain::Tuple {
            fields: vec![InputDomain::Str { max_len: 0 }]
        }
        .is_well_formed());
    }

    #[test]
    fn an_inverted_domain_still_yields_a_boundary_sample() {
        // 这是「为什么需要 is_well_formed」的注脚：矛盾域不会崩，只会安静地
        // 退化成一个样本。不拦住它，报告上就写着「测了 32 个样本」而其实只有 1 个。
        let samples = InputDomain::Int { min: 9, max: 2 }.samples(1, 8);
        assert!(samples.iter().all(|value| *value == InputValue::Int(9)));
    }
}
