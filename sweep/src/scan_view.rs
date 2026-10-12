//! 扫描进度页：KPI 卡片、点阵进度条、当前路径；遍历目录时还实时列出根下各文件夹扫到的大小。

use std::sync::atomic::Ordering;

use eframe::egui::{
    self, Align, Align2, CornerRadius, FontId, Layout, Rect, RichText, Sense, Ui, pos2, vec2,
};
use sweep_scan::system::Volume;
use sweep_scan::{Scan, t, tf};

use crate::icons_gen::Icon;
use crate::theme::{Palette, semibold};
use crate::{Action, fmt_bytes, fmt_count, fmt_duration, widgets};

pub fn show(ui: &mut Ui, scan: &Scan, volumes: &[Volume]) -> Vec<Action> {
    let p = Palette::of(ui.ctx());
    let mut actions = Vec::new();
    let pr = &scan.progress;
    let files = pr.files.load(Ordering::Relaxed);
    let dirs = pr.dirs.load(Ordering::Relaxed);
    let bytes = pr.bytes.load(Ordering::Relaxed);
    let (done, total) = (
        pr.done.load(Ordering::Relaxed),
        pr.total.load(Ordering::Relaxed),
    );
    // 进度：MFT 按已读字节；遍历整个卷时按已扫大小 / 卷已用空间估算；扫文件夹时没有总量。
    let volume = volumes
        .iter()
        .find(|v| v.root.eq_ignore_ascii_case(&scan.target));
    let incremental = pr.incremental.load(Ordering::Relaxed);
    let (frac, mft) = if incremental {
        ((total > 0).then(|| done as f32 / total as f32), false)
    } else if total > 0 {
        (Some(done as f32 / total as f32), true)
    } else {
        (
            volume.map(|v| (bytes as f32 / v.used().max(1) as f32).min(0.99)),
            false,
        )
    };

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new({
                let target = &scan.target;
                tf!(
                    "正在扫描 {target}",
                    "Scanning {target}",
                    "{target} をスキャン中"
                )
            })
            .size(22.0)
            .family(semibold())
            .color(p.text),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::tool_button(
                ui,
                Icon::Stop,
                t!("取消", "Cancel", "キャンセル"),
                true,
                false,
            )
            .clicked()
            {
                actions.push(Action::Cancel);
            }
        });
    });
    ui.label(
        RichText::new(if incremental {
            t!(
                "正在载入上次的结果，只更新之后有变动的文件夹",
                "Loading the previous result and updating only the folders that changed since",
                "前回の結果を読み込み、その後に変更のあったフォルダーだけを更新しています"
            )
        } else if mft {
            t!(
                "正在读取 NTFS 主文件表（MFT）",
                "Reading the NTFS master file table (MFT)",
                "NTFS マスター ファイル テーブル (MFT) を読み取っています"
            )
        } else {
            t!(
                "正在逐个目录遍历",
                "Walking folders one by one",
                "フォルダーを順に走査しています"
            )
        })
        .size(13.0)
        .color(p.muted),
    );
    ui.add_space(14.0);

    let gap = 12.0;
    let w = ((ui.available_width() - gap * 3.0) / 4.0).floor();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        widgets::kpi(
            ui,
            w,
            84.0,
            Icon::Files,
            t!("文件", "Files", "ファイル"),
            &fmt_count(files),
            t!("已发现", "found", "検出済み"),
            p.accent,
        );
        widgets::kpi(
            ui,
            w,
            84.0,
            Icon::Folder,
            t!("文件夹", "Folders", "フォルダー"),
            &fmt_count(dirs),
            t!("已发现", "found", "検出済み"),
            p.accent,
        );
        widgets::kpi(
            ui,
            w,
            84.0,
            Icon::HardDrive,
            t!("大小", "Size", "サイズ"),
            &fmt_bytes(bytes),
            t!("文件大小合计", "total file size", "ファイル サイズの合計"),
            p.accent,
        );
        widgets::kpi(
            ui,
            w,
            84.0,
            Icon::Clock,
            t!("用时", "Elapsed", "経過時間"),
            &fmt_duration(scan.started.elapsed()),
            &match frac {
                Some(f) => {
                    let pct = f * 100.0;
                    tf!("约 {pct:.0}%", "about {pct:.0}%", "約 {pct:.0}%")
                }
                None => t!("进行中", "in progress", "実行中").into(),
            },
            p.accent,
        );
    });
    ui.add_space(18.0);

    // 点阵进度条；扫文件夹时没有总量，不显示。
    if let Some(f) = frac {
        let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 10.0), Sense::hover());
        widgets::meter(ui.painter(), bar, f, p.accent, p.track);
        ui.add_space(10.0);
    }
    let current = pr.current();
    ui.add(egui::Label::new(RichText::new(current).size(12.0).color(p.faint)).truncate());
    ui.add_space(14.0);

    // 遍历目录时：根下各文件夹已扫到的大小。
    let mut top = pr.top.lock().unwrap().clone();
    if !top.is_empty() {
        top.sort_by_key(|t| std::cmp::Reverse(t.1));
        let max = top.first().map_or(1, |t| t.1).max(1);
        ui.label(
            RichText::new(t!("根目录下", "Top-level folders", "ルート直下"))
                .size(13.0)
                .family(semibold())
                .color(p.muted),
        );
        ui.add_space(6.0);
        let rows = ((ui.available_height() - 10.0) / 26.0).floor().max(0.0) as usize;
        for (name, size) in top.iter().take(rows.min(14)) {
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
            let name = if name.is_empty() {
                t!(
                    "（根目录里的文件）",
                    "(files in the root)",
                    "(ルートにあるファイル)"
                )
            } else {
                name
            };
            ui.painter().text(
                pos2(r.left(), r.center().y),
                Align2::LEFT_CENTER,
                name,
                FontId::proportional(13.0),
                p.text,
            );
            let meter = Rect::from_min_max(
                pos2(r.left() + r.width() * 0.45, r.center().y - 3.0),
                pos2(r.right() - 110.0, r.center().y + 3.0),
            );
            widgets::meter(
                ui.painter(),
                meter,
                *size as f32 / max as f32,
                p.accent,
                p.track,
            );
            ui.painter().text(
                pos2(r.right(), r.center().y),
                Align2::RIGHT_CENTER,
                fmt_bytes(*size),
                FontId::proportional(12.5),
                p.muted,
            );
        }
    }
    actions
}

