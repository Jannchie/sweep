//! NTFS 快速路径：直接读卷上的 $MFT，一次顺序读完所有文件记录，不用逐个目录打开。
//!
//! 需要管理员权限（打开 `\\.\C:` 卷句柄）。流程：
//! 1. 读引导扇区，得到簇大小、记录大小与 $MFT 的起始簇；
//! 2. 读 0 号记录（$MFT 自己），解析无名 $DATA 的数据运行表——$MFT 本身可能是碎片化的，
//!    极端情况下运行表还会分散在 $ATTRIBUTE_LIST 指向的扩展记录里；
//! 3. 读线程按运行表大块顺序读取，解析线程并行做 fixup 与属性解析，调用线程边收边建树；
//! 4. 按 $FILE_NAME 里的父目录引用（校验序列号）把节点挂起来，汇总、排序。

pub mod record;

use std::collections::HashMap;
use std::fs::File;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};

use crate::Progress;
use crate::tree::{Entry, NONE, Tree, flags};
use record::{ATTR_ATTRIBUTE_LIST, ATTR_BITMAP, ATTR_DATA, Run};

/// 每次读取的块大小。足够大才能吃满 NVMe 的顺序读带宽，又不至于让进度更新太粗。
const CHUNK: usize = 8 << 20;
/// 同时读盘的线程数。
const READERS: usize = 4;
/// 位图里小于这个长度的未使用空洞仍然读取。
const MERGE_GAP: u64 = 256 * 1024;
const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
const FILE_SHARE_ALL: u32 = 0x7;
/// 根目录固定是 5 号记录；0–15 号是 NTFS 元文件，11 号是 $Extend 目录。
const ROOT_RECORD: u64 = 5;
const EXTEND_RECORD: u64 = 11;
const FIRST_USER_RECORD: u64 = 16;
/// 文件属性：隐藏、系统、重解析点。
const FA_HIDDEN: u32 = 0x2;
const FA_SYSTEM: u32 = 0x4;
const FA_REPARSE: u32 = 0x400;

/// 引导扇区里的卷几何信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub sector: u64,
    pub cluster: u64,
    pub record: u64,
    pub mft_lcn: u64,
}

pub fn parse_boot(b: &[u8]) -> Result<Geometry, String> {
    if b.len() < 512 || &b[3..11] != b"NTFS    " {
        return Err("不是 NTFS 卷".into());
    }
    let sector = u16::from_le_bytes([b[0x0B], b[0x0C]]) as u64;
    // 每簇扇区数：大于 0x80 时表示 2 的 (256 - x) 次方（64 KB 以上的簇）。
    let spc = b[0x0D];
    let spc = if spc > 0x80 {
        1u64 << (256 - spc as u32)
    } else {
        spc as u64
    };
    let cluster = sector * spc;
    // 每条记录的簇数：负数表示记录大小为 2 的 (-x) 次方字节。
    let cpr = b[0x40] as i8;
    let record = if cpr < 0 {
        1u64 << (-(cpr as i32))
    } else {
        cpr as u64 * cluster
    };
    let mft_lcn = u64::from_le_bytes(b[0x30..0x38].try_into().unwrap());
    if !sector.is_power_of_two() || sector < 512 || cluster == 0 || !(256..=65536).contains(&record)
    {
        return Err("NTFS 引导扇区参数异常".into());
    }
    Ok(Geometry {
        sector,
        cluster,
        record,
        mft_lcn,
    })
}

/// 按扇区对齐的缓冲区（无缓冲读取要求地址、长度、偏移都对齐到扇区）。
struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

// 缓冲区只在线程之间移交所有权，不会共享。
unsafe impl Send for AlignedBuf {}

const ALIGN: usize = 4096;

impl AlignedBuf {
    fn new(len: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, ALIGN).unwrap();
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "out of memory");
        Self { ptr, len }
    }

    fn as_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(self.len, ALIGN).unwrap();
        unsafe { std::alloc::dealloc(self.ptr, layout) };
    }
}

struct Volume {
    file: File,
    geo: Geometry,
}

