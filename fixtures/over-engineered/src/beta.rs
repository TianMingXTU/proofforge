//! 循环依赖的另一端（FR-023）。

use crate::alpha;

/// 调用 `alpha` 之后返回。
pub fn beta() {
    alpha::alpha();
}
