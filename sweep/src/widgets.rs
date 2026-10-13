//! 自绘小部件：图标、品牌标志、点阵进度条、搜索框、按钮、菜单、开关、表头。

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Mesh, Painter, Pos2, Rect, Response, RichText,
    Sense, Shape, Stroke, Ui, emath::GuiRounding, pos2, vec2,
};

use crate::icons_gen::{Icon, PX_MAX, PX_MIN};
use crate::theme::Palette;

/// 画一个 jannchie-icons 图标。尺寸换算成设备像素后取对应的像素对齐版本，
/// 并把左上角对齐到像素网格，否则对齐就白做了。
pub fn icon(ui: &Ui, icon: Icon, center: Pos2, size: f32, color: Color32) {
    let ppp = ui.ctx().pixels_per_point();
    let px = ((size * ppp).round() as u32).clamp(PX_MIN, PX_MAX);
    let side = px as f32 / ppp;
    let min = (center - vec2(side, side) / 2.0).round_to_pixels(ppp);
    let rect = Rect::from_min_size(min, vec2(side, side));
    let uri = format!("bytes://icon/{}@{px}.svg", icon.name());
    egui::Image::from_bytes(uri, icon.svg(px).as_bytes())
        .tint(color)
        .paint_at(ui, rect);
}

/// Sweep 图标，按物理像素渲染成贴图（每个尺寸缓存一份），点阵不糊。返回所占区域。
pub fn brand_mark(ui: &mut Ui, size: f32) -> Rect {
    let ctx = ui.ctx().clone();
    let px = (size * ctx.pixels_per_point()).round().max(16.0) as u32;
    let id = egui::Id::new(("brand-mark", px));
    let tex = match ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        Some(t) => t,
        None => {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [px as usize, px as usize],
                &crate::brand_art::brand_rgba(px),
            );
            let t = ctx.load_texture(format!("brand-{px}"), image, egui::TextureOptions::NEAREST);
            ctx.data_mut(|d| d.insert_temp(id, t.clone()));
            t
        }
    };
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    // 左上角对齐到物理像素，贴图与屏幕像素一一对应。
    let ppp = ctx.pixels_per_point();
    let min = pos2(
        (rect.left() * ppp).round() / ppp,
        (rect.top() * ppp).round() / ppp,
    );
    let draw = Rect::from_min_size(min, vec2(px as f32 / ppp, px as f32 / ppp));
    ui.painter().image(
        tex.id(),
        draw,
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    rect
}

/// 水平点阵进度条。
pub fn meter(painter: &Painter, rect: Rect, frac: f32, color: Color32, track: Color32) {
    // 与图表同一套点阵语言：一排方点，边长等于条的高度，按比例从左往右点亮，
    // 最后一个点按余数调透明度，数值变化是连续的。
    // 点的边长、间隙都取整数个物理像素，点距处处相同：每个点一样大、边缘清晰、间距均匀。
    // 用点的个数去适应宽度，多出的不到一个点距的余量留在行尾。
    let ppp = painter.pixels_per_point();
    let px = |v: f32| (v * ppp).round();
    let dot = px(rect.height()).max(1.0);
    let gap = (dot * 0.5).round().max(1.0);
    let pitch = dot + gap;
    let (left, top) = (px(rect.left()), px(rect.top()));
    let width = px(rect.right()) - left;
    let n = (((width + gap) / pitch).floor() as usize).max(2);
    let lit = frac.clamp(0.0, 1.0) * n as f32;
    let mut mesh = Mesh::default();
    for i in 0..n {
        let r = Rect::from_min_size(
            pos2((left + i as f32 * pitch) / ppp, top / ppp),
            vec2(dot / ppp, dot / ppp),
        );
        mesh.add_colored_rect(r, track);
        let on = (lit - i as f32).clamp(0.0, 1.0);
        if on > 0.0 {
            mesh.add_colored_rect(r, color.gamma_multiply(on));
        }
    }
    painter.add(Shape::mesh(mesh));
}

// ---------------------------------------------------------------- 工具栏控件

