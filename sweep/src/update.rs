//! 自动更新：从 GitHub Releases 检查新版本，下载、校验并替换当前 exe 后重启。
//!
//! 与界面无关，也不依赖本程序的其他模块（只用 `windows` 与 `serde_json`），可以原样复制到别的程序：
//!
//! ```ignore
//! update::cleanup_old(&cfg);                   // 启动时删掉上次更新留下的 <exe>.old
//! let updater = Updater::new(Config { … }, move || ctx.request_repaint());
//! updater.start(settings.auto_update);         // 5 秒后检查一次，之后每 24 小时
//! // 每帧：updater.status() → 标题栏提示 / 对话框；用户点击后 updater.install(release)
//! // status().install == Install::Restarting 时新进程已启动，关闭窗口退出。
//! ```
//!
//! - 网络走 WinHTTP（`WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY`），沿用系统代理与 TLS，自动跟随重定向。
//! - 三种更新方式（[`Method`]）：
//!   - 由安装程序安装的（Inno Setup 卸载信息里的安装位置就是 exe 所在目录）：下载安装程序，
//!     校验后静默运行（所有用户安装时请求管理员权限），由它替换文件并重新启动程序。
//!   - 便携版且目录可写：下载的 exe 先写到同目录的临时文件（同一卷，改名是原子的），校验后
//!     把正在运行的 exe 改名为 `<exe>.old`（Windows 允许改名运行中的 exe），新文件移到原路径，
//!     以相同参数启动新进程。任何一步失败都会恢复原文件名。
//!   - 其他情况：只提供发布页链接。
//! - 下载的文件都按发布里的 `SHA256SUMS.txt` 校验，没有校验和或不符时放弃。
//! - 网络、哈希、文件操作都在后台线程，界面只读 [`Updater::status`]。

use std::ffi::c_void;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// 首次检查前的等待，避开启动时的繁忙期。
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(5);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// 发行说明最多保留的字符数。
const NOTES_LIMIT: usize = 6000;

/// 由调用方提供的发布信息。
#[derive(Clone, Debug)]
pub struct Config {
    /// GitHub 仓库所有者与名称，如 `"Jannchie"`、`"pulse"`。
    pub owner: &'static str,
    pub repo: &'static str,
    /// 当前版本，通常是 `env!("CARGO_PKG_VERSION")`。
    pub current_version: &'static str,
    /// 要下载的 exe 资产名，`{tag}` 会替换为标签名，如 `"pulse-{tag}-windows-x64.exe"`。
    pub asset_pattern: &'static str,
    /// 校验和资产名，每行 `<sha256>  <文件名>`，如 `"SHA256SUMS.txt"`。
    pub checksums: &'static str,
    /// 安装版的更新方式；没有安装程序时为 None。
    pub installer: Option<InstallerConfig>,
}

/// 用 Inno Setup 安装程序更新安装版。
#[derive(Clone, Copy, Debug)]
pub struct InstallerConfig {
    /// 安装程序资产名，`{tag}` 会替换为标签名，如 `"pulse-{tag}-windows-x64-setup.exe"`。
    pub asset_pattern: &'static str,
    /// 卸载信息的键名，即 `<AppId>_is1`，如 `"{99CDD8A9-…}_is1"`。
    pub uninstall_key: &'static str,
    /// 静默更新时传给安装程序的参数。另外会按原来的安装方式追加 `/ALLUSERS` 或 `/CURRENTUSER`
    /// （安装脚本需要 `PrivilegesRequiredOverridesAllowed=commandline`），否则当前用户安装的
    /// 副本会被更新成一份新的所有用户安装。
    pub args: &'static str,
}

impl Config {
    fn user_agent(&self) -> String {
        format!(
            "{}/{} (+https://github.com/{}/{})",
            self.repo, self.current_version, self.owner, self.repo
        )
    }

    fn latest_url(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/releases/latest",
            self.owner, self.repo
        )
    }

    fn asset_name(&self, tag: &str) -> String {
        self.asset_pattern.replace("{tag}", tag)
    }
}

/// 如何安装某个新版本，检查时按程序所在位置决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// 便携版：直接替换 exe。
    InPlace,
    /// 安装版：运行安装程序；`elevate` 为所有用户安装，需要管理员权限。
    Installer { elevate: bool },
    /// 无法自动更新（目录不可写，或发布里没有合适的文件），只能打开发布页。
    Manual,
}

#[derive(Clone, Debug, PartialEq)]
struct Asset {
    name: String,
    url: String,
    size: u64,
}

