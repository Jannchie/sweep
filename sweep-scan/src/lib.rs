//! Sweep 的扫描引擎：把一个卷或文件夹扫成一棵紧凑的 [`Tree`]。不依赖任何界面库。
//!
//! 两条路径：
//! - [`mft`]：NTFS 卷 + 管理员权限时直接读主文件表，几百万个文件也只要几秒；
//! - [`walk`]：其余情况（没有权限、FAT/exFAT/ReFS、网络盘、只扫一个文件夹）多线程遍历目录。
//!
//! [`Scan::start`] 在后台线程扫描，进度通过共享的 [`Progress`] 读取，可随时取消。
//!
//! 增量扫描（[`cache`]、[`usn`]）：NTFS + 管理员权限时，完整扫描后把结果连同变更日志的位置
//! 存到磁盘；下次扫描同一目标时载入上次的结果，只重新列出日志里记录有变动的目录。

pub mod cache;
pub mod i18n;
mod incremental;
pub mod mft;
pub mod system;
pub mod tree;
pub mod usn;
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
    /// 正在增量更新：done / total 是已处理 / 有变动的目录数。
    pub incremental: AtomicBool,
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
    /// 载入上次的结果，只更新有变动的目录。
    Incremental,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Auto => t!("自动", "Automatic", "自動"),
            Engine::Mft => t!("读取 MFT", "MFT read", "MFT 読み取り"),
            Engine::Walk => t!("遍历目录", "Folder walk", "フォルダー走査"),
            Engine::Incremental => t!("增量更新", "Incremental", "差分更新"),
        }
    }
}

pub struct Outcome {
    /// 共享指针：扫描线程在交出结果后还要用它在后台写缓存。
    pub tree: Arc<Tree>,
    /// 实际使用的方式（Auto 会解析成 Mft 或 Walk）。
    pub engine: Engine,
    pub elapsed: Duration,
    /// 读取 MFT 失败、已改用目录遍历时，MFT 那边的错误。
    pub mft_fallback: Option<String>,
    /// 增量更新：重新列出的目录数，以及所基于的完整扫描的时间（FILETIME）。
    pub incremental: Option<(usize, u64)>,
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

/// 目标所在的盘符（`C:\x` → 'C'）；网络路径等没有盘符时为 None。
fn drive_letter(target: &str) -> Option<char> {
    let b = target.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && &b[1..3] == b":\\")
        .then(|| (b[0] as char).to_ascii_uppercase())
}

/// 当前时间（FILETIME）。
fn now_filetime() -> u64 {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    unix.as_nanos() as u64 / 100 + 116_444_736_000_000_000
}

/// 扫描结束后要写入的缓存。
struct SaveJob {
    tree: Arc<Tree>,
    ids: Vec<u64>,
    meta: cache::Meta,
}

impl SaveJob {
    fn run(self) {
        if cache::save(&self.tree, &self.ids, &self.meta).is_err() {
            cache::remove(&self.meta.target);
        }
    }
}

