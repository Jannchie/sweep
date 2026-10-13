//! 清理页：左边是可清理的项目（最上面是"大文件"，其后是各类缓存与回收站），
//! 右边显示选中项目的内容与操作——里面有哪些文件夹 / 文件、各占多少，可以只勾选一部分删除。

use std::cmp::Reverse;

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Layout, Rect, RichText, Sense, Ui,
    UiBuilder, pos2, vec2,
};
use egui_extras::{Column, TableBuilder};
use sweep_scan::{t, tf};

use crate::cleanup::{Action as Clean, Item, Suggestion};
use crate::doc::{CleanupPick, Doc};
use crate::file_icons::{self, FileIcons};
use crate::icons_gen::Icon;
use crate::theme::{self, Palette, semibold};
use crate::{Action, List, fmt_bytes, fmt_count, fmt_time, widgets};

const LIST_ROW_H: f32 = 48.0;

pub fn show(ui: &mut Ui, doc: &mut Doc, icons: &mut FileIcons, deleting: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    // 选中的建议在重新计算后消失了（例如已清空）：回到"大文件"。
    if let (CleanupPick::Suggestion(k), Some(list)) = (doc.cleanup_pick, &doc.suggestions)
        && !list.iter().any(|s| s.kind == k)
    {
        doc.cleanup_pick = CleanupPick::Largest;
        doc.cleanup_checked.clear();
    }

    ui.add_space(4.0);
    let avail = ui.available_rect_before_wrap();
    let gap = 20.0;
    let left_w = (avail.width() * 0.34).clamp(280.0, 380.0);
    let left = Rect::from_min_size(avail.min, vec2(left_w, avail.height()));
    let right = Rect::from_min_max(pos2(left.right() + gap, avail.top()), avail.max);

    ui.scope_builder(UiBuilder::new().max_rect(left), |ui| entries(ui, doc));
    // 两栏之间一条细竖线。
    let p = Palette::of(ui.ctx());
    ui.painter().vline(
        left.right() + gap / 2.0,
        left.y_range(),
        egui::Stroke::new(1.0, p.border),
    );
    ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
        match doc.cleanup_pick {
            CleanupPick::Largest => largest_detail(ui, doc, icons, deleting, &mut actions),
            CleanupPick::Suggestion(k) => {
                let s = doc
                    .suggestions
                    .as_ref()
                    .and_then(|l| l.iter().find(|s| s.kind == k))
                    .cloned();
                if let Some(s) = s {
                    suggestion_detail(ui, doc, &s, icons, deleting, &mut actions);
                }
            }
        }
    });
    ui.allocate_rect(avail, Sense::hover());
    actions
}

// ---------------------------------------------------------------- 左栏

