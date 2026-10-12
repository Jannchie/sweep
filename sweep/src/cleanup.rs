//! 清理建议：常见的可回收位置（临时文件、浏览器缓存、node_modules、包管理器缓存、回收站），
//! 以及只作提示的 Windows 更新缓存。在后台线程里计算：扫描结果里有的直接从树上取大小，
//! 不在扫描范围内的位置另外快速遍历一次。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use sweep_scan::{Progress, Tree, flags};

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

#[derive(Clone, Debug)]
pub struct Suggestion {
    pub icon: Icon,
    pub title: String,
    pub detail: String,
    pub size: u64,
    pub files: u64,
    /// 执行清理时要删除的对象。
    pub items: Vec<Item>,
    pub action: Action,
    /// 相关的文件夹（用于"在树中定位"）。
    pub folders: Vec<String>,
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// (标题, 图标, 说明, 文件夹列表)
fn candidates() -> Vec<(String, Icon, &'static str, Vec<String>)> {
    let local = env("LOCALAPPDATA");
    let home = env("USERPROFILE");
    let windir = env("WINDIR");
    let mut v = vec![
        (
            "用户临时文件".to_string(),
            Icon::Clock,
            "程序运行时留下的临时文件。正在使用的会被跳过。",
            vec![env("TEMP")],
        ),
        (
            "系统临时文件".to_string(),
            Icon::Clock,
            "Windows 与安装程序留下的临时文件，需要管理员权限才能全部清理。",
            vec![format!(r"{windir}\Temp")],
        ),
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
        v.push((
            format!("{name} 浏览器缓存"),
            Icon::Globe,
            "网页缓存，删除后再访问时会重新下载。请先关闭浏览器。",
            dirs,
        ));
    }
    let firefox: Vec<String> = subdirs(&format!(r"{local}\Mozilla\Firefox\Profiles"))
        .into_iter()
        .map(|p| format!(r"{p}\cache2"))
        .collect();
    v.push((
        "Firefox 浏览器缓存".into(),
        Icon::Globe,
        "网页缓存，删除后再访问时会重新下载。请先关闭浏览器。",
        firefox,
    ));
    for (name, desc, dirs) in [
        (
            "npm 缓存",
            "npm 下载过的包。删除后安装依赖时会重新下载。",
            vec![format!(r"{local}\npm-cache")],
        ),
        (
            "Yarn 缓存",
            "Yarn 下载过的包。删除后安装依赖时会重新下载。",
            vec![format!(r"{local}\Yarn\Cache")],
        ),
        (
            "pnpm 存储",
            "项目里的 node_modules 硬链接到这里，删除只能腾出已无项目使用的部分；也可以运行 pnpm store prune。",
            vec![format!(r"{local}\pnpm\store")],
        ),
        (
            "pip 缓存",
            "pip 下载与编译过的包。删除后安装时会重新下载。",
            vec![format!(r"{local}\pip\Cache")],
        ),
        (
            "uv 缓存",
            "uv 下载过的包与解释器。删除后会按需重新下载。",
            vec![format!(r"{local}\uv\cache")],
        ),
        (
            "Cargo 下载缓存",
            "crates.io 的压缩包与解压后的源码。删除后构建时会重新下载。",
            vec![
                format!(r"{home}\.cargo\registry\cache"),
                format!(r"{home}\.cargo\registry\src"),
            ],
        ),
        (
            "NuGet 包缓存",
            ".NET 项目用到的包。删除后还原时会重新下载。",
            vec![format!(r"{home}\.nuget\packages")],
        ),
        (
            "Gradle 缓存",
            "Gradle 下载的依赖与构建缓存。删除后构建时会重新下载。",
            vec![format!(r"{home}\.gradle\caches")],
        ),
        (
            "Hugging Face 缓存",
            "下载过的模型与数据集，通常很大。删除后用到时需要重新下载。",
            vec![format!(r"{home}\.cache\huggingface\hub")],
        ),
    ] {
        v.push((name.to_string(), Icon::Package, desc, dirs));
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
    for (title, icon, detail, dirs) in candidates() {
        let dirs: Vec<String> = dirs
            .into_iter()
            .filter(|d| !d.is_empty() && std::path::Path::new(d).is_dir())
            .collect();
        if dirs.is_empty() {
            continue;
        }
        let k = out.len();
        let mut s = Suggestion {
            icon,
            title,
            detail: detail.to_string(),
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
            icon: Icon::Package,
            title: format!("node_modules（{} 个项目）", items.len()),
            detail: "JavaScript 项目安装的依赖。删除后在项目里重新运行 npm install / pnpm install 即可恢复。"
                .into(),
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
            icon: Icon::Trash,
            title: "回收站".into(),
            detail: "所有驱动器回收站里的文件。清空后无法恢复。".into(),
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
            icon: Icon::Info,
            title: "Windows 更新缓存".into(),
            detail:
                "已下载的更新安装包。建议用系统的“磁盘清理”（清理系统文件）处理，不要直接删除。"
                    .into(),
            size: t.value(t.root),
            files: t.node(t.root).files as u64,
            items: Vec::new(),
            action: Action::Info(Some("cleanmgr.exe")),
            folders: vec![wu],
        });
    }
    out
}
