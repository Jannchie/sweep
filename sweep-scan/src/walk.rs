//! 通用路径：多线程遍历目录。不需要管理员权限，任何文件系统、任何文件夹都能用。
//!
//! 每个目录是线程池里的一个任务（rayon 的工作窃取），用 `GetFileInformationByHandleEx`
//! 配 64 KB 缓冲区一次取回几百个目录项（与 `NtQueryDirectoryFile` 相同的批量接口），
//! 比 `FindFirstFile`/`FindNextFile` 逐项返回少很多次系统调用。
//! 不进入重解析点（目录联接、符号链接），避免重复计算和死循环。
//! 硬链接按文件 ID 去重：同一个文件只有第一个被扫到的名字计大小。

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdBothDirectoryInfo,
    GetFileInformationByHandleEx, OPEN_EXISTING,
};
use windows::core::PCWSTR;

use crate::Progress;
use crate::tree::{Entry, Tree, flags};

const FA_DIRECTORY: u32 = 0x10;
const FA_HIDDEN: u32 = 0x2;
const FA_SYSTEM: u32 = 0x4;
const FA_REPARSE: u32 = 0x400;
/// 重解析标记里的"名称代理"位：IO_REPARSE_TAG_MOUNT_POINT、IO_REPARSE_TAG_SYMLINK 等都带它。
const NAME_SURROGATE: u32 = 0x2000_0000;
/// 目录查询缓冲区（以 u64 计，保证 8 字节对齐）。64 KB 是 SMB 等网络文件系统单次能返回的上限。
const BUF_WORDS: usize = 64 * 1024 / 8;
/// 硬链接去重表分片数，减少线程之间抢锁。
const SHARDS: usize = 64;

struct Ctx<'a> {
    tree: Mutex<Tree>,
    seen: Vec<Mutex<HashSet<u64>>>,
    progress: &'a Progress,
    cancel: &'a AtomicBool,
}

/// 一个目录项（还没放进树里）。
struct Raw {
    name: String,
    attrs: u32,
    size: u64,
    alloc: u64,
    mtime: u64,
    id: u64,
    /// 重解析点的标记（目录项里的 EaSize 字段对重解析点存放的是它）。
    tag: u32,
}

/// 扫描 `root` 文件夹（如 `C:\` 或 `D:\data`）。
/// `by_alloc` 决定汇总后按哪种大小排序（见 [`Tree::set_basis`]）。
pub fn scan(
    root: &str,
    by_alloc: bool,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<Tree, String> {
    let meta = std::fs::metadata(root).map_err(|e| format!("无法访问 {root}：{e}"))?;
    if !meta.is_dir() {
        return Err(format!("{root} 不是文件夹"));
    }
    let threads = std::thread::available_parallelism()
        .map_or(8, |n| n.get())
        .clamp(4, 12);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("walk-{i}"))
        .build()
        .map_err(|e| e.to_string())?;
    let ctx = Ctx {
        tree: Mutex::new(Tree::new(root)),
        seen: (0..SHARDS).map(|_| Mutex::new(HashSet::new())).collect(),
        progress,
        cancel,
    };
    let root_path = long_path(root);
    pool.scope(|s| visit(s, &ctx, 0, root_path, None));
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".into());
    }
    progress.set_current("正在汇总…".into());
    let mut tree = ctx.tree.into_inner().unwrap();
    tree.by_alloc = by_alloc;
    tree.finish();
    Ok(tree)
}

/// 转成带 `\\?\` 前缀的宽字符路径，绕过 MAX_PATH。
fn long_path(path: &str) -> Vec<u16> {
    let p = if let Some(unc) = path.strip_prefix(r"\\") {
        if unc.starts_with(r"?\") {
            path.to_string()
        } else {
            format!(r"\\?\UNC\{unc}")
        }
    } else {
        format!(r"\\?\{path}")
    };
    p.encode_utf16().collect()
}

fn join(dir: &[u16], name: &str) -> Vec<u16> {
    let mut p = Vec::with_capacity(dir.len() + name.len() + 1);
    p.extend_from_slice(dir);
    if p.last() != Some(&(b'\\' as u16)) {
        p.push(b'\\' as u16);
    }
    p.extend(name.encode_utf16());
    p
}