impl Volume {
    /// `letter` 如 'C'。
    fn open(letter: char) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_ALL)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(format!(r"\\.\{letter}:"))
            .map_err(|e| format!("无法打开卷 {letter}:（{e}）"))?;
        let mut boot = AlignedBuf::new(ALIGN);
        file.seek_read(boot.as_mut(), 0)
            .map_err(|e| format!("无法读取引导扇区：{e}"))?;
        let geo = parse_boot(boot.as_mut())?;
        Ok(Self { file, geo })
    }

    /// 读取 `len` 字节（会向上取整到扇区）到对齐缓冲区。
    fn read(&self, offset: u64, len: usize) -> Result<AlignedBuf, String> {
        let len = (len as u64).div_ceil(self.geo.sector) * self.geo.sector;
        let mut buf = AlignedBuf::new((len as usize).div_ceil(ALIGN) * ALIGN);
        let slice = &mut buf.as_mut()[..len as usize];
        read_exact_at(&self.file, slice, offset)?;
        Ok(buf)
    }
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> Result<(), String> {
    while !buf.is_empty() {
        let n = file
            .seek_read(buf, offset)
            .map_err(|e| format!("读取卷失败（偏移 {offset}）：{e}"))?;
        if n == 0 {
            return Err(format!("读取卷时遇到意外的结尾（偏移 {offset}）"));
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

/// $MFT 在卷上的位置：若干段 (卷内字节偏移, 字节数)，按文件内顺序排列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub extents: Vec<(u64, u64)>,
    /// $MFT 的有效字节数（记录数 × 记录大小）。
    pub size: u64,
    /// $MFT:$BITMAP，每条记录一位，1 表示在用。读不到时为 None（全部读取）。
    pub bitmap: Option<Vec<u8>>,
}

/// 把数据运行转换成卷内的字节区间。$MFT 不应该有稀疏段，遇到就报错。
pub fn runs_to_extents(runs: &[Run], cluster: u64) -> Result<Vec<(u64, u64)>, String> {
    runs.iter()
        .map(|r| match r.lcn {
            Some(lcn) => Ok((lcn * cluster, r.clusters * cluster)),
            None => Err("$MFT 含有稀疏段".to_string()),
        })
        .collect()
}

/// 解析 0 号记录，得到 $MFT 的完整布局与位图。`read(偏移, 长度)` 从卷上读字节。
/// $MFT 碎片多时会有 $ATTRIBUTE_LIST：列表本身可能是非常驻的，运行表的后几段和 $BITMAP
/// 都可能放在扩展记录里，要按目前已知的区段把那些记录读出来。
pub fn mft_layout(
    rec0: &mut [u8],
    geo: &Geometry,
    read: impl Fn(u64, usize) -> Result<Vec<u8>, String>,
) -> Result<Layout, String> {
    let bad = |e: record::Error| format!("$MFT 记录损坏：{e:?}");
    // 非常驻属性的全部内容（属性列表、位图），按自己的运行表读出并截到实际长度。
    let content = |a: &record::Attr| -> Result<Vec<u8>, String> {
        if let Some(v) = a.value() {
            return Ok(v.to_vec());
        }
        let nr = a.non_resident().ok_or("属性头损坏")?;
        let runs = record::decode_runs(a.runs().unwrap_or_default()).map_err(bad)?;
        let mut bytes = Vec::new();
        for (off, len) in runs_to_extents(&runs, geo.cluster)? {
            bytes.extend(read(off, len as usize)?);
        }
        bytes.truncate(nr.real as usize);
        Ok(bytes)
    };

    #[derive(Default)]
    struct Found {
        /// (lowest_vcn, 运行表)，最后按 vcn 排序拼接。
        pieces: Vec<(u64, Vec<Run>)>,
        size: Option<u64>,
        list: Vec<record::ListEntry>,
        bitmap: Option<Vec<u8>>,
    }
    // 收集一条记录（0 号或扩展记录）里 $MFT 自己的属性。
    let take = |f: &mut Found, rec: &[u8]| -> Result<(), String> {
        let h = record::header(rec).map_err(bad)?;
        for a in record::attributes(rec, &h) {
            match a.type_code {
                ATTR_DATA if !a.named => {
                    let nr = a.non_resident().ok_or("$MFT 的 $DATA 不是非常驻属性")?;
                    if nr.lowest_vcn == 0 {
                        f.size = Some(nr.real);
                    }
                    if !f.pieces.iter().any(|p| p.0 == nr.lowest_vcn) {
                        let runs =
                            record::decode_runs(a.runs().unwrap_or_default()).map_err(bad)?;
                        f.pieces.push((nr.lowest_vcn, runs));
                    }
                }
                ATTR_BITMAP if !a.named => f.bitmap = Some(content(&a)?),
                ATTR_ATTRIBUTE_LIST => f.list = record::attribute_list(&content(&a)?),
                _ => {}
            }
        }
        Ok(())
    };
    record::apply_fixup(rec0).map_err(bad)?;
    let mut f = Found::default();
    take(&mut f, rec0)?;

    let mut others: Vec<u64> = f
        .list
        .iter()
        .filter(|e| (e.type_code == ATTR_DATA || e.type_code == ATTR_BITMAP) && !e.named)
        .map(|e| e.record)
        .filter(|&r| r != 0)
        .collect();
    others.sort_unstable();
    others.dedup();
    for n in others {
        f.pieces.sort_by_key(|p| p.0);
        let known: Vec<Run> = f.pieces.iter().flat_map(|p| p.1.clone()).collect();
        let extents = runs_to_extents(&known, geo.cluster)?;
        let off = locate(&extents, geo.record, n).ok_or("$MFT 扩展记录不在已知范围内")?;
        let mut rec = read(off, geo.record as usize)?;
        rec.truncate(geo.record as usize);
        record::apply_fixup(&mut rec).map_err(bad)?;
        take(&mut f, &rec)?;
    }

    let size = f.size.ok_or("$MFT 没有 $DATA 属性")?;
    f.pieces.sort_by_key(|p| p.0);
    let runs: Vec<Run> = f.pieces.into_iter().flat_map(|p| p.1).collect();
    let extents = runs_to_extents(&runs, geo.cluster)?;
    let total: u64 = extents.iter().map(|e| e.1).sum();
    let size = size.min(total) / geo.record * geo.record;
    Ok(Layout {
        extents,
        size,
        bitmap: f.bitmap,
    })
}

/// 在布局里定位第 `n` 条记录的卷内偏移。
fn locate(extents: &[(u64, u64)], record_size: u64, n: u64) -> Option<u64> {
    let mut pos = n * record_size;
    for &(off, len) in extents {
        if pos < len {
            return Some(off + pos);
        }
        pos -= len;
    }
    None
}

/// 一条记录解析后交给建树线程的内容。名字已在解析线程里转成 UTF-8。
struct Item {
    record: u64,
    base: Option<u64>,
    seq: u16,
    is_dir: bool,
    modified: u64,
    attributes: u32,
    has_std: bool,
    names: Vec<(u64, String)>,
    /// 只有短名时的短名（见 [`record::Parsed::dos`]）。
    dos: Option<(u64, String)>,
    data: Option<(u64, u64)>,
    streams: u64,
}

/// 扫描整个 NTFS 卷。`root` 如 `C:\`。
pub fn scan(root: &str, progress: &Progress, cancel: &AtomicBool) -> Result<Tree, String> {
    let letter = root
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .ok_or("MFT 扫描只支持带盘符的卷")?;
    let vol = Volume::open(letter)?;
    let geo = vol.geo;

    let mft_offset = geo.mft_lcn * geo.cluster;
    let mut rec0 = vol.read(mft_offset, geo.record as usize)?;
    let rec0 = &mut rec0.as_mut()[..geo.record as usize];
    let layout = mft_layout(rec0, &geo, |off, len| {
        let mut buf = vol.read(off, len)?;
        Ok(buf.as_mut()[..len].to_vec())
    })?;
    let records = layout.size / geo.record;
    progress.total.store(layout.size, Ordering::Relaxed);
    progress.set_current(format!(
        "正在读取 {letter}: 的主文件表（{} 条记录）",
        records
    ));

    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(2, 16);
    // 空闲缓冲区池：读线程从这里取，解析线程用完放回，限制同时在内存里的块数。
    let (free_tx, free_rx) = sync_channel::<AlignedBuf>(workers + READERS + 2);
    for _ in 0..workers + READERS + 2 {
        free_tx.send(AlignedBuf::new(CHUNK)).unwrap();
    }
    let (chunk_tx, chunk_rx) = sync_channel::<(u64, usize, AlignedBuf)>(workers + READERS + 2);
    let chunk_rx = Arc::new(Mutex::new(chunk_rx));
    let (item_tx, item_rx) = sync_channel::<Vec<Item>>(workers * 2);

    let chunks = plan_chunks(&layout, geo.record, geo.cluster, CHUNK);
    let bitmap = layout.bitmap.as_deref();
    let next = AtomicUsize::new(0);
    let free_rx = Mutex::new(free_rx);
    std::thread::scope(|s| -> Result<Tree, String> {
        let readers: Vec<_> = (0..READERS)
            .map(|_| {
                let (chunks, next, free_rx, chunk_tx) =
                    (&chunks, &next, &free_rx, chunk_tx.clone());
                s.spawn(move || {
                    // 同一个同步句柄上的读请求会被 I/O 管理器串行化，每个读线程各开一个句柄。
                    let own = Volume::open(letter)?;
                    read_all(&own, chunks, next, free_rx, chunk_tx, progress, cancel)
                })
            })
            .collect();
        drop(chunk_tx);
        for _ in 0..workers {
            let (chunk_rx, item_tx, free_tx) = (chunk_rx.clone(), item_tx.clone(), free_tx.clone());
            let (record, cluster) = (geo.record as usize, geo.cluster);
            s.spawn(move || {
                loop {
                    let next = chunk_rx.lock().unwrap().recv();
                    let Ok((first, len, mut buf)) = next else {
                        break;
                    };
                    let items =
                        parse_chunk(&mut buf.as_mut()[..len], record, cluster, first, bitmap);
                    let _ = free_tx.send(buf);
                    if item_tx.send(items).is_err() {
                        break;
                    }
                }
            });
        }
        drop(item_tx);
        drop(free_tx);
        let tree = build(root, records, item_rx, progress, cancel);
        for r in readers {
            r.join().map_err(|_| "读线程异常退出".to_string())??;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        Ok(tree)
    })
}

/// 一块：起始记录号、总长度，以及组成它的若干段 (卷内偏移, 块内偏移, 长度)。
/// 块边界总落在记录边界上；$MFT 碎片化时一块可能跨越多个区段。
#[derive(Debug, PartialEq, Eq)]
struct Chunk {
    first: u64,
    len: usize,
    pieces: Vec<(u64, usize, usize)>,
}

/// 按位图挑出需要读的部分，切成不超过 `chunk` 字节的块。整片未使用的记录（大量删除文件后
/// $MFT 不会收缩）直接跳过；小于 `MERGE_GAP` 的空洞照读，免得请求太碎。
fn plan_chunks(layout: &Layout, record: u64, cluster: u64, chunk: usize) -> Vec<Chunk> {
    // 判断"整片未使用"的粒度：至少 64 KB，且是簇的整数倍（无缓冲读取要求扇区对齐）。
    let granule = (64 * 1024).max(cluster).max(record);
    let per = granule / record;
    let records = layout.size / record;
    let used = |g: u64| match &layout.bitmap {
        None => true,
        Some(bm) => (g * per..((g + 1) * per).min(records)).any(|r| {
            bm.get((r / 8) as usize)
                .is_none_or(|b| b & (1 << (r % 8)) != 0)
        }),
    };
    // 需要读的 $MFT 内字节区间。
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for g in 0..records.div_ceil(per) {
        if !used(g) {
            continue;
        }
        let (start, end) = (g * granule, ((g + 1) * granule).min(layout.size));
        match ranges.last_mut() {
            Some(last) if start - last.1 <= MERGE_GAP => last.1 = end,
            _ => ranges.push((start, end)),
        }
    }
    let mut out = Vec::new();
    for (mut start, end) in ranges {
        while start < end {
            let len = ((end - start) as usize).min(chunk);
            out.push(Chunk {
                first: start / record,
                len,
                pieces: map_range(&layout.extents, start, len),
            });
            start += len as u64;
        }
    }
    out
}

/// $MFT 内 [start, start+len) 落在卷上的若干段：(卷内偏移, 块内偏移, 长度)。
fn map_range(extents: &[(u64, u64)], start: u64, len: usize) -> Vec<(u64, usize, usize)> {
    let mut pieces = Vec::new();
    let (mut pos, mut filled) = (0u64, 0usize);
    for &(off, ext_len) in extents {
        let want = start + filled as u64;
        if filled == len {
            break;
        }
        if want < pos + ext_len {
            let skip = want - pos;
            let n = ((ext_len - skip) as usize).min(len - filled);
            pieces.push((off + skip, filled, n));
            filled += n;
        }
        pos += ext_len;
    }
    pieces
}

/// 读线程：从共享的计数器领下一块，读进空闲缓冲区，交给解析线程。
/// 几个读线程同时发出请求，NVMe 在多个并发请求下才能跑满带宽（单队列深度只有一半左右）。
fn read_all(
    vol: &Volume,
    chunks: &[Chunk],
    next: &AtomicUsize,
    free: &Mutex<Receiver<AlignedBuf>>,
    out: SyncSender<(u64, usize, AlignedBuf)>,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<(), String> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some(c) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) else {
            return Ok(());
        };
        let Ok(mut buf) = free.lock().unwrap().recv() else {
            return Ok(());
        };
        for &(vol_off, buf_off, len) in &c.pieces {
            read_exact_at(
                &vol.file,
                &mut buf.as_mut()[buf_off..buf_off + len],
                vol_off,
            )?;
        }
        progress.done.fetch_add(c.len as u64, Ordering::Relaxed);
        if out.send((c.first, c.len, buf)).is_err() {
            return Ok(());
        }
    }
}

fn parse_chunk(
    buf: &mut [u8],
    record: usize,
    cluster: u64,
    first: u64,
    bitmap: Option<&[u8]>,
) -> Vec<Item> {
    let mut items = Vec::with_capacity(buf.len() / record);
    for (k, rec) in buf.chunks_exact_mut(record).enumerate() {
        let n = first + k as u64;
        // 位图说没在用的记录不看（块里合并读进来的小空洞）。
        if let Some(bm) = bitmap
            && bm
                .get((n / 8) as usize)
                .is_some_and(|b| b & (1 << (n % 8)) == 0)
        {
            continue;
        }
        // 从没用过的记录全是 0，直接跳过；撕裂或损坏的记录同样跳过。
        if rec[0] != b'F' || record::apply_fixup(rec).is_err() {
            continue;
        }
        let Ok(Some(p)) = record::parse(rec) else {
            continue;
        };
        items.push(Item {
            record: n,
            base: p.base,
            seq: p.seq,
            is_dir: p.is_dir,
            modified: p.std.map_or(0, |s| s.modified),
            attributes: p.std.map_or(0, |s| s.attributes),
            has_std: p.std.is_some(),
            names: p
                .names
                .into_iter()
                .map(|n| (n.parent, String::from_utf16_lossy(&n.name)))
                .collect(),
            dos: p.dos.map(|n| (n.parent, String::from_utf16_lossy(&n.name))),
            data: p.data,
            streams: p.streams_on_disk + p.named_clusters * cluster,
        });
    }
    items
}

/// 每个节点在建树期间的附加信息（建完即丢）。
struct Pending {
    /// 主名字的父目录引用；`UNNAMED` 表示还没见到名字。
    parent: u64,
    seq: u16,
    /// 见到了基本记录（扩展记录可能先于基本记录出现，先建占位节点）。
    seen: bool,
    streams: u64,
}

const UNNAMED: u64 = u64::MAX;

fn build(
    root: &str,
    records: u64,
    items: Receiver<Vec<Item>>,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Tree {
    let mut tree = Tree::with_capacity(root, records as usize, records as usize * 16);
    let mut pending = vec![Pending {
        parent: UNNAMED,
        seq: 0,
        seen: true,
        streams: 0,
    }];
    // 记录号 → 节点下标。
    let mut by_record = vec![NONE; records as usize];
    // 目前只见到短名的节点。几乎总会在之后的记录里等到长名，剩下的才用短名。
    let mut short_only: HashMap<u32, (u64, String)> = HashMap::new();
    if (ROOT_RECORD as usize) < by_record.len() {
        by_record[ROOT_RECORD as usize] = tree.root;
    }
    let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);

    for batch in items {
        if cancel.load(Ordering::Relaxed) {
            continue; // 排空通道，让其他线程尽快退出。
        }
        for it in batch {
            let base = it.base.unwrap_or(it.record);
            let Some(slot) = by_record.get_mut(base as usize) else {
                continue;
            };
            let node = if *slot == NONE {
                let n = tree.push(NONE, "", Entry::default());
                pending.push(Pending {
                    parent: UNNAMED,
                    seq: 0,
                    seen: false,
                    streams: 0,
                });
                *slot = n;
                n
            } else {
                *slot
            };
            let ni = node as usize;
            if it.base.is_none() {
                pending[ni].seen = true;
                pending[ni].seq = it.seq;
                let n = &mut tree.nodes[ni];
                if it.is_dir {
                    n.flags |= flags::DIR;
                    n.files = 0;
                    dirs += 1;
                } else {
                    files += 1;
                }
                if it.has_std {
                    n.mtime = it.modified;
                    if it.attributes & FA_HIDDEN != 0 {
                        n.flags |= flags::HIDDEN;
                    }
                    if it.attributes & FA_SYSTEM != 0 {
                        n.flags |= flags::SYSTEM;
                    }
                    if it.attributes & FA_REPARSE != 0 {
                        n.flags |= flags::REPARSE;
                    }
                }
                if base < FIRST_USER_RECORD && base != ROOT_RECORD {
                    n.flags |= flags::META;
                }
            }
            if let Some((size, alloc)) = it.data {
                let n = &mut tree.nodes[ni];
                n.size = size;
                n.alloc = alloc;
                bytes += size;
            }
            pending[ni].streams += it.streams;
            if base == ROOT_RECORD {
                continue; // 根目录的名字是 "."，用扫描目标代替。
            }
            if let Some(d) = it.dos
                && pending[ni].parent == UNNAMED
            {
                short_only.insert(node, d);
            }
            for (parent, name) in it.names {
                if pending[ni].parent == UNNAMED {
                    pending[ni].parent = parent;
                    tree.set_name(node, &name);
                    short_only.remove(&node);
                } else if !tree.nodes[ni].is_dir() {
                    // 硬链接的其他名字：单独成一个节点，大小记为 0，避免重复计算。
                    tree.push(
                        NONE,
                        &name,
                        Entry {
                            flags: flags::HARDLINK,
                            ..Default::default()
                        },
                    );
                    pending.push(Pending {
                        parent,
                        seq: 0,
                        seen: true,
                        streams: 0,
                    });
                }
            }
        }
        progress.files.store(files, Ordering::Relaxed);
        progress.dirs.store(dirs, Ordering::Relaxed);
        progress.bytes.store(bytes, Ordering::Relaxed);
    }
    if cancel.load(Ordering::Relaxed) {
        return tree;
    }
    progress.set_current("正在建立目录结构…".into());

    // 挂到父目录下：父记录必须存在、是目录、序列号一致（否则是已删除目录留下的孤儿）。
    for (node, (parent, name)) in short_only {
        if pending[node as usize].parent == UNNAMED {
            pending[node as usize].parent = parent;
            tree.set_name(node, &name);
        }
    }
    let mut orphans = 0u64;
    for i in 1..tree.nodes.len() {
        let p = &pending[i];
        if !p.seen || p.parent == UNNAMED {
            continue;
        }
        let parent_rec = record::ref_record(p.parent);
        let parent_seq = record::ref_seq(p.parent);
        let parent = by_record.get(parent_rec as usize).copied().unwrap_or(NONE);
        let ok = parent != NONE
            && parent as usize != i
            && tree.nodes[parent as usize].is_dir()
            && (parent_seq == 0 || pending[parent as usize].seq == parent_seq);
        if ok {
            tree.link(parent, i as u32);
        } else {
            orphans += 1;
        }
    }
    // $Extend 下的 $UsnJrnl、$ObjId 等也是元文件。
    if let Some(&ext) = by_record.get(EXTEND_RECORD as usize)
        && ext != NONE
    {
        for k in tree.bfs(ext) {
            tree.nodes[k as usize].flags |= flags::META;
        }
    }
    // 元文件的数据大多在命名流里（$UsnJrnl:$J、$Secure:$SDS），或是稀疏的（$BadClus:$Bad
    // 名义上和整个卷一样大），统一按所有流的实际占用计算。
    for (i, n) in tree.nodes.iter_mut().enumerate() {
        if n.has(flags::META) && !n.is_dir() {
            n.size = pending[i].streams;
            n.alloc = pending[i].streams;
        }
    }
    // 从根出发到达不了的节点（孤儿、没有名字的占位）标记为已删除，搜索时跳过。
    let mut reachable = vec![false; tree.nodes.len()];
    for k in tree.bfs(tree.root) {
        reachable[k as usize] = true;
    }
    for (n, r) in tree.nodes.iter_mut().zip(reachable) {
        if !r {
            n.flags |= flags::DELETED;
        }
    }
    drop(pending);
    drop(by_record);
    let _ = orphans;
    tree.finish();
    tree
}

#[cfg(test)]
mod tests {
    use super::record::tests::RecordBuilder;
    use super::record::*;
    use super::*;

    fn boot(sector: u16, spc: u8, cpr: i8, mft_lcn: u64) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&sector.to_le_bytes());
        b[0x0D] = spc;
        b[0x30..0x38].copy_from_slice(&mft_lcn.to_le_bytes());
        b[0x40] = cpr as u8;
        b
    }

    #[test]
    fn parses_boot_sector() {
        let g = parse_boot(&boot(512, 8, -10, 786432)).unwrap();
        assert_eq!(
            g,
            Geometry {
                sector: 512,
                cluster: 4096,
                record: 1024,
                mft_lcn: 786432
            }
        );
        // 4Kn 硬盘、512 KB 簇（0xF9 → 2^7 个扇区）、4 KB 记录。
        let g = parse_boot(&boot(4096, 0xF9, -12, 2)).unwrap();
        assert_eq!((g.cluster, g.record), (4096 * 128, 4096));
        assert!(parse_boot(&[0u8; 512]).is_err());
    }

    fn file_ref(record: u64, seq: u16) -> u64 {
        record | (seq as u64) << 48
    }

    #[test]
    fn reads_fragmented_mft_layout() {
        let geo = Geometry {
            sector: 512,
            cluster: 4096,
            record: 1024,
            mft_lcn: 0x100,
        };
        // 0 号记录：$DATA 的前两段（0x100 起 4 簇、0x300 起 2 簇），以及指向 20 号记录的属性列表。
        let mut list = Vec::new();
        for (vcn, rec) in [(0u64, 0u64), (6, 20)] {
            let mut e = vec![0u8; 0x20];
            e[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
            e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
            e[8..16].copy_from_slice(&vcn.to_le_bytes());
            e[0x10..0x18].copy_from_slice(&file_ref(rec, 1).to_le_bytes());
            list.extend(e);
        }
        let mut rec0 = RecordBuilder::new(RECORD_IN_USE, 1, 0)
            .resident(ATTR_ATTRIBUTE_LIST, &[], &list)
            .non_resident(
                ATTR_DATA,
                &[],
                0,
                0,
                9 * 4096,
                9 * 4096,
                9 * 4096,
                &[0x21, 0x04, 0x00, 0x01, 0x21, 0x02, 0x00, 0x02, 0x00],
            )
            .build();
        // 20 号记录：第三段（vcn 6 起，0x50 处 3 簇）。
        let ext = RecordBuilder::new(RECORD_IN_USE, 1, file_ref(0, 1))
            .non_resident(ATTR_DATA, &[], 0, 6, 0, 0, 0, &[0x11, 0x03, 0x50, 0x00])
            .build();
        let layout = mft_layout(&mut rec0, &geo, |off, len| {
            // 前一段只有 16 条记录，20 号落在第二段的第 4 条。
            assert_eq!((off, len), (0x300 * 4096 + 4 * 1024, 1024));
            Ok(ext.clone())
        })
        .unwrap();
        assert_eq!(
            layout.extents,
            vec![
                (0x100 * 4096, 4 * 4096),
                (0x300 * 4096, 2 * 4096),
                (0x50 * 4096, 3 * 4096)
            ]
        );
        assert_eq!(layout.size, 9 * 4096);
        assert_eq!(locate(&layout.extents, 1024, 16), Some(0x300 * 4096));
        assert_eq!(locate(&layout.extents, 1024, 36), None);
    }

    #[test]
    fn plans_chunks_around_unused_records() {
        // 两段：卷上 0x10000 起 128 KB、0x80000 起 1 MB。共 1152 条 1 KB 记录。
        let extents = vec![(0x10000, 128 * 1024), (0x80000, 1024 * 1024)];
        assert_eq!(
            map_range(&extents, 100 * 1024, 64 * 1024),
            vec![
                (0x10000 + 100 * 1024, 0, 28 * 1024),
                (0x80000, 28 * 1024, 36 * 1024)
            ]
        );
        // 位图：前 64 条在用，第 1000 条在用，其余全空。
        let mut bm = vec![0u8; 1152 / 8];
        bm[..8].fill(0xFF);
        bm[1000 / 8] = 1 << (1000 % 8);
        let layout = Layout {
            extents: extents.clone(),
            size: 1152 * 1024,
            bitmap: Some(bm),
        };
        let chunks = plan_chunks(&layout, 1024, 4096, 1 << 20);
        let spans: Vec<(u64, usize)> = chunks.iter().map(|c| (c.first, c.len)).collect();
        // 64 KB 粒度：第 0 片和第 960–1023 条所在的那片；两片间隔超过合并阈值。
        assert_eq!(spans, vec![(0, 64 * 1024), (960, 64 * 1024)]);
        assert_eq!(
            chunks[1].pieces,
            vec![(0x80000 + (960 - 128) * 1024, 0, 64 * 1024)]
        );
        // 没有位图：整个读，按块大小切开。
        let all = plan_chunks(
            &Layout {
                bitmap: None,
                ..layout
            },
            1024,
            4096,
            512 * 1024,
        );
        assert_eq!(all.iter().map(|c| c.len).sum::<usize>(), 1152 * 1024);
        assert_eq!(all.len(), 3);
    }

    /// 真实的 C: 盘上见过：$MFT 的属性列表本身是非常驻的。
    #[test]
    fn reads_non_resident_attribute_list() {
        let geo = Geometry {
            sector: 512,
            cluster: 4096,
            record: 1024,
            mft_lcn: 0x100,
        };
        let mut list = Vec::new();
        for (vcn, rec) in [(0u64, 0u64), (4, 3)] {
            let mut e = vec![0u8; 0x20];
            e[0..4].copy_from_slice(&ATTR_DATA.to_le_bytes());
            e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
            e[8..16].copy_from_slice(&vcn.to_le_bytes());
            e[0x10..0x18].copy_from_slice(&file_ref(rec, 1).to_le_bytes());
            list.extend(e);
        }
        let n = list.len() as u64;
        // 属性列表在 0x900 簇，1 簇长，实际 64 字节。
        let mut rec0 = RecordBuilder::new(RECORD_IN_USE, 1, 0)
            .non_resident(
                ATTR_ATTRIBUTE_LIST,
                &[],
                0,
                0,
                4096,
                n,
                4096,
                &[0x21, 0x01, 0x00, 0x09, 0x00],
            )
            .non_resident(
                ATTR_DATA,
                &[],
                0,
                0,
                6 * 4096,
                6 * 4096,
                6 * 4096,
                &[0x21, 0x04, 0x00, 0x01, 0x00],
            )
            .build();
        let ext = RecordBuilder::new(RECORD_IN_USE, 1, file_ref(0, 1))
            .non_resident(ATTR_DATA, &[], 0, 4, 0, 0, 0, &[0x11, 0x02, 0x40, 0x00])
            .build();
        let layout = mft_layout(&mut rec0, &geo, |off, len| match off {
            o if o == 0x900 * 4096 => {
                let mut b = list.clone();
                b.resize(len, 0xEE); // 实际长度之后是垃圾，必须截掉
                Ok(b)
            }
            o if o == 0x100 * 4096 + 3 * 1024 => Ok(ext.clone()),
            o => panic!("unexpected read at {o:#x}"),
        })
        .unwrap();
        assert_eq!(
            layout.extents,
            vec![(0x100 * 4096, 4 * 4096), (0x40 * 4096, 2 * 4096)]
        );
    }

    /// 用构造的记录跑完整的建树流程：硬链接、扩展记录先于基本记录、序列号不符的孤儿、元文件。
    #[test]
    fn builds_tree_from_records() {
        let mut chunk = Vec::new();
        let mut put = |rb: &mut RecordBuilder| chunk.extend(rb.build());
        let dir = RECORD_IN_USE | RECORD_IS_DIRECTORY;
        // 0 $MFT（元文件）
        put(RecordBuilder::new(RECORD_IN_USE, 1, 0)
            .file_name(file_ref(5, 5), NS_WIN32_AND_DOS, "$MFT")
            .non_resident(ATTR_DATA, &[], 0, 0, 65536, 65536, 65536, &[0x00]));
        for _ in 1..5 {
            put(&mut RecordBuilder::new(0, 1, 0));
        }
        // 5 根目录
        put(RecordBuilder::new(dir, 5, 0).std_info(9, 0).file_name(
            file_ref(5, 5),
            NS_WIN32_AND_DOS,
            ".",
        ));
        for _ in 6..11 {
            put(&mut RecordBuilder::new(0, 1, 0));
        }
        // 11 $Extend\
        put(RecordBuilder::new(dir, 11, 0).file_name(file_ref(5, 5), NS_WIN32, "$Extend"));
        for _ in 12..16 {
            put(&mut RecordBuilder::new(0, 1, 0));
        }
        // 16 docs\
        put(RecordBuilder::new(dir, 2, 0).std_info(1, 0).file_name(
            file_ref(5, 5),
            NS_WIN32,
            "docs",
        ));
        // 17 docs\a.txt，另有硬链接 b.txt 在根目录；大小在 19 号扩展记录里
        put(RecordBuilder::new(RECORD_IN_USE, 1, 0)
            .std_info(3, FA_HIDDEN)
            .file_name(file_ref(16, 2), NS_WIN32, "a.txt")
            .file_name(file_ref(16, 2), NS_DOS, "A~1.TXT")
            .file_name(file_ref(5, 5), NS_POSIX, "b.txt"));
        // 18 孤儿：父目录 16 的序列号对不上
        put(RecordBuilder::new(RECORD_IN_USE, 1, 0)
            .file_name(file_ref(16, 1), NS_WIN32, "stale.txt")
            .resident(ATTR_DATA, &[], b"xx"));
        // 19 17 号的扩展记录
        put(
            RecordBuilder::new(RECORD_IN_USE, 1, file_ref(17, 1)).non_resident(
                ATTR_DATA,
                &[],
                0,
                0,
                8192,
                5000,
                8192,
                &[0x00],
            ),
        );
        // 20 已删除的文件
        put(RecordBuilder::new(0, 1, 0).file_name(file_ref(5, 5), NS_WIN32, "gone.txt"));
        // 21 $Extend\$UsnJrnl（数据在稀疏的命名流 $J 里）
        let j: Vec<u16> = "$J".encode_utf16().collect();
        put(RecordBuilder::new(RECORD_IN_USE, 1, 0)
            .file_name(file_ref(11, 11), NS_WIN32, "$UsnJrnl")
            .non_resident(
                ATTR_DATA,
                &j,
                0,
                0,
                1 << 30,
                1 << 30,
                1 << 30,
                // 64 簇稀疏 + 0x20 簇处 16 簇实际数据。
                &[0x01, 0x40, 0x11, 0x10, 0x20, 0x00],
            ));
        // 22 extra.bin 的扩展记录先于它的基本记录（23）出现
        put(RecordBuilder::new(RECORD_IN_USE, 1, file_ref(23, 1)).resident(ATTR_DATA, &[], b"123"));
        put(RecordBuilder::new(RECORD_IN_USE, 1, 0).file_name(
            file_ref(16, 2),
            NS_WIN32,
            "extra.bin",
        ));
        // 24 基本记录里只有短名，长名在扩展记录 25 里；26 自始至终只有短名
        put(RecordBuilder::new(RECORD_IN_USE, 1, 0).file_name(
            file_ref(16, 2),
            NS_DOS,
            "LONGNA~1.TXT",
        ));
        put(
            RecordBuilder::new(RECORD_IN_USE, 1, file_ref(24, 1)).file_name(
                file_ref(16, 2),
                NS_WIN32,
                "long name.txt",
            ),
        );
        put(RecordBuilder::new(RECORD_IN_USE, 1, 0).file_name(
            file_ref(16, 2),
            NS_DOS,
            "ONLY~1.TXT",
        ));
        let records = chunk.len() as u64 / 1024;

        let items = parse_chunk(&mut chunk, 1024, 4096, 0, None);
        let (tx, rx) = sync_channel(1);
        tx.send(items).unwrap();
        drop(tx);
        let t = build(
            r"X:\",
            records,
            rx,
            &Progress::default(),
            &AtomicBool::new(false),
        );

        let a = t.find(r"X:\docs\a.txt").unwrap();
        assert_eq!((t.node(a).size, t.node(a).alloc), (5000, 8192));
        assert!(t.node(a).has(flags::HIDDEN));
        let b = t.find(r"X:\b.txt").unwrap();
        assert!(t.node(b).has(flags::HARDLINK));
        assert_eq!(t.node(b).size, 0);
        assert_eq!(t.node(t.find(r"X:\docs\extra.bin").unwrap()).size, 3);
        assert_eq!(t.find(r"X:\docs\stale.txt"), None);
        assert_eq!(t.find(r"X:\gone.txt"), None);
        let mft = t.find(r"X:\$MFT").unwrap();
        assert!(t.node(mft).has(flags::META));
        let usn = t.find(r"X:\$Extend\$UsnJrnl").unwrap();
        assert!(t.node(usn).has(flags::META));
        assert_eq!(
            t.node(usn).size,
            16 * 4096,
            "稀疏流按运行表里实际分配的簇计"
        );
        let long = t.find(r"X:\docs\long name.txt").unwrap();
        assert!(!t.node(long).has(flags::HARDLINK));
        assert_eq!(
            t.find(r"X:\docs\LONGNA~1.TXT"),
            None,
            "短名不能变成多余的硬链接"
        );
        assert!(t.find(r"X:\docs\ONLY~1.TXT").is_some());
        let docs = t.find(r"X:\docs").unwrap();
        assert_eq!(t.node(docs).size, 5003);
        assert_eq!(t.node(docs).files, 4);
        assert_eq!(t.node(t.root).size, 5003 + 65536 + 16 * 4096);
        assert_eq!(t.node(t.root).mtime, 9);
    }
}