pub fn search_box(ui: &mut Ui, text: &mut String, width: f32, hint: &str) -> Response {
    let p = Palette::of(ui.ctx());
    let height = 32.0;
    // 整个框都可点击聚焦，不只是中间的输入区。
    let (rect, bg) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(8), p.surface);
    icon(
        ui,
        Icon::Search,
        pos2(rect.left() + 17.0, rect.center().y),
        16.0,
        p.muted,
    );

    let clear_w = if text.is_empty() { 8.0 } else { 30.0 };
    let edit_rect = Rect::from_min_max(
        pos2(rect.left() + 32.0, rect.top()),
        pos2(rect.right() - clear_w, rect.bottom()),
    );
    // place 不移动布局光标；put 会把光标拉回 edit_rect 末端，导致后面的控件压到搜索框上。
    let mut resp = ui.place(
        edit_rect,
        egui::TextEdit::singleline(text)
            .hint_text(RichText::new(hint).color(p.faint))
            .frame(egui::Frame::NONE)
            .vertical_align(egui::Align::Center)
            .desired_width(edit_rect.width()),
    );
    if bg.clicked() {
        resp.request_focus();
    }
    if !text.is_empty() {
        let r =
            Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(22.0, 22.0));
        let x = ui.interact(r, resp.id.with("clear"), Sense::click());
        if x.hovered() {
            ui.painter().rect_filled(r, CornerRadius::same(6), p.hover);
        }
        icon(
            ui,
            Icon::X,
            r.center(),
            14.0,
            if x.hovered() { p.text } else { p.muted },
        );
        if x.clicked() {
            text.clear();
            resp.mark_changed();
        }
    }
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(8),
            Stroke::new(1.0, p.accent.gamma_multiply(0.7)),
            egui::StrokeKind::Inside,
        );
    }
    resp
}

/// 按钮宽度（与 [`tool_button`]、[`card_button`] 一致）。
fn pill_width(ui: &Ui, label: &str) -> f32 {
    ui.painter()
        .layout_no_wrap(
            label.to_string(),
            FontId::proportional(12.5),
            Color32::WHITE,
        )
        .size()
        .x
        + 44.0
}

