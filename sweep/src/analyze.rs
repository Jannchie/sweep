//! 分析页：面包屑与工具栏，下面是目录树（虚拟化的表格）、文件类型面板和矩形树图。
//! 合计、选中项的大小在底部状态栏。

use std::sync::Arc;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Key, Layout, Margin, Rect, RichText,
    Sense, Stroke, Ui, UiBuilder, pos2, vec2,
};
use egui_extras::{Column, TableBuilder};
use sweep_scan::{NONE, Node, Tree, flags, t, tf};

use crate::doc::Doc;
use crate::file_icons::{self, FileIcons};
use crate::icons_gen::Icon;
use crate::theme::{self, Palette, semibold, types};
use crate::treemap::{self, TileKind, Treemap};
use crate::{Action, List, fmt_bytes, fmt_count, fmt_time, widgets};

/// 每层缩进。
const INDENT: f32 = 16.0;

pub fn show(
    ui: &mut Ui,
    doc: &mut Doc,
    tm: &mut Treemap,
    icons: &mut FileIcons,
    focus_search: &mut bool,
    deleting: bool,
) -> Vec<Action> {
    let mut actions = Vec::new();
    ui.add_space(4.0);
    toolbar(ui, doc, focus_search, deleting, &mut actions);
    ui.add_space(10.0);

    let body = ui.available_rect_before_wrap();
    let p = Palette::of(ui.ctx());
    egui::Panel::bottom("treemap-panel")
        .resizable(true)
        .show_separator_line(false)
        .default_size((body.height() * 0.46).round())
        .size_range(120.0..=(body.height() - 140.0).max(130.0))
        .frame(Frame::NONE.fill(p.bg).inner_margin(Margin {
            top: 10,
            ..Default::default()
        }))
        .show(ui, |ui| treemap_view(ui, doc, tm, &mut actions));
    egui::Panel::right("types-panel")
        .resizable(true)
        .show_separator_line(false)
        .default_size(290.0)
        .size_range(220.0..=480.0)
        .frame(Frame::NONE.fill(p.bg).inner_margin(Margin {
            left: 14,
            ..Default::default()
        }))
        .show(ui, |ui| types_view(ui, doc, &mut actions));
    egui::CentralPanel::default()
        .frame(Frame::NONE.fill(p.bg))
        .show(ui, |ui| tree_table(ui, doc, icons, &mut actions));
    actions
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64 * 100.0
    }
}

