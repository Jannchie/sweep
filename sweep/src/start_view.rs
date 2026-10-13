//! 磁盘页：顶部是路径输入与"选择文件夹"，下面每个卷一块（容量条、可用空间），参照资源管理器的"此电脑"。

use eframe::egui::{
    self, Align, Align2, CornerRadius, FontId, Layout, Rect, RichText, Sense, Ui, pos2, vec2,
};
use sweep_scan::system::{DriveKind, Volume};
use sweep_scan::{t, tf};

use crate::icons_gen::Icon;
use crate::theme::{Palette, semibold};
use crate::{Action, fmt_bytes, widgets};

const TILE_H: f32 = 76.0;
const GAP: f32 = 10.0;

pub fn show(
    ui: &mut Ui,
    volumes: &[Volume],
    elevated: bool,
    path_input: &mut String,
    current: Option<&str>,
) -> Vec<Action> {
    let p = Palette::of(ui.ctx());
    let mut actions = Vec::new();
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let resp = widgets::search_box(
            ui,
            path_input,
            380.0,
            t!(
                r"输入路径，例如 D:\Projects 或 \\nas\share",
                r"Enter a path, e.g. D:\Projects or \\nas\share",
                r"パスを入力 (例: D:\Projects、\\nas\share)"
            ),
        );
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let ready = !path_input.trim().is_empty();
        if (widgets::tool_button(ui, Icon::Play, t!("扫描", "Scan", "スキャン"), ready, false)
            .clicked()
            || enter)
            && ready
        {
            actions.push(Action::Scan(path_input.trim().to_string()));
        }
        if widgets::tool_button(
            ui,
            Icon::FolderSearch,
            t!("选择文件夹…", "Choose folder…", "フォルダーを選択…"),
            true,
            false,
        )
        .clicked()
        {
            actions.push(Action::PickFolder);
        }
        if !elevated {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let resp = widgets::tool_button(
                    ui,
                    Icon::Shield,
                    t!("以管理员身份重启", "Restart as administrator", "管理者として再起動"),
                    true,
                    false,
                )
                .on_hover_text(t!(
                    "以管理员身份运行时直接读取 NTFS 主文件表（MFT），整盘扫描通常只要几秒；现在只能逐个目录遍历。",
                    "As administrator, Sweep reads the NTFS master file table (MFT) directly and usually scans a whole drive in seconds. Right now it can only walk folders one by one.",
                    "管理者として実行すると NTFS のマスター ファイル テーブル (MFT) を直接読み取り、ドライブ全体のスキャンが通常数秒で終わります。現在はフォルダーを順に走査するしかありません。"
                ));
                if resp.clicked() {
                    actions.push(Action::Elevate);
                }
            });
        }
    });
    ui.add_space(18.0);
    ui.label(
        RichText::new(t!(
            "设备和驱动器",
            "Devices and drives",
            "デバイスとドライブ"
        ))
        .size(12.5)
        .family(semibold())
        .color(p.muted),
    );
    ui.add_space(8.0);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if volumes.is_empty() {
                ui.label(
                    RichText::new(t!(
                        "正在读取磁盘信息…",
                        "Reading drive information…",
                        "ドライブ情報を読み取っています…"
                    ))
                    .color(p.muted),
                );
            }
            let width = ui.available_width();
            let cols = ((width + GAP) / (320.0 + GAP)).floor().clamp(1.0, 4.0) as usize;
            let tile_w = ((width - GAP * (cols as f32 - 1.0)) / cols as f32).floor();
            for row in volumes.chunks(cols) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = GAP;
                    for v in row {
                        if tile(ui, tile_w, v, elevated, current)
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            actions.push(Action::Scan(v.root.clone()));
                        }
                    }
                });
                ui.add_space(GAP);
            }
        });
    actions
}

/// 一个卷：左边设备图标，右边名称、容量条与可用空间。
fn tile(
    ui: &mut Ui,
    width: f32,
    v: &Volume,
    elevated: bool,
    current: Option<&str>,
) -> egui::Response {
    let p = Palette::of(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(width, TILE_H), Sense::click());
    let painter = ui.painter_at(rect.expand(1.0));
    if resp.hovered() {
        painter.rect_filled(rect, CornerRadius::same(8), p.hover);
    }
    // 当前已分析的卷：左侧一道强调色竖条。
    if current.is_some_and(|c| c.eq_ignore_ascii_case(&v.root)) {
        let bar = Rect::from_min_size(rect.left_top() + vec2(0.0, 22.0), vec2(3.0, TILE_H - 44.0));
        painter.rect_filled(bar, CornerRadius::same(2), p.accent);
    }
    let glyph = match (v.kind, v.ssd) {
        (DriveKind::Removable, _) => Icon::UsbDrive,
        (DriveKind::Network, _) => Icon::Cloud,
        (_, Some(true)) => Icon::Ssd,
        _ => Icon::HardDrive,
    };
    widgets::icon(
        ui,
        glyph,
        pos2(rect.left() + 30.0, rect.center().y),
        28.0,
        p.muted,
    );

    let left = rect.left() + 58.0;
    let right = rect.right() - 14.0;
    let letter = v.root.trim_end_matches('\\');
    let name = if v.label.is_empty() {
        match v.kind {
            DriveKind::Removable => t!("可移动磁盘", "Removable Disk", "リムーバブル ディスク"),
            DriveKind::Network => t!("网络驱动器", "Network Drive", "ネットワーク ドライブ"),
            _ => t!("本地磁盘", "Local Disk", "ローカル ディスク"),
        }
        .to_string()
    } else {
        v.label.clone()
    };
    painter.text(
        pos2(left, rect.top() + 20.0),
        Align2::LEFT_CENTER,
        format!("{name} ({letter})"),
        FontId::new(13.5, semibold()),
        p.text,
    );
    let frac = v.used() as f32 / v.total.max(1) as f32;
    let bar = Rect::from_min_max(
        pos2(left, rect.center().y - 3.0),
        pos2(right, rect.center().y + 3.0),
    );
    // 剩余不到 10% 时用警示色（与资源管理器一致）。
    let color = if frac > 0.9 { p.danger } else { p.accent };
    widgets::meter(&painter, bar, frac, color, p.track);
    painter.text(
        pos2(left, rect.bottom() - 18.0),
        Align2::LEFT_CENTER,
        {
            let (free, total) = (fmt_bytes(v.free), fmt_bytes(v.total));
            tf!(
                "可用 {free}，共 {total}",
                "{free} free of {total}",
                "空き {free} / {total}"
            )
        },
        FontId::proportional(12.0),
        p.muted,
    );

    let (root, fs) = (&v.root, &v.fs);
    let how = if v.fs == "NTFS" && elevated {
        t!(
            "读取 MFT，通常几秒完成",
            "reads the MFT, usually in seconds",
            "MFT を読み取り、通常は数秒で完了"
        )
    } else {
        t!(
            "逐个目录遍历",
            "walks folders one by one",
            "フォルダーを順に走査"
        )
    };
    resp.on_hover_text(tf!(
        "扫描 {root}（{fs}，{how}）",
        "Scan {root} ({fs}; {how})",
        "{root} をスキャン ({fs}、{how})"
    ))
}