/// 图标 + 文字的按钮底板。
fn pill(ui: &mut Ui, label: &str, enabled: bool) -> (Response, Rect) {
    let size = vec2(pill_width(ui, label), 32.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    (resp, rect)
}

fn pill_content(ui: &Ui, rect: Rect, glyph: Icon, label: &str, color: Color32) {
    icon(
        ui,
        glyph,
        pos2(rect.left() + 19.0, rect.center().y),
        15.0,
        color,
    );
    ui.painter().text(
        pos2(rect.left() + 32.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.5),
        color,
    );
}

/// 工具栏按钮；`danger` 用于删除这类破坏性操作。
pub fn tool_button(ui: &mut Ui, glyph: Icon, label: &str, enabled: bool, danger: bool) -> Response {
    let p = Palette::of(ui.ctx());
    button_on(ui, glyph, label, enabled, danger, p.surface)
}

fn button_on(
    ui: &mut Ui,
    glyph: Icon,
    label: &str,
    enabled: bool,
    danger: bool,
    base: Color32,
) -> Response {
    let p = Palette::of(ui.ctx());
    let (resp, rect) = pill(ui, label, enabled);
    let fill = if enabled && resp.hovered() {
        p.hover
    } else {
        base
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);
    let color = match (enabled, danger) {
        (false, _) => p.faint,
        (true, true) => p.danger,
        (true, false) => p.muted,
    };
    pill_content(ui, rect, glyph, label, color);
    resp
}

/// 右键菜单项：图标 + 文字，整行可点。`danger` 用于删除这类破坏性操作。
pub fn menu_item(ui: &mut Ui, glyph: Icon, label: &str, danger: bool) -> Response {
    menu_entry(ui, Some(glyph), label, danger)
}

/// 没有图标的菜单项（子菜单里的选项）。
pub fn menu_plain(ui: &mut Ui, label: &str) -> Response {
    menu_entry(ui, None, label, false)
}

/// 灰掉、不能点的菜单项（多选时只对单项有意义的操作）。`why` 作为悬停提示。
pub fn menu_item_disabled(ui: &mut Ui, glyph: Icon, label: &str, why: &str) -> Response {
    let p = Palette::of(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
    icon(
        ui,
        glyph,
        pos2(rect.left() + 14.0, rect.center().y),
        15.0,
        p.faint,
    );
    ui.painter().text(
        pos2(rect.left() + 30.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        p.faint,
    );
    resp.on_hover_text(why)
}

fn menu_entry(ui: &mut Ui, glyph: Option<Icon>, label: &str, danger: bool) -> Response {
    let (resp, bg) = menu_row(ui, glyph, label, danger);
    if resp.hovered() {
        let p = Palette::of(ui.ctx());
        ui.painter().set(
            bg,
            Shape::rect_filled(resp.rect, CornerRadius::same(6), p.hover),
        );
    }
    if resp.clicked() {
        ui.close();
    }
    resp
}

/// 菜单行的公共部分：（可选的）图标 + 文字，背景留一个占位，由调用方按悬停 / 展开状态填充。
/// 宽度跟随菜单（菜单本身定宽）。
fn menu_row(
    ui: &mut Ui,
    glyph: Option<Icon>,
    label: &str,
    danger: bool,
) -> (Response, egui::layers::ShapeIdx) {
    let p = Palette::of(ui.ctx());
    let bg = ui.painter().add(Shape::Noop);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
    if let Some(glyph) = glyph {
        icon(
            ui,
            glyph,
            pos2(rect.left() + 14.0, rect.center().y),
            15.0,
            if danger { p.danger } else { p.muted },
        );
    }
    let text_x = if glyph.is_some() { 30.0 } else { 12.0 };
    ui.painter().text(
        pos2(rect.left() + text_x, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        if danger { p.danger } else { p.text },
    );
    (resp, bg)
}

// ---------------------------------------------------------------- 表格

/// 小状态标签（"需要管理员"、"已暂停"）。
pub fn status_chip(ui: &mut Ui, text: &str, color: Color32) {
    let p = Palette::of(ui.ctx());
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), FontId::proportional(11.0), color);
    let (rect, _) = ui.allocate_exact_size(vec2(galley.size().x + 12.0, 18.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(5), p.track);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, color);
}

/// 展开/折叠箭头。
pub fn chevron(ui: &Ui, center: Pos2, expanded: bool, color: Color32) {
    let glyph = if expanded {
        Icon::ChevronDown
    } else {
        Icon::ChevronRight
    };
    icon(ui, glyph, center, 14.0, color);
}

// ---------------------------------------------------------------- 表格与菜单的共用部件

/// 表头单元格：底部细线 + 左/右对齐的标签。返回标签的响应（可用于点击排序）。
pub fn table_header(
    ui: &mut Ui,
    label: RichText,
    right_aligned: bool,
    trailing: impl FnOnce(&mut Ui),
) -> Response {
    let p = Palette::of(ui.ctx());
    let r = ui.max_rect();
    ui.painter().hline(
        r.x_range().expand(6.0),
        r.bottom() - 0.5,
        Stroke::new(1.0, p.border),
    );
    let layout = if right_aligned {
        egui::Layout::right_to_left(egui::Align::Center)
    } else {
        egui::Layout::left_to_right(egui::Align::Center)
    };
    let mut trailing = Some(trailing);
    ui.with_layout(layout, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        if !right_aligned {
            ui.add_space(4.0);
        }
        // 附加内容（排序箭头）总在标签外侧：右对齐列里先放，左对齐列里后放。
        if right_aligned && let Some(f) = trailing.take() {
            f(ui);
        }
        let resp = ui.add(egui::Label::new(label).sense(Sense::click()));
        if let Some(f) = trailing.take() {
            f(ui);
        }
        resp
    })
    .inner
}

/// 普通表头：12px 次要色标签。
pub fn header_label(ui: &mut Ui, label: &str, right_aligned: bool) {
    let p = Palette::of(ui.ctx());
    table_header(
        ui,
        RichText::new(label).size(12.0).color(p.muted),
        right_aligned,
        |_| {},
    );
}

/// 与 `painter.text` 相同，但只画一行，超过 `max_width` 时以省略号截断。返回所占区域。
pub fn text_truncated(
    painter: &Painter,
    pos: Pos2,
    align: Align2,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> Rect {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width.max(0.0));
    let galley = painter.layout_job(job);
    let rect = align.anchor_size(pos, galley.size());
    painter.galley(rect.min, galley, color);
    rect
}

/// 右对齐的单元格文字。
pub fn right_label(ui: &mut Ui, text: RichText) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(text)
    });
}

/// 右键菜单顶部的标题行。`width` 大于 0 时同时定下菜单宽度（菜单项按可用宽度铺满，
/// egui 自带的 context_menu 不定宽会被撑开）；浮动菜单自己定宽，传 0。
pub fn menu_heading(ui: &mut Ui, text: &str, width: f32) {
    let p = Palette::of(ui.ctx());
    if width > 0.0 {
        ui.set_width(width);
    }
    ui.spacing_mut().item_spacing.y = 2.0;
    ui.add_space(2.0);
    // 单行显示，过长时以省略号截断；悬停可看完整名称。
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(p.muted)).truncate());
    ui.add_space(2.0);
}