/// 面包屑（矩形树图当前的根）+ 搜索框 + 按钮。
fn toolbar(
    ui: &mut Ui,
    doc: &mut Doc,
    focus_search: &mut bool,
    deleting: bool,
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui.ctx());
    let t = doc.tree.clone();
    let summary = doc.summary();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        let chain: Vec<u32> = t.ancestors(doc.zoom).collect();
        let up = widgets::tool_button(
            ui,
            Icon::CornerLeftUp,
            t!("上一级", "Up", "上へ"),
            doc.zoom != t.root,
            false,
        )
        .on_hover_text(t!(
            "矩形树图回到上一级（Backspace）",
            "Treemap up one level (Backspace)",
            "ツリーマップを 1 つ上の階層へ (Backspace)"
        ));
        if up.clicked() && doc.zoom != t.root {
            actions.push(Action::ZoomOut);
        }
        ui.add_space(6.0);
        // 太深时只显示最后几级。
        let shown = chain.len().min(5);
        if chain.len() > shown {
            ui.label(RichText::new("…").color(p.faint));
        }
        for (k, &n) in chain.iter().take(shown).rev().enumerate() {
            if k > 0 || chain.len() > shown {
                let (r, _) = ui.allocate_exact_size(vec2(14.0, 28.0), Sense::hover());
                widgets::icon(ui, Icon::ChevronRight, r.center(), 13.0, p.faint);
            }
            let last = n == doc.zoom;
            let resp =
                crumb(ui, crate::short_name(&t, n), last, n == t.root).on_hover_text(t.path(n));
            if resp.clicked() && !last {
                actions.push(Action::ZoomTo(n));
            }
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let can = summary.deletable > 0;
            let resp = widgets::tool_button(
                ui,
                Icon::Trash,
                t!("移到回收站", "Move to Recycle Bin", "ごみ箱に移動"),
                can && !deleting,
                false,
            );
            let what = match doc.selection.single() {
                Some(s) => t.path(s),
                None => {
                    let (n, size) = (crate::n_items(summary.count as u64), fmt_bytes(summary.total));
                    format!("{n} · {size}")
                }
            };
            let resp = if can {
                resp.on_hover_text(tf!(
                    "{what}（Delete；Shift+Delete 永久删除）",
                    "{what} (Delete; Shift+Delete to delete permanently)",
                    "{what} (Delete、Shift+Delete で完全に削除)"
                ))
            } else if summary.count > 0 {
                resp.on_hover_text(t!(
                    "选中的项目不能删除",
                    "The selected items can't be deleted",
                    "選択した項目は削除できません"
                ))
            } else {
                resp.on_hover_text(t!(
                    "先在列表或矩形树图中选中项目（Ctrl/Shift+单击可多选）",
                    "Select items in the list or treemap first (Ctrl/Shift+click to select several)",
                    "先にリストかツリーマップで項目を選択してください (Ctrl/Shift+クリックで複数選択)"
                ))
            };
            if resp.clicked() && can {
                actions.push(Action::DeleteSelection { permanent: false });
            }
            let target = &doc.target;
            if widgets::tool_button(
                ui,
                Icon::Refresh,
                t!("重新扫描", "Rescan", "再スキャン"),
                true,
                false,
            )
            .on_hover_text(tf!(
                "重新扫描 {target}",
                "Rescan {target}",
                "{target} を再スキャン"
            ))
            .clicked()
            {
                actions.push(Action::Refresh);
            }
            let resp = widgets::search_box(
                ui,
                &mut doc.search.query,
                260.0,
                t!(
                    "按名称搜索（Ctrl+F）",
                    "Search by name (Ctrl+F)",
                    "名前で検索 (Ctrl+F)"
                ),
            );
            if std::mem::take(focus_search) {
                resp.request_focus();
            }
            if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                doc.search.query.clear();
            }
            if doc.search.active() {
                let text = if doc.search.busy() {
                    t!("搜索中…", "Searching…", "検索しています…").to_string()
                } else if doc.search.results.len() >= 2000 {
                    t!("前 2000 项", "First 2,000", "先頭の 2,000 件").to_string()
                } else {
                    let n = fmt_count(doc.search.results.len() as u64);
                    let s = if doc.search.results.len() == 1 {
                        ""
                    } else {
                        "s"
                    };
                    tf!("{n} 项", "{n} result{s}", "{n} 件")
                };
                ui.label(RichText::new(text).size(12.0).color(p.faint));
            }
        });
    });
}

fn crumb(ui: &mut Ui, label: &str, last: bool, root: bool) -> egui::Response {
    let p = Palette::of(ui.ctx());
    let font = if last {
        FontId::new(13.0, semibold())
    } else {
        FontId::proportional(13.0)
    };
    let extra = if root { 22.0 } else { 0.0 };
    // 每一级最多 220pt 宽，放不下时以省略号截断（悬停显示完整路径）。
    let mut job = egui::text::LayoutJob::simple_singleline(label.to_string(), font, p.text);
    job.wrap = egui::text::TextWrapping::truncate_at_width(220.0 - 16.0 - extra);
    let g = ui.painter().layout_job(job);
    let w = g.size().x + 16.0 + extra;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::click());
    if resp.hovered() && !last {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), p.hover);
    }
    let color = if last { p.text } else { p.muted };
    if root {
        widgets::icon(
            ui,
            Icon::Home,
            pos2(rect.left() + 15.0, rect.center().y),
            14.0,
            color,
        );
    }
    ui.painter().galley(
        pos2(
            rect.left() + 8.0 + extra,
            rect.center().y - g.size().y / 2.0,
        ),
        g,
        color,
    );
    resp
}

