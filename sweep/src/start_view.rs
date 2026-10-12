//! 磁盘页：每个卷一张卡片（已用 / 可用的点阵进度条、文件系统、扫描方式），以及扫描文件夹的入口。

use eframe::egui::{
    self, Align, Align2, CornerRadius, FontId, Frame, Layout, Margin, Rect, RichText, Sense,
    Stroke, Ui, pos2, vec2,
};
use sweep_scan::system::{DriveKind, Volume};

use crate::icons_gen::Icon;
use crate::theme::{Palette, semibold};
use crate::{Action, fmt_bytes, widgets};

const CARD_H: f32 = 136.0;
const GAP: f32 = 14.0;

pub fn show(
    ui: &mut Ui,
    volumes: &[Volume],
    elevated: bool,
    path_input: &mut String,
    current: Option<&str>,
) -> Vec<Action> {
    let p = Palette::of(ui.ctx());
    let mut actions = Vec::new();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(8.0);
            ui.label(
                RichText::new("选择要分析的磁盘")
                    .size(22.0)
                    .family(semibold())
                    .color(p.text),
            );
            ui.label(
                RichText::new("点一下磁盘开始扫描，也可以只分析某个文件夹。")
                    .size(13.0)
                    .color(p.muted),
            );
            ui.add_space(14.0);
            if !elevated {
                elevate_banner(ui, &mut actions);
                ui.add_space(14.0);
            }

            let width = ui.available_width();
            let cols = ((width + GAP) / (300.0 + GAP)).floor().max(1.0) as usize;
            let card_w = ((width - GAP * (cols as f32 - 1.0)) / cols as f32).floor();
            let mut cards: Vec<Option<&Volume>> = volumes.iter().map(Some).collect();
            cards.push(None); // 最后一张是"扫描文件夹"
            for row in cards.chunks(cols) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = GAP;
                    for c in row {
                        let resp = match c {
                            Some(v) => volume_card(ui, card_w, v, elevated, current),
                            None => folder_card(ui, card_w),
                        };
                        if resp
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            actions.push(match c {
                                Some(v) => Action::Scan(v.root.clone()),
                                None => Action::PickFolder,
                            });
                        }
                    }
                });
                ui.add_space(GAP);
            }
            if volumes.is_empty() {
                ui.label(RichText::new("正在读取磁盘信息…").color(p.muted));
            }

            ui.add_space(6.0);
            ui.label(
                RichText::new("或者输入路径")
                    .size(13.0)
                    .family(semibold())
                    .color(p.muted),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let resp =
                    widgets::search_box(ui, path_input, 420.0, r"例如 D:\Projects 或 \\nas\share");
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let ready = !path_input.trim().is_empty();
                if (widgets::tool_button(ui, Icon::Play, "扫描", ready, false).clicked() || enter)
                    && ready
                {
                    actions.push(Action::Scan(path_input.trim().to_string()));
                }
            });
            ui.add_space(16.0);
        });
    actions
}

fn elevate_banner(ui: &mut Ui, actions: &mut Vec<Action>) {
    let p = Palette::of(ui.ctx());
    Frame::NONE
        .fill(p.accent.gamma_multiply(p.chip_alpha))
        .stroke(Stroke::new(1.0, p.accent.gamma_multiply(0.35)))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                widgets::icon(ui, Icon::Shield, r.center(), 18.0, p.accent);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new("以管理员身份运行可以快很多")
                            .size(13.5)
                            .family(semibold())
                            .color(p.text),
                    );
                    ui.label(
                        RichText::new(
                            "管理员身份下直接读取 NTFS 主文件表（MFT），整盘扫描通常只要几秒；现在只能逐个目录遍历。",
                        )
                        .size(12.5)
                        .color(p.chip),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::tool_button(ui, Icon::Shield, "以管理员身份重启", true, false).clicked()
                    {
                        actions.push(Action::Elevate);
                    }
                });
            });
        });
}

