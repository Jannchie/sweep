//! NTFS 变更日志（USN Journal）：卷上每次创建、删除、改名、写入文件都会追加一条记录。
//!
//! 增量扫描用它找出自上次扫描以来内容变过的目录：每条记录带着文件所在目录的引用号，
//! 收集这些目录、换算成路径，只重新列出它们。需要管理员权限（打开卷句柄）。
//!
//! 日志大小有上限（系统盘默认 32 MB），写满后最旧的记录被丢弃；上次的位置已被丢弃、
//! 或日志被删除重建（ID 变化）时，无法得知中间发生了什么，只能完整扫描。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, GENERIC_READ,
    HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAGS_AND_ATTRIBUTES, FILE_ID_DESCRIPTOR,
    FILE_ID_DESCRIPTOR_0, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdType, GETFINALPATHNAMEBYHANDLE_FLAGS,
    GetFinalPathNameByHandleW, OPEN_EXISTING, OpenFileById, VOLUME_NAME_DOS,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V0,
};
use windows::core::PCWSTR;

/// 规范化的长路径、带盘符：FILE_NAME_NORMALIZED | VOLUME_NAME_DOS（两者都是 0，类型不支持 `|`）。
const FINAL_PATH: GETFINALPATHNAMEBYHANDLE_FLAGS = FILE_NAME_NORMALIZED;
const _: () = assert!(VOLUME_NAME_DOS.0 == 0);

/// 卷句柄（`\\.\C:`），关闭时释放。
pub struct Volume(HANDLE);

impl Drop for Volume {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// 日志的当前状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Journal {
    /// 日志 ID：删除重建日志、重新格式化卷都会变。
    pub id: u64,
    /// 还保留着的最旧记录的位置。
    pub first: i64,
    /// 下一条记录将写入的位置。
    pub next: i64,
}

/// 读取变更失败的原因。
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// 上次的位置已不在日志里（被覆盖、日志重建），只能完整扫描。
    Stale,
    Cancelled,
    Io(String),
}

impl Volume {
    /// 打开 `letter` 盘的卷句柄。需要管理员权限。
    pub fn open(letter: char) -> Result<Self, String> {
        let path: Vec<u16> = format!(r"\\.\{letter}:")
            .encode_utf16()
            .chain([0])
            .collect();
        let h = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .map_err(|e| e.message())?;
        Ok(Self(h))
    }

    /// 日志的当前状态；卷上没有启用日志时返回 Err。
    pub fn journal(&self) -> Result<Journal, String> {
        let mut data = USN_JOURNAL_DATA_V0::default();
        let mut got = 0u32;
        unsafe {
            DeviceIoControl(
                self.0,
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some((&mut data as *mut USN_JOURNAL_DATA_V0).cast()),
                size_of::<USN_JOURNAL_DATA_V0>() as u32,
                Some(&mut got),
                None,
            )
        }
        .map_err(|e| e.message())?;
        Ok(Journal {
            id: data.UsnJournalID,
            first: data.FirstUsn,
            next: data.NextUsn,
        })
    }

    /// 读出 `[from, until)` 之间的记录，返回它们所在目录的文件引用号（去重）。
    /// `from` 是上次保存的位置，`journal` 是刚查到的当前状态。
    pub fn changed_dirs(
        &self,
        journal: &Journal,
        saved_id: u64,
        from: i64,
        cancel: &AtomicBool,
    ) -> Result<HashSet<u64>, Error> {
        if saved_id != journal.id || from < journal.first || from > journal.next {
            return Err(Error::Stale);
        }
        let mut dirs = HashSet::new();
        // 以 u64 计，保证记录按 8 字节对齐读取。
        let mut buf = vec![0u64; 128 * 1024];
        let mut start = from;
        while start < journal.next {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let req = READ_USN_JOURNAL_DATA_V1 {
                StartUsn: start,
                ReasonMask: u32::MAX,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: journal.id,
                MinMajorVersion: 2,
                MaxMajorVersion: 3,
            };
            let mut got = 0u32;
            let r = unsafe {
                DeviceIoControl(
                    self.0,
                    FSCTL_READ_USN_JOURNAL,
                    Some((&req as *const READ_USN_JOURNAL_DATA_V1).cast()),
                    size_of::<READ_USN_JOURNAL_DATA_V1>() as u32,
                    Some(buf.as_mut_ptr().cast()),
                    (buf.len() * 8) as u32,
                    Some(&mut got),
                    None,
                )
            };
            if let Err(e) = r {
                let stale = [
                    ERROR_JOURNAL_ENTRY_DELETED,
                    ERROR_JOURNAL_DELETE_IN_PROGRESS,
                ]
                .iter()
                .any(|c| e.code() == c.to_hresult());
                return Err(if stale {
                    Error::Stale
                } else {
                    Error::Io(e.message())
                });
            }
            let bytes =
                unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), got as usize) };
            let Some(next) = parse_batch(bytes, journal.next, &mut dirs) else {
                break;
            };
            if next <= start {
                break;
            }
            start = next;
        }
        Ok(dirs)
    }

    /// 按文件引用号打开目录并取得它现在的路径（如 `C:\Windows\Temp`）。已被删除时返回 None。
    pub fn path_of(&self, frn: u64) -> Option<String> {
        let id = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 },
        };
        let h = unsafe {
            OpenFileById(
                self.0,
                &id,
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        }
        .ok()?;
        let mut buf = vec![0u16; 1024];
        let mut n = unsafe { GetFinalPathNameByHandleW(h, &mut buf, FINAL_PATH) } as usize;
        if n > buf.len() {
            buf.resize(n + 1, 0);
            n = unsafe { GetFinalPathNameByHandleW(h, &mut buf, FINAL_PATH) } as usize;
        }
        unsafe {
            let _ = CloseHandle(h);
        }
        if n == 0 || n > buf.len() {
            return None;
        }
        Some(strip_verbatim(&String::from_utf16_lossy(&buf[..n])))
    }
}