/// 一个比当前版本新的发布。
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// 不带 `v` 的版本号，如 `"0.3.2"`。
    pub version: String,
    /// 发行说明（去掉了 Markdown 标记的纯文本，过长时截断）。
    pub notes: String,
    /// 发布页地址，无法自动更新时在浏览器中打开。
    pub page_url: String,
    pub method: Method,
    exe: Option<Asset>,
    setup: Option<Asset>,
    checksums_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// 连接失败（离线、超时、DNS 等），附 WinHTTP / 系统错误码。
    Network(u32),
    /// 服务器返回了非 200 状态。
    Http(u16),
    /// GitHub API 的匿名调用次数用完了。
    RateLimited,
    /// 响应格式不对。
    BadResponse,
    /// 发布里没有本机可用的 exe。
    AssetMissing,
    /// 没有校验和文件，或其中没有这个 exe。
    ChecksumMissing,
    /// 下载的文件与校验和不符。
    ChecksumMismatch,
    /// exe 所在目录不可写。
    NotWritable,
    /// 用户在 UAC 提示里拒绝了管理员权限。
    Cancelled,
    /// 文件操作失败，附系统给出的说明。
    Io(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        let hr = e.code().0 as u32;
        // HRESULT_FROM_WIN32：设施码 7，低 16 位是 Win32 错误码。
        let code = if hr & 0xffff_0000 == 0x8007_0000 {
            hr & 0xffff
        } else {
            hr
        };
        Error::Network(code)
    }
}

#[derive(Clone, Debug, Default)]
pub enum Check {
    #[default]
    Idle,
    /// 用户手动发起的检查进行中（自动检查不改变状态）。
    Checking,
    UpToDate,
    Available(Arc<Release>),
    /// 只有手动检查会报告失败，自动检查失败时保持原状态。
    Failed(Error),
}

#[derive(Clone, Debug, Default)]
pub enum Install {
    #[default]
    Idle,
    Downloading {
        done: u64,
        total: u64,
    },
    Verifying,
    /// 新版本已就位并启动，调用方应退出。
    Restarting,
    Failed(Error),
}

impl Install {
    pub fn busy(&self) -> bool {
        matches!(
            self,
            Install::Downloading { .. } | Install::Verifying | Install::Restarting
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub check: Check,
    pub install: Install,
}

struct Inner {
    cfg: Config,
    status: Mutex<Status>,
    auto: AtomicBool,
    checking: AtomicBool,
    notify: Box<dyn Fn() + Send + Sync>,
}

/// 后台检查与安装；可以廉价地克隆。
#[derive(Clone)]
pub struct Updater {
    inner: Arc<Inner>,
}

impl Updater {
    /// `notify` 在状态变化时（后台线程里）调用，一般用来请求重绘。
    pub fn new(cfg: Config, notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg,
                status: Mutex::new(Status::default()),
                auto: AtomicBool::new(false),
                checking: AtomicBool::new(false),
                notify: Box::new(notify),
            }),
        }
    }

    /// 启动定时检查线程：启动后约 5 秒检查一次，之后每 24 小时一次；`auto` 关闭时跳过。
    pub fn start(&self, auto: bool) {
        self.set_auto(auto);
        let inner = self.inner.clone();
        let _ = std::thread::Builder::new()
            .name("update-schedule".into())
            .spawn(move || {
                std::thread::sleep(FIRST_CHECK_DELAY);
                let mut last: Option<Instant> = None;
                loop {
                    if inner.auto.load(Ordering::Relaxed)
                        && last.is_none_or(|t| t.elapsed() >= CHECK_INTERVAL)
                    {
                        last = Some(Instant::now());
                        run_check(&inner, false);
                    }
                    std::thread::sleep(Duration::from_secs(60));
                }
            });
    }

    pub fn set_auto(&self, on: bool) {
        self.inner.auto.store(on, Ordering::Relaxed);
    }

    /// 用户手动检查：状态变为 [`Check::Checking`]，失败时报告 [`Check::Failed`]。
    pub fn check_now(&self) {
        self.inner.status.lock().unwrap().check = Check::Checking;
        (self.inner.notify)();
        let inner = self.inner.clone();
        let _ = std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || run_check(&inner, true));
    }

    pub fn status(&self) -> Status {
        self.inner.status.lock().unwrap().clone()
    }

    /// 下载、校验并替换 exe，成功后以相同参数启动新进程，状态变为 [`Install::Restarting`]。
    pub fn install(&self, release: Arc<Release>) {
        {
            let mut s = self.inner.status.lock().unwrap();
            if s.install.busy() {
                return;
            }
            s.install = Install::Downloading { done: 0, total: 0 };
        }
        (self.inner.notify)();
        let inner = self.inner.clone();
        let _ = std::thread::Builder::new()
            .name("update-install".into())
            .spawn(move || {
                let result = install(&inner, &release);
                inner.status.lock().unwrap().install = match result {
                    Ok(()) => Install::Restarting,
                    Err(e) => Install::Failed(e),
                };
                (inner.notify)();
            });
    }

    /// 清除上次安装失败的提示。
    pub fn dismiss_install_error(&self) {
        let mut s = self.inner.status.lock().unwrap();
        if matches!(s.install, Install::Failed(_)) {
            s.install = Install::Idle;
        }
    }
}

