//! 循环依赖的一端（FR-023）。
//!
//! 与 `beta.rs` 互相 `use`，构成模块级循环依赖。

use crate::beta;

/// 调用 `beta` 之后返回。
pub fn alpha() {
    beta::beta();
}
