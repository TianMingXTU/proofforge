//! 一个没有已知问题的示例库。
//!
//! 用于验证「干净项目应当通过门禁」。每个公共 API 都有文档、被测试调用，
//! 命名符合惯例，没有循环依赖；函数体都做了实际工作而不是单纯转发。
//!
//! 这个文件同时是 ProofForge 集成测试的夹具（`fixtures/clean`），修改前请先
//! 确认 `crates/pf-integration` 与 `crates/pf-cli` 中的断言仍然成立。

/// 把输入加一。
pub fn increment(value: i32) -> i32 {
    value + 1
}

/// 把一个序列整体加一，返回新向量。
pub fn increment_all(values: &[i32]) -> Vec<i32> {
    values.iter().copied().map(increment).collect()
}

/// 计算一组数字的和。
pub fn total(values: &[i32]) -> i32 {
    values.iter().sum()
}

/// 求和之后再整体加一。
///
/// 与直接调用 `total` 不同：这里演示的是「组合多个公共 API 完成一件事」，
/// 而不是把参数原样转手，因此不算纯转发包装。
pub fn shifted_total(values: &[i32]) -> i32 {
    let sum = total(values);
    sum + values.len() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increment_adds_one() {
        let result = increment(1);
        assert_eq!(result, 2);
    }

    #[test]
    fn increment_all_maps_every_element() {
        let result = increment_all(&[1, 2, 3]);
        assert_eq!(result, vec![2, 3, 4]);
    }

    #[test]
    fn total_sums_the_slice() {
        let result = total(&[1, 2, 3]);
        assert_eq!(result, 6);
    }

    #[test]
    fn shifted_total_accounts_for_length() {
        let result = shifted_total(&[1, 2, 3]);
        assert_eq!(result, 9);
    }
}
