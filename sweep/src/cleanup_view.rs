//! 清理页：左边是清理建议卡片，右边是整个扫描范围里最大的文件。

use eframe::egui::{
    self, Align, Color32, CornerRadius, Frame, Layout, Margin, Rect, RichText, Sense, Ui,
    UiBuilder, pos2, vec2,
};
use egui_extras::{Column, TableBuilder};
use sweep_scan::{t, tf};

use crate::cleanup::{Action as Clean, Suggestion};
use crate::doc::Doc;
use crate::file_icons::FileIcons;
use crate::icons_gen::Icon;
use crate::theme::{self, Palette, semibold};
use crate::{Action, List, fmt_bytes, fmt_count, fmt_time, widgets};

pub fn show(ui: &mut Ui, doc: &mut Doc, icons: &mut FileIcons, deleting: bool) -> Vec<Action> {
    let p = Palette::of(ui.ctx());
    let mut actions = Vec::new();
    ui.add_space(8.0);
    ui.label(
        RichText::new(t!("清理", "Clean up", "クリーンアップ"))
            .size(22.0)
            .family(semibold())
            .color(p.text),
    );
    ui.label(
        RichText::new(t!(
            "常见的可回收空间与最大的文件。所有删除都会先列出内容让你确认。",
            "Common space you can reclaim, and the largest files. Every deletion lists what it removes and asks first.",
            "よく空き領域を確保できる場所と、最も大きいファイルです。削除する前に必ず内容を表示して確認します。"
        ))
            .size(13.0)
            .color(p.muted),
    );
    ui.add_space(12.0);

    let avail = ui.available_rect_before_wrap();
    let gap = 20.0;
    let left_w = ((avail.width() - gap) * 0.52).floor();
    let left = Rect::from_min_size(avail.min, vec2(left_w, avail.height()));
    let right = Rect::from_min_max(pos2(left.right() + gap, avail.top()), avail.max);

    ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
        section_title(ui, t!("清理建议", "Suggestions", "おすすめ"));
        egui::ScrollArea::vertical()
            .id_salt("suggestions")
            .auto_shrink([false, false])
            .show(ui, |ui| match &doc.suggestions {
                None => {
                    ui.label(
                        RichText::new(t!(
                            "正在检查常见位置…",
                            "Checking common locations…",
                            "よく使われる場所を確認しています…"
                        ))
                        .color(p.muted),
                    );
                }
                Some(list) => {
                    if doc.suggestions_loading() {
                        ui.label(
                            RichText::new(t!("正在更新…", "Updating…", "更新しています…"))
                                .size(12.0)
                                .color(p.faint),
                        );
                        ui.add_space(4.0);
                    }
                    if list.is_empty() {
                        ui.label(
                            RichText::new(t!(
                                "没有找到可以清理的常见位置。",
                                "Nothing to clean up in the usual places.",
                                "クリーンアップできる場所は見つかりませんでした。"
                            ))
                            .color(p.muted),
                        );
                    }
                    if !list.is_empty() {
                        Frame::NONE
                            .fill(p.surface)
                            .corner_radius(CornerRadius::same(12))
                            .inner_margin(Margin::symmetric(6, 4))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.spacing_mut().item_spacing.y = 0.0;
                                for (i, s) in list.iter().enumerate() {
                                    if i > 0 {
                                        let (r, _) = ui.allocate_exact_size(
                                            vec2(ui.available_width(), 1.0),
                                            Sense::hover(),
                                        );
                                        ui.painter().hline(
                                            (r.left() + 10.0)..=(r.right() - 10.0),
                                            r.center().y,
                                            egui::Stroke::new(1.0, p.track),
                                        );
                                    }
                                    suggestion_row(ui, i, s, deleting, &mut actions);
                                }
                            });
                    }
                }
            });
    });
    ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
        section_title(ui, t!("最大的文件", "Largest files", "最も大きいファイル"));
        largest(ui, doc, icons, &mut actions);
    });
    ui.allocate_rect(avail, Sense::hover());
    actions
}

fn section_title(ui: &mut Ui, text: &str) {
    let p = Palette::of(ui.ctx());
    ui.label(
        RichText::new(text)
            .size(13.0)
            .family(semibold())
            .color(p.muted),
    );
    ui.add_space(8.0);
}

