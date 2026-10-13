//! 应用页：已安装的程序列表（大小、安装日期、卸载方式），多选后快速卸载。
//! 能静默卸载的直接卸载，不弹出卸载向导；其余打开程序自己的卸载向导（见 [`crate::apps`]）。
//!
//! 列表、大小测量、卸载都在后台线程进行，结果经通道送回，[`AppsPage::poll`] 每帧收取。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};

use eframe::egui::{self, Align, Key, Layout, Rect, RichText, Sense, Ui, UiBuilder, pos2, vec2};
use egui_extras::{Column, TableBuilder};
use sweep_scan::{t, tf};

use crate::apps::{self, InstalledApp};
use crate::file_icons::{self, FileIcons};
use crate::icons_gen::Icon;
use crate::theme::{self, Palette};
use crate::{fmt_bytes, fmt_count, widgets};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Installed,
    Size,
}

/// 一个程序的卸载进度。
#[derive(Clone, Debug, PartialEq)]
enum Status {
    Queued,
    Running,
    Failed(String),
}

enum Event {
    Started(String),
    Finished(String, Result<(), String>),
    /// 整批结束：(成功数, 失败数)。
    Done(usize, usize),
}

/// 页面交给 App 处理的操作。
pub enum AppsAction {
    /// 请求卸载这些程序（App 弹出确认框）。
    Uninstall(Vec<InstalledApp>),
}

pub struct AppsPage {
    apps: Vec<InstalledApp>,
    loaded: bool,
    load_rx: Option<Receiver<Vec<InstalledApp>>>,
    sizes: HashMap<String, u64>,
    size_rx: Option<Receiver<(String, u64)>>,
    measure_cancel: Arc<AtomicBool>,
    filter: String,
    sort: SortKey,
    descending: bool,
    selected: HashSet<String>,
    anchor: Option<String>,
    status: HashMap<String, Status>,
    uninstall_rx: Option<Receiver<Event>>,
    /// 右键菜单：针对的程序与位置。
    menu: Option<(String, egui::Pos2)>,
    /// 一批卸载结束后的 (成功数, 失败数)，由 App 取走并提示。
    finished: Option<(usize, usize)>,
}

impl Default for AppsPage {
    fn default() -> Self {
        Self {
            apps: Vec::new(),
            loaded: false,
            load_rx: None,
            sizes: HashMap::new(),
            size_rx: None,
            measure_cancel: Arc::new(AtomicBool::new(false)),
            filter: String::new(),
            sort: SortKey::Size,
            descending: true,
            selected: HashSet::new(),
            anchor: None,
            status: HashMap::new(),
            uninstall_rx: None,
            menu: None,
            finished: None,
        }
    }
}

