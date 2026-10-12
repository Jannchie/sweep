//! 删除前的确认对话框，以及操作结果的通知（右下角，几秒后自动消失）。

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Frame, Id, Key, Layout, Margin, Order, RichText,
    Sense, Shadow, Stroke, Ui, pos2, vec2,
};

use crate::icons_gen::Icon;
use crate::theme::{Palette, semibold, types};
use crate::{fmt_bytes, fmt_count, widgets};

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

/// 待确认的删除。
pub struct DeleteRequest {
    /// (完整路径, 大小, 是否文件夹, 其中的文件数)
    pub items: Vec<(String, u64, bool, u64)>,
    /// 永久删除（不进回收站）。
    pub permanent: bool,
    /// 对话框标题里的对象，例如 "node_modules"、"3 项"。
    pub label: String,
}

impl DeleteRequest {
    pub fn total(&self) -> u64 {
        self.items.iter().map(|i| i.1).sum()
    }

    pub fn files(&self) -> u64 {
        self.items.iter().map(|i| i.3).sum()
    }
}

/// 删除确认框：列出要删的东西和合计大小。回收站与永久删除用不同的标题、按钮颜色和说明。
pub fn confirm_delete(ctx: &egui::Context, req: &DeleteRequest) -> Option<Decision> {
    let p = Palette::of(ctx);
    modal(ctx, ("delete", req.permanent), 460.0, |ui| {
        let mut decision = None;
        ui.spacing_mut().item_spacing.y = 6.0;
        if req.permanent {
            danger_title(ui, &format!("永久删除 {}？", req.label));
        } else {
            title(
                ui,
                Icon::Trash,
                p.accent,
                &format!("将 {} 移到回收站？", req.label),
            );
        }
        ui.add_space(6.0);
        let n = req.items.len();
        ui.label(
            RichText::new(format!(
                "共 {} 项，{}，包含 {} 个文件：",
                n,
                fmt_bytes(req.total()),
                fmt_count(req.files())
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
                for (path, size, dir, _) in req.items.iter().take(SHOWN) {
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
                    let mut job = egui::text::LayoutJob::simple_singleline(
                        path.clone(),
                        egui::FontId::proportional(12.5),
                        p.text,
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
                        RichText::new(format!("以及另外 {} 项", n - SHOWN))
                            .size(12.0)
                            .color(p.faint),
                    );
                }
            });
        ui.add_space(2.0);
        if req.permanent {
            warning(ui, "永久删除不经过回收站，无法恢复。");
        } else {
            ui.label(
                RichText::new(
                    "可以从回收站恢复。放进回收站的文件仍占用磁盘，清空回收站后才会腾出空间。",
                )
                .size(12.5)
                .color(p.muted),
            );
        }
        ui.add_space(12.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let (label, kind) = if req.permanent {
                ("永久删除", Kind::Danger)
            } else {
                ("移到回收站", Kind::Primary)
            };
            // 永久删除不响应 Enter，必须明确点按钮。
            if button(ui, label, kind, true).clicked()
                || (!req.permanent && ui.input(|i| i.key_pressed(Key::Enter)))
            {
                decision = Some(Decision::Confirm);
            }
            if button(ui, "取消", Kind::Secondary, true).clicked() {
                decision = Some(Decision::Cancel);
            }
            let hint = if req.permanent {
                "Esc 取消"
            } else {
                "Enter 确认 · Esc 取消"
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
            title,
            detail,
            at: Instant::now(),
        }
    }

    pub fn err(title: String, detail: Option<String>) -> Self {
        Self {
            kind: ToastKind::Error,
            title,
            detail,
            at: Instant::now(),
        }
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
            .on_hover_text("点击关闭")
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
            if button(ui, "取消", Kind::Secondary, true).clicked() {
                decision = Some(Decision::Cancel);
            }
        });
        decision
    })
}