fn run_check(inner: &Inner, manual: bool) {
    // 同一时间只跑一个检查；手动检查撞上自动检查时，状态已是 Checking，由正在跑的那次报告结果。
    if inner.checking.swap(true, Ordering::AcqRel) {
        return;
    }
    let result = check(&inner.cfg);
    inner.checking.store(false, Ordering::Release);
    let mut s = inner.status.lock().unwrap();
    let manual = manual || matches!(s.check, Check::Checking);
    s.check = match result {
        Ok(Some(r)) => Check::Available(Arc::new(r)),
        Ok(None) => Check::UpToDate,
        Err(e) if manual => Check::Failed(e),
        // 自动检查失败（离线、限流……）不打扰用户。
        Err(_) => return,
    };
    drop(s);
    (inner.notify)();
}

// ---------------------------------------------------------------- 检查

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// 查询最新发布；比当前版本新时返回它。
pub fn check(cfg: &Config) -> Result<Option<Release>, Error> {
    let mut body = Vec::new();
    http_get(
        &cfg.latest_url(),
        &cfg.user_agent(),
        "application/vnd.github+json",
        |chunk, _| {
            body.extend_from_slice(chunk);
            if body.len() > 4 << 20 {
                return Err(Error::BadResponse);
            }
            Ok(())
        },
    )?;
    let api: ApiRelease = serde_json::from_slice(&body).map_err(|_| Error::BadResponse)?;
    let Some(release) = select_release(cfg, api)? else {
        return Ok(None);
    };
    let installed = cfg.installer.and_then(|i| installed_scope(i.uninstall_key));
    let method = choose_method(
        installed,
        exe_dir_writable(),
        release.exe.is_some(),
        release.setup.is_some(),
    );
    Ok(Some(Release { method, ..release }))
}

/// `installed`：Some(是否为所有用户安装)；None 表示便携版。
fn choose_method(
    installed: Option<bool>,
    writable: bool,
    has_exe: bool,
    has_setup: bool,
) -> Method {
    match installed {
        Some(machine) if has_setup => Method::Installer { elevate: machine },
        _ if writable && has_exe => Method::InPlace,
        _ => Method::Manual,
    }
}

/// 当前 exe 是否由安装程序安装：卸载信息里的 InstallLocation 与 exe 所在目录相同。
/// 返回 Some(true) 表示所有用户安装（HKLM），Some(false) 表示当前用户安装（HKCU）。
fn installed_scope(uninstall_key: &str) -> Option<bool> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let (dir, _) = exe_location()?;
    let subkey = format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{uninstall_key}");
    [(HKEY_LOCAL_MACHINE, true), (HKEY_CURRENT_USER, false)]
        .into_iter()
        .find(|(root, _)| {
            reg_string(*root, &subkey, "InstallLocation")
                .is_some_and(|loc| same_dir(&loc, &dir.to_string_lossy()))
        })
        .map(|(_, machine)| machine)
}

fn same_dir(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .trim_end_matches(['\\', '/'])
            .replace('/', "\\")
            .to_lowercase()
    };
    !a.trim().is_empty() && norm(a) == norm(b)
}

