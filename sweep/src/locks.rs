//! 查哪些进程正在占用文件或文件夹，以及结束它们。
//!
//! Restart Manager 能查到打开着文件、加载着 DLL / exe 的进程；工作目录在文件夹里的进程
//! （比如在里面开着的终端）它查不到，但同样会让文件夹删不掉，所以另外读各进程的工作目录补上。
//! Pulse 与 Sweep 共用这一份文件，只依赖 windows crate。

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmCritical, RmEndSession, RmExplorer, RmGetList,
    RmRegisterResources, RmStartSession,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, PROCESS_VM_READ, TerminateProcess,
};
use windows::core::{PCWSTR, PWSTR};

/// 文件夹最多检查这么多个文件。
const MAX_FILES: usize = 50_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// 打开着其中的文件，或加载着其中的 DLL / exe。
    InUse,
    /// 工作目录在文件夹里。
    WorkingDirectory,
}

#[derive(Clone, Debug)]
pub struct Locker {
    pub pid: u32,
    /// 进程的创建时间（FILETIME），结束前核对，避免 PID 被复用后误杀别的进程。
    pub start: u64,
    /// 应用名（Restart Manager 给出的友好名称）或 exe 名。
    pub name: String,
    /// 服务进程的服务名。
    pub service: Option<String>,
    pub reason: Reason,
    /// 不提供结束：系统关键进程（结束会让系统不稳定），以及资源管理器（结束会让任务栏和桌面重启）。
    pub protected: bool,
}

#[derive(Default, Debug)]
pub struct Report {
    pub lockers: Vec<Locker>,
    /// 文件夹里的文件太多，只查了前 MAX_FILES 个。
    pub truncated: bool,
}

/// 查找占用这些文件 / 文件夹的进程（不含本进程）。会遍历文件夹、打开各进程，放在后台线程调用。
pub fn find(paths: &[PathBuf]) -> Report {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    let mut truncated = false;
    for p in paths {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => {
                dirs.push(p.clone());
                truncated |= collect_files(p, &mut files);
            }
            Ok(_) => files.push(p.clone()),
            Err(_) => {}
        }
    }
    let me = std::process::id();
    // Restart Manager 每个文件要约 0.3 ms；先（多线程）试着独占打开，能打开的文件没人占用，不必再查。
    let files = possibly_locked(files);
    let mut lockers: Vec<Locker> = restart_manager(&files)
        .into_iter()
        .filter(|l| l.pid != me)
        .collect();
    if !dirs.is_empty() {
        for (pid, name) in processes() {
            if pid == me || lockers.iter().any(|l| l.pid == pid) {
                continue;
            }
            let Some(cwd) = current_directory(pid) else {
                continue;
            };
            if dirs.iter().any(|d| inside(Path::new(&cwd), d)) {
                lockers.push(Locker {
                    pid,
                    start: start_time(pid).unwrap_or(0),
                    protected: name.eq_ignore_ascii_case("explorer.exe"),
                    name,
                    service: None,
                    reason: Reason::WorkingDirectory,
                });
            }
        }
    }
    lockers.sort_by_key(|l| l.name.to_lowercase());
    Report { lockers, truncated }
}

/// 结束进程。先核对创建时间：查询之后进程已经退出、PID 被别的进程复用时不动它。
pub fn end(l: &Locker) -> windows::core::Result<()> {
    let h = unsafe {
        OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            l.pid,
        )
    }?;
    let h = unsafe { OwnedHandle::from_raw_handle(h.0) };
    let raw = HANDLE(h.as_raw_handle());
    if l.start != 0 && process_start(raw) != Some(l.start) {
        // 原来的进程已经不在了，相当于已经结束。
        return Ok(());
    }
    unsafe { TerminateProcess(raw, 1) }
}

