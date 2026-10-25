//! 过度模块拆分（FR-019）：只有一条定义，却横跨三个文件。
//!
//! 读者必须同时打开三个底层模块才能理解这一层转调，而它自己几乎没有内容。

use crate::delta;
use crate::epsilon;
use crate::gamma;

/// 依次转调三个底层模块。
pub fn thin() {
    gamma::gamma();
    delta::delta();
    epsilon::epsilon();
}