impl AppsPage {
    /// 在后台重新读取列表（第一次打开页面、卸载之后、点刷新时）。
    pub fn reload(&mut self, ctx: &egui::Context) {
        if self.load_rx.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(apps::list());
            ctx.request_repaint();
        });
        self.load_rx = Some(rx);
    }

    /// 状态栏的文字：总数、测量进度，或选中项的个数与大小合计。
    pub fn status_line(&self) -> String {
        if !self.loaded {
            return t!(
                "正在读取已安装的程序…",
                "Reading installed apps…",
                "インストール済みのアプリを読み込んでいます…"
            )
            .into();
        }
        let chosen: Vec<&InstalledApp> = self
            .apps
            .iter()
            .filter(|a| self.selected.contains(&a.id))
            .collect();
        if !chosen.is_empty() {
            let n = fmt_count(chosen.len() as u64);
            let size = fmt_bytes(chosen.iter().filter_map(|a| self.size_of(a).0).sum());
            return tf!(
                "已选 {n} 个 · {size}",
                "{n} selected · {size}",
                "{n} 個を選択 · {size}"
            );
        }
        let total = self.apps.len();
        let n = fmt_count(total as u64);
        let measured = self.sizes.len().min(total);
        let pending = self.apps.iter().filter(|a| !a.folders.is_empty()).count();
        if measured < pending && self.size_rx.is_some() {
            tf!(
                "{n} 个应用 · 正在测量大小 {measured}/{pending}",
                "{n} apps · measuring sizes {measured}/{pending}",
                "{n} 個のアプリ · サイズを測定中 {measured}/{pending}"
            )
        } else {
            tf!("{n} 个应用", "{n} apps", "{n} 個のアプリ")
        }
    }

    pub fn busy(&self) -> bool {
        self.uninstall_rx.is_some()
    }

    pub fn take_finished(&mut self) -> Option<(usize, usize)> {
        self.finished.take()
    }

    /// 收取后台结果。每帧调用。
    pub fn poll(&mut self, ctx: &egui::Context, by_alloc: bool) {
        if let Some(rx) = &self.load_rx
            && let Ok(list) = rx.try_recv()
        {
            self.load_rx = None;
            self.loaded = true;
            let ids: HashSet<&str> = list.iter().map(|a| a.id.as_str()).collect();
            self.selected.retain(|id| ids.contains(id.as_str()));
            self.status
                .retain(|id, s| ids.contains(id.as_str()) && matches!(s, Status::Failed(_)));
            self.apps = list;
            self.measure(ctx, by_alloc);
        }
        if let Some(rx) = &self.size_rx {
            while let Ok((id, size)) = rx.try_recv() {
                self.sizes.insert(id, size);
            }
        }
        if let Some(rx) = &self.uninstall_rx {
            let mut done = None;
            while let Ok(e) = rx.try_recv() {
                match e {
                    Event::Started(id) => {
                        self.status.insert(id, Status::Running);
                    }
                    Event::Finished(id, Ok(())) => {
                        self.status.remove(&id);
                        self.selected.remove(&id);
                    }
                    Event::Finished(id, Err(e)) => {
                        self.status.insert(id, Status::Failed(e));
                    }
                    Event::Done(ok, failed) => done = Some((ok, failed)),
                }
            }
            if let Some(d) = done {
                self.uninstall_rx = None;
                self.finished = Some(d);
                self.reload(ctx);
            }
        }
    }

    /// 测量还没有大小的程序（已测过的文件夹不重测）。
    fn measure(&mut self, ctx: &egui::Context, by_alloc: bool) {
        self.measure_cancel.store(true, Ordering::Relaxed);
        let todo: Vec<InstalledApp> = self
            .apps
            .iter()
            .filter(|a| !self.sizes.contains_key(&a.id) && !a.folders.is_empty())
            .cloned()
            .collect();
        if todo.is_empty() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.measure_cancel = cancel.clone();
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("app-sizes".into())
            .spawn(move || {
                apps::measure(&todo, by_alloc, &cancel, |id, size| {
                    let _ = tx.send((id.to_string(), size));
                    ctx.request_repaint();
                });
            })
            .expect("spawn app sizes");
        self.size_rx = Some(rx);
    }

    /// 依次卸载（MSI 等不能并行）。
    pub fn start_uninstall(
        &mut self,
        ctx: &egui::Context,
        list: Vec<InstalledApp>,
        elevated: bool,
    ) {
        if self.busy() {
            return;
        }
        for a in &list {
            self.status.insert(a.id.clone(), Status::Queued);
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("uninstall".into())
            .spawn(move || {
                unsafe {
                    let _ = windows::Win32::System::Com::CoInitializeEx(
                        None,
                        windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                    );
                }
                let (mut ok, mut failed) = (0, 0);
                for app in list {
                    let _ = tx.send(Event::Started(app.id.clone()));
                    ctx.request_repaint();
                    let r = apps::uninstall(&app, elevated);
                    if r.is_ok() {
                        ok += 1;
                    } else {
                        failed += 1;
                    }
                    let _ = tx.send(Event::Finished(app.id.clone(), r));
                    ctx.request_repaint();
                }
                let _ = tx.send(Event::Done(ok, failed));
                ctx.request_repaint();
            })
            .expect("spawn uninstall");
        self.uninstall_rx = Some(rx);
    }

    fn size_of(&self, a: &InstalledApp) -> (Option<u64>, bool) {
        match self.sizes.get(&a.id) {
            Some(&s) => (Some(s), true),
            None => (a.estimated, false),
        }
    }

    /// 当前筛选、排序后的列表（下标）。
    fn rows(&self) -> Vec<usize> {
        let q = self.filter.trim().to_lowercase();
        let mut rows: Vec<usize> = (0..self.apps.len())
            .filter(|&i| {
                let a = &self.apps[i];
                q.is_empty()
                    || a.name.to_lowercase().contains(&q)
                    || a.publisher.to_lowercase().contains(&q)
            })
            .collect();
        rows.sort_by(|&x, &y| {
            let (a, b) = (&self.apps[x], &self.apps[y]);
            let ord = match self.sort {
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Installed => a.installed.cmp(&b.installed),
                SortKey::Size => self.size_of(a).0.cmp(&self.size_of(b).0),
            };
            let ord = if self.descending { ord.reverse() } else { ord };
            ord.then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        rows
    }

    fn selected_apps(&self) -> Vec<InstalledApp> {
        self.rows()
            .into_iter()
            .map(|i| &self.apps[i])
            .filter(|a| self.selected.contains(&a.id) && !self.status.contains_key(&a.id))
            .cloned()
            .collect()
    }

    fn click(&mut self, rows: &[usize], i: usize, ctrl: bool, shift: bool) {
        let id = self.apps[i].id.clone();
        if shift && let Some(anchor) = &self.anchor {
            let pos = |id: &str| rows.iter().position(|&r| self.apps[r].id == id);
            if let (Some(a), Some(b)) = (pos(anchor), pos(&id)) {
                if !ctrl {
                    self.selected.clear();
                }
                for &r in &rows[a.min(b)..=a.max(b)] {
                    self.selected.insert(self.apps[r].id.clone());
                }
                return;
            }
        }
        if ctrl {
            if !self.selected.remove(&id) {
                self.selected.insert(id.clone());
            }
        } else {
            self.selected.clear();
            self.selected.insert(id.clone());
        }
        self.anchor = Some(id);
    }

    /// 快捷键：Delete 卸载选中项、Ctrl+A 全选、Esc 取消选择。只在没有输入焦点和对话框时调用。
    pub fn keys(&mut self, ctx: &egui::Context) -> Option<AppsAction> {
        let (del, all, esc) = ctx.input(|i| {
            (
                i.key_pressed(Key::Delete) || i.events.contains(&egui::Event::Cut),
                i.modifiers.command && i.key_pressed(Key::A),
                i.key_pressed(Key::Escape),
            )
        });
        if all {
            let rows = self.rows();
            self.selected = rows.iter().map(|&i| self.apps[i].id.clone()).collect();
        }
        if esc {
            self.selected.clear();
        }
        let list = self.selected_apps();
        (del && !list.is_empty() && !self.busy()).then_some(AppsAction::Uninstall(list))
    }

    pub fn show(&mut self, ui: &mut Ui, icons: &mut FileIcons) -> Option<AppsAction> {
        if !self.loaded && self.load_rx.is_none() {
            self.reload(ui.ctx());
        }
        let p = Palette::of(ui.ctx());
        let mut action = None;
        ui.add_space(4.0);

        let rows = self.rows();
        let chosen = self.selected_apps();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            widgets::search_box(
                ui,
                &mut self.filter,
                280.0,
                t!(
                    "按名称或发布者搜索",
                    "Search by name or publisher",
                    "名前または発行元で検索"
                ),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if chosen.is_empty() {
                    t!("卸载", "Uninstall", "アンインストール").to_string()
                } else {
                    let n = chosen.len();
                    tf!("卸载 {n} 个", "Uninstall {n}", "{n} 個をアンインストール")
                };
                if widgets::tool_button(
                    ui,
                    Icon::Trash,
                    &label,
                    !chosen.is_empty() && !self.busy(),
                    true,
                )
                .clicked()
                {
                    action = Some(AppsAction::Uninstall(chosen.clone()));
                }
                if widgets::tool_button(
                    ui,
                    Icon::Refresh,
                    t!("刷新", "Refresh", "最新の情報に更新"),
                    self.load_rx.is_none() && !self.busy(),
                    false,
                )
                .clicked()
                {
                    self.sizes.clear();
                    self.reload(ui.ctx());
                }
            });
        });
        ui.add_space(10.0);

        let mods = ui.input(|i| i.modifiers);
        let mut clicked = None;
        let mut menu = None;
        let mut open_location = None;
        let header =
            |ui: &mut Ui, page: &mut Self, label: &str, key: Option<SortKey>, right: bool| {
                let p = Palette::of(ui.ctx());
                let on = key.is_some_and(|k| k == page.sort);
                let text = RichText::new(label)
                    .size(12.0)
                    .color(if on { p.text } else { p.muted });
                let resp = widgets::table_header(ui, text, right, |ui| {
                    if on {
                        let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                        let glyph = if page.descending {
                            Icon::ChevronDown
                        } else {
                            Icon::ChevronRight
                        };
                        widgets::icon(ui, glyph, r.center(), 12.0, p.muted);
                    }
                });
                if let Some(k) = key
                    && resp.clicked()
                {
                    if page.sort == k {
                        page.descending = !page.descending;
                    } else {
                        page.sort = k;
                        page.descending = k != SortKey::Name;
                    }
                }
            };
        ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
        TableBuilder::new(ui)
            .id_salt("apps")
            .striped(false)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(260.0).clip(true))
            .column(Column::exact(130.0).clip(true))
            .column(Column::exact(92.0))
            .column(Column::exact(88.0))
            .column(Column::exact(96.0))
            .header(30.0, |mut h| {
                h.col(|ui| {
                    header(
                        ui,
                        self,
                        t!("名称", "Name", "名前"),
                        Some(SortKey::Name),
                        false,
                    )
                });
                h.col(|ui| header(ui, self, t!("版本", "Version", "バージョン"), None, false));
                h.col(|ui| {
                    header(
                        ui,
                        self,
                        t!("安装日期", "Installed", "インストール日"),
                        Some(SortKey::Installed),
                        true,
                    )
                });
                h.col(|ui| {
                    header(
                        ui,
                        self,
                        t!("大小", "Size", "サイズ"),
                        Some(SortKey::Size),
                        true,
                    )
                });
                h.col(|ui| header(ui, self, t!("卸载方式", "Removal", "削除方法"), None, true));
            })
            .body(|body| {
                body.rows(theme::ROW_HEIGHT + 12.0, rows.len(), |mut row| {
                    let i = rows[row.index()];
                    let a = &self.apps[i];
                    let status = self.status.get(&a.id);
                    let dim = matches!(status, Some(Status::Queued | Status::Running));
                    row.set_selected(self.selected.contains(&a.id));
                    row.col(|ui| name_cell(ui, a, icons, dim));
                    row.col(|ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&a.version).size(12.0).color(p.faint))
                                .truncate(),
                        );
                    });
                    row.col(|ui| {
                        let date = a.installed.map(crate::fmt_time).unwrap_or_default();
                        let date = date.get(..10).unwrap_or(&date).to_string();
                        widgets::right_label(ui, RichText::new(date).size(12.0).color(p.faint));
                    });
                    row.col(|ui| {
                        let (size, measured) = self.size_of(a);
                        let text = match size {
                            Some(s) if measured => RichText::new(fmt_bytes(s)).color(p.text),
                            Some(s) => RichText::new(format!("≈ {}", fmt_bytes(s))).color(p.faint),
                            None => RichText::new("—").color(p.faint),
                        };
                        widgets::right_label(ui, text);
                    });
                    row.col(|ui| {
                        // 纯文字：进行中的状态用正文色，失败用警示色，平时的卸载方式用次要色。
                        let (label, color) = match status {
                            Some(Status::Queued) => (t!("等待中", "Queued", "待機中"), p.muted),
                            Some(Status::Running) => {
                                (t!("正在卸载…", "Removing…", "削除中…"), p.text)
                            }
                            Some(Status::Failed(_)) => (t!("失败", "Failed", "失敗"), p.danger),
                            None if a.method.quick() => (a.method.label(), p.muted),
                            None => (a.method.label(), p.faint),
                        };
                        widgets::right_label(ui, RichText::new(label).size(12.0).color(color));
                    });
                    let resp = row.response();
                    if resp.clicked() {
                        clicked = Some(i);
                    }
                    if resp.double_clicked()
                        && let Some(l) = &a.location
                    {
                        open_location = Some(l.clone());
                    }
                    if resp.secondary_clicked()
                        && let Some(pos) = resp.interact_pointer_pos()
                    {
                        menu = Some((i, pos));
                    }
                    if let Some(Status::Failed(e)) = status {
                        resp.on_hover_text(e.as_str());
                    }
                });
            });
        if let Some(i) = clicked {
            self.click(&rows, i, mods.command, mods.shift);
        }
        if let Some((i, pos)) = menu {
            // 右键未选中的项：改为只选它。
            if !self.selected.contains(&self.apps[i].id) {
                self.click(&rows, i, false, false);
            }
            self.menu = Some((self.apps[i].id.clone(), pos));
        }
        if let Some(l) = open_location {
            crate::os::open(&l);
        }
        if let Some(a) = self.context_menu(ui.ctx()) {
            action = Some(a);
        }
        action
    }

    fn context_menu(&mut self, ctx: &egui::Context) -> Option<AppsAction> {
        let (id, pos) = self.menu.clone()?;
        let app = self.apps.iter().find(|a| a.id == id)?.clone();
        let chosen = self.selected_apps();
        let busy = self.busy();
        let mut action = None;
        let open = widgets::floating_menu(ctx, "app-menu", pos, 220.0, |ui| {
            let title = if chosen.len() > 1 {
                let n = chosen.len();
                tf!(
                    "已选 {n} 个应用",
                    "{n} apps selected",
                    "{n} 個のアプリを選択"
                )
            } else {
                app.name.clone()
            };
            widgets::menu_heading(ui, &title, 0.0);
            let mut acted = false;
            if let Some(l) = &app.location
                && widgets::menu_item(
                    ui,
                    Icon::FolderOpen,
                    t!(
                        "打开安装位置",
                        "Open install location",
                        "インストール先を開く"
                    ),
                    false,
                )
                .clicked()
            {
                crate::os::open(l);
                acted = true;
            }
            if widgets::menu_item(
                ui,
                Icon::Copy,
                t!("复制名称", "Copy name", "名前をコピー"),
                false,
            )
            .clicked()
            {
                let names: Vec<&str> = if chosen.len() > 1 {
                    chosen.iter().map(|a| a.name.as_str()).collect()
                } else {
                    vec![app.name.as_str()]
                };
                ui.ctx().copy_text(names.join("\r\n"));
                acted = true;
            }
            ui.separator();
            let list = if chosen.is_empty() {
                vec![app.clone()]
            } else {
                chosen.clone()
            };
            let label = if list.len() > 1 {
                let n = list.len();
                tf!(
                    "卸载 {n} 个应用…",
                    "Uninstall {n} apps…",
                    "{n} 個のアプリをアンインストール…"
                )
            } else {
                t!("卸载…", "Uninstall…", "アンインストール…").to_string()
            };
            if busy {
                widgets::menu_item_disabled(
                    ui,
                    Icon::Trash,
                    &label,
                    t!(
                        "正在卸载其他应用",
                        "Another uninstall is running",
                        "ほかのアンインストールを実行中です"
                    ),
                );
            } else if widgets::menu_item(ui, Icon::Trash, &label, true).clicked() {
                action = Some(AppsAction::Uninstall(list));
                acted = true;
            }
            acted
        });
        if !open {
            self.menu = None;
        }
        action
    }
}