/// 进程的当前工作目录，从 PEB 的 RTL_USER_PROCESS_PARAMETERS.CurrentDirectory 读出。
/// 需要读进程内存的权限；32 位（WOW64）进程的 PEB 布局不同，不支持。
pub fn current_directory(pid: u32) -> Option<String> {
    use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Threading::{IsWow64Process, PROCESS_BASIC_INFORMATION};

    let h = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) }.ok()?;
    let owned = unsafe { OwnedHandle::from_raw_handle(h.0) };
    let h = HANDLE(owned.as_raw_handle());
    let mut wow = windows::core::BOOL(0);
    if unsafe { IsWow64Process(h, &mut wow) }.is_err() || wow.as_bool() {
        return None;
    }
    let mut info = PROCESS_BASIC_INFORMATION::default();
    let mut len = 0u32;
    unsafe {
        NtQueryInformationProcess(
            h,
            ProcessBasicInformation,
            (&mut info as *mut PROCESS_BASIC_INFORMATION).cast(),
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            &mut len,
        )
    }
    .ok()
    .ok()?;
    let read = |addr: usize, buf: &mut [u8]| {
        unsafe { ReadProcessMemory(h, addr as _, buf.as_mut_ptr().cast(), buf.len(), None) }.is_ok()
    };
    // PEB+0x20 是 ProcessParameters；其中 +0x38 是 CurrentDirectory.DosPath（UNICODE_STRING）。
    let mut ptr = [0u8; 8];
    read(info.PebBaseAddress as usize + 0x20, &mut ptr).then_some(())?;
    let params = usize::from_le_bytes(ptr);
    let mut ustr = [0u8; 16];
    read(params + 0x38, &mut ustr).then_some(())?;
    let bytes = u16::from_le_bytes([ustr[0], ustr[1]]) as usize;
    let buf = usize::from_le_bytes(ustr[8..16].try_into().ok()?);
    if bytes == 0 || bytes > 4096 || buf == 0 {
        return None;
    }
    let mut raw = vec![0u8; bytes];
    read(buf, &mut raw).then_some(())?;
    let wide: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Some(String::from_utf16_lossy(&wide))
}

/// 筛出可能被占用的文件（无法独占打开的），分给多个线程做。
fn possibly_locked(files: Vec<PathBuf>) -> Vec<PathBuf> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let chunk = files.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        let workers: Vec<_> = files
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .filter(|f| !openable_exclusively(f))
                        .cloned()
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_default())
            .collect()
    })
}

/// 能否以不共享的方式打开（申请写权限但不写入，立即关闭）。
/// 别的进程开着它时会因共享冲突失败；作为 DLL / exe 映射着的文件不能以写方式打开，同样失败
/// （只申请删除权限是可以的，所以正在运行的 exe 能被改名）。没有权限等其他失败也算"可能被占用"。
fn openable_exclusively(path: &Path) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_NONE, FILE_WRITE_DATA,
        OPEN_EXISTING,
    };
    let name = wide(path);
    match unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_WRITE_DATA.0 | DELETE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    } {
        Ok(h) => {
            unsafe {
                let _ = CloseHandle(h);
            }
            true
        }
        Err(_) => false,
    }
}

/// 递归收集文件夹里的文件（不跟随链接）。超过上限时返回 true。
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() {
                if out.len() >= MAX_FILES {
                    return true;
                }
                out.push(e.path());
            }
        }
    }
    false
}

/// `path` 是否就是 `dir` 或在它里面（不区分大小写）。
fn inside(path: &Path, dir: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy().to_lowercase().replace('/', "\\");
        format!("{}\\", s.trim_end_matches('\\'))
    };
    norm(path).starts_with(&norm(dir))
}

