//! 设置页：分组卡片，每行左侧是名称与说明，右侧是控件。

use eframe::egui::{
    self, Align, Align2, CornerRadius, FontId, Frame, Layout, Margin, Rect, RichText, Sense,
    Stroke, Ui, pos2, vec2,
};

use crate::settings::{Basis, EngineChoice, Settings, ThemeChoice};
use crate::theme::{Palette, semibold};
use crate::widgets;

pub fn show(ui: &mut Ui, settings: &mut Settings, elevated: bool) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // 内容限制在 720pt 宽并居中，宽屏上不至于左右拉得太散。
            let w = ui.available_width().min(720.0);
            let pad = ((ui.available_width() - w) / 2.0).max(0.0);
            ui.horizontal(|ui| {
                ui.add_space(pad);
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("设置")
                            .size(22.0)
                            .family(semibold())
                            .color(Palette::of(ui.ctx()).text),
                    );
                    ui.add_space(4.0);

                    group(ui, "分析", |ui| {
                        row(
                            ui,
                            "大小口径",
                            "排序、矩形树图面积与百分比按哪种大小计算。占用空间就是删除后能腾出的空间。",
                            |ui| {
                                let labels = Basis::ALL.map(|b| b.label());
                                let cur = Basis::ALL.iter().position(|&b| b == settings.basis).unwrap_or(0);
                                if let Some(i) = choice(ui, &labels, cur) {
                                    settings.basis = Basis::ALL[i];
                                }
                            },
                        );
                        divider(ui);
                        let desc = if elevated {
                            "读取 MFT 只适用于 NTFS 卷的根目录；文件夹、其他文件系统总是遍历目录。"
                        } else {
                            "读取 MFT 需要以管理员身份运行，现在总是遍历目录。"
                        };
                        row(ui, "扫描方式", desc, |ui| {
                            let labels = EngineChoice::ALL.map(|e| e.label());
                            let cur = EngineChoice::ALL
                                .iter()
                                .position(|&e| e == settings.engine)
                                .unwrap_or(0);
                            if let Some(i) = choice(ui, &labels, cur) {
                                settings.engine = EngineChoice::ALL[i];
                            }
                        });
                    });

                    group(ui, "外观", |ui| {
                        row(
                            ui,
                            "主题",
                            "跟随系统会随 Windows 的深浅色设置自动切换。",
                            |ui| {
                                let labels = ThemeChoice::ALL.map(|t| t.label());
                                let cur = ThemeChoice::ALL
                                    .iter()
                                    .position(|&t| t == settings.theme)
                                    .unwrap_or(0);
                                if let Some(i) = choice(ui, &labels, cur) {
                                    settings.theme = ThemeChoice::ALL[i];
                                }
                            },
                        );
                    });

                    group(ui, "关于", |ui| {
                        ui.add_space(16.0);
                        ui.horizontal(|ui| {
                            widgets::brand_mark(ui, 56.0);
                            ui.add_space(10.0);
                            ui.vertical(|ui| {
                                let p = Palette::of(ui.ctx());
                                ui.spacing_mut().item_spacing.y = 4.0;
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new("Sweep")
                                        .size(15.0)
                                        .family(semibold())
                                        .color(p.text),
                                );
                                ui.label(
                                    RichText::new(format!("版本 {}", env!("CARGO_PKG_VERSION")))
                                        .size(12.0)
                                        .color(p.muted),
                                );
                            });
                        });
                        ui.add_space(16.0);
                    });
                    ui.add_space(16.0);
                });
            });
        });
}

fn group(ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui)) {
    let p = Palette::of(ui.ctx());
    ui.add_space(18.0);
    ui.label(
        RichText::new(title)
            .size(13.0)
            .family(semibold())
            .color(p.muted),
    );
    ui.add_space(8.0);
    Frame::NONE
        .fill(p.surface)
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(18, 0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            body(ui);
        });
}

/// 一行设置：左边名称 + 说明，右边控件，两者都在行内垂直居中。说明太长时换行，行高随之增加。
fn row(ui: &mut Ui, title: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    let p = Palette::of(ui.ctx());
    let width = ui.available_width();
    // 给右侧控件留出位置后，说明按剩余宽度换行。
    let text_w = (width - 300.0).max(200.0);
    let title = ui
        .painter()
        .layout_no_wrap(title.to_string(), FontId::proportional(13.5), p.text);
    let desc = ui.painter().layout(
        desc.to_string(),
        FontId::proportional(12.0),
        p.muted,
        text_w,
    );
    let gap = 3.0;
    let content = title.size().y + gap + desc.size().y;
    let h = (content + 28.0).max(64.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
    let top = rect.center().y - content / 2.0;
    let desc_top = top + title.size().y + gap;
    ui.painter().galley(pos2(rect.left(), top), title, p.text);
    ui.painter()
        .galley(pos2(rect.left(), desc_top), desc, p.muted);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    control(&mut child);
}

fn divider(ui: &mut Ui) {
    let p = Palette::of(ui.ctx());
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter()
        .hline(r.x_range(), r.center().y, Stroke::new(1.0, p.track));
}

/// 文字分段选择器，返回新选中的下标。
fn choice(ui: &mut Ui, labels: &[&str], selected: usize) -> Option<usize> {
    let p = Palette::of(ui.ctx());
    let font = FontId::proportional(12.5);
    let widths: Vec<f32> = labels
        .iter()
        .map(|l| {
            ui.painter()
                .layout_no_wrap(l.to_string(), font.clone(), p.text)
                .size()
                .x
                + 24.0
        })
        .collect();
    let height = 30.0;
    let (rect, _) = ui.allocate_exact_size(
        vec2(widths.iter().sum::<f32>() + 6.0, height),
        Sense::hover(),
    );
    ui.painter()
        .rect_filled(rect, CornerRadius::same(8), p.track);
    let mut x = rect.left() + 3.0;
    let mut picked = None;
    for (i, (label, w)) in labels.iter().zip(&widths).enumerate() {
        let r = Rect::from_min_size(pos2(x, rect.top() + 3.0), vec2(*w, height - 6.0));
        x += w;
        // 按选项文字区分 id：同一页上有多个选择器，父 UI 的 id 可能相同。
        let resp = ui.interact(r, egui::Id::new(("choice", labels, i)), Sense::click());
        let on = i == selected;
        if on {
            ui.painter().rect_filled(r, CornerRadius::same(6), p.bg);
        } else if resp.hovered() {
            ui.painter().rect_filled(r, CornerRadius::same(6), p.hover);
        }
        ui.painter().text(
            r.center(),
            Align2::CENTER_CENTER,
            *label,
            font.clone(),
            if on || resp.hovered() {
                p.text
            } else {
                p.muted
            },
        );
        if resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
            && !on
        {
            picked = Some(i);
        }
    }
    picked
}