thread_local! {
    static BUF: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(vec![0; BUF_WORDS]);
}

/// 列出一个目录。打不开时返回 Err(是否因为权限不足)。
fn list(path: &[u16]) -> Result<Vec<Raw>, bool> {
    let mut z = path.to_vec();
    z.push(0);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(z.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            // 备份语义才能打开目录；进程持有 SeBackupPrivilege 时还能越过 ACL。
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(|e| e.code() == ERROR_ACCESS_DENIED.to_hresult())?;
    let out = BUF.with(|buf| read_entries(handle, &mut buf.borrow_mut()));
    unsafe {
        let _ = CloseHandle(handle);
    }
    Ok(out)
}

fn read_entries(handle: HANDLE, buf: &mut [u64]) -> Vec<Raw> {
    let mut out = Vec::new();
    loop {
        let r = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdBothDirectoryInfo,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
            )
        };
        // ERROR_NO_MORE_FILES 是正常结束；其他错误（目录在扫描中被删等）也就此停止。
        if r.is_err() {
            return out;
        }
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 8) };
        let mut off = 0usize;
        loop {
            let info = unsafe { &*(bytes.as_ptr().add(off) as *const FILE_ID_BOTH_DIR_INFO) };
            let name_off = off + std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            let len = info.FileNameLength as usize / 2;
            let name: &[u16] = unsafe {
                std::slice::from_raw_parts(bytes.as_ptr().add(name_off).cast::<u16>(), len)
            };
            let dots = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
            if !dots {
                out.push(Raw {
                    name: String::from_utf16_lossy(name),
                    attrs: info.FileAttributes,
                    size: info.EndOfFile as u64,
                    alloc: info.AllocationSize as u64,
                    mtime: info.LastWriteTime as u64,
                    id: info.FileId as u64,
                    tag: info.EaSize,
                });
            }
            if info.NextEntryOffset == 0 {
                break;
            }
            off += info.NextEntryOffset as usize;
        }
    }
}

