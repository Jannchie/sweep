//! 设置页：分组卡片，每行左侧是名称与说明，右侧是控件。

use eframe::egui::{
    self, Align, Align2, CornerRadius, FontId, Frame, Layout, Margin, Rect, RichText, Sense,
    Stroke, Ui, pos2, vec2,
};

use sweep_scan::{t, tf};

use crate::settings::{Basis, EngineChoice, Language, Settings, ThemeChoice};
use crate::theme::{Palette, semibold};
use crate::update::Check;
use crate::{dialogs, update_view, widgets};

/// 设置页上需要由 App 处理的操作。
pub enum SettingsAction {
    CheckUpdate,
    ShowUpdate,
    ClearCache,
}

pub fn show(
    ui: &mut Ui,
    settings: &mut Settings,
    elevated: bool,
    cache_usage: u64,
    update: &Check,
) -> Option<SettingsAction> {
    let mut action = None;
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

                    group(ui, t!("分析", "Analysis", "分析"), |ui| {
                        row(
                            ui,
                            t!("大小口径", "Size basis", "サイズの基準"),
                            t!(
                                "排序、矩形树图与百分比使用的大小；占用空间即删除后能腾出的空间。",
                                "Used for sorting, the treemap and percentages. Size on disk is what deleting frees.",
                                "並べ替え、ツリーマップ、割合に使うサイズ。ディスク上のサイズは削除で空く容量です。"
                            ),
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
                            t!(
                                "MFT 只用于 NTFS 卷的根目录，其余情况总是遍历目录。",
                                "The MFT is used only for NTFS volume roots; everything else is walked.",
                                "MFT は NTFS ボリュームのルートだけで使い、それ以外は常に走査します。"
                            )
                        } else {
                            t!(
                                "读取 MFT 需要管理员权限，现在总是遍历目录。",
                                "Reading the MFT needs administrator rights; folders are walked for now.",
                                "MFT の読み取りには管理者権限が必要なため、現在は常に走査します。"
                            )
                        };
                        row(ui, t!("扫描方式", "Scan method", "スキャン方法"), desc, |ui| {
                            let labels = EngineChoice::ALL.map(|e| e.label());
                            let cur = EngineChoice::ALL
                                .iter()
                                .position(|&e| e == settings.engine)
                                .unwrap_or(0);
                            if let Some(i) = choice(ui, &labels, cur) {
                                settings.engine = EngineChoice::ALL[i];
                            }
                        });
                        divider(ui);
                        row(
                            ui,
                            t!("以管理员身份启动", "Start as administrator", "管理者として起動"),
                            t!(
                                "读取 MFT、增量扫描等需要管理员权限；拒绝时以普通权限运行。",
                                "Needed for MFT reads and incremental scans; declining keeps standard rights.",
                                "MFT の読み取りや差分スキャンに必要です。拒否すると通常の権限で実行します。"
                            ),
                            |ui| {
                                widgets::switch(ui, &mut settings.elevate);
                            },
                        );
                        divider(ui);
                        let size = crate::fmt_bytes(cache_usage);
                        let desc = tf!(
                            "再次扫描时只更新有变动的文件夹。缓存占用 {size}。",
                            "Rescans update only the folders that changed. Using {size}.",
                            "再スキャンでは変更のあったフォルダーだけを更新します。使用量 {size}。"
                        );
                        row(ui, t!("增量扫描", "Incremental scans", "差分スキャン"), &desc, |ui| {
                            ui.spacing_mut().item_spacing.x = 12.0;
                            widgets::switch(ui, &mut settings.incremental);
                            if cache_usage > 0
                                && dialogs::button(
                                    ui,
                                    t!("清除缓存", "Clear", "消去"),
                                    dialogs::Kind::Secondary,
                                    true,
                                )
                                .clicked()
                            {
                                action = Some(SettingsAction::ClearCache);
                            }
                        });
                    });

                    group(ui, t!("外观", "Appearance", "外観"), |ui| {
                        row(
                            ui,
                            t!("语言", "Language", "言語"),
                            t!(
                                "跟随系统时使用 Windows 的显示语言。",
                                "System uses the Windows display language.",
                                "[システム] では Windows の表示言語を使います。"
                            ),
                            |ui| {
                                let labels = Language::ALL.map(|l| l.label());
                                let cur = Language::ALL
                                    .iter()
                                    .position(|&l| l == settings.language)
                                    .unwrap_or(0);
                                if let Some(i) = choice(ui, &labels, cur) {
                                    settings.language = Language::ALL[i];
                                }
                            },
                        );
                        divider(ui);
                        row(
                            ui,
                            t!("主题", "Theme", "テーマ"),
                            t!(
                                "跟随系统会随 Windows 的深浅色设置自动切换。",
                                "System follows the Windows light or dark mode.",
                                "[システム] では Windows のライト/ダーク設定に合わせて切り替わります。"
                            ),
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

                    group(ui, t!("关于", "About", "バージョン情報"), |ui| {
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
                                    RichText::new({
                                        let v = env!("CARGO_PKG_VERSION");
                                        tf!("版本 {v}", "Version {v}", "バージョン {v}")
                                    })
                                        .size(12.0)
                                        .color(p.muted),
                                );
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 12.0;
                                let available = matches!(update, Check::Available(_));
                                let (label, kind) = if available {
                                    (t!("查看更新", "View update", "更新を表示"), dialogs::Kind::Primary)
                                } else {
                                    (t!("检查更新", "Check for updates", "更新を確認"), dialogs::Kind::Secondary)
                                };
                                let enabled = !matches!(update, Check::Checking);
                                if dialogs::button(ui, label, kind, enabled).clicked() {
                                    action = Some(if available {
                                        SettingsAction::ShowUpdate
                                    } else {
                                        SettingsAction::CheckUpdate
                                    });
                                }
                                if let Some((text, color)) = update_view::check_status(ui, update) {
                                    ui.add(
                                        egui::Label::new(RichText::new(text).size(12.0).color(color))
                                            .truncate(),
                                    );
                                }
                            });
                        });
                        ui.add_space(16.0);
                        divider(ui);
                        row(
                            ui,
                            t!("自动检查更新", "Check for updates automatically", "更新を自動的に確認する"),
                            t!(
                                "启动时和每天检查一次新版本，确认后才安装。",
                                "Checks for a new version at startup and daily; installs only after you confirm.",
                                "起動時と 1 日 1 回、新しいバージョンを確認します。インストールは確認後のみです。"
                            ),
                            |ui| {
                                widgets::switch(ui, &mut settings.auto_update);
                            },
                        );
                    });
                    ui.add_space(16.0);
                });
            });
        });
    action
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

/// 一行设置：左边名称 + 一行说明，右边控件，两者都在行内垂直居中。
/// 说明放不下时以省略号截断，悬停在这一行上显示全文。
fn row(ui: &mut Ui, title: &str, desc: &str, control: impl FnOnce(&mut Ui)) {
    let p = Palette::of(ui.ctx());
    let width = ui.available_width();
    // 给右侧控件留出位置。
    let text_w = (width - 300.0).max(200.0);
    let title = ui
        .painter()
        .layout_no_wrap(title.to_string(), FontId::proportional(13.5), p.text);
    let mut job = egui::text::LayoutJob::simple_singleline(
        desc.to_string(),
        FontId::proportional(12.0),
        p.muted,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(text_w);
    let full = desc.to_string();
    let desc = ui.painter().layout_job(job);
    let truncated = desc.elided;
    let gap = 3.0;
    let content = title.size().y + gap + desc.size().y;
    let h = (content + 26.0).max(60.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, h), Sense::hover());
    if truncated {
        resp.on_hover_text(full);
    }
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
