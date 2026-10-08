//! 测试专用临时目录（仅在 `cfg(test)` 下编译）。
//!
//! 为什么不用 `std::env::temp_dir()`：
//!
//! 1. **沙箱**：某些托管环境（包括本仓库的开发机）禁止子进程往 `%TEMP%` 写东西 ——
//!    十几个只做文件读写的测试会以 `Os { code: 5, PermissionDenied }` 失败，看起来像
//!    代码 bug，其实是环境；
//! 2. **竞态**：同一个模块里多个测试如果共用 `temp_dir()/<模块>-<pid>`，彼此末尾的
//!    `remove_dir_all` 会把对方的目录删掉（并行跑时随机挂，串行跑就全绿）。
//!
//! 这里改成：目录建在 crate 的 `target/test-tmp/` 下（一定可写、一定在仓库内），
//! 并且**每个调用点拿到的目录都不同**（计数器 + pid），互不干扰。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// 返回一个全新的、已创建的空目录。
pub fn scratch_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("test-tmp");
    let dir = root.join(format!("{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        // 建不出来也要让调用方看到原因，而不是一个莫名其妙的「文件不存在」。
        panic!("scratch_dir({tag}) 无法创建 {}: {e}", dir.display());
    }
    dir
}

/// 返回一个**保证不存在**的路径（用于「文件缺失」这类用例）。
pub fn missing_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp")
        .join(format!("{tag}-missing-{}-{n}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_dirs_are_unique_and_created() {
        let a = scratch_dir("t");
        let b = scratch_dir("t");
        assert_ne!(a, b, "each call must hand out its own directory");
        assert!(a.is_dir() && b.is_dir());
        assert!(
            a.starts_with(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target")),
            "must live inside the crate, not in %TEMP%: {}",
            a.display()
        );
    }

    #[test]
    fn missing_dir_does_not_exist() {
        let p = missing_dir("t");
        assert!(!p.exists());
    }
}