/// 处理一个目录：列出内容、放进树里、为子目录派生任务。`top` 是它所属的根下第一级目录。
fn visit<'s>(
    s: &rayon::Scope<'s>,
    ctx: &'s Ctx<'s>,
    node: u32,
    path: Vec<u16>,
    top: Option<usize>,
) {
    if ctx.cancel.load(Ordering::Relaxed) {
        return;
    }
    let entries = match list(&path) {
        Ok(e) => e,
        Err(denied) => {
            if denied {
                ctx.tree.lock().unwrap().nodes[node as usize].flags |= flags::DENIED;
            }
            return;
        }
    };
    // 先去重硬链接（不占树的锁）。
    let dup: Vec<bool> = entries
        .iter()
        .map(|e| {
            e.attrs & FA_DIRECTORY == 0
                && e.id != 0
                && !ctx.seen[(e.id as usize).wrapping_mul(0x9E37_79B9) % SHARDS]
                    .lock()
                    .unwrap()
                    .insert(e.id)
        })
        .collect();

    let mut subdirs = Vec::new();
    let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
    {
        let mut tree = ctx.tree.lock().unwrap();
        for (e, dup) in entries.iter().zip(dup) {
            let is_dir = e.attrs & FA_DIRECTORY != 0;
            let mut f = 0u8;
            if is_dir {
                f |= flags::DIR;
            }
            if e.attrs & FA_HIDDEN != 0 {
                f |= flags::HIDDEN;
            }
            if e.attrs & FA_SYSTEM != 0 {
                f |= flags::SYSTEM;
            }
            if e.attrs & FA_REPARSE != 0 {
                f |= flags::REPARSE;
            }
            if dup {
                f |= flags::HARDLINK;
            }
            let (size, alloc) = if is_dir || dup {
                (0, 0)
            } else {
                (e.size, e.alloc)
            };
            let child = tree.push(
                node,
                &e.name,
                Entry {
                    size,
                    alloc,
                    mtime: e.mtime,
                    flags: f,
                },
            );
            if is_dir {
                dirs += 1;
                // 只跳过"名称代理"类的重解析点（目录联接、符号链接），它们指向别处的内容；
                // OneDrive 等云文件占位目录也是重解析点，但内容就在这里，要进入。
                if e.attrs & FA_REPARSE == 0 || e.tag & NAME_SURROGATE == 0 {
                    subdirs.push((child, e.name.as_str()));
                }
            } else {
                files += 1;
                bytes += size;
            }
        }
    }

    let p = ctx.progress;
    p.files.fetch_add(files, Ordering::Relaxed);
    p.dirs.fetch_add(dirs, Ordering::Relaxed);
    p.bytes.fetch_add(bytes, Ordering::Relaxed);
    p.done.fetch_add(bytes, Ordering::Relaxed);
    if node == 0 {
        // 根目录：登记第一级子目录，进度页实时显示它们各自扫到的大小。
        let mut t = p.top.lock().unwrap();
        t.clear();
        t.extend(subdirs.iter().map(|(_, n)| (n.to_string(), 0)));
        t.push((String::new(), bytes));
    } else if let Some(i) = top {
        p.top.lock().unwrap()[i].1 += bytes;
    }
    // 当前路径不必每个目录都更新，取一部分足够让进度页看起来在动。
    if p.ticks.fetch_add(1, Ordering::Relaxed).is_multiple_of(128) {
        let shown = path.strip_prefix(&[92, 92, 63, 92][..]).unwrap_or(&path);
        p.set_current(String::from_utf16_lossy(shown));
    }

    for (k, (child, name)) in subdirs.into_iter().enumerate() {
        let path = join(&path, name);
        let top = if node == 0 { Some(k) } else { top };
        s.spawn(move |s| visit(s, ctx, child, path, top));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::NONE;

    #[test]
    fn builds_long_paths() {
        let s = |v: Vec<u16>| String::from_utf16(&v).unwrap();
        assert_eq!(s(long_path(r"C:\")), r"\\?\C:\");
        assert_eq!(s(long_path(r"\\nas\share")), r"\\?\UNC\nas\share");
        assert_eq!(s(join(&long_path(r"C:\"), "Windows")), r"\\?\C:\Windows");
        assert_eq!(s(join(&long_path(r"C:\a"), "b")), r"\\?\C:\a\b");
    }

    /// 在临时目录里造一棵小树（含硬链接与目录联接）扫一遍。
    #[test]
    fn scans_a_temp_tree() {
        let base = std::env::temp_dir().join(format!("sweep-test-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a/b")).unwrap();
        std::fs::write(base.join("a/one.bin"), vec![1u8; 1000]).unwrap();
        std::fs::write(base.join("a/b/two.txt"), vec![2u8; 300]).unwrap();
        std::fs::write(base.join("root.txt"), b"hi").unwrap();
        std::fs::hard_link(base.join("a/one.bin"), base.join("link.bin")).unwrap();
        // 目录联接指回 a：不应进入，否则 a 会被算两遍。
        let junction = base.join("loop");
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(base.join("a"))
            .output()
            .is_ok_and(|o| o.status.success());

        let root = base.to_str().unwrap();
        let t = scan(root, false, &Progress::default(), &AtomicBool::new(false)).unwrap();
        std::fs::remove_dir_all(&base).unwrap();

        let r = t.node(t.root);
        assert_eq!(r.size, 1302, "硬链接只计一次");
        assert_eq!(r.files, 4);
        assert_eq!(r.dirs, if ok { 3 } else { 2 });
        let hard = [
            t.find(&format!(r"{root}\a\one.bin")),
            t.find(&format!(r"{root}\link.bin")),
        ];
        assert_eq!(
            hard.iter()
                .filter(|n| t.node(n.unwrap()).has(flags::HARDLINK))
                .count(),
            1
        );
        if ok {
            let j = t.find(&format!(r"{root}\loop")).unwrap();
            assert!(t.node(j).has(flags::REPARSE));
            assert_eq!(t.node(j).first_child, NONE);
        }
    }
}