// ---------------------------------------------------------------- 目录树

fn tree_table(ui: &mut Ui, doc: &mut Doc, icons: &mut FileIcons, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    let t: Arc<Tree> = doc.tree.clone();
    let scroll_to = doc.scroll_to.take();
    let searching = doc.search.active();
    let rows: &[(u32, u16)] = if searching { &[] } else { &doc.rows };
    let results = &doc.search.results;
    let n = if searching { results.len() } else { rows.len() };
    let selection = &doc.selection;
    let total = t.value(t.root);
    let mods = ui.input(|i| i.modifiers);

    ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
    let mut builder = TableBuilder::new(ui)
        .striped(false)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::remainder().at_least(220.0).clip(true))
        .column(Column::exact(84.0))
        .column(Column::exact(124.0))
        .column(Column::exact(82.0))
        .column(Column::exact(118.0));
    if let Some(r) = scroll_to {
        builder = builder.scroll_to_row(r, None);
    }
    builder
        .header(30.0, |mut h| {
            for (label, right) in [
                (t!("名称", "Name", "名前"), false),
                (t!("大小", "Size", "サイズ"), true),
                (t!("占上级", "% of parent", "親に占める割合"), false),
                (t!("文件", "Files", "ファイル数"), true),
                (t!("修改时间", "Modified", "更新日時"), true),
            ] {
                h.col(|ui| widgets::header_label(ui, label, right));
            }
        })
        .body(|body| {
            body.rows(theme::ROW_HEIGHT, n, |mut row| {
                let i = row.index();
                let (node, depth) = if searching { (results[i], 0) } else { rows[i] };
                let nd = *t.node(node);
                row.set_selected(selection.contains(node));
                let mut toggled = false;
                row.col(|ui| {
                    toggled = name_cell(
                        ui,
                        &t,
                        node,
                        depth,
                        searching,
                        icons,
                        doc.expanded.contains(&node),
                    );
                });
                let value = t.value(node);
                row.col(|ui| {
                    widgets::right_label(ui, RichText::new(fmt_bytes(value)).color(p.text));
                });
                row.col(|ui| {
                    let parent = nd.parent;
                    let whole = if parent == NONE || searching {
                        total
                    } else {
                        t.value(parent)
                    };
                    share_cell(ui, value, whole);
                });
                row.col(|ui| {
                    if nd.is_dir() {
                        widgets::right_label(
                            ui,
                            RichText::new(fmt_count(nd.files as u64)).color(p.muted),
                        );
                    }
                });
                row.col(|ui| {
                    widgets::right_label(
                        ui,
                        RichText::new(fmt_time(nd.mtime)).size(12.0).color(p.faint),
                    );
                });
                let resp = row.response();
                if toggled {
                    actions.push(Action::Toggle(node));
                } else if resp.clicked() {
                    actions.push(Action::Click {
                        node,
                        list: List::Tree,
                        ctrl: mods.command,
                        shift: mods.shift,
                    });
                }
                if resp.double_clicked() && !mods.command && !mods.shift {
                    // 文件像资源管理器一样用默认程序打开；文件夹展开，搜索结果里的文件夹定位到树中。
                    if !nd.is_dir() {
                        actions.push(Action::OpenFile(node));
                    } else if searching {
                        actions.push(Action::Locate(node));
                    } else {
                        actions.push(Action::Toggle(node));
                    }
                }
                if resp.secondary_clicked()
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    actions.push(Action::OpenMenu(node, pos));
                }
                resp.on_hover_ui(|ui| node_tooltip(ui, &t, node));
            });
        });
    if n == 0 && searching && !doc.search.busy() {
        ui.add_space(20.0);
        ui.label(
            RichText::new(t!(
                "没有名称匹配的项",
                "No names match",
                "名前が一致する項目はありません"
            ))
            .color(p.muted),
        );
    }
}