/// 固定在 `pos` 的右键菜单。与 `Response::context_menu` 不同，它不挂在某一行上，
/// 列表重排时不会跟着跳，内容也由调用方按记下的对象生成。
/// 底层是 egui 的菜单弹窗（`PopupKind::Menu`），所以里面可以直接用 [`submenu`] 做多级菜单；
/// 点击菜单项、点击菜单外或按 Esc 时由 egui 关闭。`add` 返回是否执行了某个操作。返回菜单是否继续显示。
pub fn floating_menu(
    ctx: &egui::Context,
    id: &str,
    pos: Pos2,
    width: f32,
    add: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    let mut open = true;
    let mut acted = false;
    // id 带上位置：在别处再次右键时是一个新菜单，不会被"点击了菜单外"立即关掉。
    egui::Popup::new(
        egui::Id::new((id, pos.x.to_bits(), pos.y.to_bits())),
        ctx.clone(),
        egui::PopupAnchor::Position(pos),
        egui::LayerId::background(),
    )
    .kind(egui::PopupKind::Menu)
    .layout(egui::Layout::top_down_justified(egui::Align::Min))
    .style(egui::containers::menu::menu_style)
    .width(width)
    .open_bool(&mut open)
    .show(|ui| {
        // 菜单项按可用宽度铺满，这里必须定死宽度，否则弹窗会被撑到很宽。
        ui.set_width(width);
        ui.spacing_mut().item_spacing.y = 2.0;
        acted = add(ui);
    });
    open && !acted
}

/// 二级菜单：入口与普通菜单项同样式，右侧有箭头，悬停时展开（egui 原生 `SubMenu`）。
pub fn submenu(
    ui: &mut Ui,
    glyph: Icon,
    label: &str,
    min_width: f32,
    add_contents: impl FnOnce(&mut Ui),
) {
    let p = Palette::of(ui.ctx());
    let (resp, bg) = menu_row(ui, Some(glyph), label, false);
    icon(
        ui,
        Icon::ChevronRight,
        pos2(resp.rect.right() - 12.0, resp.rect.center().y),
        14.0,
        p.muted,
    );
    let open = egui::containers::menu::SubMenu::new()
        .show(ui, &resp, |ui| {
            ui.set_width(min_width);
            ui.spacing_mut().item_spacing.y = 2.0;
            add_contents(ui);
        })
        .is_some();
    if open || resp.hovered() {
        ui.painter().set(
            bg,
            Shape::rect_filled(resp.rect, CornerRadius::same(6), p.hover),
        );
    }
}

/// 开关。
pub fn switch(ui: &mut Ui, on: &mut bool) -> Response {
    let p = Palette::of(ui.ctx());
    let (rect, mut resp) = ui.allocate_exact_size(vec2(36.0, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let track = if *on { p.accent } else { p.track };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(10), track);
    if !*on {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(10),
            Stroke::new(1.0, p.border),
            egui::StrokeKind::Inside,
        );
    }
    let x = if *on {
        rect.right() - 10.0
    } else {
        rect.left() + 10.0
    };
    let knob = if *on { Color32::WHITE } else { p.muted };
    ui.painter()
        .circle_filled(pos2(x, rect.center().y), 6.5, knob);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use crate::icons_gen::Icon;

    #[test]
    /// 生成的 SVG 必须渲染成白色描边（运行时靠 tint 着色）。曾因 `stroke="undefined"` 渲染成黑块。
    fn svg_rasterizes_white_strokes() {
        let svg = Icon::Search.svg(16);
        let img = egui_extras::image::load_svg_bytes_with_size(
            svg.as_bytes(),
            eframe::egui::SizeHint::Size {
                width: 16,
                height: 16,
                maintain_aspect_ratio: false,
            },
            &Default::default(),
        )
        .expect("svg");
        let opaque: Vec<_> = img.pixels.iter().filter(|c| c.a() > 0).collect();
        assert!(!opaque.is_empty());
        assert!(
            opaque.iter().all(|c| c.r() == c.a()),
            "strokes should be white (premultiplied)"
        );
    }
}