/// 同步扫描（在调用线程上执行）。`by_alloc` 见 [`Tree::set_basis`]。
/// `cache` 为 true 且条件满足（NTFS、管理员、自动方式）时使用并更新增量扫描的缓存。
pub fn scan(
    target: &str,
    engine: Engine,
    by_alloc: bool,
    cache: bool,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<Outcome, String> {
    let (out, save) = scan_inner(target, engine, by_alloc, cache, progress, cancel)?;
    if let Some(job) = save {
        job.run();
    }
    Ok(out)
}

/// 扫描，并把写缓存留给调用方：[`Scan::start`] 先交出结果再写，界面不用等。
fn scan_inner(
    target: &str,
    engine: Engine,
    by_alloc: bool,
    cache: bool,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<(Outcome, Option<SaveJob>), String> {
    let target = normalize(target);
    let started = Instant::now();
    let letter = drive_letter(&target);
    let ntfs_admin = system::is_elevated()
        && letter.is_some_and(|l| system::fs_name(&format!("{l}:\\")).as_deref() == Some("NTFS"));
    let mft_possible = ntfs_admin && is_volume_root(&target);

    // 先记下变更日志的位置：完整扫描期间发生的变更下次会被重放，不会漏掉。
    let journal = letter
        .filter(|_| cache && engine == Engine::Auto && ntfs_admin)
        .and_then(|l| {
            let v = usn::Volume::open(l).ok()?;
            let j = v.journal().ok()?;
            Some((v, j))
        });
    if let Some((vol, j)) = &journal {
        match incremental_scan(&target, vol, j, by_alloc, progress, cancel) {
            Ok(Some(done)) => return Ok(done),
            Ok(None) => {}
            Err(e) if cancel.load(Ordering::Relaxed) => return Err(e),
            // 缓存不可用（损坏、读日志出错）：删掉它，完整扫描。
            Err(_) => cache::remove(&target),
        }
        reset(progress);
    }
    let full_scan_at = now_filetime();
    let (mut out, ids) = full_scan(&target, engine, by_alloc, mft_possible, progress, cancel)?;
    out.elapsed = started.elapsed();
    let save = journal.map(|(_, j)| SaveJob {
        tree: out.tree.clone(),
        ids,
        meta: cache::Meta {
            target: target.clone(),
            journal_id: j.id,
            next_usn: j.next,
            full_scan_at,
        },
    });
    Ok((out, save))
}

/// 载入上次的结果并套用之后的变更。没有可用的缓存（不存在、日志已覆盖上次的位置）时返回 None。
fn incremental_scan(
    target: &str,
    vol: &usn::Volume,
    journal: &usn::Journal,
    by_alloc: bool,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<Option<(Outcome, Option<SaveJob>)>, String> {
    let started = Instant::now();
    if !cache::file_for(target).is_some_and(|p| p.exists()) {
        return Ok(None);
    }
    progress.incremental.store(true, Ordering::Relaxed);
    progress.set_current(
        t!(
            "正在读取上次的扫描结果…",
            "Loading the previous result…",
            "前回のスキャン結果を読み込んでいます…"
        )
        .into(),
    );
    let (meta, mut tree, mut ids) = cache::load(target).map_err(|e| e.to_string())?;
    let frns = match vol.changed_dirs(journal, meta.journal_id, meta.next_usn, cancel) {
        Ok(d) => d,
        Err(usn::Error::Stale) => return Ok(None),
        Err(usn::Error::Cancelled) => return Err(cancelled()),
        Err(usn::Error::Io(e)) => return Err(e),
    };
    let (files, dirs, bytes) = tree.nodes.iter().fold((0, 0, 0), |(f, d, b), n| {
        if n.is_dir() {
            (f, d + 1, b)
        } else {
            (f + 1, d, b + n.size)
        }
    });
    progress.files.store(files, Ordering::Relaxed);
    progress.dirs.store(dirs, Ordering::Relaxed);
    progress.bytes.store(bytes, Ordering::Relaxed);
    progress.set_current(tf!(
        "正在定位 {} 个有变动的文件夹…",
        "Locating {} changed folders…",
        "変更のあった {} 個のフォルダーを特定しています…",
        frns.len()
    ));
    let root = target.trim_end_matches('\\');
    let paths: Vec<String> = frns
        .into_iter()
        .filter_map(|f| vol.path_of(f))
        .filter(|p| {
            p.len() >= root.len()
                && p.is_char_boundary(root.len())
                && p[..root.len()].eq_ignore_ascii_case(root)
                && (p.len() == root.len() || p.as_bytes()[root.len()] == b'\\')
        })
        .collect();
    let listed = incremental::apply(&mut tree, &mut ids, paths, progress, cancel)?;
    progress.set_current(t!("正在汇总…", "Totaling…", "集計しています…").into());
    tree.by_alloc = by_alloc;
    tree.finish();
    let tree = Arc::new(tree);
    let out = Outcome {
        tree: tree.clone(),
        engine: Engine::Incremental,
        elapsed: started.elapsed(),
        mft_fallback: None,
        incremental: Some((listed, meta.full_scan_at)),
    };
    let save = SaveJob {
        tree,
        ids,
        meta: cache::Meta {
            next_usn: journal.next,
            ..meta
        },
    };
    Ok(Some((out, Some(save))))
}

/// 完整扫描：能读 MFT 就读，否则遍历目录。另外返回各节点的 MFT 记录号（供缓存使用）。
fn full_scan(
    target: &str,
    engine: Engine,
    by_alloc: bool,
    mft_possible: bool,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<(Outcome, Vec<u64>), String> {
    let started = Instant::now();
    let mut mft_fallback = None;
    if engine == Engine::Mft && !mft_possible {
        return Err(t!(
            "读取 MFT 需要以管理员身份运行，且目标是 NTFS 卷的根目录",
            "Reading the MFT requires administrator rights and the root of an NTFS volume",
            "MFT の読み取りには管理者権限と NTFS ボリュームのルートが必要です"
        )
        .into());
    }
    if engine != Engine::Walk && mft_possible {
        match mft::scan_with_ids(target, by_alloc, progress, cancel) {
            Ok((tree, ids)) => {
                let out = Outcome {
                    tree: Arc::new(tree),
                    engine: Engine::Mft,
                    elapsed: started.elapsed(),
                    mft_fallback,
                    incremental: None,
                };
                return Ok((out, ids));
            }
            Err(e) if engine == Engine::Mft || cancel.load(Ordering::Relaxed) => return Err(e),
            Err(e) => {
                mft_fallback = Some(e);
                reset(progress);
            }
        }
    }
    // 管理员身份时打开备份特权，遍历能进入 ACL 不允许列出的目录。
    if system::is_elevated() {
        system::enable_backup_privilege();
    }
    let (mut tree, ids) = walk::scan_raw(target, progress, cancel)?;
    progress.set_current(t!("正在汇总…", "Totaling…", "集計しています…").into());
    tree.by_alloc = by_alloc;
    tree.finish();
    let out = Outcome {
        tree: Arc::new(tree),
        engine: Engine::Walk,
        elapsed: started.elapsed(),
        mft_fallback,
        incremental: None,
    };
    Ok((out, ids))
}

/// 取消扫描时返回的错误。
pub fn cancelled() -> String {
    t!("已取消", "Canceled", "キャンセルしました").into()
}

fn reset(p: &Progress) {
    for c in [&p.files, &p.dirs, &p.bytes, &p.done, &p.total, &p.ticks] {
        c.store(0, Ordering::Relaxed);
    }
    p.top.lock().unwrap().clear();
    p.incremental.store(false, Ordering::Relaxed);
    p.set_current(String::new());
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
        cache: bool,
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
                let (result, save) = match scan_inner(&t, engine, by_alloc, cache, &p, &c) {
                    Ok((out, save)) => (Ok(out), save),
                    Err(e) => (Err(e), None),
                };
                let _ = tx.send(result);
                notify();
                // 结果已经交出，界面可以操作；树的修改会等到这里放开共享指针之后。
                if let Some(job) = save {
                    job.run();
                }
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
