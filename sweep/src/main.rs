#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analyze;
mod apps;
mod apps_view;
mod brand_art;
mod cleanup;
mod cleanup_view;
mod dialogs;
mod doc;
mod file_icons;
mod fonts;
mod icons_gen;
mod menu;
mod os;
mod scan_view;
mod selection;
mod settings;
mod settings_view;
mod start_view;
mod theme;
mod titlebar;
mod treemap;
mod update;
mod update_view;
mod widgets;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Frame, Key, Layout, Margin, RichText, Sense, Ui, pos2, vec2};
use sweep_scan::system::Volume;
use sweep_scan::{NONE, Scan, flags, t, tf};

use cleanup::Item;
use dialogs::{Decision, DeleteRequest, Target, Toast};
use doc::{Doc, Mutation};
use file_icons::FileIcons;
use icons_gen::Icon;
use settings::{Basis, Settings};
use theme::Palette;
use treemap::Treemap;

fn main() -> eframe::Result {
    // 默认以管理员身份运行：不是管理员时请求提权，以新进程重新启动自己。
    // 在 UAC 提示里拒绝（或没有管理员账户）时照常以普通权限运行。
    if Settings::load().elevate && !sweep_scan::system::is_elevated() {
        let args: Vec<String> = std::env::args()
            .skip(1)
            // 结尾的反斜杠会把收尾的引号转义掉（"C:\" → C:"），先去掉。
            .map(|a| format!("\"{}\"", a.trim_end_matches('\\')))
            .collect();
        if os::relaunch_elevated(&args.join(" ")) {
            return Ok(());
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Sweep")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([980.0, 640.0])
            // 先给一个图标，免得出现 eframe 默认的 egui 标志；窗口建好后 titlebar 会换成 exe 资源里的多尺寸图标。
            .with_icon(Arc::new(egui::IconData {
                rgba: brand_art::brand_rgba(64),
                width: 64,
                height: 64,
            })),
        // NVIDIA 等驱动的 OpenGL vsync 在 SwapBuffers 里忙等，鼠标划过列表时能吃满一个核。
        // 窗口由 DWM 合成本就不会撕裂，所以直接关掉。重绘只由输入事件触发（见 theme 里关掉的动画），
        // 不会空转。
        glow_options: eframe::egui_glow::GlowConfiguration {
            vsync: false,
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "Sweep",
        options,
        Box::new(|cc| {
            titlebar::install(cc);
            Ok(Box::new(App::new(&cc.egui_ctx)))
        }),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Disks,
    Analyze,
    Cleanup,
    Apps,
    Settings,
}

/// 各页面产生的操作，统一由 [`App::apply`] 执行。
/// 单击发生在哪个列表里：Shift 范围选择按这个列表当前的顺序计算。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum List {
    /// 分析页的目录树（搜索时是搜索结果）。
    Tree,
    /// 清理页的"最大的文件"。
    Largest,
    /// 矩形树图：没有行的顺序，只支持单击与 Ctrl+单击。
    Treemap,
}

pub enum Action {
    /// 在列表里单击一项（带修饰键），按资源管理器的规则改选中项。
    Click {
        node: u32,
        list: List,
        ctrl: bool,
        shift: bool,
    },
    Toggle(u32),
    /// 矩形树图放大到这个目录（文件取所在目录）。
    ZoomTo(u32),
    ZoomOut,
    /// 切到分析页，在树里展开并选中。
    Locate(u32),
    LocatePath(String),
    /// 右键菜单。右键的项没被选中时先只选它；已选中时菜单作用于整个选择。
    OpenMenu(u32, egui::Pos2),
    Reveal(String),
    Open(String),
    /// 用默认程序打开文件（没有关联程序时弹出"打开方式"）。
    OpenFile(u32),
    /// 复制文字到剪贴板。
    Copy(String),
    Properties(String),
    /// 弹出删除确认框。
    Delete {
        items: Vec<Item>,
        label: String,
        permanent: bool,
    },
    /// 删除当前选中的项（规范化、排除不能删除的之后弹出确认框）。
    DeleteSelection {
        permanent: bool,
    },
    /// Esc：取消全部选择。
    ClearSelection,
    /// Ctrl+A。
    SelectAll(List),
    Rescan(u32),
    EmptyRecycleBin(u64),
    Run(String),
    Highlight(Option<u16>),
    Scan(String),
    PickFolder,
    Elevate,
    Cancel,
    Refresh,
}

enum Modal {
    Delete(DeleteRequest),
    EmptyRecycleBin(u64),
    Update(Arc<update::Release>),
    Uninstall(Vec<apps::InstalledApp>),
}

/// 后台任务的结果。
enum Job {
    Toast(Toast),
    Deleted {
        /// 发起删除时的 Doc 编号；与当前的不同时节点下标作废，按路径查找。
        doc: u64,
        /// (节点下标或 NONE, 路径, 结果)
        results: Vec<(u32, String, Result<(), String>)>,
        permanent: bool,
        size: u64,
    },
    Rescanned {
        node: u32,
        tree: Box<sweep_scan::Tree>,
    },
    Picked(String),
    Volumes(Vec<Volume>),
    RecycleEmptied(Result<(), String>),
    /// 没有需要处理的结果。
    Nothing,
}

struct App {
    view: View,
    settings: Settings,
    elevated: bool,
    volumes: Vec<Volume>,
    volumes_at: Option<Instant>,
    volumes_loading: bool,
    scan: Option<Scan>,
    scan_error: Option<String>,
    doc: Option<Doc>,
    treemap: Treemap,
    icons: FileIcons,
    /// 打开着的右键菜单：针对的节点与位置。
    menu: Option<(u32, egui::Pos2)>,
    modal: Option<Modal>,
    toasts: Vec<Toast>,
    jobs: (Sender<Job>, Receiver<Job>),
    /// 正在后台删除（期间禁用删除按钮）。
    deleting: bool,
    path_input: String,
    focus_search: bool,
    /// 最近一秒内的重绘时刻，用来在持续高帧率输入时限帧。
    passes: VecDeque<Instant>,
    /// 上一帧显示的页面，用来发现切到了清理页。
    last_view: View,
    /// 增量扫描缓存占用的空间；None 表示需要重新统计（打开设置页、扫描、清除后）。
    cache_usage: Option<u64>,
    apps: apps_view::AppsPage,
    updater: update::Updater,
    /// 本帧开始时读到的更新状态。
    update: update::Status,
    /// 已经弹过通知的新版本。
    update_announced: Option<String>,
    /// 新版本已启动，正在关闭本窗口。
    restarting: bool,
}

/// 本程序在 GitHub 上的发布信息。
const UPDATE_CONFIG: update::Config = update::Config {
    owner: "Jannchie",
    repo: "sweep",
    current_version: env!("CARGO_PKG_VERSION"),
    asset_pattern: "sweep-{tag}-windows-x64.exe",
    checksums: "SHA256SUMS.txt",
    // 与 installer/sweep.iss 的 AppId 对应。
    installer: Some(update::InstallerConfig {
        asset_pattern: "sweep-{tag}-windows-x64-setup.exe",
        uninstall_key: "{9B83B075-7D2D-49D9-8547-11D6640C0858}_is1",
        args: "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /LAUNCH=1",
    }),
};

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let settings = Settings::load();
        theme::install(ctx);
        apply_language(ctx, settings.language);
        egui_extras::install_image_loaders(ctx);
        ctx.set_theme(settings.theme.preference());
        update::cleanup_old(&UPDATE_CONFIG);
        let updater = {
            let ctx = ctx.clone();
            update::Updater::new(UPDATE_CONFIG, move || ctx.request_repaint())
        };
        updater.start(settings.auto_update);
        let mut app = Self {
            view: View::Disks,
            settings,
            elevated: sweep_scan::system::is_elevated(),
            volumes: Vec::new(),
            volumes_at: None,
            volumes_loading: false,
            scan: None,
            scan_error: None,
            doc: None,
            treemap: Treemap::new(ctx),
            icons: FileIcons::new(ctx),
            menu: None,
            modal: None,
            toasts: Vec::new(),
            jobs: channel(),
            deleting: false,
            path_input: String::new(),
            focus_search: false,
            passes: VecDeque::new(),
            last_view: View::Disks,
            cache_usage: None,
            apps: apps_view::AppsPage::default(),
            updater,
            update: update::Status::default(),
            update_announced: None,
            restarting: false,
        };
        app.load_volumes(ctx);
        // 命令行参数作为扫描目标，例如 `sweep D:\data`。
        let arg: Vec<String> = std::env::args().skip(1).collect();
        if !arg.is_empty() {
            app.start_scan(ctx, &arg.join(" "));
        }
        app
    }

    /// 卷信息可能在网络盘上卡住，放到后台读。
    fn load_volumes(&mut self, ctx: &egui::Context) {
        if self.volumes_loading {
            return;
        }
        self.volumes_loading = true;
        self.spawn(ctx, || Job::Volumes(sweep_scan::system::volumes()));
    }

    fn spawn(&self, ctx: &egui::Context, job: impl FnOnce() -> Job + Send + 'static) {
        let (tx, ctx) = (self.jobs.0.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(job());
            ctx.request_repaint();
        });
    }

    fn start_scan(&mut self, ctx: &egui::Context, target: &str) {
        if let Some(s) = &self.scan {
            s.cancel();
        }
        let by_alloc = self.settings.basis == Basis::Allocated;
        let c = ctx.clone();
        self.scan = Some(Scan::start(
            target,
            self.settings.engine.engine(),
            by_alloc,
            self.settings.incremental,
            move || c.request_repaint(),
        ));
        self.cache_usage = None;
        self.scan_error = None;
        self.view = View::Analyze;
        self.menu = None;
    }

    fn poll_scan(&mut self, ctx: &egui::Context) {
        let Some(scan) = &self.scan else { return };
        let Some(result) = scan.poll() else { return };
        let target = scan.target.clone();
        self.scan = None;
        match result {
            Ok(out) => {
                self.treemap.reset();
                let mut doc = Doc::new(out, target);
                // 扫描期间改过大小口径设置的话，按新设置重新排序。
                doc.set_basis(self.settings.basis == Basis::Allocated, ctx);
                self.doc = Some(doc);
            }
            Err(e) => self.scan_error = Some(e),
        }
    }

    fn notify_ok(&mut self, title: String, detail: Option<String>) {
        self.toasts.push(Toast::ok(title, detail));
    }

    fn notify_err(&mut self, title: String, detail: Option<String>) {
        self.toasts.push(Toast::err(title, detail));
    }

    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::Click {
                node,
                list,
                ctrl,
                shift,
            } => {
                let Some(d) = &mut self.doc else { return };
                let order = match list {
                    List::Tree => d.visible(),
                    List::Largest => d.largest(),
                    List::Treemap => Vec::new(),
                };
                match order.iter().position(|&n| n == node) {
                    Some(i) => d.selection.click(|k| order[k], order.len(), i, ctrl, shift),
                    None if ctrl => d.selection.toggle(node),
                    None => d.selection.set_single(node),
                }
            }
            Action::ClearSelection => {
                if let Some(d) = &mut self.doc {
                    d.selection.clear();
                }
            }
            Action::SelectAll(list) => {
                if let Some(d) = &mut self.doc {
                    match list {
                        List::Largest => {
                            let all = d.largest();
                            d.selection.select_all(all);
                        }
                        _ => d.select_all_in_view(),
                    }
                }
            }
            Action::Toggle(n) => {
                if let Some(d) = &mut self.doc {
                    d.toggle(n);
                }
            }
            Action::ZoomTo(n) => {
                if let Some(d) = &mut self.doc {
                    d.zoom_to(n);
                }
            }
            Action::ZoomOut => {
                if let Some(d) = &mut self.doc {
                    d.zoom_out();
                }
            }
            Action::Locate(n) => {
                if let Some(d) = &mut self.doc {
                    d.search.query.clear();
                    d.reveal(n);
                    self.view = View::Analyze;
                }
            }
            Action::LocatePath(p) => {
                if let Some(d) = &mut self.doc
                    && let Some(n) = d.tree.find(&p)
                {
                    d.search.query.clear();
                    d.reveal(n);
                    self.view = View::Analyze;
                } else {
                    os::reveal(&p);
                }
            }
            Action::OpenMenu(n, pos) => {
                if let Some(d) = &mut self.doc
                    && !d.selection.contains(n)
                {
                    d.selection.set_single(n);
                }
                self.menu = Some((n, pos));
            }
            Action::Reveal(p) => os::reveal(&p),
            Action::Open(p) => os::open(&p),
            Action::OpenFile(n) => {
                let Some(d) = &self.doc else { return };
                let path = d.tree.path(n);
                let name = d.tree.name(n).to_string();
                self.spawn(ctx, move || match os::open_file(&path) {
                    Ok(()) => Job::Nothing,
                    Err(e) => Job::Toast(Toast::err(
                        tf!(
                            "无法打开 {name}",
                            "Couldn't open {name}",
                            "{name} を開けませんでした"
                        ),
                        Some(e),
                    )),
                });
            }
            Action::Copy(text) => {
                ctx.copy_text(text.clone());
                // 多行（多选时的路径列表）只报行数，不把整段塞进通知。
                let lines = text.lines().count() as u64;
                let detail = if lines > 1 { n_items(lines) } else { text };
                self.notify_ok(
                    t!("已复制", "Copied", "コピーしました").into(),
                    Some(detail),
                );
            }
            Action::Properties(p) => os::properties(&p),
            Action::Delete {
                items,
                label,
                permanent,
            } => {
                if self.deleting {
                    self.notify_err(
                        t!(
                            "上一次删除还没有完成",
                            "The previous deletion is still running",
                            "前回の削除がまだ完了していません"
                        )
                        .into(),
                        None,
                    );
                } else if !items.is_empty() {
                    self.modal = Some(Modal::Delete(DeleteRequest {
                        items: items
                            .into_iter()
                            .map(|i| Target {
                                node: NONE,
                                path: i.path,
                                size: i.size,
                                dir: i.dir,
                                files: i.files,
                            })
                            .collect(),
                        label,
                        permanent,
                        excluded: 0,
                        doc: 0,
                    }));
                }
            }
            Action::DeleteSelection { permanent } => self.delete_selection(permanent),
            Action::Rescan(n) => self.rescan(ctx, n),
            Action::EmptyRecycleBin(size) => self.modal = Some(Modal::EmptyRecycleBin(size)),
            Action::Run(cmd) => {
                let _ = std::process::Command::new(&cmd).spawn();
            }
            Action::Highlight(e) => {
                if let Some(d) = &mut self.doc {
                    d.highlight_ext = e;
                }
            }
            Action::Scan(target) => self.start_scan(ctx, &target),
            Action::PickFolder => self.spawn(ctx, || match os::pick_folder() {
                Some(p) => Job::Picked(p),
                None => Job::Volumes(Vec::new()),
            }),
            Action::Elevate => {
                let args = self
                    .doc
                    .as_ref()
                    .map(|d| format!("\"{}\"", d.target.trim_end_matches('\\')))
                    .unwrap_or_default();
                if os::relaunch_elevated(&args) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            Action::Cancel => {
                if let Some(s) = &self.scan {
                    s.cancel();
                }
            }
            Action::Refresh => {
                if let Some(d) = &self.doc {
                    let t = d.target.clone();
                    self.start_scan(ctx, &t);
                }
            }
        }
    }

    /// Delete / Shift+Delete、工具栏和右键菜单的删除：规范化选中项、排除不能删除的，弹出确认框。
    fn delete_selection(&mut self, permanent: bool) {
        if self.deleting {
            self.notify_err(
                t!(
                    "上一次删除还没有完成",
                    "The previous deletion is still running",
                    "前回の削除がまだ完了していません"
                )
                .into(),
                None,
            );
            return;
        }
        let Some(d) = &self.doc else { return };
        if d.selection.is_empty() {
            return;
        }
        let t = &d.tree;
        let (nodes, excluded) = d.selection.deletion_set(t, deletable);
        if nodes.is_empty() {
            self.notify_err(
                t!(
                    "选中的项目不能删除",
                    "The selected items can't be deleted",
                    "選択した項目は削除できません"
                )
                .into(),
                Some(
                    t!(
                        "驱动器或扫描的根文件夹、NTFS 元数据不能删除。",
                        "Drive roots, the scanned folder itself and NTFS metadata can't be deleted.",
                        "ドライブやスキャンしたフォルダー自体、NTFS メタデータは削除できません。"
                    )
                    .into(),
                ),
            );
            return;
        }
        let label = match nodes.as_slice() {
            [n] => t.name(*n).to_string(),
            v => n_items(v.len() as u64),
        };
        let items = nodes
            .iter()
            .map(|&n| Target {
                node: n,
                path: t.path(n),
                size: t.value(n),
                dir: t.node(n).is_dir(),
                files: t.node(n).files as u64,
            })
            .collect();
        self.modal = Some(Modal::Delete(DeleteRequest {
            items,
            label,
            permanent,
            excluded,
            doc: d.id,
        }));
    }

    /// 重新扫描一个目录，完成后替换树里的这一段。
    fn rescan(&mut self, ctx: &egui::Context, n: u32) {
        let Some(d) = &self.doc else { return };
        let path = d.tree.path(n);
        let by_alloc = d.tree.by_alloc;
        let name = d.tree.name(n);
        self.toasts.push(
            Toast::ok(
                tf!(
                    "正在重新扫描 {name}…",
                    "Rescanning {name}…",
                    "{name} を再スキャンしています…"
                ),
                None,
            )
            .tag(TAG_RESCAN),
        );
        self.spawn(ctx, move || {
            match sweep_scan::walk::scan(
                &path,
                by_alloc,
                &sweep_scan::Progress::default(),
                &std::sync::atomic::AtomicBool::new(false),
            ) {
                Ok(tree) => Job::Rescanned {
                    node: n,
                    tree: Box::new(tree),
                },
                Err(e) => Job::Toast(Toast::err(
                    tf!(
                        "无法重新扫描 {path}",
                        "Couldn't rescan {path}",
                        "{path} を再スキャンできませんでした"
                    ),
                    Some(e),
                )),
            }
        });
    }

    fn confirm_modal(&mut self, ctx: &egui::Context, modal: Modal) {
        match modal {
            Modal::Delete(req) => {
                let permanent = req.permanent;
                let size = req.total();
                let doc = req.doc;
                let targets: Vec<(u32, String)> =
                    req.items.into_iter().map(|i| (i.node, i.path)).collect();
                self.deleting = true;
                let title = if permanent {
                    t!("正在删除…", "Deleting…", "削除しています…")
                } else {
                    t!(
                        "正在移到回收站…",
                        "Moving to the Recycle Bin…",
                        "ごみ箱に移動しています…"
                    )
                };
                self.toasts
                    .push(Toast::ok(title.into(), None).tag(TAG_DELETE));
                self.spawn(ctx, move || {
                    let results = targets
                        .into_iter()
                        .map(|(n, p)| {
                            let r = os::delete(&p, permanent);
                            (n, p, r)
                        })
                        .collect();
                    Job::Deleted {
                        doc,
                        results,
                        permanent,
                        size,
                    }
                });
            }
            Modal::Update(_) => {}
            Modal::Uninstall(list) => self.apps.start_uninstall(ctx, list, self.elevated),
            Modal::EmptyRecycleBin(_) => {
                self.spawn(ctx, || Job::RecycleEmptied(os::empty_recycle_bin()));
            }
        }
    }

    fn poll_jobs(&mut self, ctx: &egui::Context) {
        while let Ok(job) = self.jobs.1.try_recv() {
            match job {
                Job::Toast(t) => self.toasts.push(t),
                Job::Volumes(v) => {
                    // 选择文件夹被取消时也会发一个空列表，不覆盖已有数据。
                    if !v.is_empty() {
                        self.volumes = v;
                        self.volumes_at = Some(Instant::now());
                        self.volumes_loading = false;
                    }
                }
                Job::Picked(p) => self.start_scan(ctx, &p),
                Job::Nothing => {}
                Job::Rescanned { node, tree } => {
                    self.toasts.retain(|t| t.tag != TAG_RESCAN);
                    if let Some(d) = &mut self.doc {
                        let name = d.tree.name(node).to_string();
                        d.mutate(Mutation::Graft(node, tree));
                        self.notify_ok(
                            tf!(
                                "已重新扫描 {name}",
                                "Rescanned {name}",
                                "{name} を再スキャンしました"
                            ),
                            None,
                        );
                    }
                }
                Job::Deleted {
                    doc,
                    results,
                    permanent,
                    size,
                } => self.after_delete(ctx, doc, results, permanent, size),
                Job::RecycleEmptied(r) => {
                    match r {
                        Ok(()) => self.notify_ok(
                            t!(
                                "已清空回收站",
                                "Recycle Bin emptied",
                                "ごみ箱を空にしました"
                            )
                            .into(),
                            None,
                        ),
                        Err(e) => self.notify_err(
                            t!(
                                "无法清空回收站",
                                "Couldn't empty the Recycle Bin",
                                "ごみ箱を空にできませんでした"
                            )
                            .into(),
                            Some(e),
                        ),
                    }
                    self.after_space_change(ctx);
                }
            }
        }
    }

    /// 删除完成：删掉的一次性从树上摘除（选中项随之去掉），没删干净的目录重新扫描，不必整盘重扫。
    fn after_delete(
        &mut self,
        ctx: &egui::Context,
        doc: u64,
        results: Vec<(u32, String, Result<(), String>)>,
        permanent: bool,
        size: u64,
    ) {
        self.deleting = false;
        self.toasts.retain(|t| t.tag != TAG_DELETE);
        let failed: Vec<(&String, &String)> = results
            .iter()
            .filter_map(|r| r.2.as_ref().err().map(|e| (&r.1, e)))
            .collect();
        let mut rescan = Vec::new();
        if let Some(d) = &mut self.doc {
            let mut removed = Vec::new();
            for (node, path, r) in &results {
                // 同一棵树、节点还在时直接用下标（十万项也不用逐个按路径查找），否则按路径找。
                let same = d.id == doc && *node != NONE;
                let n = if same && !d.tree.node(*node).has(flags::DELETED) {
                    Some(*node)
                } else {
                    d.tree.find(path)
                };
                let Some(n) = n else { continue };
                match r {
                    Ok(()) => removed.push(n),
                    Err(_) if d.tree.node(n).is_dir() => rescan.push(n),
                    Err(_) => {}
                }
            }
            if !removed.is_empty() {
                d.mutate(Mutation::Remove(removed));
            }
        }
        for n in rescan {
            self.rescan(ctx, n);
        }
        let done = (results.len() - failed.len()) as u64;
        let n = n_items(done);
        let failures = failed.len();
        match failed.first() {
            None => {
                let title = if permanent {
                    tf!(
                        "已永久删除 {n}",
                        "Permanently deleted {n}",
                        "{n}を完全に削除しました"
                    )
                } else {
                    tf!(
                        "已将 {n}移到回收站",
                        "Moved {n} to the Recycle Bin",
                        "{n}をごみ箱に移動しました"
                    )
                };
                let size = fmt_bytes(size);
                self.notify_ok(
                    title,
                    Some(tf!("共 {size}", "{size} in total", "合計 {size}")),
                );
            }
            Some((p, e)) => {
                let title = match (done, permanent) {
                    (0, true) => t!("无法删除", "Couldn't delete", "削除できませんでした").into(),
                    (0, false) => t!(
                        "无法移到回收站",
                        "Couldn't move to the Recycle Bin",
                        "ごみ箱に移動できませんでした"
                    )
                    .into(),
                    (_, true) => tf!(
                        "已永久删除 {n}，{failures} 项失败",
                        "Permanently deleted {n}; {failures} failed",
                        "{n}を完全に削除しました ({failures} 個は失敗)"
                    ),
                    (_, false) => tf!(
                        "已将 {n}移到回收站，{failures} 项失败",
                        "Moved {n} to the Recycle Bin; {failures} failed",
                        "{n}をごみ箱に移動しました ({failures} 個は失敗)"
                    ),
                };
                self.notify_err(title, Some(format!("{p}: {e}")));
            }
        }
        self.after_space_change(ctx);
    }

    /// 磁盘空间变了：刷新卷信息，清理建议在下次打开清理页时重新计算。
    fn after_space_change(&mut self, ctx: &egui::Context) {
        self.volumes_at = None;
        self.load_volumes(ctx);
    }

    /// 分析页和清理页共用的快捷键：Delete / Shift+Delete、Ctrl+A、Esc、Ctrl+C。
    /// 只在没有输入焦点、对话框和菜单时调用。
    fn selection_keys(&mut self, ctx: &egui::Context, list: List) -> Option<Action> {
        let d = self.doc.as_ref()?;
        let (del, all, esc, copy) = ctx.input(|i| {
            (
                delete_shortcut(i),
                i.modifiers.command && i.key_pressed(Key::A),
                i.key_pressed(Key::Escape),
                copy_shortcut(i),
            )
        });
        if let Some(permanent) = del {
            return Some(Action::DeleteSelection { permanent });
        }
        if all {
            return Some(Action::SelectAll(list));
        }
        if esc && !d.selection.is_empty() {
            return Some(Action::ClearSelection);
        }
        if copy && !d.selection.is_empty() {
            let t = &d.tree;
            let paths: Vec<String> = d.selection.items().iter().map(|&n| t.path(n)).collect();
            return Some(Action::Copy(paths.join("\r\n")));
        }
        None
    }

    /// 方向键、Enter、Backspace 等。只在分析页、没有输入焦点和对话框时生效。
    /// 单项操作（Enter、F5、←/→）作用于键盘光标所在的项。
    fn keyboard(&mut self, ctx: &egui::Context) -> Option<Action> {
        if let Some(a) = self.selection_keys(ctx, List::Tree) {
            return Some(a);
        }
        let d = self.doc.as_mut()?;
        let (up, down, home, end, pgup, pgdn) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::Home),
                i.key_pressed(Key::End),
                i.key_pressed(Key::PageUp),
                i.key_pressed(Key::PageDown),
            )
        });
        let (left, right, enter, back, shift, f5) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Backspace),
                i.modifiers.shift,
                i.key_pressed(Key::F5),
            )
        });
        let sel = d.selection.focused_selected();
        if f5 {
            return sel.filter(|&n| d.tree.node(n).is_dir()).map(Action::Rescan);
        }
        if enter {
            // 与双击一致：文件用默认程序打开，文件夹在矩形树图中放大。
            return sel.map(|n| {
                if d.tree.node(n).is_dir() {
                    Action::ZoomTo(n)
                } else {
                    Action::OpenFile(n)
                }
            });
        }
        if back {
            return Some(Action::ZoomOut);
        }
        if d.search.active() {
            return None;
        }
        let rows = d.rows.len();
        if rows == 0 {
            return None;
        }
        let cur = d.focus_row();
        let page = 15;
        let next = if home {
            Some(0)
        } else if end {
            Some(rows - 1)
        } else if up {
            Some(cur.map_or(0, |c| c.saturating_sub(1)))
        } else if down {
            Some(cur.map_or(0, |c| (c + 1).min(rows - 1)))
        } else if pgup {
            Some(cur.map_or(0, |c| c.saturating_sub(page)))
        } else if pgdn {
            Some(cur.map_or(0, |c| (c + page).min(rows - 1)))
        } else {
            None
        };
        if let Some(r) = next {
            // Shift+方向键：从锚点扩展；否则只选新的一行。
            d.select_row(r, shift);
            return None;
        }
        let n = sel?;
        let t = d.tree.clone();
        let node = t.node(n);
        if right && node.is_dir() {
            if !d.expanded.contains(&n) {
                d.toggle(n);
            } else if node.first_child != NONE {
                d.reveal(node.first_child);
            }
        } else if left {
            if d.expanded.contains(&n) && n != t.root {
                d.toggle(n);
            } else if node.parent != NONE {
                d.reveal(node.parent);
            }
        }
        None
    }

    /// 标题行：品牌、页签、设置，接管标题栏时还有最小化 / 最大化 / 关闭。
    /// 空白处可拖动窗口（见 `titlebar`）。
    fn title_bar(&mut self, ui: &mut Ui) {
        let p = Palette::of(ui.ctx());
        let custom = titlebar::installed();
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), TITLE_H), Sense::hover());
        let mut holes = Vec::new();
        let mut max_button = egui::Rect::NOTHING;
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(row)
                .layout(Layout::left_to_right(Align::Center)),
        );
        let ui = &mut child;
        ui.spacing_mut().item_spacing.x = 4.0;

        widgets::brand_mark(ui, 20.0);
        ui.add_space(2.0);
        ui.label(
            RichText::new("Sweep")
                .size(13.5)
                .family(theme::semibold())
                .color(p.text),
        );
        ui.add_space(14.0);

        for (view, glyph, label) in [
            (
                View::Disks,
                Icon::HardDrive,
                t!("磁盘", "Drives", "ドライブ"),
            ),
            (View::Analyze, Icon::Treemap, t!("分析", "Analyze", "分析")),
            (
                View::Cleanup,
                Icon::Broom,
                t!("清理", "Clean up", "クリーンアップ"),
            ),
            (View::Apps, Icon::Package, t!("应用", "Apps", "アプリ")),
        ] {
            let resp = tab(ui, self.view == view, glyph, label);
            holes.push(resp.rect);
            if resp.clicked() {
                self.view = view;
            }
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if custom {
                max_button = caption_buttons(ui, &mut holes);
                ui.add_space(8.0);
            } else {
                ui.add_space(20.0);
            }
            let resp = tab(
                ui,
                self.view == View::Settings,
                Icon::Settings,
                t!("设置", "Settings", "設定"),
            );
            holes.push(resp.rect);
            if resp.clicked() {
                self.view = View::Settings;
            }
            if let Some(release) = self.pending_update() {
                ui.add_space(6.0);
                let resp = update_view::title_pill(ui, &release.version);
                holes.push(resp.rect);
                if resp.clicked() {
                    self.modal = Some(Modal::Update(release));
                }
            }
        });
        if custom {
            let caption =
                egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(f32::INFINITY, row.bottom()));
            titlebar::set_layout(ui.ctx(), caption, holes, max_button);
        }
    }

    /// 有新版本且用户没有跳过它。
    fn pending_update(&self) -> Option<Arc<update::Release>> {
        match &self.update.check {
            update::Check::Available(r) if r.version != self.settings.skipped_version => {
                Some(r.clone())
            }
            _ => None,
        }
    }

    /// 读取更新状态：新版本第一次出现时通知一次；新进程已启动时关闭窗口。
    fn poll_update(&mut self, ctx: &egui::Context) {
        self.update = self.updater.status();
        if let Some(r) = self.pending_update()
            && self.update_announced.as_deref() != Some(r.version.as_str())
        {
            self.update_announced = Some(r.version.clone());
            self.toasts.push(Toast::info(
                tf!(
                    "Sweep {} 可用",
                    "Sweep {} is available",
                    "Sweep {} を利用できます",
                    r.version
                ),
                Some(
                    t!(
                        "点击标题栏中的“新版本”查看更新内容。",
                        "Click “Update” in the title bar to see what's new.",
                        "タイトル バーの「新しいバージョン」をクリックすると、更新内容を確認できます。"
                    )
                    .into(),
                ),
            ));
        }
        if matches!(self.update.install, update::Install::Restarting) && !self.restarting {
            self.restarting = true;
            if let Some(s) = &self.scan {
                s.cancel();
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn apply_update_choice(&mut self, release: Arc<update::Release>, choice: update_view::Choice) {
        use update_view::Choice;
        match choice {
            Choice::Install => self.updater.install(release),
            Choice::Later => {
                self.updater.dismiss_install_error();
                self.modal = None;
            }
            Choice::Skip => {
                self.updater.dismiss_install_error();
                self.settings.skipped_version = release.version.clone();
                self.settings.save();
                self.modal = None;
            }
            Choice::OpenPage => {
                os::open(&release.page_url);
                self.modal = None;
            }
        }
    }

    fn footer(&mut self, ui: &mut Ui) {
        let p = Palette::of(ui.ctx());
        let faint = |t: String| RichText::new(t).size(11.5).color(p.faint);
        ui.horizontal(|ui| {
            if let Some(d) = &self.doc {
                let t = &d.tree;
                let mem = t.memory() as f64 / 1048576.0;
                let (target, engine) = (&d.target, d.engine.label());
                let files = fmt_count(t.node(t.root).files as u64);
                let dirs = fmt_count(t.node(t.root).dirs as u64);
                let took = fmt_duration(d.elapsed);
                ui.label(faint(tf!(
                    "{target} · {engine} · {files} 个文件 · {dirs} 个文件夹 · 用时 {took} · 内存 {mem:.0} MB",
                    "{target} · {engine} · {files} files · {dirs} folders · {took} · {mem:.0} MB memory",
                    "{target} · {engine} · ファイル {files} 個 · フォルダー {dirs} 個 · {took} · メモリ {mem:.0} MB"
                )));
            } else {
                ui.label(faint(
                    t!(
                        "还没有分析过磁盘",
                        "Nothing analyzed yet",
                        "まだ何も分析していません"
                    )
                    .into(),
                ));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.elevated {
                    ui.label(faint(t!("管理员", "Administrator", "管理者").into()));
                }
                if let Some((dirs, at)) = self.doc.as_ref().and_then(|d| d.incremental) {
                    let (dirs, at) = (fmt_count(dirs as u64), fmt_time(at));
                    ui.label(faint(tf!(
                        "基于 {at} 的完整扫描，更新了 {dirs} 个有变动的文件夹",
                        "Based on the full scan of {at}; {dirs} changed folders updated",
                        "{at} の完全スキャンを基に、変更のあった {dirs} 個のフォルダーを更新"
                    )));
                }
                if let Some(e) = self.doc.as_ref().and_then(|d| d.mft_fallback.as_deref()) {
                    ui.label(faint(tf!(
                        "读取 MFT 失败（{e}），已改用目录遍历",
                        "Couldn't read the MFT ({e}); walked folders instead",
                        "MFT を読み取れなかったため、フォルダー走査に切り替えました ({e})"
                    )));
                }
                if self.doc.as_ref().is_some_and(|d| d.basis_busy()) {
                    ui.label(faint(
                        t!(
                            "正在按新的大小口径重新排序…",
                            "Re-sorting by the new size basis…",
                            "新しいサイズ基準で並べ替えています…"
                        )
                        .into(),
                    ));
                }
            });
        });
    }
}

/// 显示用的短名字：根节点的名字是完整路径，只取最后一段（卷根保留 `C:`）。
pub fn short_name(t: &sweep_scan::Tree, n: u32) -> &str {
    let name = t.name(n);
    if n != t.root {
        return name;
    }
    let trimmed = name.trim_end_matches('\\');
    trimmed
        .rsplit('\\')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed)
}