fn entries(ui: &mut Ui, doc: &mut Doc) {
    let p = Palette::of(ui.ctx());
    let mut pick = None;
    egui::ScrollArea::vertical()
        .id_salt("cleanup-entries")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            let largest = doc.largest();
            let total: u64 = largest.iter().map(|&n| doc.tree.value(n)).sum();
            let sub = if doc.derived.is_none() {
                t!("正在统计…", "Counting…", "集計しています…").to_string()
            } else {
                let n = fmt_count(largest.len() as u64);
                tf!(
                    "最大的 {n} 个文件",
                    "The {n} largest files",
                    "最も大きい {n} 個のファイル"
                )
            };
            if entry_row(
                ui,
                Icon::File,
                t!("大文件", "Large files", "大きいファイル"),
                &sub,
                total,
                doc.cleanup_pick == CleanupPick::Largest,
            )
            .clicked()
            {
                pick = Some(CleanupPick::Largest);
            }

            ui.add_space(10.0);
            ui.label(
                RichText::new(t!(
                    "可清理的位置",
                    "Places to clean up",
                    "クリーンアップできる場所"
                ))
                .size(12.5)
                .family(semibold())
                .color(p.muted),
            );
            ui.add_space(4.0);
            match &doc.suggestions {
                None => {
                    ui.label(
                        RichText::new(t!(
                            "正在检查常见位置…",
                            "Checking common locations…",
                            "よく使われる場所を確認しています…"
                        ))
                        .size(12.5)
                        .color(p.muted),
                    );
                }
                Some(list) => {
                    if list.is_empty() {
                        ui.label(
                            RichText::new(t!(
                                "没有找到可以清理的常见位置。",
                                "Nothing to clean up in the usual places.",
                                "クリーンアップできる場所は見つかりませんでした。"
                            ))
                            .size(12.5)
                            .color(p.muted),
                        );
                    }
                    for s in list {
                        let sub = match s.action {
                            Clean::EmptyRecycleBin => crate::n_items(s.files),
                            _ => {
                                let (n, plural) =
                                    (fmt_count(s.files), if s.files == 1 { "" } else { "s" });
                                tf!("{n} 个文件", "{n} file{plural}", "ファイル {n} 個")
                            }
                        };
                        let on = doc.cleanup_pick == CleanupPick::Suggestion(s.kind);
                        if entry_row(ui, s.kind.icon(), &s.title(), &sub, s.size, on).clicked() {
                            pick = Some(CleanupPick::Suggestion(s.kind));
                        }
                    }
                    if doc.suggestions_loading() {
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(t!("正在更新…", "Updating…", "更新しています…"))
                                .size(12.0)
                                .color(p.faint),
                        );
                    }
                }
            }
        });
    if let Some(pk) = pick
        && pk != doc.cleanup_pick
    {
        doc.cleanup_pick = pk;
        doc.cleanup_checked.clear();
    }
}

/// 左栏的一项：图标、名称与一行说明，右边是大小。选中时中性底色加一道强调色竖条。
fn entry_row(
    ui: &mut Ui,
    glyph: Icon,
    title: &str,
    sub: &str,
    size: u64,
    on: bool,
) -> egui::Response {
    let p = Palette::of(ui.ctx());
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), LIST_ROW_H), Sense::click());
    if on {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), p.text.gamma_multiply(0.08));
        let bar = Rect::from_min_size(
            rect.left_top() + vec2(0.0, 14.0),
            vec2(3.0, LIST_ROW_H - 28.0),
        );
        ui.painter()
            .rect_filled(bar, CornerRadius::same(2), p.accent);
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), p.hover);
    }
    let cy = rect.center().y;
    widgets::icon(ui, glyph, pos2(rect.left() + 20.0, cy), 16.0, p.muted);
    let size_text = fmt_bytes(size);
    let size_g = ui
        .painter()
        .layout_no_wrap(size_text, FontId::new(13.0, semibold()), p.text);
    let size_x = rect.right() - 10.0 - size_g.size().x;
    ui.painter()
        .galley(pos2(size_x, cy - size_g.size().y / 2.0), size_g, p.text);
    let text_w = (size_x - rect.left() - 40.0 - 10.0).max(40.0);
    widgets::text_truncated(
        ui.painter(),
        pos2(rect.left() + 40.0, cy - 9.0),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(13.5),
        p.text,
        text_w,
    );
    widgets::text_truncated(
        ui.painter(),
        pos2(rect.left() + 40.0, cy + 10.0),
        Align2::LEFT_CENTER,
        sub,
        FontId::proportional(11.5),
        p.faint,
        text_w,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// ---------------------------------------------------------------- 右栏

/// 详情顶部：名称、说明与一行汇总。
fn header(ui: &mut Ui, title: &str, desc: &str, summary: &str) {
    let p = Palette::of(ui.ctx());
    ui.label(
        RichText::new(title)
            .size(16.0)
            .family(semibold())
            .color(p.text),
    );
    ui.add_space(2.0);
    ui.add(egui::Label::new(RichText::new(desc).size(12.5).color(p.muted)).wrap());
    if !summary.is_empty() {
        ui.add_space(2.0);
        ui.label(RichText::new(summary).size(12.5).color(p.text));
    }
    ui.add_space(10.0);
}

fn largest_detail(
    ui: &mut Ui,
    doc: &mut Doc,
    icons: &mut FileIcons,
    deleting: bool,
    actions: &mut Vec<Action>,
) {
    let summary = doc.summary();
    let chosen = if summary.count > 0 {
        let (n, size) = (
            crate::n_items(summary.count as u64),
            fmt_bytes(summary.total),
        );
        tf!(
            "已选 {n} · {size}",
            "{n} selected · {size}",
            "{n} を選択 · {size}"
        )
    } else {
        String::new()
    };
    header(
        ui,
        t!("大文件", "Large files", "大きいファイル"),
        t!(
            "扫描范围里最大的文件。Ctrl/Shift+单击多选，Delete 移到回收站，双击在目录树中定位。",
            "The largest files in the scanned location. Ctrl/Shift+click to select several, Delete to recycle, double-click to locate in the tree.",
            "スキャン範囲内で最も大きいファイルです。Ctrl/Shift+クリックで複数選択、Delete でごみ箱へ、ダブルクリックでツリー内の場所を表示します。"
        ),
        &chosen,
    );
    let can = summary.deletable > 0 && !deleting;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if widgets::tool_button(
            ui,
            Icon::Trash,
            t!("移到回收站", "Move to Recycle Bin", "ごみ箱に移動"),
            can,
            false,
        )
        .clicked()
        {
            actions.push(Action::DeleteSelection { permanent: false });
        }
        if widgets::tool_button(
            ui,
            Icon::TrashX,
            t!("永久删除…", "Delete permanently…", "完全に削除…"),
            can,
            true,
        )
        .clicked()
        {
            actions.push(Action::DeleteSelection { permanent: true });
        }
    });
    ui.add_space(8.0);
    largest_table(ui, doc, icons, actions);
}