/// 名称列：图标、名称，下面一行发布者（商店应用加注）。
fn name_cell(ui: &mut Ui, a: &InstalledApp, icons: &mut FileIcons, dim: bool) {
    let p = Palette::of(ui.ctx());
    let rect = ui.max_rect();
    let icon = Rect::from_center_size(pos2(rect.left() + 12.0, rect.center().y), vec2(20.0, 20.0));
    let tint = if dim {
        egui::Color32::from_white_alpha(110)
    } else {
        egui::Color32::WHITE
    };
    match a
        .icon
        .as_deref()
        .and_then(|i| icons.get(&file_icons::app_key(i)))
    {
        Some(tex) => {
            ui.painter().image(
                tex.id(),
                icon,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                tint,
            );
        }
        None => widgets::icon(ui, Icon::Package, icon.center(), 18.0, p.muted),
    }
    let text = Rect::from_min_max(pos2(rect.left() + 32.0, rect.top()), rect.max);
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(text)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add_space(4.0);
            let color = if dim { p.faint } else { p.text };
            ui.add(egui::Label::new(RichText::new(&a.name).color(color)).truncate());
            let mut sub = a.publisher.clone();
            if a.store {
                let tag = t!("商店应用", "Store app", "ストア アプリ");
                sub = if sub.is_empty() {
                    tag.into()
                } else {
                    format!("{sub} · {tag}")
                };
            }
            ui.add(egui::Label::new(RichText::new(sub).size(11.5).color(p.faint)).truncate());
        },
    );
}
