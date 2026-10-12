//! 清理建议：常见的可回收位置（临时文件、浏览器缓存、node_modules、包管理器缓存、回收站），
//! 以及只作提示的 Windows 更新缓存。在后台线程里计算：扫描结果里有的直接从树上取大小，
//! 不在扫描范围内的位置另外快速遍历一次。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use sweep_scan::{Progress, Tree, flags, t, tf};

use crate::icons_gen::Icon;

/// 一个可删除的对象。
#[derive(Clone, Debug)]
pub struct Item {
    pub path: String,
    pub size: u64,
    pub dir: bool,
    pub files: u64,
}

#[derive(Clone, Debug)]
pub enum Action {
    /// 删除这些文件夹里的内容（保留文件夹本身），例如临时文件夹。
    Contents,
    /// 删除这些文件夹本身，例如 node_modules。
    Items,
    EmptyRecycleBin,
    /// 只作提示；可以打开一个系统工具。
    Info(Option<&'static str>),
}

/// 清理建议的种类。标题和说明在显示时按当前语言生成，切换语言后立即生效。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    UserTemp,
    SystemTemp,
    /// 浏览器缓存（浏览器名）。
    Browser(&'static str),
    Npm,
    Yarn,
    Pnpm,
    Pip,
    Uv,
    Cargo,
    NuGet,
    Gradle,
    HuggingFace,
    NodeModules,
    RecycleBin,
    WindowsUpdate,
}

impl Kind {
    pub fn icon(self) -> Icon {
        match self {
            Kind::UserTemp | Kind::SystemTemp => Icon::Clock,
            Kind::Browser(_) => Icon::Globe,
            Kind::RecycleBin => Icon::Trash,
            Kind::WindowsUpdate => Icon::Info,
            _ => Icon::Package,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Suggestion {
    pub kind: Kind,
    pub size: u64,
    pub files: u64,
    /// 执行清理时要删除的对象。
    pub items: Vec<Item>,
    pub action: Action,
    /// 相关的文件夹（用于"在树中定位"）。
    pub folders: Vec<String>,
}

impl Suggestion {
    pub fn title(&self) -> String {
        match self.kind {
            Kind::UserTemp => t!(
                "用户临时文件",
                "User temporary files",
                "ユーザーの一時ファイル"
            )
            .into(),
            Kind::SystemTemp => t!(
                "系统临时文件",
                "System temporary files",
                "システムの一時ファイル"
            )
            .into(),
            Kind::Browser(name) => tf!("{name} 浏览器缓存", "{name} cache", "{name} のキャッシュ"),
            Kind::Npm => t!("npm 缓存", "npm cache", "npm キャッシュ").into(),
            Kind::Yarn => t!("Yarn 缓存", "Yarn cache", "Yarn キャッシュ").into(),
            Kind::Pnpm => t!("pnpm 存储", "pnpm store", "pnpm ストア").into(),
            Kind::Pip => t!("pip 缓存", "pip cache", "pip キャッシュ").into(),
            Kind::Uv => t!("uv 缓存", "uv cache", "uv キャッシュ").into(),
            Kind::Cargo => t!(
                "Cargo 下载缓存",
                "Cargo download cache",
                "Cargo ダウンロード キャッシュ"
            )
            .into(),
            Kind::NuGet => t!(
                "NuGet 包缓存",
                "NuGet package cache",
                "NuGet パッケージ キャッシュ"
            )
            .into(),
            Kind::Gradle => t!("Gradle 缓存", "Gradle cache", "Gradle キャッシュ").into(),
            Kind::HuggingFace => t!(
                "Hugging Face 缓存",
                "Hugging Face cache",
                "Hugging Face キャッシュ"
            )
            .into(),
            Kind::NodeModules => {
                let n = self.items.len();
                let s = if n == 1 { "" } else { "s" };
                tf!(
                    "node_modules（{n} 个项目）",
                    "node_modules ({n} project{s})",
                    "node_modules ({n} プロジェクト)"
                )
            }
            Kind::RecycleBin => t!("回收站", "Recycle Bin", "ごみ箱").into(),
            Kind::WindowsUpdate => t!(
                "Windows 更新缓存",
                "Windows Update cache",
                "Windows Update キャッシュ"
            )
            .into(),
        }
    }

    pub fn detail(&self) -> &'static str {
        match self.kind {
            Kind::UserTemp => t!(
                "程序运行时留下的临时文件。正在使用的会被跳过。",
                "Temporary files left by apps. Files in use are skipped.",
                "アプリが残した一時ファイルです。使用中のファイルはスキップされます。"
            ),
            Kind::SystemTemp => t!(
                "Windows 与安装程序留下的临时文件，需要管理员权限才能全部清理。",
                "Temporary files left by Windows and installers. Removing all of them requires administrator rights.",
                "Windows やインストーラーが残した一時ファイルです。すべて削除するには管理者権限が必要です。"
            ),
            Kind::Browser(_) => t!(
                "网页缓存，删除后再访问时会重新下载。请先关闭浏览器。",
                "Cached web content. Sites download it again on your next visit. Close the browser first.",
                "Web ページのキャッシュです。次回アクセス時に再ダウンロードされます。先にブラウザーを閉じてください。"
            ),
            Kind::Npm => t!(
                "npm 下载过的包。删除后安装依赖时会重新下载。",
                "Packages downloaded by npm. They're downloaded again when you install dependencies.",
                "npm がダウンロードしたパッケージです。依存関係のインストール時に再ダウンロードされます。"
            ),
            Kind::Yarn => t!(
                "Yarn 下载过的包。删除后安装依赖时会重新下载。",
                "Packages downloaded by Yarn. They're downloaded again when you install dependencies.",
                "Yarn がダウンロードしたパッケージです。依存関係のインストール時に再ダウンロードされます。"
            ),
            Kind::Pnpm => t!(
                "项目里的 node_modules 硬链接到这里，删除只能腾出已无项目使用的部分；也可以运行 pnpm store prune。",
                "Projects hard-link their node_modules here, so deleting only frees what no project uses. You can also run pnpm store prune.",
                "各プロジェクトの node_modules はここへのハード リンクのため、削除で空くのはどのプロジェクトも使っていない部分だけです。pnpm store prune も使えます。"
            ),
            Kind::Pip => t!(
                "pip 下载与编译过的包。删除后安装时会重新下载。",
                "Packages downloaded and built by pip. They're downloaded again on the next install.",
                "pip がダウンロード・ビルドしたパッケージです。次回のインストール時に再ダウンロードされます。"
            ),
            Kind::Uv => t!(
                "uv 下载过的包与解释器。删除后会按需重新下载。",
                "Packages and interpreters downloaded by uv. They're downloaded again when needed.",
                "uv がダウンロードしたパッケージとインタープリターです。必要に応じて再ダウンロードされます。"
            ),
            Kind::Cargo => t!(
                "crates.io 的压缩包与解压后的源码。删除后构建时会重新下载。",
                "Crate archives and unpacked sources from crates.io. They're downloaded again on the next build.",
                "crates.io のアーカイブと展開済みソースです。次回のビルド時に再ダウンロードされます。"
            ),
            Kind::NuGet => t!(
                ".NET 项目用到的包。删除后还原时会重新下载。",
                "Packages used by .NET projects. They're downloaded again on the next restore.",
                ".NET プロジェクトが使うパッケージです。次回の復元時に再ダウンロードされます。"
            ),
            Kind::Gradle => t!(
                "Gradle 下载的依赖与构建缓存。删除后构建时会重新下载。",
                "Dependencies and build cache downloaded by Gradle. They're downloaded again on the next build.",
                "Gradle がダウンロードした依存関係とビルド キャッシュです。次回のビルド時に再ダウンロードされます。"
            ),
            Kind::HuggingFace => t!(
                "下载过的模型与数据集，通常很大。删除后用到时需要重新下载。",
                "Downloaded models and datasets, often very large. They have to be downloaded again when used.",
                "ダウンロード済みのモデルとデータセットで、通常かなり大きくなります。使用時に再ダウンロードが必要です。"
            ),
            Kind::NodeModules => t!(
                "JavaScript 项目安装的依赖。删除后在项目里重新运行 npm install / pnpm install 即可恢复。",
                "Dependencies installed by JavaScript projects. Run npm install / pnpm install in a project to restore them.",
                "JavaScript プロジェクトにインストールされた依存関係です。プロジェクトで npm install / pnpm install を実行すれば元に戻せます。"
            ),
            Kind::RecycleBin => t!(
                "所有驱动器回收站里的文件。清空后无法恢复。",
                "Files in the Recycle Bin on all drives. They can't be restored after you empty it.",
                "すべてのドライブのごみ箱にあるファイルです。空にすると復元できません。"
            ),
            Kind::WindowsUpdate => t!(
                "已下载的更新安装包。建议用系统的“磁盘清理”（清理系统文件）处理，不要直接删除。",
                "Downloaded update packages. Use Disk Cleanup (Clean up system files) instead of deleting them directly.",
                "ダウンロード済みの更新プログラムです。直接削除せず、ディスク クリーンアップ (システム ファイルのクリーンアップ) を使ってください。"
            ),
        }
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// (种类, 文件夹列表)
fn candidates() -> Vec<(Kind, Vec<String>)> {
    let local = env("LOCALAPPDATA");
    let home = env("USERPROFILE");
    let windir = env("WINDIR");
    let mut v = vec![
        (Kind::UserTemp, vec![env("TEMP")]),
        (Kind::SystemTemp, vec![format!(r"{windir}\Temp")]),
    ];
    // 浏览器缓存：每个配置文件下的 Cache、Code Cache、GPUCache。
    for (name, base) in [
        ("Chrome", format!(r"{local}\Google\Chrome\User Data")),
        ("Edge", format!(r"{local}\Microsoft\Edge\User Data")),
        (
            "Brave",
            format!(r"{local}\BraveSoftware\Brave-Browser\User Data"),
        ),
    ] {
        let mut dirs = Vec::new();
        for profile in subdirs(&base) {
            for sub in ["Cache", "Code Cache", "GPUCache"] {
                dirs.push(format!(r"{profile}\{sub}"));
            }
        }
        v.push((Kind::Browser(name), dirs));
    }
    let firefox: Vec<String> = subdirs(&format!(r"{local}\Mozilla\Firefox\Profiles"))
        .into_iter()
        .map(|p| format!(r"{p}\cache2"))
        .collect();
    v.push((Kind::Browser("Firefox"), firefox));
    for (kind, dirs) in [
        (Kind::Npm, vec![format!(r"{local}\npm-cache")]),
        (Kind::Yarn, vec![format!(r"{local}\Yarn\Cache")]),
        (Kind::Pnpm, vec![format!(r"{local}\pnpm\store")]),
        (Kind::Pip, vec![format!(r"{local}\pip\Cache")]),
        (Kind::Uv, vec![format!(r"{local}\uv\cache")]),
        (
            Kind::Cargo,
            vec![
                format!(r"{home}\.cargo\registry\cache"),
                format!(r"{home}\.cargo\registry\src"),
            ],
        ),
        (Kind::NuGet, vec![format!(r"{home}\.nuget\packages")]),
        (Kind::Gradle, vec![format!(r"{home}\.gradle\caches")]),
        (
            Kind::HuggingFace,
            vec![format!(r"{home}\.cache\huggingface\hub")],
        ),
    ] {
        v.push((kind, dirs));
    }
    v
}

fn subdirs(path: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path().to_string_lossy().to_string())
        .collect()
}

/// 一个文件夹的 (大小, 文件数, 直接子项)。
fn measure(t: &Tree, n: u32) -> (u64, u64, Vec<Item>) {
    let items = t
        .children(n)
        .map(|c| Item {
            path: t.path(c),
            size: t.value(c),
            dir: t.node(c).is_dir(),
            files: t.node(c).files as u64,
        })
        .collect();
    (t.value(n), t.node(n).files as u64, items)
}

/// 计算全部清理建议，大的在前，只作提示的放最后。
pub fn compute(tree: Arc<Tree>) -> Vec<Suggestion> {
    let by_alloc = tree.by_alloc;
    let mut out = Vec::new();
    let mut outside: Vec<(usize, String)> = Vec::new();

    // 第一步：扫描结果里能找到的直接取。
    for (kind, dirs) in candidates() {
        let dirs: Vec<String> = dirs
            .into_iter()
            .filter(|d| !d.is_empty() && std::path::Path::new(d).is_dir())
            .collect();
        if dirs.is_empty() {
            continue;
        }
        let k = out.len();
        let mut s = Suggestion {
            kind,
            size: 0,
            files: 0,
            items: Vec::new(),
            action: Action::Contents,
            folders: dirs.clone(),
        };
        for d in dirs {
            match tree.find(&d) {
                Some(n) if !tree.node(n).has(flags::DENIED) => {
                    let (size, files, items) = measure(&tree, n);
                    s.size += size;
                    s.files += files;
                    s.items.extend(items);
                }
                _ => outside.push((k, d)),
            }
        }
        out.push(s);
    }

    // node_modules：只取最外层的（里面嵌套的随外层一起删）。
    let mut found = Vec::new();
    let mut stack = vec![tree.root];
    while let Some(n) = stack.pop() {
        for c in tree.children(n) {
            let node = tree.node(c);
            if !node.is_dir() || node.has(flags::REPARSE) {
                continue;
            }
            if tree.name(c).eq_ignore_ascii_case("node_modules") {
                found.push(c);
            } else {
                stack.push(c);
            }
        }
    }
    if !found.is_empty() {
        let items: Vec<Item> = found
            .iter()
            .map(|&n| Item {
                path: tree.path(n),
                size: tree.value(n),
                dir: true,
                files: tree.node(n).files as u64,
            })
            .collect();
        out.push(Suggestion {
            kind: Kind::NodeModules,
            size: items.iter().map(|i| i.size).sum(),
            files: items.iter().map(|i| i.files).sum(),
            folders: items.iter().take(1).map(|i| i.path.clone()).collect(),
            items,
            action: Action::Items,
        });
    }
    // 后面的遍历可能要几秒，先放开对树的引用，好让 UI 线程能修改它。
    drop(tree);

    // 第二步：扫描范围以外的位置单独遍历。
    for (k, d) in outside {
        let Ok(t) =
            sweep_scan::walk::scan(&d, by_alloc, &Progress::default(), &AtomicBool::new(false))
        else {
            continue;
        };
        let (size, files, items) = measure(&t, t.root);
        let s = &mut out[k];
        s.size += size;
        s.files += files;
        s.items.extend(items);
    }
    out.retain(|s| s.size > 0);

    if let Some((size, n)) = crate::os::recycle_bin()
        && n > 0
    {
        out.push(Suggestion {
            kind: Kind::RecycleBin,
            size,
            files: n,
            items: Vec::new(),
            action: Action::EmptyRecycleBin,
            folders: Vec::new(),
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.size));

    // Windows 更新缓存：只提示，用系统的磁盘清理处理更安全。
    let wu = format!(r"{}\SoftwareDistribution\Download", env("WINDIR"));
    if let Ok(t) =
        sweep_scan::walk::scan(&wu, by_alloc, &Progress::default(), &AtomicBool::new(false))
        && t.value(t.root) > 0
    {
        out.push(Suggestion {
            kind: Kind::WindowsUpdate,
            size: t.value(t.root),
            files: t.node(t.root).files as u64,
            items: Vec::new(),
            action: Action::Info(Some("cleanmgr.exe")),
            folders: vec![wu],
        });
    }
    out
}