/// 名称列：缩进、展开箭头、图标、名字与状态标签。返回是否点了展开箭头。
fn name_cell(
    ui: &mut Ui,
    t: &Tree,
    n: u32,
    depth: u16,
    flat: bool,
    icons: &mut FileIcons,
    expanded: bool,
) -> bool {
    let p = Palette::of(ui.ctx());
    let node = t.node(n);
    let rect = ui.max_rect();
    let cy = rect.center().y;
    let mut x = rect.left() + 2.0 + depth as f32 * INDENT;
    let mut toggled = false;
    if !flat {
        if node.is_dir() && node.first_child != NONE {
            let r = Rect::from_center_size(pos2(x + 7.0, cy), vec2(16.0, rect.height()));
            let resp = ui.interact(r, ui.id().with(("chevron", n)), Sense::click());
            widgets::chevron(
                ui,
                r.center(),
                expanded,
                if resp.hovered() { p.text } else { p.muted },
            );
            toggled = resp.clicked();
        }
        x += INDENT;
    }
    let key = if node.is_dir() {
        file_icons::FOLDER
    } else {
        t.ext_name(n)
    };
    let icon_rect = Rect::from_min_size(pos2(x, cy - 8.0), vec2(16.0, 16.0));
    match icons.get(key) {
        Some(tex) => {
            ui.painter().image(
                tex.id(),
                icon_rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        None => widgets::icon(
            ui,
            if node.is_dir() {
                Icon::Folder
            } else {
                Icon::File
            },
            icon_rect.center(),
            15.0,
            p.muted,
        ),
    }
    x += 24.0;
    let text_rect = Rect::from_min_max(pos2(x, rect.top()), rect.right_bottom());
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(text_rect)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.add(egui::Label::new(RichText::new(t.name(n)).color(p.text)).truncate());
            for (flag, label) in [
                (flags::DENIED, t!("无权限", "No access", "アクセス不可")),
                (
                    flags::META,
                    t!("NTFS 元数据", "NTFS metadata", "NTFS メタデータ"),
                ),
                (flags::HARDLINK, t!("硬链接", "Hard link", "ハード リンク")),
            ] {
                if node.has(flag) {
                    widgets::status_chip(ui, label, p.muted);
                }
            }
            if node.has(flags::REPARSE) && node.is_dir() && node.first_child == NONE {
                widgets::status_chip(ui, t!("链接", "Link", "リンク"), p.muted);
            }
            if let Some(label) = cloud_label(node) {
                widgets::status_chip(ui, label, p.muted);
            }
            if flat {
                let parent = node.parent;
                if parent != NONE {
                    ui.add(
                        egui::Label::new(RichText::new(t.path(parent)).size(12.0).color(p.faint))
                            .truncate(),
                    );
                }
            }
        },
    );
    toggled
}

/// 占上级的比例：点阵进度条 + 百分比。
fn share_cell(ui: &mut Ui, part: u64, whole: u64) {
    let p = Palette::of(ui.ctx());
    let rect = ui.max_rect();
    let frac = if whole == 0 {
        0.0
    } else {
        part as f32 / whole as f32
    };
    let bar = Rect::from_min_max(
        pos2(rect.left(), rect.center().y - 3.0),
        pos2(rect.left() + 72.0, rect.center().y + 3.0),
    );
    widgets::meter(ui.painter(), bar, frac, p.faint, p.track);
    ui.painter().text(
        pos2(rect.right(), rect.center().y),
        Align2::RIGHT_CENTER,
        format!("{:.1}%", frac * 100.0),
        FontId::proportional(12.0),
        p.muted,
    );
}

