//! 扫描结果的磁盘缓存：`%LOCALAPPDATA%\Sweep\cache\<目标路径的哈希>.bin`。
//!
//! 只保存汇总前的原始数据（文件自身的大小、目录的结构），载入后由 [`Tree::finish`] 重新汇总。
//! 按列存放（父节点差值、标记、大小、时间、记录号、名字各成一列）再用 zstd 压缩，
//! 同类数据挨在一起压缩率高得多。节点按广度优先序写出，已删除的节点不写，下标顺带被压实。

use std::io::{self, Read, Write};
use std::path::PathBuf;

use crate::tree::{Entry, NONE, Tree, flags};

const MAGIC: &[u8; 8] = b"SWEEPC01";
const LEVEL: i32 = 3;

/// 缓存对应的扫描状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// 规范化后的扫描目标（如 `C:\`）。
    pub target: String,
    /// 保存时变更日志的 ID 与位置：树反映了这个位置之前的所有变更。
    pub journal_id: u64,
    pub next_usn: i64,
    /// 最近一次完整扫描的时间（FILETIME）。
    pub full_scan_at: u64,
}

/// 缓存目录。
pub fn dir() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("Sweep")
            .join("cache"),
    )
}

/// 某个目标的缓存文件。
pub fn file_for(target: &str) -> Option<PathBuf> {
    // FNV-1a，路径不区分大小写。
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in target.to_lowercase().bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    Some(dir()?.join(format!("{h:016x}.bin")))
}