/// 还没有扫描结果时的占位页。
pub fn empty(ui: &mut Ui, error: Option<&str>) -> Vec<Action> {
    let p = Palette::of(ui.ctx());
    let mut actions = Vec::new();
    let rect = ui.available_rect_before_wrap();
    let c = rect.center() - vec2(0.0, 40.0);
    widgets::icon(ui, Icon::Treemap, c - vec2(0.0, 30.0), 36.0, p.faint);
    ui.painter().text(
        c + vec2(0.0, 6.0),
        Align2::CENTER_CENTER,
        if error.is_some() {
            t!("扫描失败", "Scan failed", "スキャンに失敗しました")
        } else {
            t!(
                "还没有扫描结果",
                "No scan results yet",
                "スキャン結果はまだありません"
            )
        },
        FontId::new(16.0, semibold()),
        p.text,
    );
    ui.painter().text(
        c + vec2(0.0, 30.0),
        Align2::CENTER_CENTER,
        error.unwrap_or(t!(
            "在“磁盘”页选择一个磁盘或文件夹开始分析。",
            "Pick a drive or folder on the Drives page to start.",
            "[ドライブ] ページでドライブかフォルダーを選んで分析を始めます。"
        )),
        FontId::proportional(13.0),
        if error.is_some() { p.danger } else { p.muted },
    );
    let label = ui.painter().layout_no_wrap(
        t!("扫描文件夹…", "Scan a folder…", "フォルダーをスキャン…").into(),
        FontId::proportional(12.5),
        p.text,
    );
    // 宽度随文字：图标 22 + 文字 + 右侧留白 22。
    let b = Rect::from_center_size(
        c + vec2(0.0, 70.0),
        vec2((label.size().x + 58.0).max(140.0), 32.0),
    );
    let resp = ui.interact(b, egui::Id::new("empty-pick"), Sense::click());
    ui.painter().rect_filled(
        b,
        CornerRadius::same(8),
        if resp.hovered() { p.hover } else { p.surface },
    );
    widgets::icon(
        ui,
        Icon::FolderSearch,
        pos2(b.left() + 22.0, b.center().y),
        15.0,
        p.muted,
    );
    let y = b.center().y - label.size().y / 2.0;
    ui.painter().galley(pos2(b.left() + 36.0, y), label, p.text);
    if resp.clicked() {
        actions.push(Action::PickFolder);
    }
    actions
}