fn reg_string(
    root: windows::Win32::System::Registry::HKEY,
    subkey: &str,
    value: &str,
) -> Option<String> {
    use windows::Win32::System::Registry::{RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::PCWSTR;
    let (subkey, value) = (wide(subkey), wide(value));
    let mut buf = vec![0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let err = unsafe {
        RegGetValueW(
            root,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if err.is_err() {
        return None;
    }
    let n = (len as usize / 2).min(buf.len());
    Some(
        String::from_utf16_lossy(&buf[..n])
            .trim_end_matches('\0')
            .to_string(),
    )
}

/// 从 API 响应挑出可安装的新版本：草稿、预发布、不比当前新的都忽略。
fn select_release(cfg: &Config, api: ApiRelease) -> Result<Option<Release>, Error> {
    if api.draft || api.prerelease {
        return Ok(None);
    }
    let version = api
        .tag_name
        .strip_prefix(['v', 'V'])
        .unwrap_or(&api.tag_name);
    let (Some(latest), Some(current)) =
        (Version::parse(version), Version::parse(cfg.current_version))
    else {
        return Err(Error::BadResponse);
    };
    if !latest.release || latest <= current {
        return Ok(None);
    }
    let find = |name: &str| {
        api.assets
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name))
            .map(|a| Asset {
                name: a.name.clone(),
                url: a.browser_download_url.clone(),
                size: a.size,
            })
    };
    let exe = find(&cfg.asset_name(&api.tag_name));
    let setup = cfg
        .installer
        .and_then(|i| find(&i.asset_pattern.replace("{tag}", &api.tag_name)));
    let checksums_url = find(cfg.checksums).map(|a| a.url);
    let mut notes = plain_notes(&api.body.unwrap_or_default());
    if let Some((i, _)) = notes.char_indices().nth(NOTES_LIMIT) {
        notes.truncate(i);
        notes.push_str("\n…");
    }
    Ok(Some(Release {
        version: version.to_string(),
        notes: notes.trim().to_string(),
        page_url: api.html_url,
        method: Method::Manual,
        exe,
        setup,
        checksums_url,
    }))
}

/// 语义化版本，只比较 `主.次.修订`；带 `-xxx` 后缀的是预发布，排在同号正式版之前。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    /// 正式版为 true：同号时预发布（false）排在正式版之前。
    release: bool,
}

impl Version {
    fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.split_once('+').map_or(s, |(v, _)| v);
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, true),
            Some(_) => return None,
            None => (s, false),
        };
        let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
        let major = parts.next()??;
        let minor = parts.next().unwrap_or(Some(0))?;
        let patch = parts.next().unwrap_or(Some(0))?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
            release: !pre,
        })
    }
}

/// `exe` 所在目录能否创建文件。
/// 把 GitHub 发行说明的常用 Markdown 转成纯文本：标题去掉 `#`，列表项换成 `•`，
/// 去掉加粗与行内代码标记，`[文字](链接)` 只留文字。
fn plain_notes(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    for line in md.lines() {
        let line = line.trim_end();
        let text = line.trim_start();
        out.push_str(&line[..line.len() - text.len()]);
        let mut text = text.trim_start_matches('#').trim_start();
        if let Some(rest) = text.strip_prefix("* ").or_else(|| text.strip_prefix("- ")) {
            out.push_str("• ");
            text = rest;
        }
        let mut text = text.replace("**", "").replace('`', "");
        while let Some(open) = text.find('[') {
            let Some(mid) = text[open..].find("](").map(|i| open + i) else {
                break;
            };
            let Some(close) = text[mid..].find(')').map(|i| mid + i) else {
                break;
            };
            text.replace_range(mid..=close, "");
            text.remove(open);
        }
        out.push_str(&text);
        out.push('\n');
    }
    out
}

fn exe_dir_writable() -> bool {
    let Some((dir, file)) = exe_location() else {
        return false;
    };
    let probe = dir.join(format!("{file}.update-probe"));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(&probe);
            true
        }
        // 上次的探测文件没删掉：目录显然可写。
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// (exe 所在目录, exe 文件名)。
fn exe_location() -> Option<(PathBuf, String)> {
    let exe = std::env::current_exe().ok()?;
    let file = exe.file_name()?.to_string_lossy().into_owned();
    Some((exe.parent()?.to_path_buf(), file))
}

// ---------------------------------------------------------------- 安装

fn install(inner: &Inner, release: &Release) -> Result<(), Error> {
    match release.method {
        Method::InPlace => install_in_place(inner, release),
        Method::Installer { elevate } => install_with_setup(inner, release, elevate),
        Method::Manual => Err(Error::NotWritable),
    }
}

/// 从发布的 `SHA256SUMS.txt` 里取 `asset` 的哈希。先取校验和：没有就不必下载。
fn expected_hash(release: &Release, asset: &Asset, ua: &str) -> Result<[u8; 32], Error> {
    let url = release
        .checksums_url
        .as_deref()
        .ok_or(Error::ChecksumMissing)?;
    let mut sums = Vec::new();
    http_get(url, ua, "application/octet-stream", |chunk, _| {
        sums.extend_from_slice(chunk);
        if sums.len() > 1 << 20 {
            return Err(Error::BadResponse);
        }
        Ok(())
    })?;
    parse_checksums(&String::from_utf8_lossy(&sums), &asset.name).ok_or(Error::ChecksumMissing)
}

