//! 删除前的确认对话框，以及操作结果的通知（右下角，几秒后自动消失）。

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Frame, Id, Key, Layout, Margin, Order, RichText,
    Sense, Shadow, Stroke, Ui, pos2, vec2,
};

use sweep_scan::{t, tf};

use crate::icons_gen::Icon;
use crate::theme::{Palette, semibold, types};
use crate::{fmt_bytes, fmt_count, n_items, widgets};

/// 白字在其上 ≥ 7:1（AAA）。
const DANGER_FILL: Color32 = Color32::from_rgb(0x99, 0x1b, 0x1b);
const DANGER_FILL_HOVER: Color32 = Color32::from_rgb(0x7f, 0x1d, 0x1d);
/// 白字在其上 ≥ 7:1（AAA）。
const PRIMARY_FILL: Color32 = Color32::from_rgb(0x9a, 0x34, 0x12);
const PRIMARY_FILL_HOVER: Color32 = Color32::from_rgb(0x7c, 0x2d, 0x12);

pub enum Decision {
    Confirm,
    Cancel,
}

/// 要删除的一项。
pub struct Target {
    /// 树里的节点；清理建议里按路径给出的项为 `NONE`。
    pub node: u32,
    pub path: String,
    pub size: u64,
    pub dir: bool,
    /// 其中的文件数。
    pub files: u64,
}

/// 待确认的删除。
pub struct DeleteRequest {
    /// 已规范化（不含被选中的文件夹里的项），不会重复计算。
    pub items: Vec<Target>,
    /// 永久删除（不进回收站）。
    pub permanent: bool,
    /// 对话框标题里的对象，例如 "node_modules"、"3 items"。
    pub label: String,
    /// 选中了、但不能删除而被排除的项数（驱动器根目录、NTFS 元数据）。
    pub excluded: usize,
    /// 发起删除时的 Doc 编号（按路径给出的请求为 0）。
    pub doc: u64,
}

impl DeleteRequest {
    pub fn total(&self) -> u64 {
        self.items.iter().map(|i| i.size).sum()
    }

    pub fn files(&self) -> u64 {
        self.items.iter().map(|i| i.files).sum()
    }
}

