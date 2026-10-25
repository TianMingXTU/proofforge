//! 输入缩减（反例缩减，FR-042）。
//!
//! 需求文档 7.5：反例必须被缩减到「尽可能保留触发缺陷的必要条件」。一个
//! 400 元素的列表里第 317 个元素触发崩溃，与一个只有该元素的列表触发同一个
//! 崩溃，对人的可读性相差一个数量级。
//!
//! # 算法
//!
//! 采用**候选枚举 + 首次接受**而不是经典 ddmin 的均匀二分：输入是结构化值
//! 而不是字节序列，按类型语义生成候选（整数朝零收缩、列表成块删除、元组按位
//! 删除）比盲目切字节得到更小的结果，且实现更容易论证正确性。
//!
//! 收敛条件：没有任何候选能在保持失败的前提下减小 [`InputValue::size`]。
//! 该条件是**确定性**的——相同的输入与相同的判定函数必然得到相同的缩减结果，
//! 因此缩减过程本身可以复现（FR-043）。

use super::domain::InputValue;

/// 输入缩减器。
///
/// 无状态：全部输入都是参数，因此同一个实例可以并发服务于多个反例。
#[derive(Debug, Default)]
pub struct Reducer;

impl Reducer {
    /// 反复缩减 `input`，直到找不到更小但仍触发失败的候选。
    ///
    /// `still_fails` 是判定函数：返回 `true` 表示这个输入仍然触发缺陷。它通常
    /// 需要真实执行目标程序，因此由调用方注入——本模块不关心执行是怎么发生的。
    ///
    /// 缩减失败（判定函数对候选返回 `false`）时原样返回输入：宁可不缩减，
    /// 也不能给出一个不再触发缺陷的「反例」。
    #[must_use]
    pub fn reduce(
        input: &InputValue,
        still_fails: &impl Fn(&InputValue) -> bool,
    ) -> ReductionOutcome {
        if !still_fails(input) {
            return ReductionOutcome {
                value: input.clone(),
                original_size: input.size(),
                final_size: input.size(),
                rounds: 0,
                converged: true,
            };
        }
        let mut current = input.clone();
        let mut rounds = 0usize;
        loop {
            let candidate = Self::candidates(&current)
                .into_iter()
                .find(|candidate| candidate.size() < current.size() && still_fails(candidate));
            match candidate {
                Some(smaller) => {
                    current = smaller;
                    rounds += 1;
                }
                None => break,
            }
        }
        ReductionOutcome {
            value: current.clone(),
            original_size: input.size(),
            // 必须是缩减后的规模：用回原始规模会让 `shrinkage()` 恒为 0，
            // 报告于是声称「这个反例没有被压缩」，而它其实被压得很小。
            final_size: current.size(),
            rounds,
            converged: true,
        }
    }

    /// 生成比当前值「小一档」的全部候选，按收益从大到小排序。
    ///
    /// 排序很重要：先试收益最大的候选，能用更少的执行次数达到最小反例。执行
    /// 次数直接对应验证成本（需求文档 15.3「自动验证成本过高」）。
    pub(crate) fn candidates(value: &InputValue) -> Vec<InputValue> {
        let mut out = Vec::new();
        match value {
            InputValue::Int(current) => {
                // 先试零，再试对半收缩：0 与 ±1 覆盖了绝大多数边界缺陷。
                if *current != 0 {
                    out.push(InputValue::Int(0));
                }
                if *current > 1 {
                    out.push(InputValue::Int(current / 2));
                }
                if *current < -1 {
                    out.push(InputValue::Int(current / 2));
                }
                if current.abs() > 1 {
                    out.push(InputValue::Int(current.signum()));
                }
            }
            InputValue::Str(text) => {
                let chars: Vec<char> = text.chars().collect();
                if chars.len() > 1 {
                    // 先删前半，再删后半，再逐段删：与 ddmin 的块删除同构。
                    out.push(InputValue::Str(chars[..chars.len() / 2].iter().collect()));
                    out.push(InputValue::Str(chars[chars.len() / 2..].iter().collect()));
                    for chunk in chunk_ranges(chars.len()) {
                        let kept: String = chars
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| !chunk.contains(index))
                            .map(|(_, c)| *c)
                            .collect();
                        out.push(InputValue::Str(kept));
                    }
                }
                if !chars.is_empty() {
                    out.push(InputValue::Str(String::new()));
                }
            }
            InputValue::Tuple(fields) => {
                // 元组**不**做按位删除。调用签名 `f(a, b)` 要求两个实参，删掉一个
                // 得到的候选连编译都过不去——判定函数只能看到「编译失败」，于是
                // 它既不缩小反例，还让缩减看起来一直在工作。
                //
                // 列表能删元素是因为签名是 `f(&[T])`，长度是值的一部分；
                // 元组的长度是**类型**的一部分，删不得。这里只缩减各个位置的值。
                for (index, field) in fields.iter().enumerate() {
                    for candidate in Self::candidates(field) {
                        let mut reduced = fields.clone();
                        reduced[index] = candidate;
                        out.push(InputValue::Tuple(reduced));
                    }
                }
            }
            InputValue::List(items) => {
                if items.len() > 1 {
                    out.push(InputValue::List(items[..items.len() / 2].to_vec()));
                    out.push(InputValue::List(items[items.len() / 2..].to_vec()));
                    for chunk in chunk_ranges(items.len()) {
                        let kept: Vec<InputValue> = items
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| !chunk.contains(index))
                            .map(|(_, item)| item.clone())
                            .collect();
                        out.push(InputValue::List(kept));
                    }
                }
                if !items.is_empty() {
                    out.push(InputValue::List(Vec::new()));
                }
                for (index, item) in items.iter().enumerate() {
                    for candidate in Self::candidates(item) {
                        let mut reduced = items.clone();
                        reduced[index] = candidate;
                        out.push(InputValue::List(reduced));
                    }
                }
            }
            InputValue::Bool(_) => {}
        }
        // 去重并按规模降序：相同的候选只执行一次，收益大的先试。
        //
        // 排序键必须是**全序**的：`InputValue` 没有实现 `Ord`（变体之间没有天然
        // 大小关系），只按规模排序再 `dedup` 只能消掉相邻的重复，规模相同的重复
        // 会活下来，于是同一批候选被执行两遍。规模加文本描述就是全序。
        out.sort_by_key(|value| (std::cmp::Reverse(value.size()), value.describe()));
        out.dedup();
        out
    }
}

