//! 过度配置（FR-018）与重复代码（FR-011）的植入样本。
//!
//! 该文件是 ProofForge 集成测试的夹具，每个问题都对应需求文档中的一条编号规则。

/// 签名被开关参数主导（FR-018）：五个参数里有四个是 `bool` 或 `Option<_>`。
pub fn export_report(
    path: &str,
    verbose: bool,
    dry_run: bool,
    retries: Option<u32>,
    strict: bool,
) -> bool {
    true
}

// 逐字重复的两个实现（FR-011）：函数体的标识符完全一致，只有函数名不同。
fn normalize_first(input: &str) -> String {
    let trimmed = input.trim();
    let lowered = trimmed.to_lowercase();
    let parts: Vec<&str> = lowered.split(',').collect();
    let mut out = Vec::new();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        out.push(part.to_owned());
    }
    out.join("-")
}

fn normalize_second(input: &str) -> String {
    let trimmed = input.trim();
    let lowered = trimmed.to_lowercase();
    let parts: Vec<&str> = lowered.split(',').collect();
    let mut out = Vec::new();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        out.push(part.to_owned());
    }
    out.join("-")
}