/// 所有缓存文件的大小合计。
pub fn usage() -> u64 {
    let Some(d) = dir() else { return 0 };
    let Ok(entries) = std::fs::read_dir(d) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok()?.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// 删除所有缓存。
pub fn clear() -> io::Result<()> {
    match dir() {
        Some(d) if d.exists() => std::fs::remove_dir_all(d),
        _ => Ok(()),
    }
}

/// 删除一个目标的缓存（扫描方式改了、缓存损坏时）。
pub fn remove(target: &str) {
    if let Some(p) = file_for(target) {
        let _ = std::fs::remove_file(p);
    }
}

struct Writer(Vec<u8>);

impl Writer {
    fn var(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push(v as u8 | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    fn bytes(&mut self, b: &[u8]) {
        self.var(b.len() as u64);
        self.0.extend_from_slice(b);
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn var(&mut self) -> io::Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = self.0.split_first().ok_or_else(corrupt)?;
            self.0 = rest;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        Err(corrupt())
    }

    fn bytes(&mut self) -> io::Result<&[u8]> {
        let n = self.var()? as usize;
        if n > self.0.len() {
            return Err(corrupt());
        }
        let (b, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(b)
    }

    fn str(&mut self) -> io::Result<&str> {
        std::str::from_utf8(self.bytes()?).map_err(|_| corrupt())
    }
}

fn corrupt() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "corrupt cache file")
}

/// 保存树（已汇总或未汇总都可以）与对应的记录号。先写临时文件再改名，中途失败不会留下半个文件。
pub fn save(tree: &Tree, ids: &[u64], meta: &Meta) -> io::Result<u64> {
    let path = file_for(&meta.target).ok_or_else(|| io::Error::other("no LOCALAPPDATA"))?;
    std::fs::create_dir_all(path.parent().unwrap())?;

    let order = tree.bfs(tree.root);
    let mut index = vec![NONE; tree.nodes.len()];
    for (k, &i) in order.iter().enumerate() {
        index[i as usize] = k as u32;
    }
    let mut head = Writer(Vec::new());
    head.bytes(meta.target.as_bytes());
    head.var(meta.journal_id);
    head.var(meta.next_usn as u64);
    head.var(meta.full_scan_at);
    head.var(u64::from(tree.by_alloc));
    head.var(order.len() as u64);
    head.var(tree.exts.len() as u64);
    for e in &tree.exts {
        head.bytes(e.as_bytes());
    }
    let mut cols: [Writer; 8] = std::array::from_fn(|_| Writer(Vec::new()));
    let [parents, fl, sizes, allocs, mtimes, recs, exts, names] = &mut cols;
    let mut last_parent = 0u64;
    for &i in &order {
        let n = tree.node(i);
        // 广度优先序里父节点下标单调不减，存差值。
        let p = if n.parent == NONE {
            0
        } else {
            u64::from(index[n.parent as usize])
        };
        parents.var(p - last_parent);
        last_parent = p;
        fl.0.push(n.flags & !flags::DELETED);
        // 目录只存结构，大小由载入后的汇总得出。
        let (size, alloc) = if n.is_dir() {
            (0, 0)
        } else {
            (n.size, n.alloc)
        };
        sizes.var(size);
        allocs.var(alloc);
        mtimes.var(n.mtime);
        recs.var(ids.get(i as usize).copied().unwrap_or(0));
        exts.var(u64::from(n.ext));
        names.bytes(tree.name(i).as_bytes());
    }

    // 每次保存用不同的临时文件：上一次扫描的后台保存可能还没结束。
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}-{seq}.tmp", std::process::id()));
    let file = std::fs::File::create(&tmp)?;
    let mut out = io::BufWriter::new(file);
    out.write_all(MAGIC)?;
    let mut z = zstd::Encoder::new(out, LEVEL)?;
    // 几百 MB 的原始数据单线程压缩要一秒左右，分给多个线程。
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    z.multithread(workers as u32)?;
    for w in std::iter::once(&head).chain(cols.iter()) {
        z.write_all(&(w.0.len() as u64).to_le_bytes())?;
        z.write_all(&w.0)?;
    }
    let out = z.finish()?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok(std::fs::metadata(&path)?.len())
}

/// 载入 `target` 的缓存，返回未汇总的树（需要调用 [`Tree::finish`]）与记录号。
pub fn load(target: &str) -> io::Result<(Meta, Tree, Vec<u64>)> {
    let path = file_for(target).ok_or_else(|| io::Error::other("no LOCALAPPDATA"))?;
    let mut file = io::BufReader::new(std::fs::File::open(path)?);
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(corrupt());
    }
    let data = zstd::decode_all(file)?;
    let mut parts = Vec::with_capacity(9);
    let mut rest = data.as_slice();
    for _ in 0..9 {
        let len = u64::from_le_bytes(rest.get(..8).ok_or_else(corrupt)?.try_into().unwrap());
        let len = usize::try_from(len).map_err(|_| corrupt())?;
        let body = rest.get(8..8 + len).ok_or_else(corrupt)?;
        parts.push(body);
        rest = &rest[8 + len..];
    }

    let mut head = Reader(parts[0]);
    let meta_target = head.str()?.to_string();
    if !meta_target.eq_ignore_ascii_case(target) {
        return Err(corrupt());
    }
    let journal_id = head.var()?;
    let next_usn = head.var()? as i64;
    let full_scan_at = head.var()?;
    let by_alloc = head.var()? != 0;
    let count = head.var()? as usize;
    let ext_count = head.var()? as usize;
    let mut ext_table = Vec::with_capacity(ext_count);
    for _ in 0..ext_count {
        ext_table.push(head.str()?.to_string());
    }
    if count == 0 || ext_table.is_empty() {
        return Err(corrupt());
    }

    let [
        mut parents,
        fl,
        mut sizes,
        mut allocs,
        mut mtimes,
        mut recs,
        mut exts,
        mut names,
    ] = std::array::from_fn(|k| Reader(parts[k + 1]));
    if fl.0.len() != count {
        return Err(corrupt());
    }
    let mut tree = Tree::with_capacity(&meta_target, count, names.0.len());
    let mut ids = Vec::with_capacity(count);
    let mut parent = 0u64;
    for (k, &f) in fl.0.iter().enumerate() {
        parent += parents.var()?;
        let e = Entry {
            size: sizes.var()?,
            alloc: allocs.var()?,
            mtime: mtimes.var()?,
            flags: f,
        };
        let id = recs.var()?;
        let ext = exts.var()?;
        let name = std::str::from_utf8(names.bytes()?).map_err(|_| corrupt())?;
        if ext as usize >= ext_table.len() {
            return Err(corrupt());
        }
        let i = if k == 0 {
            // 根节点已由 Tree::with_capacity 建好，名字就是扫描目标。
            let r = &mut tree.nodes[0];
            r.flags = f;
            r.mtime = e.mtime;
            0
        } else {
            if parent >= k as u64 || !tree.nodes[parent as usize].is_dir() {
                return Err(corrupt());
            }
            tree.push(parent as u32, name, e)
        };
        tree.nodes[i as usize].ext = ext as u16;
        ids.push(id);
    }
    tree.exts = ext_table;
    tree.by_alloc = by_alloc;
    Ok((
        Meta {
            target: meta_target,
            journal_id,
            next_usn,
            full_scan_at,
        },
        tree,
        ids,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        let mut w = Writer(Vec::new());
        for v in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            w.var(v);
        }
        w.bytes("名前.txt".as_bytes());
        let mut r = Reader(&w.0);
        for v in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            assert_eq!(r.var().unwrap(), v);
        }
        assert_eq!(r.str().unwrap(), "名前.txt");
        assert!(r.var().is_err());
    }

    #[test]
    fn saves_and_loads_a_tree() {
        // 用临时的 LOCALAPPDATA，不碰真实缓存。
        let base = std::env::temp_dir().join(format!("sweep-test-cache-{}", std::process::id()));
        // SAFETY: 测试里只有这里改环境变量。
        unsafe { std::env::set_var("LOCALAPPDATA", &base) };

        let mut t = Tree::new(r"X:\");
        let docs = t.push(
            t.root,
            "docs",
            Entry {
                flags: flags::DIR,
                mtime: 7,
                ..Default::default()
            },
        );
        t.push(
            docs,
            "a.txt",
            Entry {
                size: 5000,
                alloc: 8192,
                mtime: 9,
                flags: flags::HIDDEN,
            },
        );
        t.push(
            docs,
            "b.txt",
            Entry {
                flags: flags::HARDLINK,
                mtime: 3,
                ..Default::default()
            },
        );
        let gone = t.push(
            t.root,
            "gone.bin",
            Entry {
                size: 1,
                alloc: 4096,
                ..Default::default()
            },
        );
        t.push(
            t.root,
            "日本語.md",
            Entry {
                size: 12,
                alloc: 4096,
                ..Default::default()
            },
        );
        t.by_alloc = true;
        t.finish();
        t.remove(gone);
        let ids: Vec<u64> = (0..t.len() as u64).map(|i| 100 + i).collect();
        let meta = Meta {
            target: r"X:\".into(),
            journal_id: 42,
            next_usn: 1 << 40,
            full_scan_at: 123,
        };
        save(&t, &ids, &meta).unwrap();
        assert!(usage() > 0);

        let (m, mut l, lids) = load(r"x:\").unwrap();
        assert_eq!(m, meta);
        assert_eq!(l.len(), t.len() - 1, "已删除的节点不保存");
        l.finish();
        assert!(l.by_alloc);
        for path in [
            r"X:\docs",
            r"X:\docs\a.txt",
            r"X:\docs\b.txt",
            r"X:\日本語.md",
        ] {
            let (a, b) = (t.find(path).unwrap(), l.find(path).unwrap());
            let (x, y) = (t.node(a), l.node(b));
            assert_eq!(
                (x.size, x.alloc, x.mtime, x.flags, x.files, x.dirs),
                (y.size, y.alloc, y.mtime, y.flags, y.files, y.dirs),
                "{path}"
            );
            assert_eq!(t.ext_name(a), l.ext_name(b));
            assert_eq!(ids[a as usize], lids[b as usize]);
        }
        assert_eq!(l.find(r"X:\gone.bin"), None);
        assert_eq!(l.node(l.root).size, t.node(t.root).size);
        assert!(load(r"Y:\").is_err());

        clear().unwrap();
        assert_eq!(usage(), 0);
        let _ = std::fs::remove_dir_all(base);
    }
}