/// 一次缩减的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReductionOutcome {
    /// 缩减后的输入。
    pub value: InputValue,
    /// 缩减前的规模。
    pub original_size: usize,
    /// 缩减后的规模。
    pub final_size: usize,
    /// 成功应用的缩减步数。
    pub rounds: usize,
    /// 是否已收敛（找不到更小但仍失败的候选）。
    pub converged: bool,
}

impl ReductionOutcome {
    /// 规模缩减的比例（0.0 表示没有缩减，1.0 表示缩减到空）。
    ///
    /// 用于验证报告：它告诉读者这个反例被压缩了多少，从而判断它的可信度。
    ///
    /// 规模是输入元素的个数， realistically 远小于 2^53，因此 `usize -> f64`
    /// 的精度损失不影响这个比例的任何一位有效数字。用一个具名函数收住转换，
    /// 比在两处 `as f64` 上挂 `#[allow]` 更能说明「我们想过这件事」。
    #[must_use]
    pub fn shrinkage(&self) -> f64 {
        if self.original_size == 0 {
            return 0.0;
        }
        let ratio = as_f64(self.final_size) / as_f64(self.original_size);
        1.0 - ratio
    }
}

/// 把规模转成 `f64`。
///
/// 只用于 [`ReductionOutcome::shrinkage`] 这种「给人看的比例」：输入规模是元素
/// 个数，超过 2^53 个元素的输入在生成阶段就不可能存在（样本数受配置约束）。
fn as_f64(size: usize) -> f64 {
    // 精度截断在这里是可接受的，理由见调用方的文档注释。
    #[allow(clippy::cast_precision_loss)]
    let value = size as f64;
    value
}