/// 云端占位文件的标签：本地没有占用是"仅在云端"，下载了一部分是"部分在云端"。
fn cloud_label(node: &Node) -> Option<&'static str> {
    (node.has(flags::CLOUD) && !node.is_dir()).then(|| {
        if node.alloc == 0 {
            t!("仅在云端", "Online-only", "オンライン専用")
        } else {
            t!("部分在云端", "Partly online", "一部オンライン")
        }
    })
}

fn node_tooltip(ui: &mut Ui, t: &Tree, n: u32) {
    let p = Palette::of(ui.ctx());
    let node = t.node(n);
    ui.set_max_width(460.0);
    ui.label(RichText::new(t.name(n)).family(semibold()).color(p.text));
    ui.label(RichText::new(t.path(n)).size(12.0).color(p.faint));
    ui.add_space(4.0);
    ui.label(
        RichText::new({
            let (size, alloc) = (fmt_bytes(node.size), fmt_bytes(node.alloc));
            tf!(
                "文件大小 {size} · 占用空间 {alloc}",
                "Size {size} · Size on disk {alloc}",
                "サイズ {size} · ディスク上のサイズ {alloc}"
            )
        })
        .size(12.5)
        .color(p.muted),
    );
    if node.is_dir() {
        ui.label(
            RichText::new({
                let (files, dirs) = (fmt_count(node.files as u64), fmt_count(node.dirs as u64));
                tf!(
                    "{files} 个文件 · {dirs} 个文件夹",
                    "{files} files · {dirs} folders",
                    "ファイル {files} 個 · フォルダー {dirs} 個"
                )
            })
            .size(12.5)
            .color(p.muted),
        );
    }
    ui.label(
        RichText::new({
            let time = fmt_time(node.mtime);
            tf!("修改于 {time}", "Modified {time}", "更新日時 {time}")
        })
        .size(12.5)
        .color(p.muted),
    );
    if cloud_label(node).is_some() {
        ui.label(
            RichText::new(t!(
                "云端文件的占位：内容在云端，本地只占用上面的占用空间。按「占用空间」统计时只计本地这部分。",
                "A cloud file placeholder: its content lives online and only the size on disk above is stored locally. Counting by Size on disk includes only the local part.",
                "クラウド ファイルのプレースホルダーです。内容はオンラインにあり、ローカルには上のディスク上のサイズだけが保存されています。「ディスク上のサイズ」で集計すると、ローカルの分だけが数えられます。"
            ))
            .size(12.0)
            .color(p.faint),
        );
    }
    if node.has(flags::HARDLINK) {
        ui.label(
            RichText::new(t!(
                "同一文件的另一个硬链接，大小已计入第一个名字。",
                "Another hard link to the same file. Its size is counted under the first name.",
                "同じファイルへの別のハード リンクです。サイズは最初の名前で計上されています。"
            ))
            .size(12.0)
            .color(p.faint),
        );
    }
}

// ---------------------------------------------------------------- 文件类型

