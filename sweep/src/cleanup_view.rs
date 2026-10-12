//! 清理页：左边是清理建议卡片，右边是整个扫描范围里最大的文件。

use eframe::egui::{
    self, Align, Color32, CornerRadius, Frame, Layout, Margin, Rect, RichText, Sense, Ui,
    UiBuilder, pos2, vec2,
};
use egui_extras::{Column, TableBuilder};

use crate::cleanup::{Action as Clean, Suggestion};
use crate::doc::Doc;
use crate::file_icons::FileIcons;
use crate::icons_gen::Icon;
use crate::theme::{self, Palette, semibold};
use crate::{Action, fmt_bytes, fmt_count, fmt_time, widgets};

pub fn show(ui: &mut Ui, doc: &mut Doc, icons: &mut FileIcons, deleting: bool) -> Vec<Action> {
    let p = Palette::of(ui.ctx());
    let mut actions = Vec::new();
    ui.add_space(8.0);
    ui.label(
        RichText::new("清理")
            .size(22.0)
            .family(semibold())
            .color(p.text),
    );
    ui.label(
        RichText::new("常见的可回收空间与最大的文件。所有删除都会先列出内容让你确认。")
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
        section_title(ui, "清理建议");
        egui::ScrollArea::vertical()
            .id_salt("suggestions")
            .auto_shrink([false, false])
            .show(ui, |ui| match &doc.suggestions {
                None => {
                    ui.label(RichText::new("正在检查常见位置…").color(p.muted));
                }
                Some(list) => {
                    if doc.suggestions_loading() {
                        ui.label(RichText::new("正在更新…").size(12.0).color(p.faint));
                        ui.add_space(4.0);
                    }
                    if list.is_empty() {
                        ui.label(RichText::new("没有找到可以清理的常见位置。").color(p.muted));
                    }
                    for s in list {
                        card(ui, s, deleting, &mut actions);
                        ui.add_space(10.0);
                    }
                }
            });
    });
    ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
        section_title(ui, "最大的文件");
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

fn card(ui: &mut Ui, s: &Suggestion, deleting: bool, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    Frame::NONE
        .fill(p.surface)
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            // 标题行：图标 + 标题，右侧大小，垂直居中。
            let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
            widgets::icon(
                ui,
                s.icon,
                pos2(row.left() + 9.0, row.center().y),
                17.0,
                p.accent,
            );
            ui.painter().text(
                pos2(row.left() + 26.0, row.center().y),
                egui::Align2::LEFT_CENTER,
                &s.title,
                egui::FontId::new(14.0, semibold()),
                p.text,
            );
            ui.painter().text(
                pos2(row.right(), row.center().y),
                egui::Align2::RIGHT_CENTER,
                fmt_bytes(s.size),
                egui::FontId::new(18.0, semibold()),
                p.text,
            );
            ui.add(egui::Label::new(RichText::new(&s.detail).size(12.5).color(p.muted)).wrap());
            let count = match s.action {
                Clean::EmptyRecycleBin => format!("{} 项", fmt_count(s.files)),
                _ => format!("{} 个文件", fmt_count(s.files)),
            };
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(count).size(12.0).color(p.faint));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    match &s.action {
                        Clean::Contents | Clean::Items => {
                            let label = match s.action {
                                Clean::Contents => format!("{} 中的 {} 项", s.title, s.items.len()),
                                _ => s.title.clone(),
                            };
                            let ok = !deleting && !s.items.is_empty();
                            if widgets::card_button(ui, Icon::TrashX, "永久删除…", ok, true)
                                .clicked()
                            {
                                actions.push(Action::Delete {
                                    items: s.items.clone(),
                                    label: label.clone(),
                                    permanent: true,
                                });
                            }
                            if widgets::card_button(ui, Icon::Trash, "移到回收站", ok, false)
                                .clicked()
                            {
                                actions.push(Action::Delete {
                                    items: s.items.clone(),
                                    label,
                                    permanent: false,
                                });
                            }
                        }
                        Clean::EmptyRecycleBin => {
                            if widgets::card_button(
                                ui,
                                Icon::TrashX,
                                "清空回收站…",
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
                                && widgets::card_button(
                                    ui,
                                    Icon::ExternalLink,
                                    "打开磁盘清理",
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
                        && widgets::card_button(ui, Icon::Target, "定位", true, false)
                            .on_hover_text(f.as_str())
                            .clicked()
                    {
                        actions.push(Action::LocatePath(f.clone()));
                    }
                });
            });
        });
}

fn largest(ui: &mut Ui, doc: &mut Doc, icons: &mut FileIcons, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    let Some(d) = &doc.derived else {
        ui.label(RichText::new("正在统计…").color(p.muted));
        return;
    };
    let t = doc.tree.clone();
    let list: Vec<u32> = d
        .largest
        .iter()
        .copied()
        .filter(|&n| !t.node(n).has(sweep_scan::flags::DELETED))
        .collect();
    let selected = doc.selected;
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
            h.col(|ui| widgets::header_label(ui, "文件", false));
            h.col(|ui| widgets::header_label(ui, "大小", true));
            h.col(|ui| widgets::header_label(ui, "修改时间", true));
        })
        .body(|body| {
            body.rows(theme::ROW_HEIGHT + 8.0, list.len(), |mut row| {
                let n = list[row.index()];
                row.set_selected(selected == Some(n));
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
                if resp.clicked() || resp.secondary_clicked() {
                    actions.push(Action::Select(n));
                }
                if resp.double_clicked() {
                    actions.push(Action::Locate(n));
                }
                if resp.secondary_clicked()
                    && let Some(pos) = resp.interact_pointer_pos()
                {
                    actions.push(Action::OpenMenu(n, pos));
                }
                resp.on_hover_text("双击在目录树中定位，右键查看更多操作");
            });
        });
}