/// 删除确认框：列出要删的东西和合计大小。回收站与永久删除用不同的标题、按钮颜色和说明。
pub fn confirm_delete(ctx: &egui::Context, req: &DeleteRequest) -> Option<Decision> {
    let p = Palette::of(ctx);
    modal(ctx, ("delete", req.permanent), 460.0, |ui| {
        let mut decision = None;
        ui.spacing_mut().item_spacing.y = 6.0;
        let label = &req.label;
        if req.permanent {
            danger_title(
                ui,
                &tf!(
                    "永久删除 {label}？",
                    "Permanently delete {label}?",
                    "{label}を完全に削除しますか?"
                ),
            );
        } else {
            title(
                ui,
                Icon::Trash,
                p.accent,
                &tf!(
                    "将 {label} 移到回收站？",
                    "Move {label} to the Recycle Bin?",
                    "{label}をごみ箱に移動しますか?"
                ),
            );
        }
        ui.add_space(6.0);
        let n = req.items.len();
        let (items, size, files) = (
            n_items(n as u64),
            fmt_bytes(req.total()),
            fmt_count(req.files()),
        );
        let s = if req.files() == 1 { "" } else { "s" };
        ui.label(
            RichText::new(tf!(
                "共 {items}，{size}，包含 {files} 个文件：",
                "{items}, {size}, containing {files} file{s}:",
                "{items}、{size}、ファイル {files} 個:"
            ))
            .color(p.muted),
        );
        const SHOWN: usize = 8;
        Frame::NONE
            .fill(p.bg)
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                for Target {
                    path, size, dir, ..
                } in req.items.iter().take(SHOWN)
                {
                    // 一行：图标、路径（过长截断）、右侧大小，都垂直居中。
                    let (row, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::hover());
                    let glyph = if *dir { Icon::Folder } else { Icon::File };
                    widgets::icon(
                        ui,
                        glyph,
                        pos2(row.left() + 7.0, row.center().y),
                        14.0,
                        p.muted,
                    );
                    let size = ui.painter().text(
                        pos2(row.right(), row.center().y),
                        Align2::RIGHT_CENTER,
                        fmt_bytes(*size),
                        egui::FontId::proportional(12.0),
                        p.faint,
                    );
                    // 名字在前，所在文件夹淡色跟在后面；放不下时截断的是文件夹路径。
                    let trimmed = path.trim_end_matches('\\');
                    let (parent, name) = trimmed.rsplit_once('\\').unwrap_or(("", trimmed));
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        name,
                        0.0,
                        egui::TextFormat::simple(egui::FontId::proportional(12.5), p.text),
                    );
                    job.append(
                        parent,
                        8.0,
                        egui::TextFormat::simple(egui::FontId::proportional(11.5), p.faint),
                    );
                    job.wrap = egui::text::TextWrapping::truncate_at_width(
                        (size.left() - row.left() - 32.0).max(40.0),
                    );
                    let g = ui.painter().layout_job(job);
                    ui.painter().galley(
                        pos2(row.left() + 20.0, row.center().y - g.size().y / 2.0),
                        g,
                        p.text,
                    );
                }
                if n > SHOWN {
                    ui.label(
                        RichText::new({
                            let more = n - SHOWN;
                            tf!("以及另外 {more} 项", "and {more} more", "ほか {more} 個")
                        })
                        .size(12.0)
                        .color(p.faint),
                    );
                }
            });
        if req.excluded > 0 {
            let n = n_items(req.excluded as u64);
            ui.label(
                RichText::new(tf!(
                    "已跳过 {n}（不能删除：驱动器或扫描的根文件夹、NTFS 元数据）。",
                    "Skipping {n} that can't be deleted (drive roots, the scanned folder itself, NTFS metadata).",
                    "削除できない {n}はスキップします (ドライブやスキャンしたフォルダー自体、NTFS メタデータ)。"
                ))
                .size(12.5)
                .color(p.muted),
            );
        }
        ui.add_space(2.0);
        if req.permanent {
            warning(
                ui,
                t!(
                    "永久删除不经过回收站，无法恢复。",
                    "Permanently deleted items skip the Recycle Bin and can't be restored.",
                    "完全に削除した項目はごみ箱を経由せず、復元できません。"
                ),
            );
        } else {
            ui.label(
                RichText::new(t!(
                    "可以从回收站恢复。放进回收站的文件仍占用磁盘，清空回收站后才会腾出空间。",
                    "You can restore them from the Recycle Bin. They still take up disk space until you empty it.",
                    "ごみ箱から復元できます。ごみ箱を空にするまでディスク領域は解放されません。"
                ))
                .size(12.5)
                .color(p.muted),
            );
        }
        ui.add_space(12.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let (label, kind) = if req.permanent {
                (
                    t!("永久删除", "Delete permanently", "完全に削除"),
                    Kind::Danger,
                )
            } else {
                (
                    t!("移到回收站", "Move to Recycle Bin", "ごみ箱に移動"),
                    Kind::Primary,
                )
            };
            // 永久删除不响应 Enter，必须明确点按钮。
            if button(ui, label, kind, true).clicked()
                || (!req.permanent && ui.input(|i| i.key_pressed(Key::Enter)))
            {
                decision = Some(Decision::Confirm);
            }
            if button(
                ui,
                t!("取消", "Cancel", "キャンセル"),
                Kind::Secondary,
                true,
            )
            .clicked()
            {
                decision = Some(Decision::Cancel);
            }
            let hint = if req.permanent {
                t!("Esc 取消", "Esc to cancel", "Esc でキャンセル")
            } else {
                t!(
                    "Enter 确认 · Esc 取消",
                    "Enter to confirm · Esc to cancel",
                    "Enter で確定 · Esc でキャンセル"
                )
            };
            ui.label(RichText::new(hint).size(11.5).color(p.faint));
        });
        decision
    })
}