fn install_in_place(inner: &Inner, release: &Release) -> Result<(), Error> {
    let asset = release.exe.as_ref().ok_or(Error::AssetMissing)?;
    let exe = std::env::current_exe()?;
    let (dir, file) = exe_location().ok_or(Error::NotWritable)?;
    if !exe_dir_writable() {
        return Err(Error::NotWritable);
    }
    let ua = inner.cfg.user_agent();
    let expected = expected_hash(release, asset, &ua)?;
    let tmp = dir.join(format!("{file}.download"));
    let result = download_verified(inner, asset, &ua, &tmp, &expected)
        .and_then(|()| swap_and_restart(&exe, &dir, &file, &tmp));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 下载安装程序到临时目录，校验后静默运行；它会关闭本程序、替换文件并重新启动。
fn install_with_setup(inner: &Inner, release: &Release, elevate: bool) -> Result<(), Error> {
    let cfg = &inner.cfg;
    let installer = cfg.installer.ok_or(Error::AssetMissing)?;
    let asset = release.setup.as_ref().ok_or(Error::AssetMissing)?;
    let ua = cfg.user_agent();
    let expected = expected_hash(release, asset, &ua)?;
    let path = std::env::temp_dir().join(format!("{}-update-{}", cfg.repo, asset.name));
    let result = download_verified(inner, asset, &ua, &path, &expected).and_then(|()| {
        let scope = if elevate { "/ALLUSERS" } else { "/CURRENTUSER" };
        run_installer(&path, &format!("{} {scope}", installer.args), elevate)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    result
}

/// 启动安装程序；`elevate` 时以管理员身份运行，用户拒绝 UAC 时返回 [`Error::Cancelled`]。
fn run_installer(path: &Path, args: &str, elevate: bool) -> Result<(), Error> {
    use windows::Win32::UI::Shell::{
        SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};
    let file = wide(&path.to_string_lossy());
    let params = wide(args);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: if elevate { w!("runas") } else { w!("open") },
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        // ERROR_CANCELLED：用户在 UAC 提示里点了"否"。
        return Err(if e.code().0 as u32 == 0x8007_04c7 {
            Error::Cancelled
        } else {
            Error::Io(e.message())
        });
    }
    Ok(())
}

fn set_install(inner: &Inner, state: Install) {
    inner.status.lock().unwrap().install = state;
    (inner.notify)();
}

/// 下载到 `tmp`，边写边算 SHA-256，与 `expected` 不符时报错。
fn download_verified(
    inner: &Inner,
    asset: &Asset,
    ua: &str,
    tmp: &Path,
    expected: &[u8; 32],
) -> Result<(), Error> {
    let mut out = std::fs::File::create(tmp)?;
    let mut hasher = Sha256::new()?;
    let mut done = 0u64;
    let mut reported = Instant::now();
    http_get(&asset.url, ua, "application/octet-stream", |chunk, len| {
        out.write_all(chunk)?;
        hasher.update(chunk)?;
        done += chunk.len() as u64;
        if reported.elapsed() >= Duration::from_millis(100) {
            reported = Instant::now();
            let total = len.unwrap_or(asset.size).max(done);
            set_install(inner, Install::Downloading { done, total });
        }
        Ok(())
    })?;
    set_install(inner, Install::Verifying);
    out.sync_all()?;
    drop(out);
    if hasher.finish()? != *expected {
        return Err(Error::ChecksumMismatch);
    }
    Ok(())
}

/// 运行中的 exe 改名为 `.old`，新文件放到原路径，再以相同参数启动。失败时恢复原文件名。
fn swap_and_restart(exe: &Path, dir: &Path, file: &str, tmp: &Path) -> Result<(), Error> {
    let old = old_path(dir, file);
    std::fs::rename(exe, &old)?;
    if let Err(e) = std::fs::rename(tmp, exe) {
        let _ = std::fs::rename(&old, exe);
        return Err(e.into());
    }
    let spawned = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .spawn();
    if let Err(e) = spawned {
        // 新 exe 移回临时文件名（随后会被删掉），旧 exe 改回原名。
        let _ = std::fs::rename(exe, tmp);
        let _ = std::fs::rename(&old, exe);
        return Err(e.into());
    }
    Ok(())
}

/// 旧 exe 的去处：`<exe>.old`；它还被占着（上一次更新的进程没退出）时换个序号。
fn old_path(dir: &Path, file: &str) -> PathBuf {
    let first = dir.join(format!("{file}.old"));
    if !first.exists() || std::fs::remove_file(&first).is_ok() {
        return first;
    }
    (1..)
        .map(|n| dir.join(format!("{file}.{n}.old")))
        .find(|p| !p.exists() || std::fs::remove_file(p).is_ok())
        .unwrap_or(first)
}

/// 启动时在后台删掉上次更新留下的 `<exe>.old`；旧进程可能还没完全退出，删不掉就隔一会儿再试。
/// 临时目录里上次运行过的安装程序（一小时以前的）也一并删掉。
pub fn cleanup_old(cfg: &Config) {
    let Some((dir, file)) = exe_location() else {
        return;
    };
    let setup_prefix = format!("{}-update-", cfg.repo).to_lowercase();
    let _ = std::thread::Builder::new()
        .name("update-cleanup".into())
        .spawn(move || {
            if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_lowercase();
                    // 放过新文件：启动后立刻手动检查并更新时，刚下载的安装程序也在这里。
                    if name.starts_with(&setup_prefix) && name.ends_with(".exe") && is_stale(&entry)
                    {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            let prefix = format!("{file}.").to_lowercase();
            for _ in 0..30 {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    return;
                };
                let mut left = false;
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_lowercase();
                    if !name.starts_with(&prefix) {
                        continue;
                    }
                    if name.ends_with(".old") && std::fs::remove_file(entry.path()).is_err() {
                        left = true;
                    }
                    // 中途退出留下的下载文件；放过最近的，可能是另一个实例正在下载。
                    if name.ends_with(".download") && is_stale(&entry) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
                if !left {
                    return;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
}

/// 一小时以前修改的文件。
fn is_stale(entry: &std::fs::DirEntry) -> bool {
    entry
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > Duration::from_secs(3600))
}

/// 在 `SHA256SUMS.txt`（`<hex>  <文件名>`，文件名前可能有表示二进制模式的 `*`）里找 `name` 的哈希。
fn parse_checksums(text: &str, name: &str) -> Option<[u8; 32]> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .find_map(|line| {
            let (hash, file) = line.trim().split_once(char::is_whitespace)?;
            let file = file.trim_start();
            let file = file.strip_prefix('*').unwrap_or(file);
            if !file.eq_ignore_ascii_case(name) {
                return None;
            }
            parse_hex32(hash)
        })
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

// ---------------------------------------------------------------- SHA-256（BCrypt）

struct Sha256(windows::Win32::Security::Cryptography::BCRYPT_HASH_HANDLE);

impl Sha256 {
    fn new() -> Result<Self, Error> {
        use windows::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptCreateHash};
        let mut h = Default::default();
        unsafe { BCryptCreateHash(BCRYPT_SHA256_ALG_HANDLE, &mut h, None, None, 0) }
            .ok()
            .map_err(|e| Error::Io(e.message()))?;
        Ok(Self(h))
    }

    fn update(&mut self, data: &[u8]) -> Result<(), Error> {
        use windows::Win32::Security::Cryptography::BCryptHashData;
        unsafe { BCryptHashData(self.0, data, 0) }
            .ok()
            .map_err(|e| Error::Io(e.message()))
    }

    fn finish(self) -> Result<[u8; 32], Error> {
        use windows::Win32::Security::Cryptography::BCryptFinishHash;
        let mut out = [0u8; 32];
        unsafe { BCryptFinishHash(self.0, &mut out, 0) }
            .ok()
            .map_err(|e| Error::Io(e.message()))?;
        Ok(out)
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Security::Cryptography::BCryptDestroyHash(self.0);
        }
    }
}

// ---------------------------------------------------------------- HTTP（WinHTTP）

struct HInternet(*mut c_void);

impl HInternet {
    fn new(h: *mut c_void) -> Result<Self, Error> {
        if h.is_null() {
            Err(windows::core::Error::from_thread().into())
        } else {
            Ok(Self(h))
        }
    }
}

impl Drop for HInternet {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Networking::WinHttp::WinHttpCloseHandle(self.0);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// HTTPS GET，自动跟随重定向。`sink` 依次收到响应体的各段，以及 Content-Length（若有）。
/// 非 200 状态返回 [`Error::Http`]（403 / 429 视为限流）。
fn http_get(
    url: &str,
    user_agent: &str,
    accept: &str,
    mut sink: impl FnMut(&[u8], Option<u64>) -> Result<(), Error>,
) -> Result<(), Error> {
    use windows::Win32::Networking::WinHttp::*;
    use windows::core::PCWSTR;

    let rest = url.strip_prefix("https://").ok_or(Error::BadResponse)?;
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| Error::BadResponse)?),
        None => (authority, INTERNET_DEFAULT_HTTPS_PORT),
    };

    let ua = wide(user_agent);
    let host = wide(host);
    let path = wide(path);
    let headers = wide(&format!(
        "Accept: {accept}\r\nX-GitHub-Api-Version: 2022-11-28\r\n"
    ));
    unsafe {
        let session = HInternet::new(WinHttpOpen(
            PCWSTR(ua.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))?;
        WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, 30_000)?;
        let connect = HInternet::new(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), port, 0))?;
        let request = HInternet::new(WinHttpOpenRequest(
            connect.0,
            windows::core::w!("GET"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        // 去掉结尾的 NUL，长度按字符数传。
        WinHttpAddRequestHeaders(
            request.0,
            &headers[..headers.len() - 1],
            WINHTTP_ADDREQ_FLAG_ADD,
        )?;
        WinHttpSendRequest(request.0, None, None, 0, 0, 0)?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())?;

        let number = |level: u32| -> Option<u64> {
            let mut v = 0u32;
            let mut len = size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                request.0,
                level | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut v as *mut u32 as *mut c_void),
                &mut len,
                std::ptr::null_mut(),
            )
            .ok()
            .map(|()| v as u64)
        };
        let status = number(WINHTTP_QUERY_STATUS_CODE).unwrap_or(0) as u16;
        match status {
            200 => {}
            403 | 429 => return Err(Error::RateLimited),
            s => return Err(Error::Http(s)),
        }
        let length = number(WINHTTP_QUERY_CONTENT_LENGTH);

        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(
                request.0,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut read,
            )?;
            if read == 0 {
                break;
            }
            sink(&buf[..read as usize], length)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: Config = Config {
        owner: "Jannchie",
        repo: "pulse",
        current_version: "0.3.1",
        asset_pattern: "pulse-{tag}-windows-x64.exe",
        checksums: "SHA256SUMS.txt",
        installer: Some(InstallerConfig {
            asset_pattern: "pulse-{tag}-windows-x64-setup.exe",
            uninstall_key: "{99CDD8A9-DE19-4475-9D3B-DEA7657484E0}_is1",
            args: "/VERYSILENT",
        }),
    };

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn compares_versions() {
        assert!(v("0.3.2") > v("0.3.1"));
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("0.3.1"), v("0.3.1"));
        assert_eq!(v("0.3.1+build.7"), v("0.3.1"));
        assert!(v("0.4.0-beta.1") < v("0.4.0"));
        assert!(v("0.4.0-beta.1") > v("0.3.9"));
        assert_eq!(v("1.2"), v("1.2.0"));
        for bad in ["", "x", "1.2.3.4", "1.-2.0", "1.2.3-", "v1.2.3"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn parses_checksums() {
        let a = "f25a6b9531396c89a3a71f343265311b3fa6b2227f328dbf09705385536353e6";
        let b = "00".repeat(32);
        let text = format!(
            "\u{feff}{a}  pulse-v0.3.1-windows-x64.exe\r\n{b} *pulse-v0.3.1-windows-x64.zip\r\n\r\n"
        );
        let got = parse_checksums(&text, "pulse-v0.3.1-windows-x64.exe").unwrap();
        assert_eq!(got[0], 0xf2);
        assert_eq!(got[31], 0xe6);
        assert_eq!(
            parse_checksums(&text, "PULSE-v0.3.1-windows-x64.zip"),
            Some([0; 32])
        );
        assert_eq!(parse_checksums(&text, "pulse-v0.3.2-windows-x64.exe"), None);
        // 哈希长度不对或含非十六进制字符时不认。
        assert_eq!(parse_checksums("abcd  x.exe\n", "x.exe"), None);
        let bad = format!("{}zz  x.exe", "0".repeat(62));
        assert_eq!(parse_checksums(&bad, "x.exe"), None);
    }

    fn api(tag: &str, assets: &[&str]) -> ApiRelease {
        ApiRelease {
            tag_name: tag.into(),
            body: Some("notes\r\nline 2".into()),
            html_url: format!("https://github.com/Jannchie/pulse/releases/tag/{tag}"),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|n| ApiAsset {
                    name: n.to_string(),
                    browser_download_url: format!("https://example.com/{n}"),
                    size: 10,
                })
                .collect(),
        }
    }

    #[test]
    fn selects_matching_asset() {
        let r = select_release(
            &CFG,
            api(
                "v0.3.2",
                &[
                    "pulse-v0.3.2-windows-x64.zip",
                    "pulse-v0.3.2-windows-x64.exe",
                    "pulse-v0.3.2-windows-x64-setup.exe",
                    "SHA256SUMS.txt",
                ],
            ),
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.version, "0.3.2");
        let exe = r.exe.unwrap();
        assert_eq!(exe.name, "pulse-v0.3.2-windows-x64.exe");
        assert_eq!(exe.url, "https://example.com/pulse-v0.3.2-windows-x64.exe");
        assert_eq!(r.setup.unwrap().name, "pulse-v0.3.2-windows-x64-setup.exe");
        assert_eq!(
            r.checksums_url.as_deref(),
            Some("https://example.com/SHA256SUMS.txt")
        );
        assert_eq!(r.notes, "notes\nline 2");

        // 没有 exe 也没有安装程序：仍然报告新版本，只能手动下载。
        let r = select_release(&CFG, api("v0.3.2", &["pulse-v0.3.2-windows-x64.zip"]))
            .unwrap()
            .unwrap();
        assert_eq!((r.exe, r.setup), (None, None));
        // 没有校验和文件也能选出来，安装时再报错。
        let r = select_release(&CFG, api("v0.4.0", &["pulse-v0.4.0-windows-x64.exe"]))
            .unwrap()
            .unwrap();
        assert_eq!(r.checksums_url, None);
    }

    #[test]
    fn ignores_old_and_prerelease() {
        let assets = ["pulse-v0.3.1-windows-x64.exe"];
        assert_eq!(select_release(&CFG, api("v0.3.1", &assets)), Ok(None));
        assert_eq!(select_release(&CFG, api("v0.2.9", &assets)), Ok(None));
        assert_eq!(select_release(&CFG, api("v0.4.0-rc.1", &assets)), Ok(None));
        let mut pre = api("v0.4.0", &["pulse-v0.4.0-windows-x64.exe"]);
        pre.prerelease = true;
        assert_eq!(select_release(&CFG, pre), Ok(None));
        assert_eq!(
            select_release(&CFG, api("latest", &assets)),
            Err(Error::BadResponse)
        );
    }

    #[test]
    fn chooses_install_method() {
        // 安装版优先用安装程序；所有用户安装要提权。
        assert_eq!(
            choose_method(Some(true), false, true, true),
            Method::Installer { elevate: true }
        );
        assert_eq!(
            choose_method(Some(false), true, true, true),
            Method::Installer { elevate: false }
        );
        // 安装版但发布里没有安装程序：目录可写就直接替换，否则手动。
        assert_eq!(
            choose_method(Some(true), true, true, false),
            Method::InPlace
        );
        assert_eq!(
            choose_method(Some(true), false, true, false),
            Method::Manual
        );
        // 便携版。
        assert_eq!(choose_method(None, true, true, true), Method::InPlace);
        assert_eq!(choose_method(None, false, true, true), Method::Manual);
        assert_eq!(choose_method(None, true, false, true), Method::Manual);
    }

    #[test]
    fn compares_install_dirs() {
        assert!(same_dir(
            r"C:\Program Files\Pulse\",
            r"C:\Program Files\Pulse"
        ));
        assert!(same_dir(
            r"c:\program files\pulse",
            r"C:\Program Files\Pulse"
        ));
        assert!(same_dir("C:/Tools/Pulse/", r"C:\Tools\Pulse"));
        assert!(!same_dir(
            r"C:\Program Files\Pulse",
            r"C:\Program Files\Pulse2"
        ));
        assert!(!same_dir("", r"C:\"));
    }

    #[test]
    fn truncates_long_notes() {
        let mut a = api("v0.3.2", &["pulse-v0.3.2-windows-x64.exe"]);
        a.body = Some("字".repeat(NOTES_LIMIT + 100));
        let r = select_release(&CFG, a).unwrap().unwrap();
        assert_eq!(r.notes.chars().count(), NOTES_LIMIT + 2);
        assert!(r.notes.ends_with('…'));
    }

    #[test]
    fn strips_markdown_from_notes() {
        let md = "## What's Changed\r\n* **ui**: add `--flag` by [@a](https://x)\r\n  - nested\r\n\r\n**Full Changelog**: https://x/compare/v1...v2";
        assert_eq!(
            plain_notes(md),
            "What's Changed\n• ui: add --flag by @a\n  • nested\n\nFull Changelog: https://x/compare/v1...v2\n"
        );
    }

    /// 访问真实的 GitHub API：`cargo test -p pulse -- --ignored live`。
    #[test]
    #[ignore]
    fn live_check_finds_newer_release() {
        let cfg = Config {
            current_version: "0.0.1",
            ..CFG
        };
        let r = check(&cfg).unwrap().expect("a release newer than 0.0.1");
        let exe = r.exe.clone().unwrap();
        assert!(exe.url.starts_with("https://"));
        let mut sums = Vec::new();
        http_get(
            r.checksums_url.as_deref().unwrap(),
            &cfg.user_agent(),
            "application/octet-stream",
            |c, _| {
                sums.extend_from_slice(c);
                Ok(())
            },
        )
        .unwrap();
        assert!(parse_checksums(&String::from_utf8_lossy(&sums), &exe.name).is_some());
    }

    #[test]
    fn hashes_sha256() {
        let mut h = Sha256::new().unwrap();
        h.update(b"a").unwrap();
        h.update(b"bc").unwrap();
        assert_eq!(
            h.finish().unwrap(),
            parse_hex32("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
                .unwrap()
        );
    }
}