/// 根目录、NTFS 元文件、已删除的节点不能删除。
pub fn deletable(t: &sweep_scan::Tree, n: u32) -> bool {
    let node = t.node(n);
    n != t.root && !node.has(flags::META) && !node.has(flags::DELETED)
}

impl eframe::App for App {
    /// egui 会把鼠标滚轮的一格（按行计）拆到之后几十帧里平滑消化；我们关了 vsync，
    /// 这些帧会不限速地连续渲染，滚一下就吃满一个核。把行滚动改写成若干段 <8pt 的像素滚动，
    /// egui 对这种"触控板式"输入不做平滑，一帧内全部生效。
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let line = ctx.options(|o| o.input_options.line_scroll_speed);
        let page = raw_input.screen_rect.map_or(400.0, |r| r.height());
        let mut events = Vec::with_capacity(raw_input.events.len());
        for e in raw_input.events.drain(..) {
            match e {
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    phase,
                    modifiers,
                } if unit != egui::MouseWheelUnit::Point => {
                    let total = delta
                        * if unit == egui::MouseWheelUnit::Line {
                            line
                        } else {
                            page
                        };
                    let steps = (total.length() / 7.5).ceil().max(1.0);
                    for _ in 0..steps as usize {
                        events.push(egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Point,
                            delta: total / steps,
                            phase,
                            modifiers,
                        });
                    }
                }
                e => events.push(e),
            }
        }
        raw_input.events = events;
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // 自适应限帧：只有持续高帧率（1000Hz 游戏鼠标等）时才睡眠到一个刷新周期。
        // 平时不睡：在帧内睡眠会让 NVIDIA 驱动线程忙等，每帧额外多耗约 1.7ms。
        const BURST_WINDOW: Duration = Duration::from_millis(100);
        const BURST_FRAMES: usize = 20; // 100ms 内超过 20 帧即 >200fps
        const MIN_FRAME: Duration = Duration::from_micros(6900); // ~144fps
        let now = Instant::now();
        let recent = self
            .passes
            .iter()
            .rev()
            .take_while(|t| now.duration_since(**t) < BURST_WINDOW)
            .count();
        if recent > BURST_FRAMES
            && let Some(&last) = self.passes.back()
        {
            let since = now.duration_since(last);
            if since < MIN_FRAME {
                std::thread::sleep(MIN_FRAME - since);
            }
        }
        let started = Instant::now();
        self.passes.push_back(started);
        while self
            .passes
            .front()
            .is_some_and(|t| started.duration_since(*t) > Duration::from_secs(1))
        {
            self.passes.pop_front();
        }

        let ctx = ui.ctx().clone();
        self.poll_scan(&ctx);
        self.poll_jobs(&ctx);
        self.poll_update(&ctx);
        self.apps
            .poll(&ctx, self.settings.basis == Basis::Allocated);
        if let Some((ok, failed)) = self.apps.take_finished() {
            let (ok_n, failed_n) = (fmt_count(ok as u64), fmt_count(failed as u64));
            if failed == 0 {
                self.notify_ok(
                    tf!(
                        "已卸载 {ok_n} 个应用",
                        "Uninstalled {ok_n} apps",
                        "{ok_n} 個のアプリをアンインストールしました"
                    ),
                    None,
                );
            } else {
                self.notify_err(
                    tf!(
                        "{failed_n} 个应用未能卸载",
                        "{failed_n} apps weren't uninstalled",
                        "{failed_n} 個のアプリをアンインストールできませんでした"
                    ),
                    Some(tf!(
                        "已卸载 {ok_n} 个。把鼠标停在标为“失败”的行上可查看原因。",
                        "{ok_n} uninstalled. Hover a row marked “Failed” to see why.",
                        "{ok_n} 個はアンインストールしました。「失敗」と表示された行にマウスを合わせると理由を確認できます。"
                    )),
                );
            }
            self.after_space_change(&ctx);
        }
        self.icons.poll(&ctx);
        self.treemap.poll(&ctx);
        if let Some(d) = &mut self.doc {
            d.update(&ctx);
        }
        if self.scan.is_some() {
            // 扫描期间每 100ms 刷新一次进度。
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.view == View::Disks
            && self
                .volumes_at
                .is_none_or(|t| t.elapsed() > Duration::from_secs(10))
        {
            self.load_volumes(&ctx);
        }

        let p = Palette::of(&ctx);
        let custom = titlebar::installed();
        egui::Panel::top("top")
            .show_separator_line(false)
            .frame(Frame::NONE.fill(p.bg).inner_margin(Margin {
                left: 20,
                right: if custom { 0 } else { 20 },
                top: if custom { 0 } else { 8 },
                bottom: 4,
            }))
            .show(ui, |ui| self.title_bar(ui));
        egui::Panel::bottom("footer")
            .show_separator_line(false)
            .frame(
                Frame::NONE
                    .fill(p.bg)
                    .inner_margin(Margin::symmetric(20, 8)),
            )
            .show(ui, |ui| self.footer(ui));
        let central = egui::CentralPanel::default().frame(
            Frame::NONE
                .fill(p.bg)
                .inner_margin(Margin::symmetric(20, 8)),
        );
        // 清理页只显示"最大的文件"：进入时把选中项收窄到这个列表里，Delete 和右键菜单
        // 不会作用于分析页里选中、在这里看不到的项。
        if self.view == View::Cleanup
            && self.last_view != View::Cleanup
            && let Some(d) = &mut self.doc
        {
            let shown: std::collections::HashSet<u32> = d.largest().into_iter().collect();
            d.selection.retain(|n| shown.contains(&n));
        }
        self.last_view = self.view;
        let mut actions = Vec::new();
        match self.view {
            View::Disks => {
                let a = central
                    .show(ui, |ui| {
                        start_view::show(
                            ui,
                            &self.volumes,
                            self.elevated,
                            &mut self.path_input,
                            self.doc.as_ref().map(|d| d.target.as_str()),
                        )
                    })
                    .inner;
                actions.extend(a);
            }
            View::Analyze => {
                let a = central
                    .show(ui, |ui| {
                        if let Some(scan) = &self.scan {
                            scan_view::show(ui, scan, &self.volumes)
                        } else if let Some(d) = &mut self.doc {
                            analyze::show(
                                ui,
                                d,
                                &mut self.treemap,
                                &mut self.icons,
                                &mut self.focus_search,
                                self.deleting,
                            )
                        } else {
                            scan_view::empty(ui, self.scan_error.as_deref())
                        }
                    })
                    .inner;
                actions.extend(a);
            }
            View::Cleanup => {
                let a = central
                    .show(ui, |ui| match &mut self.doc {
                        Some(d) => {
                            d.want_suggestions(ui.ctx());
                            cleanup_view::show(ui, d, &mut self.icons, self.deleting)
                        }
                        None => scan_view::empty(ui, None),
                    })
                    .inner;
                actions.extend(a);
            }
            View::Apps => {
                let a = central
                    .show(ui, |ui| self.apps.show(ui, &mut self.icons))
                    .inner;
                if let Some(apps_view::AppsAction::Uninstall(list)) = a {
                    self.modal = Some(Modal::Uninstall(list));
                }
            }
            View::Settings => {
                let before = self.settings.clone();
                let action = central
                    .show(ui, |ui| {
                        settings_view::show(
                            ui,
                            &mut self.settings,
                            self.elevated,
                            *self
                                .cache_usage
                                .get_or_insert_with(sweep_scan::cache::usage),
                            &self.update.check,
                        )
                    })
                    .inner;
                match action {
                    Some(settings_view::SettingsAction::CheckUpdate) => self.updater.check_now(),
                    Some(settings_view::SettingsAction::ClearCache) => {
                        match sweep_scan::cache::clear() {
                            Ok(()) => self.notify_ok(
                                t!(
                                    "已清除扫描结果缓存",
                                    "Saved scan results cleared",
                                    "保存したスキャン結果を消去しました"
                                )
                                .into(),
                                None,
                            ),
                            Err(e) => self.notify_err(
                                t!(
                                    "无法清除扫描结果缓存",
                                    "Couldn't clear the saved scan results",
                                    "保存したスキャン結果を消去できませんでした"
                                )
                                .into(),
                                Some(e.to_string()),
                            ),
                        }
                        self.cache_usage = None;
                    }
                    Some(settings_view::SettingsAction::ShowUpdate) => {
                        if let update::Check::Available(r) = &self.update.check {
                            self.modal = Some(Modal::Update(r.clone()));
                        }
                    }
                    None => {}
                }
                if self.settings != before {
                    if self.settings.auto_update != before.auto_update {
                        self.updater.set_auto(self.settings.auto_update);
                    }
                    if self.settings.language != before.language {
                        apply_language(&ctx, self.settings.language);
                    }
                    if self.settings.theme != before.theme {
                        ctx.set_theme(self.settings.theme.preference());
                    }
                    if self.settings.basis != before.basis
                        && let Some(d) = &mut self.doc
                    {
                        d.set_basis(self.settings.basis == Basis::Allocated, &ctx);
                    }
                    self.settings.save();
                }
            }
        }

        // 右键菜单（固定在点击位置）。
        if let Some((n, pos)) = self.menu
            && let Some(d) = &mut self.doc
        {
            let summary = d.summary();
            let (a, open) = menu::node_menu(&ctx, d, n, pos, summary, self.deleting);
            if !open {
                self.menu = None;
            }
            actions.extend(a);
        }

        let mut update_choice = None;
        if let Some(modal) = &self.modal {
            let decision = match modal {
                Modal::Update(r) => {
                    update_choice = update_view::dialog(
                        &ctx,
                        "Sweep",
                        env!("CARGO_PKG_VERSION"),
                        r,
                        &self.update.install,
                    )
                    .map(|c| (r.clone(), c));
                    None
                }
                Modal::Delete(req) => dialogs::confirm_delete(&ctx, req),
                Modal::Uninstall(list) => dialogs::confirm_uninstall(&ctx, list, self.elevated),
                Modal::EmptyRecycleBin(size) => dialogs::confirm(
                    &ctx,
                    "recycle",
                    t!(
                        "清空回收站？",
                        "Empty the Recycle Bin?",
                        "ごみ箱を空にしますか?"
                    ),
                    &{
                        let size = fmt_bytes(*size);
                        tf!(
                            "将永久删除所有驱动器回收站里的文件（共 {size}），无法恢复。",
                            "Everything in the Recycle Bin on all drives ({size}) will be permanently deleted. This can't be undone.",
                            "すべてのドライブのごみ箱にあるファイル (合計 {size}) を完全に削除します。元に戻すことはできません。"
                        )
                    },
                    t!("清空回收站", "Empty Recycle Bin", "ごみ箱を空にする"),
                ),
            };
            match decision {
                Some(Decision::Confirm) => {
                    let modal = self.modal.take().unwrap();
                    self.confirm_modal(&ctx, modal);
                }
                Some(Decision::Cancel) => self.modal = None,
                None => {}
            }
        }
        if let Some((release, choice)) = update_choice {
            self.apply_update_choice(release, choice);
        }
        dialogs::toasts(&ctx, &mut self.toasts);

        let typing = ctx.memory(|m| m.focused().is_some());
        if ctx.input(|i| i.modifiers.command && i.key_pressed(Key::F)) && self.doc.is_some() {
            self.view = View::Analyze;
            self.focus_search = true;
        }
        // 快捷键只在没有输入焦点、对话框和菜单时生效。
        let idle = self.modal.is_none() && self.menu.is_none() && self.scan.is_none() && !typing;
        let key_action = match self.view {
            View::Analyze if idle => self.keyboard(&ctx),
            View::Cleanup if idle => self.selection_keys(&ctx, List::Largest),
            View::Apps if idle => {
                if let Some(apps_view::AppsAction::Uninstall(list)) = self.apps.keys(&ctx) {
                    self.modal = Some(Modal::Uninstall(list));
                }
                None
            }
            _ => None,
        };
        actions.extend(key_action);
        for a in actions {
            self.apply(&ctx, a);
        }
    }
}