fn types_view(ui: &mut Ui, doc: &Doc, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    let (Some(d), Some((scope, exts))) = (&doc.derived, doc.type_stats()) else {
        ui.label(
            RichText::new(t!(
                "正在统计文件类型…",
                "Counting file types…",
                "ファイルの種類を集計しています…"
            ))
            .color(p.muted),
        );
        return;
    };
    let t = &doc.tree;
    let total = t.value(scope).max(1);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(t!("文件类型", "File types", "ファイルの種類"))
                .size(13.0)
                .family(semibold())
                .color(p.muted),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if doc.highlight_ext.is_some() {
                if ui
                    .add(
                        egui::Label::new(
                            RichText::new(t!("取消突出显示", "Clear highlight", "強調表示を解除"))
                                .size(12.0)
                                .color(p.accent),
                        )
                        .sense(Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    actions.push(Action::Highlight(None));
                }
            } else {
                ui.label(
                    RichText::new({
                        let n = fmt_count(exts.len() as u64);
                        tf!("{n} 种", "{n} types", "{n} 種類")
                    })
                    .size(12.0)
                    .color(p.faint),
                );
            }
        });
    });
    // 统计范围：跟随选中的文件夹（选中文件时是它所在的文件夹）。
    // 整个驱动器（如 `C:\`）用磁盘图标，其余用文件夹。
    let glyph = if t.name(scope).ends_with(":\\") {
        Icon::HardDrive
    } else {
        Icon::Folder
    };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (r, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
        widgets::icon(ui, glyph, r.center(), 13.0, p.faint);
        ui.add(
            egui::Label::new(RichText::new(t.name(scope)).size(12.0).color(p.faint))
                .truncate()
                .selectable(false),
        )
        .on_hover_text(t.path(scope));
    });
    ui.add_space(6.0);
    const ROW: f32 = 38.0;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, ROW, exts.len(), |ui, range| {
            for &(ext, size, count) in &exts[range] {
                let (rect, resp) =
                    ui.allocate_exact_size(vec2(ui.available_width(), ROW), Sense::click());
                let on = doc.highlight_ext == Some(ext);
                if on || resp.hovered() {
                    ui.painter().rect_filled(
                        rect,
                        CornerRadius::same(8),
                        if on { p.surface } else { p.hover },
                    );
                }
                let color = d
                    .colors
                    .get(ext as usize)
                    .and_then(|&c| types::PALETTE.get(c as usize))
                    .copied()
                    .unwrap_or(types::OTHER);
                let inner = rect.shrink2(vec2(10.0, 5.0));
                let sw = Rect::from_center_size(
                    pos2(inner.left() + 5.0, inner.top() + 8.0),
                    vec2(10.0, 10.0),
                );
                ui.painter().rect_filled(sw, CornerRadius::same(2), color);
                let name = if ext == 0 {
                    t!("无扩展名", "No extension", "拡張子なし").to_string()
                } else {
                    format!(".{}", t.exts[ext as usize])
                };
                ui.painter().text(
                    pos2(inner.left() + 18.0, inner.top() + 8.0),
                    Align2::LEFT_CENTER,
                    name,
                    FontId::proportional(13.0),
                    p.text,
                );
                ui.painter().text(
                    pos2(inner.right(), inner.top() + 8.0),
                    Align2::RIGHT_CENTER,
                    fmt_bytes(size),
                    FontId::proportional(12.5),
                    p.text,
                );
                let bar = Rect::from_min_max(
                    pos2(inner.left() + 18.0, inner.bottom() - 6.0),
                    pos2(inner.right() - 74.0, inner.bottom() - 1.0),
                );
                widgets::meter(
                    ui.painter(),
                    bar,
                    size as f32 / total as f32,
                    color,
                    p.track,
                );
                ui.painter().text(
                    pos2(inner.right(), inner.bottom() - 3.0),
                    Align2::RIGHT_CENTER,
                    {
                        let n = fmt_count(count);
                        tf!("{n} 个", "{n} files", "{n} 個")
                    },
                    FontId::proportional(11.0),
                    p.faint,
                );
                let share = pct(size, total);
                let resp = resp.on_hover_text(tf!(
                    "{share:.1}% · 点击在矩形树图中突出显示",
                    "{share:.1}% · Click to highlight in the treemap",
                    "{share:.1}% · クリックでツリーマップ内を強調表示"
                ));
                if resp.clicked() {
                    actions.push(Action::Highlight(if on { None } else { Some(ext) }));
                }
            }
        });
}

// ---------------------------------------------------------------- 矩形树图