/// 生成一组递增的块区间，用于成块删除。
///
/// 块大小为 1、2、4……直到覆盖一半长度。这是 ddmin 的标准块序列：先小步删除
/// （保住更多上下文），再大步删除（快速缩小规模）。
fn chunk_ranges(len: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut chunk = 1usize;
    while chunk * 2 <= len {
        let mut start = 0usize;
        while start < len {
            out.push(start..(start + chunk).min(len));
            start += chunk;
        }
        chunk *= 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::InputDomain;

    /// 判定函数：只有包含 7 的列表才「失败」。
    fn fails_on_seven(value: &InputValue) -> bool {
        match value {
            InputValue::List(items) => items.iter().any(|item| matches!(item, InputValue::Int(7))),
            InputValue::Int(7) => true,
            _ => false,
        }
    }

    #[test]
    fn list_is_reduced_to_the_single_triggering_element() {
        let input = InputValue::List(vec![
            InputValue::Int(1),
            InputValue::Int(2),
            InputValue::Int(7),
            InputValue::Int(3),
        ]);
        let outcome = Reducer::reduce(&input, &fails_on_seven);
        assert_eq!(outcome.value, InputValue::List(vec![InputValue::Int(7)]));
        // 规模按结构算：`[1, 2, 7, 3]` 是 13，`[7]` 是 7。
        assert_eq!(outcome.original_size, 13);
        assert_eq!(outcome.final_size, 7);
        assert!(outcome.converged);
        assert!(outcome.shrinkage() > 0.4, "{outcome:?}");
    }

    #[test]
    fn integer_is_reduced_toward_zero() {
        // 只有 100 的倍数才失败，因此缩减必须停在能保留触发条件的最小倍数。
        let input = InputValue::Int(800);
        let outcome = Reducer::reduce(
            &input,
            &|value| matches!(value, InputValue::Int(v) if v % 100 == 0 && *v != 0),
        );
        assert_eq!(outcome.value, InputValue::Int(100));
    }

    #[test]
    fn tuple_arity_is_never_changed_by_reduction() {
        // 回归：元组按位删除会产出 `(x,)` 这样的候选，而调用签名 `f(a, b)` 要求
        // 两个实参——候选连编译都过不去。缩减于是每一批都拿 `ran=false`，
        // 反例保持原样还被记 `converged=false`：看起来在工作，其实一步没走。
        let input = InputValue::Tuple(vec![
            InputValue::Int(100),
            InputValue::List(vec![InputValue::Int(1), InputValue::Int(7)]),
        ]);
        for candidate in Reducer::candidates(&input) {
            let InputValue::Tuple(fields) = &candidate else {
                panic!("candidate is not a tuple: {candidate:?}");
            };
            assert_eq!(
                fields.len(),
                2,
                "元组长度是类型的一部分，删不得：{candidate:?}"
            );
        }
    }

    #[test]
    fn a_tuple_field_is_reduced_without_touching_the_others() {
        let input = InputValue::Tuple(vec![
            InputValue::Int(800),
            InputValue::List(vec![
                InputValue::Int(1),
                InputValue::Int(2),
                InputValue::Int(7),
            ]),
        ]);
        // 只有第二个字段里的 7 触发失败，因此缩减必须压它而不动第一个字段。
        let fails = |value: &InputValue| {
            matches!(value, InputValue::Tuple(fields)
                if fields.len() == 2
                    && matches!(&fields[1], InputValue::List(items)
                        if items.iter().any(|i| matches!(i, InputValue::Int(n) if *n == 7))))
        };
        let outcome = Reducer::reduce(&input, &fails);
        let InputValue::Tuple(fields) = &outcome.value else {
            panic!("expected a tuple, got {:?}", outcome.value);
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[1], InputValue::List(vec![InputValue::Int(7)]));
        assert!(outcome.converged);
        assert!(outcome.shrinkage() > 0.5, "{outcome:?}");
    }

    #[test]
    fn string_is_reduced_to_the_necessary_characters() {
        // 只有包含 'x' 的字符串才失败。
        let input = InputValue::Str("aaaaxbbbb".to_owned());
        let outcome = Reducer::reduce(
            &input,
            &|value| matches!(value, InputValue::Str(text) if text.contains('x')),
        );
        assert_eq!(outcome.value, InputValue::Str("x".to_owned()));
    }

    #[test]
    fn reduction_is_a_fixpoint() {
        // 缩减结果必须让判定函数继续返回 true：否则给出的「反例」不再触发缺陷。
        let input = InputValue::List(vec![
            InputValue::Int(7),
            InputValue::Int(8),
            InputValue::Int(9),
        ]);
        let outcome = Reducer::reduce(&input, &fails_on_seven);
        assert!(fails_on_seven(&outcome.value));
        assert!(
            Reducer::candidates(&outcome.value)
                .into_iter()
                .all(|candidate| !fails_on_seven(&candidate)
                    || candidate.size() >= outcome.value.size()),
            "收敛后不应还有更小且仍失败的候选"
        );
    }

    #[test]
    fn reduction_is_deterministic() {
        let input = InputValue::List(vec![
            InputValue::Int(1),
            InputValue::Int(7),
            InputValue::Int(2),
            InputValue::Int(7),
        ]);
        let first = Reducer::reduce(&input, &fails_on_seven);
        let second = Reducer::reduce(&input, &fails_on_seven);
        assert_eq!(first, second);
    }

    #[test]
    fn non_triggering_input_is_returned_unchanged() {
        // 判定函数一票否决时不能硬缩减：那会产出一个不再触发缺陷的假反例。
        let input = InputValue::Int(42);
        let outcome = Reducer::reduce(&input, &|_| false);
        assert_eq!(outcome.value, input);
        assert_eq!(outcome.rounds, 0);
        assert_eq!(outcome.shrinkage(), 0.0);
    }

    #[test]
    fn bool_has_no_smaller_candidate() {
        assert_eq!(Reducer::candidates(&InputValue::Bool(true)), Vec::new());
    }

    #[test]
    fn chunk_ranges_cover_every_block_size() {
        let ranges = chunk_ranges(8);
        assert!(ranges.contains(&(0..1)));
        assert!(ranges.contains(&(0..2)));
        assert!(ranges.contains(&(4..8)));
        assert!(ranges.iter().all(|range| range.end <= 8));
    }

    #[test]
    fn reduction_works_on_values_from_a_domain() {
        // 端到端：从域生成样本，缩减，结果仍然是域内的合法值。
        let domain = InputDomain::List {
            item: Box::new(InputDomain::Int { min: 0, max: 9 }),
            max_len: 6,
        };
        let samples = domain.samples(11, 1);
        let outcome = Reducer::reduce(&samples[0], &|_| true);
        assert!(matches!(outcome.value, InputValue::List(_)));
    }
}
