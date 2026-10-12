//! 系统信息：卷列表与容量、文件系统类型、管理员权限、进程内存。

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows::Win32::Foundation::{HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    SE_BACKUP_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TokenElevation,
};
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PCWSTR;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let token = OwnedHandle::from_raw_handle(token.0);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0;
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok()
            && elevation.TokenIsElevated != 0
    }
}

/// 打开 SeBackupPrivilege（管理员令牌里有但默认关闭）。配合 FILE_FLAG_BACKUP_SEMANTICS
/// 可以列出 ACL 不允许访问的目录，只读，不改变任何文件。
pub fn enable_backup_privilege() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .is_err()
        {
            return false;
        }
        let token = OwnedHandle::from_raw_handle(token.0);
        let mut luid = LUID::default();
        if LookupPrivilegeValueW(PCWSTR::null(), SE_BACKUP_NAME, &mut luid).is_err() {
            return false;
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        AdjustTokenPrivileges(
            HANDLE(token.as_raw_handle()),
            false,
            Some(&tp),
            0,
            None,
            None,
        )
        .is_ok()
    }
}

/// 卷上的文件系统名（"NTFS"、"exFAT"、"ReFS"…）。`root` 如 `C:\`。
pub fn fs_name(root: &str) -> Option<String> {
    let mut fs = [0u16; 32];
    let w = wide(root);
    unsafe { GetVolumeInformationW(PCWSTR(w.as_ptr()), None, None, None, None, Some(&mut fs)) }
        .ok()?;
    Some(from_wide(&fs))
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Optical,
    Other,
}

#[derive(Clone, Debug)]
pub struct Volume {
    /// 如 `C:\`。
    pub root: String,
    pub label: String,
    pub fs: String,
    pub kind: DriveKind,
    pub total: u64,
    pub free: u64,
}

impl Volume {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
}

/// 所有可用的卷。光驱没盘、网络盘断开等读不到信息的会被跳过。
/// 可能在网络盘上卡住几秒，调用方应放在后台线程。
pub fn volumes() -> Vec<Volume> {
    let mut buf = [0u16; 512];
    let n = unsafe { GetLogicalDriveStringsW(Some(&mut buf)) } as usize;
    buf[..n.min(buf.len())]
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| volume(&String::from_utf16_lossy(s)))
        .collect()
}

pub fn volume(root: &str) -> Option<Volume> {
    let w = wide(root);
    let kind = match unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) } {
        2 => DriveKind::Removable,
        3 => DriveKind::Fixed,
        4 => DriveKind::Network,
        5 => DriveKind::Optical,
        _ => DriveKind::Other,
    };
    let mut label = [0u16; 256];
    let mut fs = [0u16; 32];
    unsafe {
        GetVolumeInformationW(
            PCWSTR(w.as_ptr()),
            Some(&mut label),
            None,
            None,
            None,
            Some(&mut fs),
        )
    }
    .ok()?;
    let (mut total, mut free) = (0u64, 0u64);
    unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), None, Some(&mut total), Some(&mut free)) }
        .ok()?;
    Some(Volume {
        root: root.to_string(),
        label: from_wide(&label),
        fs: from_wide(&fs),
        kind,
        total,
        free,
    })
}

/// (当前工作集, 峰值工作集, 提交的私有内存)，字节。
pub fn memory() -> (u64, u64, u64) {
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb);
    }
    (
        c.WorkingSetSize as u64,
        c.PeakWorkingSetSize as u64,
        c.PagefileUsage as u64,
    )
}