fn suggestion_detail(
    ui: &mut Ui,
    doc: &mut Doc,
    s: &Suggestion,
    icons: &mut FileIcons,
    deleting: bool,
    actions: &mut Vec<Action>,
) {
    let p = Palette::of(ui.ctx());
    let summary = {
        let size = fmt_bytes(s.size);
        match s.action {
            Clean::EmptyRecycleBin => {
                let n = crate::n_items(s.files);
                tf!("{size} · {n}", "{size} · {n}", "{size} · {n}")
            }
            _ => {
                let (n, plural) = (fmt_count(s.files), if s.files == 1 { "" } else { "s" });
                tf!(
                    "{size} · {n} 个文件",
                    "{size} · {n} file{plural}",
                    "{size} · ファイル {n} 個"
                )
            }
        }
    };
    header(ui, &s.title(), s.detail(), &summary);

    let checked: Vec<Item> = s
        .items
        .iter()
        .filter(|i| doc.cleanup_checked.contains(&i.path))
        .cloned()
        .collect();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        match &s.action {
            Clean::Contents | Clean::Items => {
                // 勾选了就只删勾选的，否则删全部。
                let (targets, scope) = if checked.is_empty() {
                    let n = crate::n_items(s.items.len() as u64);
                    (s.items.clone(), tf!("全部 {n}", "all {n}", "すべて ({n})"))
                } else {
                    let size = fmt_bytes(checked.iter().map(|i| i.size).sum());
                    let n = crate::n_items(checked.len() as u64);
                    (
                        checked.clone(),
                        tf!(
                            "已选 {n} · {size}",
                            "{n} selected · {size}",
                            "選択した {n} · {size}"
                        ),
                    )
                };
                let label = {
                    let (title, n) = (s.title(), crate::n_items(targets.len() as u64));
                    match (s.action.clone(), checked.is_empty()) {
                        (Clean::Items, true) => title,
                        _ => tf!("{title} 中的 {n}", "{n} in {title}", "{title} 内の {n}"),
                    }
                };
                let ok = !deleting && !targets.is_empty();
                if widgets::tool_button(
                    ui,
                    Icon::Trash,
                    &tf!(
                        "移到回收站（{scope}）",
                        "Move to Recycle Bin ({scope})",
                        "ごみ箱に移動 ({scope})"
                    ),
                    ok,
                    false,
                )
                .clicked()
                {
                    actions.push(Action::Delete {
                        items: targets.clone(),
                        label: label.clone(),
                        permanent: false,
                    });
                }
                if widgets::tool_button(
                    ui,
                    Icon::TrashX,
                    t!("永久删除…", "Delete permanently…", "完全に削除…"),
                    ok,
                    true,
                )
                .clicked()
                {
                    actions.push(Action::Delete {
                        items: targets,
                        label,
                        permanent: true,
                    });
                }
            }
            Clean::EmptyRecycleBin => {
                if widgets::tool_button(
                    ui,
                    Icon::TrashX,
                    t!("清空回收站…", "Empty Recycle Bin…", "ごみ箱を空にする…"),
                    !deleting,
                    true,
                )
                .clicked()
                {
                    actions.push(Action::EmptyRecycleBin(s.size));
                }
            }
            Clean::Info(cmd) => {
                if let Some(cmd) = cmd
                    && widgets::tool_button(
                        ui,
                        Icon::ExternalLink,
                        t!(
                            "打开磁盘清理",
                            "Open Disk Cleanup",
                            "ディスク クリーンアップを開く"
                        ),
                        true,
                        false,
                    )
                    .clicked()
                {
                    actions.push(Action::Run(cmd.to_string()));
                }
            }
        }
        if let Some(f) = s.folders.first()
            && widgets::tool_button(
                ui,
                Icon::Target,
                t!("在目录树中定位", "Locate in tree", "ツリーで場所を表示"),
                true,
                false,
            )
            .on_hover_text(f.as_str())
            .clicked()
        {
            actions.push(Action::LocatePath(f.clone()));
        }
    });
    ui.add_space(8.0);
    if s.items.is_empty() {
        return;
    }
    items_table(ui, doc, s, icons, &p);
}

