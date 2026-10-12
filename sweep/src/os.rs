//! 系统操作：在资源管理器中定位、属性、回收站 / 永久删除、选择文件夹、以管理员身份重启。
//! 这些调用可能很慢（删除大文件夹、网络盘）或弹出系统对话框，除了几个只发消息的函数，
//! 都应在后台线程调用。

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree,
};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_WANTNUKEWARNING, FOFX_RECYCLEONDELETE,
    FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, FileOperation, IFileOpenDialog,
    IFileOperation, ILCreateFromPathW, ILFree, IShellItem, SHCreateItemFromParsingName,
    SHERB_NOCONFIRMATION, SHERB_NOSOUND, SHEmptyRecycleBinW, SHOP_FILEPATH, SHObjectProperties,
    SHOpenFolderAndSelectItems, SHQUERYRBINFO, SHQueryRecycleBinW, SIGDN_FILESYSPATH,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn owner() -> HWND {
    HWND(crate::titlebar::hwnd() as *mut _)
}

/// 本线程的 COM 初始化（单线程套间，Shell 接口要求）。重复调用无害。
fn com() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    }
}

/// 在资源管理器中定位（选中）这一项。在后台线程里调用 Shell，不阻塞界面。
///
/// 不用 `explorer /select,路径`：路径里有空格时参数会被整个加上引号，逗号也会把路径截断，
/// 资源管理器解析失败就退回到默认位置。卷根目录没有"所在文件夹"，直接打开它；
/// 无法解析的项（例如已被删除）退而打开它所在的文件夹。
pub fn reveal(path: &str) {
    let path = path.to_string();
    std::thread::spawn(move || reveal_now(&path));
}

fn reveal_now(path: &str) {
    com();
    let root = sweep_scan::is_volume_root(path);
    if root || !select_in_folder(path) {
        let folder = if root || std::path::Path::new(path).is_dir() {
            path
        } else {
            parent(path)
        };
        open(folder);
    }
}

/// `C:\a\b.txt` → `C:\a`，`C:\a` → `C:\`。
fn parent(path: &str) -> &str {
    match path.trim_end_matches('\\').rfind('\\') {
        Some(i) if i <= 2 => &path[..i + 1],
        Some(i) => &path[..i],
        None => path,
    }
}

/// 打开所在文件夹并选中这一项。
fn select_in_folder(path: &str) -> bool {
    let p = wide(path);
    unsafe {
        let pidl = ILCreateFromPathW(PCWSTR(p.as_ptr()));
        if pidl.is_null() {
            return false;
        }
        let ok = SHOpenFolderAndSelectItems(pidl, None, 0).is_ok();
        ILFree(Some(pidl));
        ok
    }
}

/// 用默认方式打开（文件夹在资源管理器里打开）。
pub fn open(path: &str) {
    let p = wide(path);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(p.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

/// 系统的"属性"对话框（非模态，立即返回）。
pub fn properties(path: &str) {
    let p = wide(path);
    unsafe {
        let _ = SHObjectProperties(
            Some(owner()),
            SHOP_FILEPATH,
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
        );
    }
}

/// 删除一项：`permanent` 为 false 时移到回收站。每项单独一个 IFileOperation：
/// 一批里某一项失败（文件被占用）不会连累其他项。
/// 回收站放不下（文件太大、网络盘）时系统会询问是否改为永久删除（FOF_WANTNUKEWARNING），
/// 不会悄悄地永久删掉。返回删除后它是否确实已不存在，以及失败原因。
pub fn delete(path: &str, permanent: bool) -> Result<(), String> {
    com();
    let p = wide(path);
    let result = unsafe {
        (|| -> windows::core::Result<()> {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            let flags = if permanent {
                FOF_NOCONFIRMATION | FOF_NOERRORUI
            } else {
                FOF_ALLOWUNDO
                    | FOF_NOCONFIRMATION
                    | FOF_NOERRORUI
                    | FOF_WANTNUKEWARNING
                    | FOFX_RECYCLEONDELETE
            };
            op.SetOperationFlags(flags)?;
            let _ = op.SetOwnerWindow(owner());
            let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(p.as_ptr()), None)?;
            op.DeleteItem(&item, None)?;
            op.PerformOperations()?;
            if op.GetAnyOperationsAborted()?.as_bool() {
                return Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_ABORT,
                ));
            }
            Ok(())
        })()
    };
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    // 被占用、没有权限时 Shell 只报"操作已中止"，换成说得清楚的话。
    Err(match result {
        Err(e) if e.code() != windows::Win32::Foundation::E_ABORT => error_text(&e),
        _ => "部分内容正被其他程序使用或没有权限，未能全部删除".into(),
    })
}

pub fn error_text(e: &windows::core::Error) -> String {
    let msg = e.message().trim().trim_end_matches('。').to_string();
    if msg.is_empty() {
        format!("错误 0x{:08X}", e.code().0)
    } else {
        msg
    }
}

/// 所有驱动器回收站的合计：(字节数, 项数)。
pub fn recycle_bin() -> Option<(u64, u64)> {
    let mut info = SHQUERYRBINFO {
        cbSize: size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info) }.ok()?;
    Some((info.i64Size.max(0) as u64, info.i64NumItems.max(0) as u64))
}

/// 清空所有驱动器的回收站（调用方已经确认过）。
pub fn empty_recycle_bin() -> Result<(), String> {
    unsafe {
        SHEmptyRecycleBinW(
            Some(owner()),
            PCWSTR::null(),
            SHERB_NOCONFIRMATION | SHERB_NOSOUND,
        )
    }
    .map_err(|e| error_text(&e))
}

/// 系统的"选择文件夹"对话框。阻塞到用户选完，放在后台线程调用。
pub fn pick_folder() -> Option<String> {
    com();
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;
        dlg.SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = dlg.SetTitle(w!("选择要分析的文件夹"));
        dlg.Show(Some(owner())).ok()?;
        let item = dlg.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        s
    }
}

/// 以管理员身份重新启动自己，`args` 原样传给新进程。成功返回 true，调用方随后应退出。
pub fn relaunch_elevated(args: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = wide(&exe.to_string_lossy());
    let args = wide(args);
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(exe.as_ptr()),
            PCWSTR(args.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    // 返回值 > 32 表示成功。
    r.0 as usize > 32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_folders() {
        assert_eq!(parent(r"C:\a\b.txt"), r"C:\a");
        assert_eq!(parent(r"C:\a"), r"C:\");
        assert_eq!(parent(r"C:\a b,c\d"), r"C:\a b,c");
        assert_eq!(parent(r"\\nas\share\x.iso"), r"\\nas\share");
    }

    /// 只在自己建的临时目录里试永久删除（回收站删除会在用户的回收站里留下条目，不放进单元测试）。
    #[test]
    fn deletes_inside_temp() {
        let base = std::env::temp_dir().join(format!("sweep-test-os-{}", std::process::id()));
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub/a.txt"), b"hello").unwrap();
        std::fs::write(base.join("b.txt"), b"bye").unwrap();
        let sub = base.join("sub").to_string_lossy().to_string();
        let b = base.join("b.txt").to_string_lossy().to_string();
        assert_eq!(delete(&sub, true), Ok(()));
        assert!(!base.join("sub").exists());
        assert_eq!(delete(&b, true), Ok(()));
        assert!(!base.join("b.txt").exists());
        assert!(delete(&b, true).is_ok(), "已经不存在也算成功");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
