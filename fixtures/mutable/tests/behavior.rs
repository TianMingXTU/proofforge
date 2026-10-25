//! 只为**有测试守护**的那几个函数写断言。
//!
//! 刻意不覆盖 `unchecked_discount` 与 `odd_offset`：变异测试要观察的正是
//! 「测试套件发现不了的行为变化」，全测了就看不到幸存条目。

use mutable_fixture::{add, clamp, is_even};

#[test]
fn add_is_commutative() {
    assert_eq!(add(2, 3), 5);
    assert_eq!(add(3, 2), 5);
    assert_eq!(add(-1, 1), 0);
}

#[test]
fn evenness_is_detected() {
    assert!(is_even(4));
    assert!(!is_even(5));
    assert!(is_even(0));
}

#[test]
fn clamp_keeps_values_inside_the_range() {
    assert_eq!(clamp(5, 0, 10), 5);
    assert_eq!(clamp(-3, 0, 10), 0);
    assert_eq!(clamp(42, 0, 10), 10);
}