fn volume_card(
    ui: &mut Ui,
    width: f32,
    v: &Volume,
    elevated: bool,
    current: Option<&str>,
) -> egui::Response {
    let p = Palette::of(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(width, CARD_H), Sense::click());
    let painter = ui.painter_at(rect.expand(1.0));
    painter.rect_filled(
        rect,
        CornerRadius::same(12),
        if resp.hovered() { p.hover } else { p.surface },
    );
    let letter = v.root.trim_end_matches('\\');
    if current.is_some_and(|c| c.eq_ignore_ascii_case(&v.root)) {
        // 当前已分析的卷：左侧一道强调色竖条。
        let bar = Rect::from_min_size(rect.left_top() + vec2(0.0, 18.0), vec2(3.0, CARD_H - 36.0));
        painter.rect_filled(bar, CornerRadius::same(2), p.accent);
    }
    let inner = rect.shrink(16.0);
    let glyph = match v.kind {
        DriveKind::Removable => Icon::UsbDrive,
        DriveKind::Network => Icon::Cloud,
        _ => Icon::HardDrive,
    };
    widgets::icon(
        ui,
        glyph,
        pos2(inner.left() + 8.0, inner.top() + 9.0),
        17.0,
        p.accent,
    );
    let name = if v.label.is_empty() {
        match v.kind {
            DriveKind::Removable => "可移动磁盘",
            DriveKind::Network => "网络驱动器",
            _ => "本地磁盘",
        }
        .to_string()
    } else {
        v.label.clone()
    };
    painter.text(
        pos2(inner.left() + 24.0, inner.top() + 9.0),
        Align2::LEFT_CENTER,
        format!("{name} ({letter})"),
        FontId::new(14.0, semibold()),
        p.text,
    );
    // 右上角：文件系统标签。
    let fs = painter.layout_no_wrap(v.fs.clone(), FontId::proportional(11.0), p.chip);
    let chip = Rect::from_min_size(
        pos2(inner.right() - fs.size().x - 12.0, inner.top()),
        vec2(fs.size().x + 12.0, 18.0),
    );
    painter.rect_filled(
        chip,
        CornerRadius::same(5),
        p.accent.gamma_multiply(p.chip_alpha),
    );
    painter.galley(chip.center() - fs.size() / 2.0, fs, p.chip);

    let used = v.used();
    let frac = used as f32 / v.total.max(1) as f32;
    let big = painter.text(
        pos2(inner.left(), inner.top() + 30.0),
        Align2::LEFT_TOP,
        fmt_bytes(used),
        FontId::new(24.0, semibold()),
        p.text,
    );
    painter.text(
        pos2(big.right() + 6.0, big.bottom() - 4.0),
        Align2::LEFT_BOTTOM,
        format!("/ {}", fmt_bytes(v.total)),
        FontId::proportional(13.0),
        p.faint,
    );
    let bar = Rect::from_min_max(
        pos2(inner.left(), inner.bottom() - 28.0),
        pos2(inner.right(), inner.bottom() - 20.0),
    );
    // 剩余不到 10% 时用警示色。
    let color = if frac > 0.9 {
        crate::theme::types::PALETTE[3]
    } else {
        p.accent
    };
    widgets::meter(&painter, bar, frac, color, p.track);
    painter.text(
        inner.left_bottom(),
        Align2::LEFT_BOTTOM,
        format!("可用 {} · 已用 {:.0}%", fmt_bytes(v.free), frac * 100.0),
        FontId::proportional(11.5),
        p.faint,
    );
    let mft = v.fs == "NTFS" && elevated;
    painter.text(
        inner.right_bottom(),
        Align2::RIGHT_BOTTOM,
        if mft {
            "读取 MFT · 快速"
        } else {
            "遍历目录"
        },
        FontId::proportional(11.5),
        if mft { p.chip } else { p.faint },
    );
    resp.on_hover_text(format!("扫描 {}", v.root))
}

fn folder_card(ui: &mut Ui, width: f32) -> egui::Response {
    let p = Palette::of(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(width, CARD_H), Sense::click());
    let painter = ui.painter_at(rect.expand(1.0));
    painter.rect_filled(
        rect,
        CornerRadius::same(12),
        if resp.hovered() { p.hover } else { p.bg },
    );
    painter.rect_stroke(
        rect.shrink(0.5),
        CornerRadius::same(12),
        Stroke::new(1.0, p.border),
        egui::StrokeKind::Inside,
    );
    let c = rect.center();
    widgets::icon(ui, Icon::FolderSearch, c - vec2(0.0, 18.0), 26.0, p.accent);
    painter.text(
        c + vec2(0.0, 14.0),
        Align2::CENTER_CENTER,
        "扫描文件夹…",
        FontId::new(14.0, semibold()),
        p.text,
    );
    painter.text(
        c + vec2(0.0, 34.0),
        Align2::CENTER_CENTER,
        "只分析选中的文件夹",
        FontId::proportional(12.0),
        p.faint,
    );
    resp
}