fn wide(s: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn utf16(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn ft(t: FILETIME) -> u64 {
    (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64
}

fn restart_manager(files: &[PathBuf]) -> Vec<Locker> {
    if files.is_empty() {
        return Vec::new();
    }
    let mut session = 0u32;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    if unsafe { RmStartSession(&mut session, None, PWSTR(key.as_mut_ptr())) } != ERROR_SUCCESS {
        return Vec::new();
    }
    let names: Vec<Vec<u16>> = files.iter().map(|f| wide(f)).collect();
    let ptrs: Vec<PCWSTR> = names.iter().map(|n| PCWSTR(n.as_ptr())).collect();
    let mut out = Vec::new();
    // 分批登记，单次传入几万个路径时 Restart Manager 会失败。
    let registered = ptrs.chunks(1000).all(|chunk| {
        let r = unsafe { RmRegisterResources(session, Some(chunk), None, None) };
        r == ERROR_SUCCESS
    });
    if registered {
        let mut infos: Vec<RM_PROCESS_INFO> = Vec::new();
        loop {
            let mut needed = 0u32;
            let mut count = infos.len() as u32;
            let mut reasons = 0u32;
            let ptr = if infos.is_empty() {
                None
            } else {
                Some(infos.as_mut_ptr())
            };
            let r = unsafe { RmGetList(session, &mut needed, &mut count, ptr, &mut reasons) };
            if r == ERROR_MORE_DATA {
                infos = vec![RM_PROCESS_INFO::default(); needed as usize + 4];
                continue;
            }
            if r == ERROR_SUCCESS {
                infos.truncate(count as usize);
                out = infos
                    .iter()
                    .map(|i| {
                        let service = utf16(&i.strServiceShortName);
                        Locker {
                            pid: i.Process.dwProcessId,
                            start: ft(i.Process.ProcessStartTime),
                            name: utf16(&i.strAppName),
                            service: (!service.is_empty()).then_some(service),
                            reason: Reason::InUse,
                            protected: i.ApplicationType == RmCritical
                                || i.ApplicationType == RmExplorer,
                        }
                    })
                    .collect();
            }
            break;
        }
    }
    unsafe {
        let _ = RmEndSession(session);
    }
    out
}

/// 所有进程的 (PID, exe 名)。
fn processes() -> Vec<(u32, String)> {
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    let snap_owned = unsafe { OwnedHandle::from_raw_handle(snap.0) };
    let snap = HANDLE(snap_owned.as_raw_handle());
    let mut out = Vec::new();
    let mut e = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut ok = unsafe { Process32FirstW(snap, &mut e) }.is_ok();
    while ok {
        if e.th32ProcessID > 4 {
            out.push((e.th32ProcessID, utf16(&e.szExeFile)));
        }
        ok = unsafe { Process32NextW(snap, &mut e) }.is_ok();
    }
    out
}

fn process_start(h: HANDLE) -> Option<u64> {
    let (mut c, mut x, mut k, mut u) = Default::default();
    unsafe { GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u) }.ok()?;
    Some(ft(c))
}

fn start_time(pid: u32) -> Option<u64> {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let h = unsafe { OwnedHandle::from_raw_handle(h.0) };
    process_start(HANDLE(h.as_raw_handle()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_is_case_insensitive_and_component_aware() {
        assert!(inside(
            Path::new(r"E:\Code\Pulse\"),
            Path::new(r"e:\code\pulse")
        ));
        assert!(inside(
            Path::new(r"E:\code\pulse\src"),
            Path::new(r"E:\code\pulse\")
        ));
        assert!(!inside(
            Path::new(r"E:\code\pulse2"),
            Path::new(r"E:\code\pulse")
        ));
    }

    /// 自己打开一个临时文件，再查它：应当查到本进程——但 find 会排除本进程，
    /// 所以这里直接调用 Restart Manager 那一步。
    #[test]
    fn finds_own_open_file() {
        let dir = std::env::temp_dir().join(format!("locks-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("held.txt");
        let held = std::fs::File::create(&file).unwrap();
        assert!(!openable_exclusively(&file));
        let found = restart_manager(std::slice::from_ref(&file));
        assert!(found.iter().any(|l| l.pid == std::process::id()));
        drop(held);
        assert!(openable_exclusively(&file));
        let found = restart_manager(std::slice::from_ref(&file));
        assert!(!found.iter().any(|l| l.pid == std::process::id()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