fn treemap_view(ui: &mut Ui, doc: &mut Doc, tm: &mut Treemap, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    let ctx = ui.ctx().clone();
    let ppp = ctx.pixels_per_point();
    let avail = ui.available_rect_before_wrap();
    // 左上角对齐到物理像素，贴图与屏幕像素一一对应。
    let min = pos2(
        (avail.left() * ppp).round() / ppp,
        (avail.top() * ppp).round() / ppp,
    );
    let size = [
        ((avail.right() - min.x) * ppp).floor().max(0.0) as usize,
        ((avail.bottom() - min.y) * ppp).floor().max(0.0) as usize,
    ];
    let rect = Rect::from_min_size(min, vec2(size[0] as f32 / ppp, size[1] as f32 / ppp));
    let resp = ui.allocate_rect(rect, Sense::click());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, p.bg);

    let dark = ctx.theme() == egui::Theme::Dark;
    let unit = ppp.round().max(1.0) as u8;
    let key = treemap::Key {
        version: doc.version,
        root: doc.zoom,
        size,
        dark,
        highlight: doc.highlight_ext,
        unit,
    };
    if let Some(d) = &doc.derived {
        tm.request(key, || treemap::Request {
            key,
            tree: doc.tree.clone(),
            colors: d.colors.clone(),
            bg: p.bg,
        });
    }
    let Some(layout) = &tm.current else {
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            t!("正在布局…", "Laying out…", "レイアウトしています…"),
            FontId::proportional(13.0),
            p.faint,
        );
        return;
    };
    if let Some(tex) = &tm.texture {
        // 尺寸变化后、新结果到达前，先把旧图拉伸显示。
        ui.painter().image(
            tex.id(),
            rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    // 布局还没跟上（尺寸、数据、主题变了）时贴图是拉伸的旧图，覆盖层对不上，先不画。
    let stale = layout.key != key;
    let to_screen = |r: [f32; 4]| {
        let sx = rect.width() / layout.key.size[0].max(1) as f32;
        let sy = rect.height() / layout.key.size[1].max(1) as f32;
        Rect::from_min_max(
            pos2(rect.left() + r[0] * sx, rect.top() + r[1] * sy),
            pos2(rect.left() + r[2] * sx, rect.top() + r[3] * sy),
        )
    };
    // 块的圆角（物理像素）换成界面坐标。
    let corner = |tile: &treemap::Tile| {
        CornerRadius::same((tile.radius(unit) / ppp).round().clamp(0.0, 255.0) as u8)
    };
    let t = doc.tree.clone();
    // 文件夹标题栏：名字 + 大小，放不下时以省略号截断。只有够大的文件夹才有，数量很少。
    if !stale {
        let painter = ui.painter().with_clip_rect(rect);
        for &(i, hr) in &layout.headers {
            let n = layout.tiles[i].node;
            let hr = to_screen(hr);
            let inset = 6.0;
            let mut job = egui::text::LayoutJob::default();
            job.append(
                crate::short_name(&t, n),
                0.0,
                egui::TextFormat::simple(FontId::new(11.5, semibold()), p.text),
            );
            job.append(
                &fmt_bytes(t.value(n)),
                6.0,
                egui::TextFormat::simple(FontId::proportional(11.0), p.muted),
            );
            job.wrap =
                egui::text::TextWrapping::truncate_at_width((hr.width() - inset * 2.0).max(0.0));
            let g = painter.layout_job(job);
            let y = ((hr.center().y - g.size().y / 2.0) * ppp).round() / ppp;
            painter.galley(pos2(hr.left() + inset, y), g, p.text);
        }
    }
    // 每个选中项的外框。选中了哪些块只在布局或选中项变化时重新找一遍（块的数量与像素有关，
    // 与节点数无关，按位图判断）；上级文件夹也被选中时只画上级的框，太小的块不画。
    let sel = &doc.selection;
    if !stale && !sel.is_empty() {
        let cache_key = (layout.key, sel.generation());
        if tm.selected_tiles.as_ref().is_none_or(|c| c.0 != cache_key) {
            let t = &doc.tree;
            let hits = layout
                .tiles
                .iter()
                .enumerate()
                .filter(|(_, tile)| {
                    let n = tile.node;
                    let [x0, y0, x1, y1] = tile.rect;
                    tile.kind != TileKind::Rest
                        && n != doc.zoom
                        && x1 - x0 >= 4.0
                        && y1 - y0 >= 4.0
                        && sel.contains(n)
                        && !sel.contains(t.node(n).parent)
                })
                .map(|(i, _)| i)
                .collect();
            tm.selected_tiles = Some((cache_key, hits));
        }
        if let Some((_, hits)) = &tm.selected_tiles {
            // 细的 accent 色框，内侧再描一圈底色细线，橙色块上也看得清。
            let painter = ui.painter();
            for &i in hits {
                let tile = &layout.tiles[i];
                let r = to_screen(tile.rect);
                let cr = corner(tile);
                painter.rect_stroke(r, cr, Stroke::new(2.0, p.accent), egui::StrokeKind::Inside);
                painter.rect_stroke(
                    r.shrink(2.0),
                    cr,
                    Stroke::new(1.0, p.bg.gamma_multiply(0.85)),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
    let hovered = resp.hover_pos().filter(|_| !stale).and_then(|pos| {
        let x = (pos.x - rect.left()) * ppp;
        let y = (pos.y - rect.top()) * ppp;
        layout.hit(x, y).copied()
    });
    if let Some(tile) = hovered {
        // 悬停：色块提亮一点；文件夹（悬停在它的标题栏或缝隙上）描一圈淡边。
        let r = to_screen(tile.rect);
        match tile.kind {
            TileKind::Dir if tile.node != doc.zoom => {
                ui.painter().rect_stroke(
                    r,
                    corner(&tile),
                    Stroke::new(1.0, p.text.gamma_multiply(0.35)),
                    egui::StrokeKind::Inside,
                );
            }
            TileKind::Dir => {}
            _ => {
                let lift = if dark {
                    Color32::from_white_alpha(34)
                } else {
                    Color32::from_white_alpha(80)
                };
                ui.painter().rect_filled(r, corner(&tile), lift);
            }
        }
        let total = t.value(doc.zoom);
        let resp = resp.on_hover_ui_at_pointer(|ui| {
            let n = tile.node;
            if tile.kind == TileKind::Rest {
                ui.label(
                    RichText::new(t!("其余小文件", "Other small items", "その他の小さな項目"))
                        .family(semibold())
                        .color(p.text),
                );
                ui.label(RichText::new(t.path(n)).size(12.0).color(p.faint));
                ui.label(
                    RichText::new(t!(
                        "面积太小、合并显示的若干项",
                        "Items too small to draw, shown together",
                        "小さすぎるためまとめて表示している項目"
                    ))
                    .size(12.0)
                    .color(p.muted),
                );
            } else {
                node_tooltip(ui, &t, n);
                ui.label(
                    RichText::new({
                        let share = pct(t.value(n), total);
                        tf!(
                            "占当前视图 {share:.2}%",
                            "{share:.2}% of this view",
                            "このビューの {share:.2}%"
                        )
                    })
                    .size(12.0)
                    .color(p.muted),
                );
            }
        });
        let ctrl = ui.input(|i| i.modifiers.command);
        if resp.clicked() {
            if ctrl {
                // Ctrl+单击：切换这一项，不改变树的展开与滚动位置。
                actions.push(Action::Click {
                    node: tile.node,
                    list: List::Treemap,
                    ctrl: true,
                    shift: false,
                });
            } else {
                actions.push(Action::Locate(tile.node));
            }
        }
        if resp.double_clicked() && !ctrl {
            // 放大一级：命中节点在当前根下的那个直接子目录。
            let child = t
                .ancestors(tile.node)
                .find(|&a| t.node(a).parent == doc.zoom);
            if let Some(c) = child.filter(|&c| t.node(c).is_dir()) {
                actions.push(Action::ZoomTo(c));
            }
        }
        if resp.secondary_clicked()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            actions.push(Action::OpenMenu(tile.node, pos));
        }
    }
}