/// Delete 移到回收站（`Some(false)`），Shift+Delete 永久删除（`Some(true)`）。
/// egui-winit 在 Windows 上把 Shift+Delete 当作"剪切"，只产生 Cut 事件、没有 Delete 按键事件。
fn delete_shortcut(i: &egui::InputState) -> Option<bool> {
    if i.modifiers.shift && i.events.contains(&egui::Event::Cut) {
        return Some(true);
    }
    i.key_pressed(Key::Delete).then_some(i.modifiers.shift)
}

/// Ctrl+C：egui-winit 把它转成 Copy 事件，没有 C 的按键事件。
fn copy_shortcut(i: &egui::InputState) -> bool {
    i.events.contains(&egui::Event::Copy) || (i.modifiers.command && i.key_pressed(Key::C))
}

/// 通知的标记：进行中的通知在操作完成时按标记收走。
const TAG_RESCAN: &str = "rescan";
const TAG_DELETE: &str = "delete";

/// 切换界面语言：文案在每帧按当前语言生成，这里只需换掉字体（日文用日本的汉字字形）。
fn apply_language(ctx: &egui::Context, language: settings::Language) {
    let lang = language.resolve();
    sweep_scan::i18n::set_lang(lang);
    fonts::install(ctx, lang);
}

/// "3 项" / "3 items" / "3 個の項目"。
pub fn n_items(n: u64) -> String {
    let c = fmt_count(n);
    let s = if n == 1 { "" } else { "s" };
    tf!("{c} 项", "{c} item{s}", "{c} 個の項目")
}

