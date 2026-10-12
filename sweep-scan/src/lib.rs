//! Sweep 的扫描引擎：把一个卷或文件夹扫成一棵紧凑的 [`Tree`]。不依赖任何界面库。
//!
//! 两条路径：
//! - [`mft`]：NTFS 卷 + 管理员权限时直接读主文件表，几百万个文件也只要几秒；
//! - [`walk`]：其余情况（没有权限、FAT/exFAT/ReFS、网络盘、只扫一个文件夹）多线程遍历目录。
//!
//! [`Scan::start`] 在后台线程扫描，进度通过共享的 [`Progress`] 读取，可随时取消。

pub mod mft;
pub mod system;
pub mod tree;
pub mod walk;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use tree::{Entry, NONE, Node, Tree, flags};

/// 扫描进度。扫描线程写，UI 线程随时读。
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    /// 已扫到的文件大小合计。
    pub bytes: AtomicU64,
    /// 完成量 / 总量：MFT 路径是已读 / 总字节数；遍历路径总量未知时为 0。
    pub done: AtomicU64,
    pub total: AtomicU64,
    /// 已处理的目录数，用来节流"当前路径"的更新。
    pub ticks: AtomicU64,
    current: Mutex<String>,
    /// 遍历路径：根下第一级目录各自已扫到的大小（名字为空的一项是根目录里的文件）。
    pub top: Mutex<Vec<(String, u64)>>,
}

impl Progress {
    pub fn set_current(&self, s: String) {
        *self.current.lock().unwrap() = s;
    }

    pub fn current(&self) -> String {
        self.current.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Engine {
    /// 能读 MFT 就读，否则遍历目录。
    Auto,
    Mft,
    Walk,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Auto => "自动",
            Engine::Mft => "读取 MFT",
            Engine::Walk => "遍历目录",
        }
    }
}

pub struct Outcome {
    pub tree: Tree,
    /// 实际使用的方式（Auto 会解析成 Mft 或 Walk）。
    pub engine: Engine,
    pub elapsed: Duration,
    /// 例如"读取 MFT 失败，已改用目录遍历"。
    pub note: Option<String>,
}

/// 规范化扫描目标：`c:` → `C:\`，去掉多余的结尾反斜杠。
pub fn normalize(target: &str) -> String {
    let t = target.trim().trim_matches('"').replace('/', "\\");
    let bytes = t.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && t[2..].trim_matches('\\').is_empty() {
        return format!("{}:\\", t[..1].to_ascii_uppercase());
    }
    let t = t.trim_end_matches('\\');
    if t.len() >= 2 && t.as_bytes()[1] == b':' {
        format!("{}{}", t[..1].to_ascii_uppercase(), &t[1..])
    } else {
        t.to_string()
    }
}

/// 是否是整个卷的根目录（`C:\`），只有这种目标能走 MFT。
pub fn is_volume_root(target: &str) -> bool {
    let b = target.as_bytes();
    b.len() == 3 && b[0].is_ascii_alphabetic() && &b[1..] == b":\\"
}

/// 同步扫描（在调用线程上执行）。`by_alloc` 见 [`Tree::set_basis`]。
pub fn scan(
    target: &str,
    engine: Engine,
    by_alloc: bool,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<Outcome, String> {
    let target = normalize(target);
    let started = Instant::now();
    let mft_possible = is_volume_root(&target)
        && system::fs_name(&target).as_deref() == Some("NTFS")
        && system::is_elevated();
    let mut note = None;
    if engine == Engine::Mft && !mft_possible {
        return Err("读取 MFT 需要以管理员身份运行，且目标是 NTFS 卷的根目录".into());
    }
    if engine != Engine::Walk && mft_possible {
        match mft::scan(&target, by_alloc, progress, cancel) {
            Ok(tree) => {
                return Ok(Outcome {
                    tree,
                    engine: Engine::Mft,
                    elapsed: started.elapsed(),
                    note,
                });
            }
            Err(e) if engine == Engine::Mft || cancel.load(Ordering::Relaxed) => return Err(e),
            Err(e) => {
                note = Some(format!("读取 MFT 失败（{e}），已改用目录遍历"));
                reset(progress);
            }
        }
    }
    // 管理员身份时打开备份特权，遍历能进入 ACL 不允许列出的目录。
    if system::is_elevated() {
        system::enable_backup_privilege();
    }
    let tree = walk::scan(&target, by_alloc, progress, cancel)?;
    Ok(Outcome {
        tree,
        engine: Engine::Walk,
        elapsed: started.elapsed(),
        note,
    })
}

fn reset(p: &Progress) {
    for c in [&p.files, &p.dirs, &p.bytes, &p.done, &p.total, &p.ticks] {
        c.store(0, Ordering::Relaxed);
    }
    p.top.lock().unwrap().clear();
}

/// 后台扫描任务。
pub struct Scan {
    pub target: String,
    pub progress: Arc<Progress>,
    pub started: Instant,
    cancel: Arc<AtomicBool>,
    rx: Receiver<Result<Outcome, String>>,
}

impl Scan {
    /// 在新线程上开始扫描；完成（或失败、取消）时调用 `notify`，UI 用它请求重绘。
    pub fn start(
        target: &str,
        engine: Engine,
        by_alloc: bool,
        notify: impl FnOnce() + Send + 'static,
    ) -> Self {
        let target = normalize(target);
        let progress = Arc::new(Progress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (p, c, t) = (progress.clone(), cancel.clone(), target.clone());
        std::thread::Builder::new()
            .name("scan".into())
            .spawn(move || {
                let _ = tx.send(scan(&t, engine, by_alloc, &p, &c));
                notify();
            })
            .expect("spawn scan");
        Self {
            target,
            progress,
            started: Instant::now(),
            cancel,
            rx,
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// 完成时返回结果（只返回一次）。
    pub fn poll(&self) -> Option<Result<Outcome, String>> {
        self.rx.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_targets() {
        assert_eq!(normalize("c:"), r"C:\");
        assert_eq!(normalize(r"d:\\"), r"D:\");
        assert_eq!(normalize(r"d:/data/"), r"D:\data");
        assert_eq!(normalize(r#""E:\a b\""#), r"E:\a b");
        assert_eq!(normalize(r"\\nas\share\"), r"\\nas\share");
        assert!(is_volume_root(r"C:\"));
        assert!(!is_volume_root(r"C:\x"));
    }
}