fn warning(ui: &mut Ui, text: &str) {
    let p = Palette::of(ui.ctx());
    ui.horizontal_wrapped(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
        widgets::icon(ui, Icon::AlertCircle, r.center(), 15.0, p.danger);
        ui.label(RichText::new(text).size(12.5).color(p.danger));
    });
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Primary,
    Danger,
    Secondary,
}

pub fn button(ui: &mut Ui, label: &str, kind: Kind, enabled: bool) -> egui::Response {
    let p = Palette::of(ui.ctx());
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        egui::FontId::new(13.0, semibold()),
        Color32::WHITE,
    );
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) =
        ui.allocate_exact_size(vec2((galley.size().x + 32.0).max(72.0), 34.0), sense);
    let hovered = enabled && resp.hovered();
    let (fill, fg) = match (kind, enabled, hovered) {
        (_, false, _) => (p.track, p.faint),
        (Kind::Primary, _, false) => (PRIMARY_FILL, Color32::WHITE),
        (Kind::Primary, _, true) => (PRIMARY_FILL_HOVER, Color32::WHITE),
        (Kind::Danger, _, false) => (DANGER_FILL, Color32::WHITE),
        (Kind::Danger, _, true) => (DANGER_FILL_HOVER, Color32::WHITE),
        (Kind::Secondary, _, false) => (p.track, p.text),
        (Kind::Secondary, _, true) => (p.hover, p.text),
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, fg);
    resp
}

// ---------------------------------------------------------------- 通知

#[derive(Clone, Copy, PartialEq)]
pub enum ToastKind {
    Success,
    Error,
}

pub struct Toast {
    pub kind: ToastKind,
    /// 进行中的通知的标记，操作完成时按它收走；普通通知为空。
    pub tag: &'static str,
    pub title: String,
    pub detail: Option<String>,
    pub at: Instant,
}

const TOAST_TTL: Duration = Duration::from_secs(4);
const TOAST_ERROR_TTL: Duration = Duration::from_secs(7);

impl Toast {
    pub fn ok(title: String, detail: Option<String>) -> Self {
        Self {
            kind: ToastKind::Success,
            tag: "",
            title,
            detail,
            at: Instant::now(),
        }
    }

    pub fn err(title: String, detail: Option<String>) -> Self {
        Self {
            kind: ToastKind::Error,
            tag: "",
            title,
            detail,
            at: Instant::now(),
        }
    }

    pub fn tag(self, tag: &'static str) -> Self {
        Self { tag, ..self }
    }

    fn ttl(&self) -> Duration {
        if self.kind == ToastKind::Error {
            TOAST_ERROR_TTL
        } else {
            TOAST_TTL
        }
    }
}