/// 标题行里的页签：图标 + 文字，选中时有底色。
fn tab(ui: &mut Ui, on: bool, glyph: Icon, label: &str) -> egui::Response {
    let p = Palette::of(ui.ctx());
    let galley =
        ui.painter()
            .layout_no_wrap(label.into(), egui::FontId::proportional(14.0), p.text);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 44.0, 32.0), Sense::click());
    if on || resp.hovered() {
        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(8),
            if on { p.surface } else { p.hover },
        );
    }
    let fg = if on { p.text } else { p.muted };
    widgets::icon(
        ui,
        glyph,
        pos2(rect.left() + 18.0, rect.center().y),
        16.0,
        if on { p.accent } else { fg },
    );
    ui.painter().galley(
        pos2(rect.left() + 31.0, rect.center().y - galley.size().y / 2.0),
        galley,
        fg,
    );
    resp
}

/// 标题行高度。
const TITLE_H: f32 = 44.0;

/// 对齐到物理像素中心，让 1px 线条清晰。
fn pixel_center(ui: &Ui, p: egui::Pos2) -> egui::Pos2 {
    let painter = ui.painter();
    pos2(
        painter.round_to_pixel_center(p.x),
        painter.round_to_pixel_center(p.y),
    )
}

/// 自绘的最小化 / 最大化 / 关闭按钮（从右往左放），返回最大化按钮的区域。
/// 最大化按钮的鼠标事件由系统送到非客户区，状态从 `titlebar` 读取。
fn caption_buttons(ui: &mut Ui, holes: &mut Vec<egui::Rect>) -> egui::Rect {
    let p = Palette::of(ui.ctx());
    let (focused, maximized) = ui.input(|i| {
        let v = i.viewport();
        (v.focused.unwrap_or(true), v.maximized.unwrap_or(false))
    });
    let fg = if focused { p.text } else { p.faint };
    let size = vec2(46.0, TITLE_H);
    ui.spacing_mut().item_spacing.x = 0.0;
    let stroke = |c| egui::Stroke::new(1.0, c);

    // 关闭
    let (close, resp) = ui.allocate_exact_size(size, Sense::click());
    holes.push(close);
    let hot = resp.hovered() || resp.is_pointer_button_down_on();
    if hot {
        let red = egui::Color32::from_rgb(0xc4, 0x2b, 0x1c);
        let fill = if resp.is_pointer_button_down_on() {
            red.gamma_multiply(0.85)
        } else {
            red
        };
        ui.painter()
            .rect_filled(close, egui::CornerRadius::ZERO, fill);
    }
    let c = pixel_center(ui, close.center());
    let color = if hot { egui::Color32::WHITE } else { fg };
    for (a, b) in [
        (vec2(-5.0, -5.0), vec2(5.0, 5.0)),
        (vec2(-5.0, 5.0), vec2(5.0, -5.0)),
    ] {
        ui.painter().line_segment([c + a, c + b], stroke(color));
    }
    if resp.clicked() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
    }

    // 最大化 / 还原
    let (max, _) = ui.allocate_exact_size(size, Sense::hover());
    let (hover, pressed) = titlebar::max_button_state();
    if hover || pressed {
        ui.painter().rect_filled(
            max,
            egui::CornerRadius::ZERO,
            if pressed { p.track } else { p.hover },
        );
    }
    let c = pixel_center(ui, max.center());
    if maximized {
        let front = egui::Rect::from_min_size(c + vec2(-5.0, -3.0), vec2(8.0, 8.0));
        ui.painter().rect_stroke(
            front,
            egui::CornerRadius::same(1),
            stroke(fg),
            egui::StrokeKind::Middle,
        );
        ui.painter().add(egui::Shape::line(
            vec![
                c + vec2(-3.0, -3.0),
                c + vec2(-3.0, -5.0),
                c + vec2(5.0, -5.0),
                c + vec2(5.0, 3.0),
                c + vec2(3.0, 3.0),
            ],
            stroke(fg),
        ));
    } else {
        let r = egui::Rect::from_center_size(c, vec2(10.0, 10.0));
        ui.painter().rect_stroke(
            r,
            egui::CornerRadius::same(1),
            stroke(fg),
            egui::StrokeKind::Middle,
        );
    }

    // 最小化
    let (min, resp) = ui.allocate_exact_size(size, Sense::click());
    holes.push(min);
    if resp.hovered() || resp.is_pointer_button_down_on() {
        let fill = if resp.is_pointer_button_down_on() {
            p.track
        } else {
            p.hover
        };
        ui.painter()
            .rect_filled(min, egui::CornerRadius::ZERO, fill);
    }
    let c = pixel_center(ui, min.center());
    ui.painter()
        .line_segment([c + vec2(-5.0, 0.0), c + vec2(5.0, 0.0)], stroke(fg));
    if resp.clicked() {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    }
    max
}

