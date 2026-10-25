//! 样板代码（FR-020）的植入样本。
//!
//! 该文件是 ProofForge 集成测试的夹具。判定对象是代码质量而不是作者身份：
//! 空占位实现与逐字复制的分支都不承载信息，这一点与代码由谁编写无关。

/// 只有一种实现的 trait，且实现是空占位。
pub trait Cache {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct NoCache;

impl Cache for NoCache {
    fn get(&self, _key: &str) -> Option<String> {
        unimplemented!()
    }
}

/// 三个分支的返回值逐字相同。
pub enum Level {
    Debug,
    Info,
    Warn,
}

pub fn label(level: Level) -> &'static str {
    match level {
        Level::Debug => "generic",
        Level::Info => "generic",
        Level::Warn => "generic",
    }
}
