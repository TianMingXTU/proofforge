//! 一个「一部分行为有测试守护、一部分没有」的示例库。
//!
//! 它存在的理由是：变异测试（FR-037）要能同时观察到 `Killed` 与 `Survived`
//! 两种结论。所有函数都有测试时，任何变异体都会被杀死，通过率恒为 100%——
//! 那样跑一遍证明不了工具能区分两者。
//!
//! 因此 [`unchecked_discount`] 与 [`odd_offset`] 刻意没有测试：对它们做的每一处
//! 行为变化都应当幸存，而报告要把这些幸存条目如数列出来。

/// 两数相加。
pub fn add(left: i64, right: i64) -> i64 {
    left + right
}

/// 判断偶数。
pub fn is_even(value: i64) -> bool {
    value % 2 == 0
}

/// 把值夹进 `[low, high]`。
pub fn clamp(value: i64, low: i64, high: i64) -> i64 {
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

/// 打折后的价格。
///
/// **故意没有测试**：对 `rate` 做的算术变异应当幸存，报告要能把它指出来。
pub fn unchecked_discount(price: i64, rate: i64) -> i64 {
    price * rate / 100
}

/// 奇数时给一个偏移量。
///
/// **故意没有测试**：对 `==` 做的关系变异应当幸存。
pub fn odd_offset(value: i64) -> i64 {
    if value % 2 == 1 {
        value + 1
    } else {
        value
    }
}