/// 二进制单位，与资源管理器一致（1 KB = 1024 字节）。
pub fn fmt_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut v = b as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    let digits = if v >= 100.0 {
        0
    } else if v >= 10.0 {
        1
    } else {
        2
    };
    format!("{v:.digits$} {}", UNITS[u])
}

/// 千位分隔：1234567 → "1,234,567"。
pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        tf!("{s:.1} 秒", "{s:.1} s", "{s:.1} 秒")
    } else {
        let (m, s) = (s as u64 / 60, s as u64 % 60);
        tf!("{m} 分 {s} 秒", "{m} min {s} s", "{m} 分 {s} 秒")
    }
}

/// FILETIME（100ns 自 1601 年起，UTC）→ 本地时间 "2026-10-12 14:03"。0 显示为"—"。
pub fn fmt_time(ft: u64) -> String {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    if ft == 0 {
        return "—".into();
    }
    let f = FILETIME {
        dwLowDateTime: ft as u32,
        dwHighDateTime: (ft >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        if FileTimeToSystemTime(&f, &mut utc).is_err()
            || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err()
        {
            return "—".into();
        }
    }
    format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在一帧里送入 `events`，返回 (删除快捷键, 复制快捷键) 的判断结果。
    fn shortcuts(modifiers: egui::Modifiers, events: Vec<egui::Event>) -> (Option<bool>, bool) {
        let ctx = egui::Context::default();
        let mut out = (None, false);
        let mut all = vec![egui::Event::ModifiersChanged(modifiers)];
        all.extend(events);
        let raw = egui::RawInput {
            events: all,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw, |ui| {
            out = ui.ctx().input(|i| (delete_shortcut(i), copy_shortcut(i)));
        });
        output.textures_delta.clear();
        out
    }

    fn key(key: Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn keyboard_shortcuts() {
        use egui::Modifiers;
        let none = Modifiers::NONE;
        assert_eq!(
            shortcuts(none, vec![key(Key::Delete, none)]),
            (Some(false), false)
        );
        // egui-winit 送来的 Shift+Delete。
        assert_eq!(
            shortcuts(Modifiers::SHIFT, vec![egui::Event::Cut]),
            (Some(true), false)
        );
        // 其他平台或输入方式仍可能送来按键事件。
        assert_eq!(
            shortcuts(Modifiers::SHIFT, vec![key(Key::Delete, Modifiers::SHIFT)]),
            (Some(true), false)
        );
        // Ctrl+X 也是 Cut，但不是删除。
        assert_eq!(
            shortcuts(Modifiers::COMMAND, vec![egui::Event::Cut]),
            (None, false)
        );
        assert_eq!(
            shortcuts(Modifiers::COMMAND, vec![egui::Event::Copy]),
            (None, true)
        );
        assert_eq!(shortcuts(none, vec![]), (None, false));
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1536), "1.50 KB");
        assert_eq!(fmt_bytes(15 * 1024 * 1024), "15.0 MB");
        assert_eq!(fmt_bytes(726 * (1 << 30)), "726 GB");
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(fmt_count(100), "100");
        assert_eq!(fmt_time(0), "—");
        assert!(fmt_time(134_000_000_000_000_000).starts_with("20"));
    }
}
