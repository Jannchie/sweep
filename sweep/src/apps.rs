//! 已安装的程序：读取、测量大小、卸载。
//!
//! - 桌面程序：注册表的卸载信息（所有用户的 64 / 32 位视图，以及当前用户），与"设置 → 应用"同源；
//! - 商店 / MSIX 应用：WinRT 的 PackageManager（只列当前用户的主包，不含框架与系统组件）。
//!
//! 快速卸载：能确定静默参数的直接静默运行（程序自己登记的静默命令、MSI、Inno Setup、NSIS），
//! 商店应用直接移除；其余打开程序自己的卸载向导。卸载完以注册表项 / 包是否消失为准。
//! 这些调用都可能很慢或弹出 UAC，全部在后台线程里做。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sweep_scan::{t, tf};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY,
    REG_SAM_FLAGS, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW,
    RegOpenKeyExW,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";

/// 程序登记在哪里。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// HKLM，64 位视图。
    Machine,
    /// HKLM，32 位视图（WOW6432Node）。
    Machine32,
    /// HKCU（只为当前用户安装）。
    User,
}

impl Scope {
    fn hive(self) -> HKEY {
        match self {
            Scope::User => HKEY_CURRENT_USER,
            _ => HKEY_LOCAL_MACHINE,
        }
    }

    fn view(self) -> REG_SAM_FLAGS {
        match self {
            Scope::Machine32 => KEY_WOW64_32KEY,
            _ => KEY_WOW64_64KEY,
        }
    }
}

/// 怎样卸载。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    /// 静默运行：(程序, 参数)。
    Quiet(String, String),
    /// 打开程序自己的卸载向导：(程序, 参数)。
    Wizard(String, String),
    /// 商店 / MSIX 应用：包全名。
    Package(String),
}

impl Method {
    pub fn quick(&self) -> bool {
        !matches!(self, Method::Wizard(..))
    }

