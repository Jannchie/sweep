#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analyze;
mod brand_art;
mod cleanup;
mod cleanup_view;
mod dialogs;
mod doc;
mod file_icons;
mod icons_gen;
mod menu;
mod os;
mod scan_view;
mod settings;
mod settings_view;
mod start_view;
mod theme;
mod titlebar;
mod treemap;
mod widgets;

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Frame, Key, Layout, Margin, RichText, Sense, Ui, pos2, vec2};
use sweep_scan::system::Volume;
use sweep_scan::{NONE, Scan, flags};

use cleanup::Item;
use dialogs::{Decision, DeleteRequest, Toast};
use doc::{Doc, Mutation};
use file_icons::FileIcons;
use icons_gen::Icon;
use settings::{Basis, Settings};
use theme::Palette;
use treemap::Treemap;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Sweep")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([980.0, 640.0])
            // 先给一个图标，免得出现 eframe 默认的 egui 标志；窗口建好后 titlebar 会换成 exe 资源里的多尺寸图标。
            .with_icon(std::sync::Arc::new(egui::IconData {
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
    Settings,
}

/// 各页面产生的操作，统一由 [`App::apply`] 执行。
pub enum Action {
    Select(u32),
    Toggle(u32),
    /// 矩形树图放大到这个目录（文件取所在目录）。
    ZoomTo(u32),
    ZoomOut,
    /// 切到分析页，在树里展开并选中。
    Locate(u32),
    LocatePath(String),
    OpenMenu(u32, egui::Pos2),
    Reveal(String),
    Open(String),
    /// 复制文字到剪贴板。
    Copy(String),
    Properties(String),
    /// 弹出删除确认框。
    Delete {
        items: Vec<Item>,
        label: String,
        permanent: bool,
    },
    DeleteNodes {
        nodes: Vec<u32>,
        permanent: bool,
    },
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
}

/// 后台任务的结果。
enum Job {
    Toast(Toast),
    Deleted {
        results: Vec<(String, Result<(), String>)>,
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
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        theme::install(ctx);
        egui_extras::install_image_loaders(ctx);
        let settings = Settings::load();
        ctx.set_theme(settings.theme.preference());
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
            move || c.request_repaint(),
        ));
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
            Action::Select(n) => {
                if let Some(d) = &mut self.doc {
                    d.selected = Some(n);
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
            Action::OpenMenu(n, pos) => self.menu = Some((n, pos)),
            Action::Reveal(p) => os::reveal(&p),
            Action::Open(p) => os::open(&p),
            Action::Copy(text) => {
                ctx.copy_text(text.clone());
                self.notify_ok("已复制".into(), Some(text));
            }
            Action::Properties(p) => os::properties(&p),
            Action::Delete {
                items,
                label,
                permanent,
            } => {
                if self.deleting {
                    self.notify_err("上一次删除还没有完成".into(), None);
                } else if !items.is_empty() {
                    self.modal = Some(Modal::Delete(DeleteRequest {
                        items: items
                            .into_iter()
                            .map(|i| (i.path, i.size, i.dir, i.files))
                            .collect(),
                        label,
                        permanent,
                    }));
                }
            }
            Action::DeleteNodes { nodes, permanent } => {
                let Some(d) = &self.doc else { return };
                let t = &d.tree;
                let nodes: Vec<u32> = nodes.into_iter().filter(|&n| deletable(t, n)).collect();
                let label = match nodes.as_slice() {
                    [n] => t.name(*n).to_string(),
                    v => format!("{} 项", v.len()),
                };
                let items = nodes
                    .iter()
                    .map(|&n| Item {
                        path: t.path(n),
                        size: t.value(n),
                        dir: t.node(n).is_dir(),
                        files: t.node(n).files as u64,
                    })
                    .collect();
                self.apply(
                    ctx,
                    Action::Delete {
                        items,
                        label,
                        permanent,
                    },
                );
            }
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

    /// 重新扫描一个目录，完成后替换树里的这一段。
    fn rescan(&mut self, ctx: &egui::Context, n: u32) {
        let Some(d) = &self.doc else { return };
        let path = d.tree.path(n);
        let by_alloc = d.tree.by_alloc;
        self.notify_ok(format!("正在重新扫描 {}", d.tree.name(n)), None);
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
                Err(e) => Job::Toast(Toast::err(format!("无法重新扫描 {path}"), Some(e))),
            }
        });
    }

    fn confirm_modal(&mut self, ctx: &egui::Context, modal: Modal) {
        match modal {
            Modal::Delete(req) => {
                let permanent = req.permanent;
                let size = req.total();
                let paths: Vec<String> = req.items.into_iter().map(|i| i.0).collect();
                self.deleting = true;
                let verb = if permanent {
                    "删除"
                } else {
                    "移到回收站"
                };
                self.notify_ok(format!("正在{verb}…"), None);
                self.spawn(ctx, move || {
                    let results = paths
                        .into_iter()
                        .map(|p| {
                            let r = os::delete(&p, permanent);
                            (p, r)
                        })
                        .collect();
                    Job::Deleted {
                        results,
                        permanent,
                        size,
                    }
                });
            }
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
                Job::Rescanned { node, tree } => {
                    self.toasts.retain(|t| !t.title.starts_with("正在重新扫描"));
                    if let Some(d) = &mut self.doc {
                        let name = d.tree.name(node).to_string();
                        d.mutate(Mutation::Graft(node, tree));
                        self.notify_ok(format!("已重新扫描 {name}"), None);
                    }
                }
                Job::Deleted {
                    results,
                    permanent,
                    size,
                } => self.after_delete(ctx, results, permanent, size),
                Job::RecycleEmptied(r) => {
                    match r {
                        Ok(()) => self.notify_ok("已清空回收站".into(), None),
                        Err(e) => self.notify_err("无法清空回收站".into(), Some(e)),
                    }
                    self.after_space_change(ctx);
                }
            }
        }
    }

    /// 删除完成：删掉的从树上摘除，没删干净的目录重新扫描，不必整盘重扫。
    fn after_delete(
        &mut self,
        ctx: &egui::Context,
        results: Vec<(String, Result<(), String>)>,
        permanent: bool,
        size: u64,
    ) {
        self.deleting = false;
        self.toasts.retain(|t| !t.title.starts_with("正在"));
        let failed: Vec<&(String, Result<(), String>)> =
            results.iter().filter(|r| r.1.is_err()).collect();
        let mut rescan = Vec::new();
        if let Some(d) = &mut self.doc {
            for (path, r) in &results {
                let Some(n) = d.tree.find(path) else { continue };
                match r {
                    Ok(()) => d.mutate(Mutation::Remove(n)),
                    Err(_) if d.tree.node(n).is_dir() => rescan.push(n),
                    Err(_) => {}
                }
            }
        }
        for n in rescan {
            self.rescan(ctx, n);
        }
        let verb = if permanent {
            "永久删除"
        } else {
            "移到回收站"
        };
        let done = results.len() - failed.len();
        match failed.first() {
            None => self.notify_ok(
                format!("已{verb} {done} 项"),
                Some(format!("共 {}", fmt_bytes(size))),
            ),
            Some((p, Err(e))) => {
                let title = if done == 0 {
                    format!("无法{verb}")
                } else {
                    format!("已{verb} {done} 项，{} 项失败", failed.len())
                };
                self.notify_err(title, Some(format!("{p}：{e}")));
            }
            Some(_) => {}
        }
        self.after_space_change(ctx);
    }

    /// 磁盘空间变了：刷新卷信息，清理建议在下次打开清理页时重新计算。
    fn after_space_change(&mut self, ctx: &egui::Context) {
        self.volumes_at = None;
        self.load_volumes(ctx);
    }

    /// 方向键、Enter、Backspace、Delete 等。只在分析页、没有输入焦点和对话框时生效。
    fn keyboard(&mut self, ctx: &egui::Context) -> Option<Action> {
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
        let (left, right, enter, back, del, shift, copy, f5) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::Backspace),
                i.key_pressed(Key::Delete),
                i.modifiers.shift,
                i.modifiers.command && i.key_pressed(Key::C),
                i.key_pressed(Key::F5),
            )
        });
        let sel = d.selected;
        if del {
            return sel.map(|n| Action::DeleteNodes {
                nodes: vec![n],
                permanent: shift,
            });
        }
        if copy {
            return sel.map(|n| Action::Copy(d.tree.path(n)));
        }
        if f5 {
            return sel.filter(|&n| d.tree.node(n).is_dir()).map(Action::Rescan);
        }
        if enter {
            return sel.map(Action::ZoomTo);
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
        let cur = d.selected_row();
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
            d.select_row(r);
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
            (View::Disks, Icon::HardDrive, "磁盘"),
            (View::Analyze, Icon::Treemap, "分析"),
            (View::Cleanup, Icon::Broom, "清理"),
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
            let resp = tab(ui, self.view == View::Settings, Icon::Settings, "设置");
            holes.push(resp.rect);
            if resp.clicked() {
                self.view = View::Settings;
            }
        });
        if custom {
            let caption =
                egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(f32::INFINITY, row.bottom()));
            titlebar::set_layout(ui.ctx(), caption, holes, max_button);
        }
    }

    fn footer(&mut self, ui: &mut Ui) {
        let p = Palette::of(ui.ctx());
        let faint = |t: String| RichText::new(t).size(11.5).color(p.faint);
        ui.horizontal(|ui| {
            if let Some(d) = &self.doc {
                let t = &d.tree;
                let mem = t.memory() as f64 / 1048576.0;
                ui.label(faint(format!(
                    "{} · {} · {} 个文件 · {} 个文件夹 · 用时 {} · 内存 {mem:.0} MB",
                    d.target,
                    d.engine.label(),
                    fmt_count(t.node(t.root).files as u64),
                    fmt_count(t.node(t.root).dirs as u64),
                    fmt_duration(d.elapsed),
                )));
            } else {
                ui.label(faint("还没有分析过磁盘".into()));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.elevated {
                    ui.label(faint("管理员".into()));
                }
                if let Some(e) = self.doc.as_ref().and_then(|d| d.mft_fallback.as_deref()) {
                    ui.label(faint(format!("读取 MFT 失败（{e}），已改用目录遍历")));
                }
                if self.doc.as_ref().is_some_and(|d| d.basis_busy()) {
                    ui.label(faint("正在按新的大小口径重新排序…".into()));
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
            View::Settings => {
                let before = self.settings.clone();
                central.show(ui, |ui| {
                    settings_view::show(ui, &mut self.settings, self.elevated)
                });
                if self.settings != before {
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
            && let Some(d) = &self.doc
        {
            let (a, open) = menu::node_menu(&ctx, &d.tree, n, pos, self.deleting);
            if !open {
                self.menu = None;
            }
            actions.extend(a);
        }

        if let Some(modal) = &self.modal {
            let decision = match modal {
                Modal::Delete(req) => dialogs::confirm_delete(&ctx, req),
                Modal::EmptyRecycleBin(size) => dialogs::confirm(
                    &ctx,
                    "recycle",
                    "清空回收站？",
                    &format!(
                        "将永久删除所有驱动器回收站里的文件（共 {}），无法恢复。",
                        fmt_bytes(*size)
                    ),
                    "清空回收站",
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
        dialogs::toasts(&ctx, &mut self.toasts);

        let typing = ctx.memory(|m| m.focused().is_some());
        if ctx.input(|i| i.modifiers.command && i.key_pressed(Key::F)) && self.doc.is_some() {
            self.view = View::Analyze;
            self.focus_search = true;
        }
        if self.view == View::Analyze
            && self.modal.is_none()
            && self.menu.is_none()
            && self.scan.is_none()
            && !typing
            && let Some(a) = self.keyboard(&ctx)
        {
            actions.push(a);
        }
        for a in actions {
            self.apply(&ctx, a);
        }
    }
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
        format!("{s:.1} 秒")
    } else {
        format!("{} 分 {} 秒", s as u64 / 60, s as u64 % 60)
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