/// `\\?\C:\x` → `C:\x`；`\\?\UNC\srv\share` → `\\srv\share`。
fn strip_verbatim(p: &str) -> String {
    if let Some(unc) = p.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        p.strip_prefix(r"\\?\").unwrap_or(p).to_string()
    }
}

/// 解析一批 FSCTL_READ_USN_JOURNAL 的输出：开头 8 字节是下一批的起点，之后是若干条记录。
/// 把 `until` 之前的记录所在目录放进 `dirs`，返回下一批的起点；已经读到 `until` 时返回 None。
fn parse_batch(bytes: &[u8], until: i64, dirs: &mut HashSet<u64>) -> Option<i64> {
    let next = i64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
    let mut off = 8;
    let mut any = false;
    while off + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[off..off + 4].try_into().ok()?) as usize;
        if len < 8 || off + len > bytes.len() {
            break;
        }
        let rec = &bytes[off..off + len];
        let major = u16::from_le_bytes(rec[4..6].try_into().ok()?);
        // V2：父引用号在 16，USN 在 24。V3（ReFS 等，128 位引用号）：父引用在 24，USN 在 40；
        // NTFS 上 128 位引用号的高 64 位为 0，取低 64 位即可。
        let (parent, usn) = match major {
            2 if len >= 32 => (u64_at(rec, 16), i64_at(rec, 24)),
            3 if len >= 48 => (u64_at(rec, 24), i64_at(rec, 40)),
            _ => {
                off += len;
                continue;
            }
        };
        if usn >= until {
            return None;
        }
        dirs.insert(parent);
        any = true;
        off += len;
    }
    // 没有任何记录：已经到达日志末尾。
    (any || next > 0).then_some(next)
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn i64_at(b: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(parent: u64, usn: i64) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[0..4].copy_from_slice(&64u32.to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&7u64.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[24..32].copy_from_slice(&usn.to_le_bytes());
        r
    }

    #[test]
    fn parses_records_until_limit() {
        let mut b = 300i64.to_le_bytes().to_vec();
        b.extend(v2(5, 100));
        b.extend(v2(9, 150));
        b.extend(v2(5, 200));
        let mut dirs = HashSet::new();
        assert_eq!(parse_batch(&b, 1000, &mut dirs), Some(300));
        assert_eq!(dirs, HashSet::from([5, 9]));

        // 到达读取开始时记下的末尾就停下，之后的记录留给下次。
        let mut dirs = HashSet::new();
        assert_eq!(parse_batch(&b, 150, &mut dirs), None);
        assert_eq!(dirs, HashSet::from([5]));
    }

    #[test]
    fn strips_verbatim_prefix() {
        assert_eq!(strip_verbatim(r"\\?\C:\Windows"), r"C:\Windows");
        assert_eq!(strip_verbatim(r"\\?\C:\"), r"C:\");
        assert_eq!(strip_verbatim(r"\\?\UNC\nas\share"), r"\\nas\share");
    }

    /// 读真实的系统盘日志（需要管理员）：`cargo test -p sweep-scan -- --ignored live_journal --nocapture`。
    #[test]
    #[ignore]
    fn live_journal() {
        let v = Volume::open('C').unwrap();
        let j = v.journal().unwrap();
        let from = (j.next - (1 << 20)).max(j.first);
        let t = std::time::Instant::now();
        let dirs = v
            .changed_dirs(&j, j.id, from, &AtomicBool::new(false))
            .unwrap();
        let read = t.elapsed();
        let paths: Vec<String> = dirs.iter().filter_map(|&d| v.path_of(d)).collect();
        println!(
            "{} dirs in the last 1 MiB of the journal (read {:?}, resolved {} in {:?})",
            dirs.len(),
            read,
            paths.len(),
            t.elapsed() - read
        );
        for p in paths.iter().take(10) {
            println!("  {p}");
        }
        assert_eq!(
            v.changed_dirs(&j, j.id ^ 1, from, &AtomicBool::new(false)),
            Err(Error::Stale)
        );
    }
}