/// 一条清理建议：图标、名称与一行说明（截断，悬停看全文），右侧大小与文件数，最右是图标按钮。
fn suggestion_row(
    ui: &mut Ui,
    index: usize,
    s: &Suggestion,
    deleting: bool,
    actions: &mut Vec<Action>,
) {
    const ROW_H: f32 = 54.0;
    const BTN: f32 = 28.0;
    const SIZE_W: f32 = 92.0;
    let p = Palette::of(ui.ctx());
    let bg = ui.painter().add(egui::Shape::Noop);
    let (row, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    if resp.hovered() {
        ui.painter().set(
            bg,
            egui::Shape::rect_filled(row, CornerRadius::same(8), p.hover.gamma_multiply(0.6)),
        );
    }

    let del_l = t!("永久删除…", "Delete permanently…", "完全に削除…");
    let recycle_l = t!("移到回收站", "Move to Recycle Bin", "ごみ箱に移動");
    let empty_l = t!("清空回收站…", "Empty Recycle Bin…", "ごみ箱を空にする…");
    let disk_l = t!(
        "打开磁盘清理",
        "Open Disk Cleanup",
        "ディスク クリーンアップを開く"
    );
    let locate_l = t!("定位", "Locate", "場所を表示");

    // 右侧：大小（上）与文件数（下）。
    let cy = row.center().y;
    let size_right = row.right() - 8.0;
    ui.painter().text(
        pos2(size_right, cy - 8.0),
        egui::Align2::RIGHT_CENTER,
        fmt_bytes(s.size),
        egui::FontId::new(14.5, semibold()),
        p.text,
    );
    let count = match s.action {
        Clean::EmptyRecycleBin => crate::n_items(s.files),
        _ => {
            let (n, plural) = (fmt_count(s.files), if s.files == 1 { "" } else { "s" });
            tf!("{n} 个文件", "{n} file{plural}", "ファイル {n} 個")
        }
    };
    ui.painter().text(
        pos2(size_right, cy + 10.0),
        egui::Align2::RIGHT_CENTER,
        count,
        egui::FontId::proportional(11.5),
        p.faint,
    );

    // 图标按钮，从右往左排：删除类在最右，定位在最左。
    let mut x = row.right() - 8.0 - SIZE_W;
    let mut next_button =
        |ui: &mut Ui, k: usize, glyph: Icon, tip: &str, enabled: bool, danger: bool| {
            x -= BTN;
            let r = Rect::from_center_size(pos2(x + BTN / 2.0, cy), vec2(BTN, BTN));
            x -= 4.0;
            let id = ui.id().with(("clean", index, k));
            widgets::icon_button(ui, r, id, glyph, tip, enabled, danger).clicked()
        };
    match &s.action {
        Clean::Contents | Clean::Items => {
            let label = match s.action {
                Clean::Contents => {
                    let (title, items) = (s.title(), crate::n_items(s.items.len() as u64));
                    tf!(
                        "{title} 中的 {items}",
                        "{items} in {title}",
                        "{title} 内の {items}"
                    )
                }
                _ => s.title(),
            };
            let ok = !deleting && !s.items.is_empty();
            if next_button(ui, 0, Icon::TrashX, del_l, ok, true) {
                actions.push(Action::Delete {
                    items: s.items.clone(),
                    label: label.clone(),
                    permanent: true,
                });
            }
            if next_button(ui, 1, Icon::Trash, recycle_l, ok, false) {
                actions.push(Action::Delete {
                    items: s.items.clone(),
                    label,
                    permanent: false,
                });
            }
        }
        Clean::EmptyRecycleBin => {
            if next_button(ui, 0, Icon::TrashX, empty_l, !deleting, true) {
                actions.push(Action::EmptyRecycleBin(s.size));
            }
        }
        Clean::Info(cmd) => {
            if let Some(cmd) = cmd
                && next_button(ui, 0, Icon::ExternalLink, disk_l, true, false)
            {
                actions.push(Action::Run(cmd.to_string()));
            }
        }
    }
    if let Some(f) = s.folders.first()
        && next_button(
            ui,
            2,
            Icon::Target,
            &format!("{locate_l}\n{f}"),
            true,
            false,
        )
    {
        actions.push(Action::LocatePath(f.clone()));
    }

    // 左侧：图标、名称与一行说明，宽度到按钮为止。
    widgets::icon(
        ui,
        s.kind.icon(),
        pos2(row.left() + 18.0, cy),
        17.0,
        p.accent,
    );
    let text_left = row.left() + 38.0;
    let text_right = (x - 8.0).max(text_left + 40.0);
    let title_rect = Rect::from_min_max(pos2(text_left, cy - 19.0), pos2(text_right, cy - 1.0));
    let detail_rect = Rect::from_min_max(pos2(text_left, cy + 1.0), pos2(text_right, cy + 19.0));
    // 在左对齐的子区域里画文字（`ui.put` 会把控件居中）。
    for (rect, text) in [
        (
            title_rect,
            RichText::new(s.title())
                .size(13.5)
                .family(semibold())
                .color(p.text),
        ),
        (
            detail_rect,
            RichText::new(s.detail()).size(12.0).color(p.muted),
        ),
    ] {
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::left_to_right(Align::Center)),
            |ui| ui.add(egui::Label::new(text).truncate()),
        );
    }
}

fn largest(ui: &mut Ui, doc: &mut Doc, icons: &mut FileIcons, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    if doc.derived.is_none() {
        ui.label(RichText::new(t!("正在统计…", "Counting…", "集計しています…")).color(p.muted));
        return;
    }
    let t = doc.tree.clone();
    let list = doc.largest();
    let selection = &doc.selection;
    let mods = ui.input(|i| i.modifiers);
    ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
    TableBuilder::new(ui)
        .id_salt("largest")
        .striped(false)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::remainder().at_least(200.0).clip(true))
        .column(Column::exact(80.0))
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
                    let rect = ui.max_rect();
                    let icon = Rect::from_min_size(
                        pos2(rect.left() + 2.0, rect.center().y - 8.0),
                        vec2(16.0, 16.0),
                    );
                    match icons.get(t.ext_name(n)) {
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
                    let text = Rect::from_min_max(
                        pos2(rect.left() + 26.0, rect.top()),
                        rect.right_bottom(),
                    );
                    ui.scope_builder(
                        UiBuilder::new()
                            .max_rect(text)
                            .layout(Layout::top_down(Align::Min)),
                        |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.add_space(3.0);
                            ui.add(
                                egui::Label::new(RichText::new(t.name(n)).color(p.text)).truncate(),
                            );
                            let parent = t.node(n).parent;
                            ui.add(
                                egui::Label::new(
                                    RichText::new(t.path(parent)).size(11.5).color(p.faint),
                                )
                                .truncate(),
                            );
                        },
                    );
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
                resp.on_hover_text(t!(
                    "双击在目录树中定位，右键查看更多操作。Ctrl/Shift+单击多选，Delete 删除。",
                    "Double-click to locate in the tree. Right-click for more. Ctrl/Shift+click to select several; Delete to delete.",
                    "ダブルクリックでツリー内の場所を表示、右クリックでその他の操作。Ctrl/Shift+クリックで複数選択、Delete で削除。"
                ));
            });
        });
}