/// 在窗口右下角堆叠显示通知，过期的自动移除。
pub fn toasts(ctx: &egui::Context, toasts: &mut Vec<Toast>) {
    toasts.retain(|t| t.at.elapsed() < t.ttl());
    if toasts.is_empty() {
        return;
    }
    let p = Palette::of(ctx);
    let mut dismissed = None;
    let mut y = -44.0;
    for (i, t) in toasts.iter().enumerate().rev().take(3) {
        let (glyph, accent) = match t.kind {
            ToastKind::Success => (Icon::CheckCircle, types::OK),
            ToastKind::Error => (Icon::XCircle, p.danger),
        };
        let resp = egui::Area::new(Id::new(("toast", i)))
            .order(Order::Foreground)
            .anchor(Align2::RIGHT_BOTTOM, vec2(-20.0, y))
            .interactable(true)
            .show(ctx, |ui| {
                Frame::NONE
                    .fill(p.surface)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(14, 10))
                    .shadow(Shadow {
                        offset: [0, 6],
                        blur: 20,
                        spread: 0,
                        color: Color32::from_black_alpha(80),
                    })
                    .show(ui, |ui| {
                        ui.set_width(320.0);
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                            widgets::icon(ui, glyph, r.center(), 17.0, accent);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(RichText::new(&t.title).color(p.text));
                                if let Some(d) = &t.detail {
                                    ui.label(RichText::new(d).size(12.0).color(p.muted));
                                }
                            });
                        });
                    });
            })
            .response;
        if resp
            .interact(Sense::click())
            .on_hover_text(t!("点击关闭", "Click to dismiss", "クリックして閉じる"))
            .clicked()
        {
            dismissed = Some(i);
        }
        y -= resp.rect.height() + 8.0;
    }
    if let Some(i) = dismissed {
        toasts.remove(i);
    }
    // 到点后需要再画一帧把通知收走。
    if let Some(next) = toasts
        .iter()
        .map(|t| t.ttl().saturating_sub(t.at.elapsed()))
        .min()
    {
        ctx.request_repaint_after(next);
    }
}

/// 统一的模态框：外框、遮罩，点遮罩或按 Esc 视为取消。`body` 返回用户的决定。
fn modal(
    ctx: &egui::Context,
    id: impl std::hash::Hash + std::fmt::Debug,
    width: f32,
    body: impl FnOnce(&mut Ui) -> Option<Decision>,
) -> Option<Decision> {
    let p = Palette::of(ctx);
    let resp = egui::Modal::new(Id::new(id))
        .frame(dialog_frame(&p))
        .backdrop_color(Color32::from_black_alpha(110))
        .show(ctx, |ui| {
            ui.set_width(width);
            let w = &mut ui.visuals_mut().widgets;
            w.inactive.bg_fill = p.track;
            w.inactive.weak_bg_fill = p.track;
            w.inactive.bg_stroke = Stroke::new(1.0, p.border);
            w.hovered.bg_fill = p.hover;
            w.hovered.bg_stroke = Stroke::new(1.0, p.faint);
            body(ui)
        });
    let closed = resp.should_close();
    resp.inner.or(closed.then_some(Decision::Cancel))
}

fn title(ui: &mut Ui, glyph: Icon, color: Color32, text: &str) {
    let p = Palette::of(ui.ctx());
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
        widgets::icon(ui, glyph, r.center(), 20.0, color);
        ui.label(
            RichText::new(text)
                .size(16.0)
                .family(semibold())
                .color(p.text),
        );
    });
}

/// 警告图标 + 标题。
fn danger_title(ui: &mut Ui, text: &str) {
    let p = Palette::of(ui.ctx());
    title(ui, Icon::AlertTriangle, p.danger, text);
}

fn dialog_frame(p: &Palette) -> Frame {
    Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(20))
        .shadow(Shadow {
            offset: [0, 12],
            blur: 32,
            spread: 0,
            color: Color32::from_black_alpha(90),
        })
}

/// 通用的危险操作确认框（清空回收站等）。
pub fn confirm(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    body: &str,
    button_label: &str,
) -> Option<Decision> {
    let p = Palette::of(ctx);
    modal(ctx, ("confirm", id), 420.0, |ui| {
        let mut decision = None;
        danger_title(ui, title);
        ui.add_space(6.0);
        ui.label(RichText::new(body).size(13.0).color(p.muted));
        ui.add_space(14.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if button(ui, button_label, Kind::Danger, true).clicked() {
                decision = Some(Decision::Confirm);
            }
            if button(
                ui,
                t!("取消", "Cancel", "キャンセル"),
                Kind::Secondary,
                true,
            )
            .clicked()
            {
                decision = Some(Decision::Cancel);
            }
        });
        decision
    })
}