/// 建议里的条目：勾选框、名称（下面是所在文件夹）、大小、文件数。单击一行切换勾选。
fn items_table(ui: &mut Ui, doc: &mut Doc, s: &Suggestion, icons: &mut FileIcons, p: &Palette) {
    let all = !s.items.is_empty()
        && s.items
            .iter()
            .all(|i| doc.cleanup_checked.contains(&i.path));
    let mut toggle_all = false;
    let mut toggle = None;
    let mut locate = None;
    ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
    TableBuilder::new(ui)
        .id_salt(("cleanup-items", format!("{:?}", s.kind)))
        .striped(false)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::exact(22.0))
        .column(Column::remainder().at_least(200.0).clip(true))
        .column(Column::exact(84.0))
        .column(Column::exact(84.0))
        .header(30.0, |mut h| {
            h.col(|ui| {
                let mut v = all;
                if ui
                    .checkbox(&mut v, "")
                    .on_hover_text(t!("全选", "Select all", "すべて選択"))
                    .changed()
                {
                    toggle_all = true;
                }
            });
            h.col(|ui| widgets::header_label(ui, t!("名称", "Name", "名前"), false));
            h.col(|ui| widgets::header_label(ui, t!("大小", "Size", "サイズ"), true));
            h.col(|ui| widgets::header_label(ui, t!("文件", "Files", "ファイル"), true));
        })
        .body(|body| {
            body.rows(theme::ROW_HEIGHT + 8.0, s.items.len(), |mut row| {
                let item = &s.items[row.index()];
                let on = doc.cleanup_checked.contains(&item.path);
                row.set_selected(on);
                row.col(|ui| {
                    let mut v = on;
                    if ui.checkbox(&mut v, "").changed() {
                        toggle = Some(item.path.clone());
                    }
                });
                row.col(|ui| item_name(ui, item, icons, p));
                row.col(|ui| {
                    widgets::right_label(ui, RichText::new(fmt_bytes(item.size)).color(p.text))
                });
                row.col(|ui| {
                    let files = if item.dir {
                        fmt_count(item.files)
                    } else {
                        String::new()
                    };
                    widgets::right_label(ui, RichText::new(files).size(12.0).color(p.faint));
                });
                let resp = row.response();
                if resp.double_clicked() {
                    locate = Some(item.path.clone());
                } else if resp.clicked() {
                    toggle = Some(item.path.clone());
                }
                resp.on_hover_text(item.path.as_str());
            });
        });
    if toggle_all {
        if all {
            doc.cleanup_checked.clear();
        } else {
            doc.cleanup_checked = s.items.iter().map(|i| i.path.clone()).collect();
        }
    }
    if let Some(path) = toggle
        && !doc.cleanup_checked.remove(&path)
    {
        doc.cleanup_checked.insert(path);
    }
    if let Some(path) = locate {
        crate::os::reveal(&path);
    }
}