    pub fn label(&self) -> &'static str {
        match self {
            Method::Quiet(..) => t!("快速卸载", "Quick", "クイック"),
            Method::Wizard(..) => t!("卸载向导", "Wizard", "ウィザード"),
            Method::Package(..) => t!("快速卸载", "Quick", "クイック"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct InstalledApp {
    /// 唯一标识：注册表项（`<范围>\<键名>`）或包全名。
    pub id: String,
    pub name: String,
    pub publisher: String,
    pub version: String,
    /// 安装日期（FILETIME）。
    pub installed: Option<u64>,
    /// 用来测量大小的文件夹（安装位置；商店应用还有数据文件夹）。
    pub folders: Vec<String>,
    /// 安装位置，"打开安装位置"用。
    pub location: Option<String>,
    /// 程序登记的估计大小（字节），没有实测值时显示。
    pub estimated: Option<u64>,
    /// 图标：`路径,序号`。
    pub icon: Option<String>,
    pub store: bool,
    /// 需要管理员权限才能卸载（为所有用户安装）。
    pub machine: bool,
    pub method: Method,
    /// 卸载信息的位置（判断是否已卸载）。
    key: Option<(Scope, String)>,
}

impl InstalledApp {
    /// 是否还在系统里。
    fn still_installed(&self) -> bool {
        match (&self.key, &self.method) {
            (Some((scope, key)), _) => {
                RegKey::open(scope.hive(), &format!(r"{UNINSTALL}\{key}"), scope.view()).is_some()
            }
            (None, Method::Package(full)) => package_installed(full),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------- 注册表

struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

impl RegKey {
    fn open(root: HKEY, path: &str, view: REG_SAM_FLAGS) -> Option<Self> {
        let p = HSTRING::from(path);
        let mut key = HKEY::default();
        unsafe { RegOpenKeyExW(root, &p, None, KEY_READ | view, &mut key) }
            .ok()
            .ok()?;
        Some(Self(key))
    }

    fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = [0u16; 256];
        for i in 0.. {
            let mut len = buf.len() as u32;
            let r = unsafe {
                RegEnumKeyExW(
                    self.0,
                    i,
                    Some(windows::core::PWSTR(buf.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if r.is_err() {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        out
    }

    fn string(&self, sub: &str, name: &str) -> Option<String> {
        let (sub, name) = (HSTRING::from(sub), HSTRING::from(name));
        let mut len = 0u32;
        unsafe {
            RegGetValueW(
                self.0,
                &sub,
                &name,
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut len),
            )
        }
        .ok()
        .ok()?;
        let mut buf = vec![0u16; len as usize / 2 + 1];
        let mut len = (buf.len() * 2) as u32;
        unsafe {
            RegGetValueW(
                self.0,
                &sub,
                &name,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        }
        .ok()
        .ok()?;
        let s = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    }

    fn dword(&self, sub: &str, name: &str) -> Option<u32> {
        let (sub, name) = (HSTRING::from(sub), HSTRING::from(name));
        let mut v = 0u32;
        let mut len = 4u32;
        unsafe {
            RegGetValueW(
                self.0,
                &sub,
                &name,
                RRF_RT_REG_DWORD,
                None,
                Some((&mut v as *mut u32).cast()),
                Some(&mut len),
            )
        }
        .ok()
        .ok()?;
        Some(v)
    }
}

/// 读取所有已安装的程序（桌面程序 + 商店应用）。会读注册表和包信息，在后台线程调用。
pub fn list() -> Vec<InstalledApp> {
    let mut apps = Vec::new();
    for scope in [Scope::Machine, Scope::Machine32, Scope::User] {
        let Some(root) = RegKey::open(scope.hive(), UNINSTALL, scope.view()) else {
            continue;
        };
        for key in root.subkeys() {
            if let Some(app) = desktop_app(&root, scope, &key) {
                apps.push(app);
            }
        }
    }
    // 同一个程序在两个视图里都登记的情况很少，按 (名称, 版本, 范围) 去重即可。
    let mut seen = std::collections::HashSet::new();
    apps.retain(|a| seen.insert((a.name.to_lowercase(), a.version.clone(), a.machine)));
    apps.extend(store_apps());
    apps
}

fn desktop_app(root: &RegKey, scope: Scope, key: &str) -> Option<InstalledApp> {
    let s = |name: &str| root.string(key, name);
    let d = |name: &str| root.dword(key, name);
    let name = s("DisplayName")?;
    // 系统组件、更新补丁、不允许卸载的不列出（"设置 → 应用"也不列）。
    if d("SystemComponent") == Some(1) || d("NoRemove") == Some(1) || s("ParentKeyName").is_some() {
        return None;
    }
    if matches!(
        s("ReleaseType").as_deref(),
        Some("Update" | "Hotfix" | "Security Update")
    ) {
        return None;
    }
    let uninstall = s("UninstallString");
    let quiet = s("QuietUninstallString");
    let msi = d("WindowsInstaller") == Some(1) && is_guid(key);
    let method = if let Some(q) = &quiet {
        let (exe, args) = split_command(q)?;
        Method::Quiet(exe, args)
    } else if msi {
        Method::Quiet("msiexec.exe".into(), format!("/x {key} /qn /norestart"))
    } else {
        let (exe, args) = split_command(uninstall.as_deref()?)?;
        if key.ends_with("_is1") {
            Method::Quiet(
                exe,
                join_args(&args, "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART"),
            )
        } else if is_nsis(&exe) {
            Method::Quiet(exe, join_args(&args, "/S"))
        } else {
            Method::Wizard(exe, args)
        }
    };
    let icon = s("DisplayIcon").map(|i| i.trim_matches('"').to_string());
    let location = s("InstallLocation")
        .map(|l| l.trim_matches('"').trim_end_matches('\\').to_string())
        .filter(|l| measurable(l))
        .or_else(|| {
            // 没登记安装位置：用图标所在的文件夹。
            let exe = icon
                .as_deref()?
                .rsplit_once(',')
                .map_or(icon.as_deref()?, |(p, _)| p);
            let dir = parent(exe.trim_matches('"'))?;
            measurable(&dir).then_some(dir)
        });
    Some(InstalledApp {
        id: format!("{scope:?}\\{key}"),
        name,
        publisher: s("Publisher").unwrap_or_default(),
        version: s("DisplayVersion").unwrap_or_default(),
        installed: s("InstallDate").and_then(|d| date_filetime(&d)),
        folders: location.iter().cloned().collect(),
        location,
        estimated: d("EstimatedSize").map(|kb| u64::from(kb) * 1024),
        icon,
        store: false,
        machine: scope != Scope::User,
        method,
        key: Some((scope, key.to_string())),
    })
}

/// 商店 / MSIX 应用（当前用户的主包）。
fn store_apps() -> Vec<InstalledApp> {
    use windows::ApplicationModel::PackageSignatureKind;
    use windows::Management::Deployment::{PackageManager, PackageTypes};
    let Ok(pm) = PackageManager::new() else {
        return Vec::new();
    };
    let Ok(packages) =
        pm.FindPackagesByUserSecurityIdWithPackageTypes(&HSTRING::new(), PackageTypes::Main)
    else {
        return Vec::new();
    };
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let mut out = Vec::new();
    for p in packages {
        // 系统自带的组件（设置、开始菜单等）不能卸载。
        if p.SignatureKind()
            .is_ok_and(|k| k == PackageSignatureKind::System)
            || p.IsFramework().unwrap_or(true)
            || p.IsResourcePackage().unwrap_or(true)
        {
            continue;
        }
        let Ok(id) = p.Id() else { continue };
        let (Ok(full), Ok(family)) = (id.FullName(), id.FamilyName()) else {
            continue;
        };
        let name = p.DisplayName().map(|s| s.to_string()).unwrap_or_default();
        // 没有显示名、或显示名是未解析的资源引用的，多半是内部组件。
        if name.is_empty() || name.starts_with("ms-resource:") {
            continue;
        }
        let v = id.Version().unwrap_or_default();
        let location = p.InstalledPath().ok().map(|s| s.to_string());
        let mut folders: Vec<String> = location.iter().cloned().collect();
        if !local.is_empty() {
            folders.push(format!(r"{local}\Packages\{family}"));
        }
        out.push(InstalledApp {
            id: full.to_string(),
            name,
            publisher: p
                .PublisherDisplayName()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            version: format!("{}.{}.{}.{}", v.Major, v.Minor, v.Build, v.Revision),
            installed: p.InstalledDate().ok().map(|d| d.UniversalTime as u64),
            folders,
            location,
            estimated: None,
            icon: None,
            store: true,
            machine: false,
            method: Method::Package(full.to_string()),
            key: None,
        });
    }
    out
}

fn package_installed(full: &str) -> bool {
    use windows::Management::Deployment::{PackageManager, PackageTypes};
    // 按全名直接查（FindPackageByPackageFullName）要管理员权限，这里在当前用户的包里找。
    PackageManager::new()
        .and_then(|pm| {
            pm.FindPackagesByUserSecurityIdWithPackageTypes(&HSTRING::new(), PackageTypes::Main)
        })
        .is_ok_and(|ps| {
            ps.into_iter()
                .any(|p| p.Id().and_then(|id| id.FullName()).is_ok_and(|n| n == full))
        })
}

// ---------------------------------------------------------------- 解析

/// 把命令行拆成 (程序, 参数)：带引号的取引号内；否则取到 `.exe` 为止（路径里常有空格却不加引号）。
fn split_command(cmd: &str) -> Option<(String, String)> {
    let c = cmd.trim();
    if let Some(rest) = c.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some((rest[..end].to_string(), rest[end + 1..].trim().to_string()));
    }
    let lower = c.to_ascii_lowercase();
    let end = match lower.find(".exe") {
        Some(i) => i + 4,
        None => c.find(' ').unwrap_or(c.len()),
    };
    (end > 0).then(|| (c[..end].to_string(), c[end..].trim().to_string()))
}

fn join_args(args: &str, extra: &str) -> String {
    if args.is_empty() {
        extra.to_string()
    } else {
        format!("{args} {extra}")
    }
}

fn is_guid(s: &str) -> bool {
    s.len() == 38 && s.starts_with('{') && s.ends_with('}')
}

/// NSIS 生成的卸载程序：资源里的清单带 "Nullsoft" 字样。清单在开头的程序部分里，后面附加的
/// 数据可能有几百 MB，只读前 1 MB。
fn is_nsis(exe: &str) -> bool {
    use std::io::Read;
    let mut head = Vec::with_capacity(1 << 20);
    let Ok(f) = std::fs::File::open(exe) else {
        return false;
    };
    f.take(1 << 20).read_to_end(&mut head).is_ok() && head.windows(8).any(|w| w == b"Nullsoft")
}

fn parent(path: &str) -> Option<String> {
    let p = path.trim_end_matches('\\');
    let (dir, _) = p.rsplit_once('\\')?;
    Some(if dir.len() == 2 {
        format!("{dir}\\")
    } else {
        dir.to_string()
    })
}

/// 是否可以拿来算一个程序的大小：存在的文件夹，且不是盘符根目录、Program Files、Windows 这类
/// 许多程序共用的位置（否则会把整个 Program Files 算成一个程序的大小）。
fn measurable(dir: &str) -> bool {
    let d = dir.trim_end_matches('\\');
    if d.len() < 4 || !std::path::Path::new(d).is_dir() {
        return false;
    }
    let shared: Vec<String> = [
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
        "SystemRoot",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "CommonProgramFiles",
        "CommonProgramFiles(x86)",
    ]
    .iter()
    .filter_map(|v| std::env::var(v).ok())
    .chain(std::env::var("LOCALAPPDATA").map(|l| format!(r"{l}\Programs")))
    .collect();
    let lower = d.to_lowercase();
    if shared
        .iter()
        .any(|s| s.trim_end_matches('\\').eq_ignore_ascii_case(d))
    {
        return false;
    }
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    !lower.starts_with(&windir.to_lowercase())
}

/// `20261012` → FILETIME（当天 0 点，UTC）。
fn date_filetime(s: &str) -> Option<u64> {
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (
        s[..4].parse().ok()?,
        s[4..6].parse().ok()?,
        s[6..].parse().ok()?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1980 {
        return None;
    }
    // 公历日期到 1970-01-01 起的天数（Howard Hinnant 的 days_from_civil）。
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 * 10_000_000 + 116_444_736_000_000_000) as u64)
}

// ---------------------------------------------------------------- 大小

/// 测量各程序文件夹的大小（字节，按占用空间），每测完一个回调一次。同一文件夹只测一次。
pub fn measure(
    apps: &[InstalledApp],
    by_alloc: bool,
    cancel: &AtomicBool,
    mut done: impl FnMut(&str, u64),
) {
    let mut cache: HashMap<String, u64> = HashMap::new();
    let progress = sweep_scan::Progress::default();
    for app in apps {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if app.folders.is_empty() {
            continue;
        }
        let mut total = 0;
        for f in &app.folders {
            let key = f.to_lowercase();
            let size = match cache.get(&key) {
                Some(&s) => s,
                None => {
                    let s = sweep_scan::walk::scan(f, by_alloc, &progress, cancel)
                        .map(|t| t.value(t.root))
                        .unwrap_or(0);
                    cache.insert(key, s);
                    s
                }
            };
            total += size;
        }
        done(&app.id, total);
    }
}

// ---------------------------------------------------------------- 卸载

/// 卸载一个程序，等它完成。成功（已从系统中消失）返回 Ok。
pub fn uninstall(app: &InstalledApp, elevated: bool) -> Result<(), String> {
    match &app.method {
        Method::Package(full) => remove_package(full),
        Method::Quiet(exe, args) | Method::Wizard(exe, args) => {
            let elevate = app.machine && !elevated;
            run_and_wait(exe, args, elevate)?;
            // 有的卸载程序把自己复制到临时目录运行、原进程立即退出（NSIS 等），再等一会儿。
            let patience = if app.method.quick() {
                Duration::from_secs(120)
            } else {
                Duration::from_secs(20)
            };
            let start = Instant::now();
            while app.still_installed() {
                if start.elapsed() > patience {
                    return Err(if app.method.quick() {
                        t!(
                            "卸载程序已结束，但该程序仍然登记为已安装",
                            "The uninstaller finished, but the app is still registered as installed",
                            "アンインストーラーは終了しましたが、アプリはまだインストール済みとして登録されています"
                        )
                        .into()
                    } else {
                        t!(
                            "卸载向导已关闭，程序仍然存在（可能取消了卸载）",
                            "The uninstall wizard closed and the app is still there (it may have been canceled)",
                            "アンインストール ウィザードが閉じられましたが、アプリは残っています (キャンセルされた可能性があります)"
                        )
                        .into()
                    });
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(())
        }
    }
}

fn remove_package(full: &str) -> Result<(), String> {
    use windows::Management::Deployment::PackageManager;
    let pm = PackageManager::new().map_err(|e| e.message())?;
    let result = pm
        .RemovePackageAsync(&HSTRING::from(full))
        .and_then(|op| op.join())
        .map_err(|e| e.message())?;
    match result.ExtendedErrorCode() {
        Ok(code) if code.is_err() => Err(result
            .ErrorText()
            .map(|s| s.to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| code.message())),
        _ => Ok(()),
    }
}

/// 启动卸载程序并等它退出。`elevate` 时以管理员身份运行（会弹 UAC）。
fn run_and_wait(exe: &str, args: &str, elevate: bool) -> Result<(), String> {
    let file = HSTRING::from(exe);
    let params = HSTRING::from(args);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: if elevate { w!("runas") } else { w!("open") },
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.map_err(|e| {
        // ERROR_CANCELLED：在 UAC 提示里点了"否"。
        if e.code() == windows::Win32::Foundation::ERROR_CANCELLED.to_hresult() {
            t!(
                "没有获得管理员权限",
                "Administrator permission was not granted",
                "管理者権限が許可されませんでした"
            )
            .to_string()
        } else {
            tf!(
                "无法启动卸载程序：{}",
                "Couldn't start the uninstaller: {}",
                "アンインストーラーを起動できませんでした: {}",
                e.message()
            )
        }
    })?;
    if !info.hProcess.is_invalid() {
        unsafe {
            if WaitForSingleObject(info.hProcess, u32::MAX) != WAIT_OBJECT_0 {
                let _ = CloseHandle(info.hProcess);
                return Err("wait failed".into());
            }
            let _ = CloseHandle(info.hProcess);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_commands() {
        assert_eq!(
            split_command(r#""C:\Program Files\App\unins000.exe" /SILENT"#),
            Some((
                r"C:\Program Files\App\unins000.exe".into(),
                "/SILENT".into()
            ))
        );
        assert_eq!(
            split_command(r"C:\Program Files\App\uninstall.exe --x"),
            Some((r"C:\Program Files\App\uninstall.exe".into(), "--x".into()))
        );
        assert_eq!(
            split_command("MsiExec.exe /X{12345678-1234-1234-1234-123456789012}"),
            Some((
                "MsiExec.exe".into(),
                "/X{12345678-1234-1234-1234-123456789012}".into()
            ))
        );
        assert_eq!(
            split_command(r#""C:\a b\x.exe""#),
            Some((r"C:\a b\x.exe".into(), String::new()))
        );
        assert_eq!(split_command(""), None);
    }

    #[test]
    fn parses_install_dates() {
        // 2026-10-12 00:00 UTC
        assert_eq!(date_filetime("20261012"), Some(134_362_368_000_000_000));
        assert_eq!(date_filetime("19800101"), Some(119600064000000000));
        assert_eq!(
            date_filetime("19700101"),
            None,
            "早于 1980 年的日期是填错的"
        );
        assert_eq!(date_filetime("2026-10-12"), None);
        assert_eq!(date_filetime("20261332"), None);
    }

    #[test]
    fn rejects_shared_folders() {
        assert!(!measurable(r"C:\"));
        assert!(!measurable(&std::env::var("ProgramFiles").unwrap()));
        assert!(!measurable(&std::env::var("SystemRoot").unwrap()));
        assert!(!measurable(r"C:\definitely\not\here"));
        assert_eq!(parent(r"C:\App\app.exe").as_deref(), Some(r"C:\App"));
        assert_eq!(parent(r"C:\app.exe").as_deref(), Some(r"C:\"));
        assert!(is_guid("{12345678-1234-1234-1234-123456789012}"));
    }

    /// 在 HKCU 里登记两个假程序，它们的"卸载程序"只删除自己的注册表项；分别走快速卸载与卸载向导，
    /// 确认能找到、能卸载、卸载后从列表消失。测试结束时无论成败都删除这两个项：
    /// `cargo test -p sweep -- --ignored uninstalls_test_entries --nocapture`。
    #[test]
    #[ignore]
    fn uninstalls_test_entries() {
        let pid = std::process::id();
        let keys = [
            format!("SweepTestQuiet{pid}"),
            format!("SweepTestWizard{pid}"),
        ];
        let reg = |args: &[&str]| {
            std::process::Command::new("reg.exe")
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        };
        let cleanup = || {
            for k in &keys {
                reg(&["delete", &format!(r"HKCU\{UNINSTALL}\{k}"), "/f"]);
            }
        };
        for (k, value) in keys.iter().zip(["QuietUninstallString", "UninstallString"]) {
            let path = format!(r"HKCU\{UNINSTALL}\{k}");
            let cmd = format!(r#"C:\Windows\System32\cmd.exe /c reg delete "{path}" /f"#);
            assert!(reg(&["add", &path, "/v", "DisplayName", "/d", k, "/f"]));
            assert!(reg(&["add", &path, "/v", value, "/d", &cmd, "/f"]));
        }
        let result = std::panic::catch_unwind(|| {
            let apps = list();
            for (k, quick) in keys.iter().zip([true, false]) {
                let app = apps.iter().find(|a| &a.name == k).expect("listed");
                assert_eq!(app.method.quick(), quick, "{:?}", app.method);
                assert!(!app.machine);
                assert!(app.still_installed());
                assert_eq!(uninstall(app, false), Ok(()));
                assert!(!app.still_installed());
            }
            assert!(!list().iter().any(|a| keys.contains(&a.name)));
        });
        cleanup();
        result.unwrap();
    }

    /// 读真实的已安装程序列表（只读）：`cargo test -p sweep -- --ignored lists_installed --nocapture`。
    #[test]
    #[ignore]
    fn lists_installed_apps() {
        let t = Instant::now();
        let store = store_apps().len();
        println!("{store} store apps in {:?}", t.elapsed());
        let t = Instant::now();
        let apps = list();
        println!("{} apps in {:?}", apps.len(), t.elapsed());
        let mut by_method: HashMap<&str, usize> = HashMap::new();
        for a in &apps {
            let m = match &a.method {
                Method::Quiet(e, _) if e.eq_ignore_ascii_case("msiexec.exe") => "msi",
                Method::Quiet(..) => "quiet",
                Method::Wizard(..) => "wizard",
                Method::Package(..) => "store",
            };
            *by_method.entry(m).or_default() += 1;
        }
        println!("{by_method:?}");
        for a in apps
            .iter()
            .filter(|a| matches!(a.method, Method::Wizard(..)))
            .take(15)
        {
            println!("  wizard: {} | {:?}", a.name, a.method);
        }
        for a in apps.iter().take(10) {
            println!(
                "  {} | {} | {:?} | {:?}",
                a.name, a.version, a.folders, a.method
            );
        }
        assert!(!apps.is_empty());
    }
}
