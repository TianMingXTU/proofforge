//! 示例库：故意包含若干已知的工程质量问题。
//!
//! 该文件是 ProofForge 集成测试的夹具（`fixtures/over-engineered`），每个问题
//! 都对应需求文档中的一条编号规则。修改前请先确认 `crates/pf-integration`
//! 中的断言仍然成立。

pub mod alpha;
pub mod beta;
pub mod boilerplate;
pub mod config;
pub mod delta;
pub mod epsilon;
pub mod gamma;
pub mod thin;

/// 正常实现：有文档、被调用。
pub fn documented_and_used(x: i32) -> i32 {
    x + 1
}

/// 使用上面的函数。
pub fn caller(x: i32) -> i32 {
    documented_and_used(x)
}

// 缺少文档的公共 API（FR-014）。
pub fn undocumented_api(value: i32) -> i32 {
    value * 2
}

// 命名不符合惯例（FR-007）。
pub struct badName;

// 只有一种实现的 trait（FR-015）。
pub trait Repository {
    fn load(&self, id: u64) -> Option<String>;
}

pub struct MemoryRepository;

impl Repository for MemoryRepository {
    fn load(&self, _id: u64) -> Option<String> {
        None
    }
}

// 纯转发包装（FR-016）。
fn inner(value: i32) -> i32 {
    value - 1
}

pub fn wrapper(value: i32) -> i32 {
    inner(value)
}

// 没有选择逻辑的工厂函数（FR-017）：名字像工厂，函数体里却没有任何分支，
// 总是返回同一种实现。
pub struct Widget {
    size: u32,
}

pub fn create_widget(size: u32) -> Widget {
    Widget { size }
}

// 参数过多（FR-008）。
pub fn too_many_parameters(
    first: i32,
    second: i32,
    third: i32,
    fourth: i32,
    fifth: i32,
    sixth: i32,
) -> i32 {
    first + second + third + fourth + fifth + sixth
}

// 复述代码的注释（FR-013）：只是在念函数名，没有说明意图或约束。
// count
pub fn count(items: &[i32]) -> usize {
    items.len()
}