fn item_name(ui: &mut Ui, item: &Item, icons: &mut FileIcons, p: &Palette) {
    let rect = ui.max_rect();
    let icon = Rect::from_min_size(
        pos2(rect.left() + 2.0, rect.center().y - 8.0),
        vec2(16.0, 16.0),
    );
    let (name, parent) = match item.path.rsplit_once('\\') {
        Some((dir, name)) => (name, dir),
        None => (item.path.as_str(), ""),
    };
    let key = if item.dir {
        file_icons::FOLDER.to_string()
    } else {
        sweep_scan::tree::extension(name)
            .map(|e| e.to_lowercase())
            .unwrap_or_default()
    };
    match icons.get(&key) {
        Some(tex) => {
            ui.painter().image(
                tex.id(),
                icon,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        None => widgets::icon(ui, Icon::File, icon.center(), 15.0, p.muted),
    }
    let text = Rect::from_min_max(pos2(rect.left() + 26.0, rect.top()), rect.right_bottom());
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(text)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add_space(3.0);
            ui.add(egui::Label::new(RichText::new(name).color(p.text)).truncate());
            ui.add(egui::Label::new(RichText::new(parent).size(11.5).color(p.faint)).truncate());
        },
    );
}

fn largest_table(ui: &mut Ui, doc: &mut Doc, icons: &mut FileIcons, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    if doc.derived.is_none() {
        ui.label(RichText::new(t!("正在统计…", "Counting…", "集計しています…")).color(p.muted));
        return;
    }
    let t = doc.tree.clone();
    let mut list = doc.largest();
    list.sort_by_key(|&n| Reverse(t.value(n)));
    let selection = &doc.selection;
    let mods = ui.input(|i| i.modifiers);
    ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
    TableBuilder::new(ui)
        .id_salt("largest")
        .striped(false)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::remainder().at_least(200.0).clip(true))
        .column(Column::exact(84.0))
        .column(Column::exact(112.0))
        .header(30.0, |mut h| {
            h.col(|ui| widgets::header_label(ui, t!("文件", "File", "ファイル"), false));
            h.col(|ui| widgets::header_label(ui, t!("大小", "Size", "サイズ"), true));
            h.col(|ui| widgets::header_label(ui, t!("修改时间", "Modified", "更新日時"), true));
        })
        .body(|body| {
            body.rows(theme::ROW_HEIGHT + 8.0, list.len(), |mut row| {
                let n = list[row.index()];
                row.set_selected(selection.contains(n));
                row.col(|ui| {
                    let item = Item {
                        path: t.path(n),
                        size: t.value(n),
                        dir: false,
                        files: 1,
                    };
                    item_name(ui, &item, icons, &p);
                });
                row.col(|ui| {
                    widgets::right_label(ui, RichText::new(fmt_bytes(t.value(n))).color(p.text))
                });
                row.col(|ui| {
                    widgets::right_label(
                        ui,
                        RichText::new(fmt_time(t.node(n).mtime))
                            .size(12.0)
                            .color(p.faint),
                    )
                });
                let resp = row.response();
                if resp.clicked() {
                    actions.push(Action::Click {
                        node: n,
                        list: List::Largest,
                        ctrl: mods.command,
                        shift: mods.shift,
                    });
                }
                if resp.double_clicked() && !mods.command && !mods.shift {
                    actions.push(Action::Locate(n));
                }
                if resp.secondary_clicked()
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    actions.push(Action::OpenMenu(n, pos));
                }
            });
        });
}
