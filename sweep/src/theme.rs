//! 配色、字体与全局样式。跟随系统深浅色。

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Theme,
    Visuals,
};

/// 文字色均满足 WCAG AAA（≥ 7:1），覆盖它们会出现的全部背景：窗口、卡片、悬停行、
/// 选中行（`selection` 叠在 `bg` 上）以及标签底色。改色后请重新核对对比度。
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub surface: Color32,
    pub hover: Color32,
    /// 进度条、仪表盘的轨道色。
    pub track: Color32,
    pub border: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    /// 标签文字（accent 色调的浅底标签上）。
    pub chip: Color32,
    pub danger: Color32,
    /// 选中行底色的 accent 透明度。
    pub selection_alpha: f32,
    /// 标签底色的 accent 透明度。
    pub chip_alpha: f32,
}

/// 配色对齐 Windows 11（资源管理器、任务管理器）：暗色背景 #202020、卡片 #2B2B2B，
/// 浅色背景 #F3F3F3、卡片 #FBFBFB，都是不带色偏的中性灰；次要文字对比度不低于 7:1。
const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x20, 0x20, 0x20),
    surface: Color32::from_rgb(0x2b, 0x2b, 0x2b),
    hover: Color32::from_rgb(0x33, 0x33, 0x33),
    track: Color32::from_rgb(0x3a, 0x3a, 0x3a),
    border: Color32::from_rgb(0x38, 0x38, 0x38),
    text: Color32::from_rgb(0xff, 0xff, 0xff),  // ≥ 12.6:1
    muted: Color32::from_rgb(0xd6, 0xd6, 0xd6), // ≥ 8.6:1
    faint: Color32::from_rgb(0xc2, 0xc2, 0xc2), // ≥ 7.0:1
    accent: Color32::from_rgb(0xff, 0x8a, 0x3d),
    chip: Color32::from_rgb(0xff, 0xcb, 0xa6),   // ≥ 10:1
    danger: Color32::from_rgb(0xff, 0xb4, 0xb4), // ≥ 7.4:1（只出现在按钮上）
    selection_alpha: 0.14,
    chip_alpha: 0.12,
};

const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xf3, 0xf3, 0xf3),
    surface: Color32::from_rgb(0xfb, 0xfb, 0xfb),
    hover: Color32::from_rgb(0xe9, 0xe9, 0xe9),
    track: Color32::from_rgb(0xe3, 0xe3, 0xe3),
    border: Color32::from_rgb(0xe5, 0xe5, 0xe5),
    text: Color32::from_rgb(0x1b, 0x1b, 0x1b),  // ≥ 14.1:1
    muted: Color32::from_rgb(0x3d, 0x3d, 0x3d), // ≥ 8.9:1
    faint: Color32::from_rgb(0x4a, 0x4a, 0x4a), // ≥ 7.3:1
    accent: Color32::from_rgb(0xea, 0x58, 0x0c),
    chip: Color32::from_rgb(0x7c, 0x2d, 0x12),   // ≥ 8.4:1
    danger: Color32::from_rgb(0x8a, 0x18, 0x18), // ≥ 7.7:1（只出现在按钮上）
    selection_alpha: 0.09,
    chip_alpha: 0.09,
};

/// 矩形树图与类型面板共用的扩展名配色：按占用大小排前 12 的扩展名各取一色，其余用中性灰。
/// 深浅主题共用，色块上不放文字。
pub mod types {
    use eframe::egui::Color32;
    pub const PALETTE: [Color32; 12] = [
        Color32::from_rgb(0x4f, 0x8c, 0xff),
        Color32::from_rgb(0xf5, 0x9e, 0x0b),
        Color32::from_rgb(0x10, 0xb9, 0x81),
        Color32::from_rgb(0xef, 0x44, 0x44),
        Color32::from_rgb(0xa4, 0x6b, 0xff),
        Color32::from_rgb(0x06, 0xb6, 0xd4),
        Color32::from_rgb(0xf4, 0x72, 0xb6),
        Color32::from_rgb(0x84, 0xcc, 0x16),
        Color32::from_rgb(0xfb, 0x92, 0x3c),
        Color32::from_rgb(0x63, 0x66, 0xf1),
        Color32::from_rgb(0x2d, 0xd4, 0xbf),
        Color32::from_rgb(0xea, 0xb3, 0x08),
    ];
    pub const OTHER: Color32 = Color32::from_rgb(0x71, 0x76, 0x86);
    /// 成功提示的图标色。
    pub const OK: Color32 = Color32::from_rgb(0x10, 0xb9, 0x81);
}

impl Palette {
    pub fn of(ctx: &egui::Context) -> Self {
        match ctx.theme() {
            Theme::Dark => DARK,
            Theme::Light => LIGHT,
        }
    }
}

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

/// 列表的行高。滚轮一格滚 3 行，与 Windows 默认一致。
pub const ROW_HEIGHT: f32 = 28.0;

pub fn install(ctx: &egui::Context) {
    ctx.options_mut(|o| o.input_options.line_scroll_speed = ROW_HEIGHT * 3.0);
    ctx.set_visuals_of(Theme::Dark, visuals(&DARK, true));
    ctx.set_visuals_of(Theme::Light, visuals(&LIGHT, false));
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Heading, FontId::new(20.0, semibold())),
            (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(13.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        s.spacing.menu_margin = Margin::same(6);
        s.interaction.selectable_labels = false;
        s.interaction.tooltip_delay = 0.4;
        // 默认要等指针"静止"才弹 tooltip，期间每帧 request_repaint，鼠标在表格上一动就按刷新率连续重绘。
        // 改为纯延时触发，重绘只跟随真实输入事件。
        s.interaction.show_tooltips_only_when_still = false;
        // 关掉动画（平滑滚动、悬停淡入等）。动画期间 egui 每帧都请求重绘，而我们关了 vsync，
        // 没有动画就保证了重绘只由真实输入触发。
        s.animation_time = 0.0;
        s.scroll_animation = egui::style::ScrollAnimation::none();
        // 滚动条：平时 4px 细条，悬停变 8px。滑块用正文色按透明度绘制（深浅主题下与背景都 ≥ 3:1），
        // 默认的滑块取控件底色，和窗口底色几乎一样，看不见。
        // 预留 floating_allocated_width，滚动条不压住最右一列；animation_time 为 0，悬停变宽不会连续重绘。
        s.spacing.scroll = egui::style::ScrollStyle {
            floating: true,
            bar_width: 8.0,
            floating_width: 4.0,
            floating_allocated_width: 10.0,
            handle_min_length: 28.0,
            bar_inner_margin: 2.0,
            bar_outer_margin: 2.0,
            foreground_color: true,
            dormant_background_opacity: 0.0,
            active_background_opacity: 0.0,
            interact_background_opacity: 0.6,
            dormant_handle_opacity: 0.5,
            active_handle_opacity: 0.65,
            interact_handle_opacity: 0.8,
            ..egui::style::ScrollStyle::floating()
        };
    });
}

fn visuals(p: &Palette, dark: bool) -> Visuals {
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.surface;
    v.faint_bg_color = p.surface;
    v.text_edit_bg_color = Some(p.surface);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(10);
    let shadow_alpha = if dark { 120 } else { 28 };
    v.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 20,
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha),
    };
    v.window_shadow = v.popup_shadow;
    v.weak_text_color = Some(p.muted);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent.gamma_multiply(p.selection_alpha);
    // 表格选中行用这个颜色画文字，保持正文色即可。
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.striped = false;

    let w = &mut v.widgets;
    for (wv, fill) in [
        (&mut w.noninteractive, p.bg),
        (&mut w.inactive, p.surface),
        (&mut w.hovered, p.hover),
        (&mut w.active, p.hover),
        (&mut w.open, p.hover),
    ] {
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        wv.bg_stroke = Stroke::NONE;
        wv.corner_radius = CornerRadius::same(8);
        wv.fg_stroke = Stroke::new(1.0, p.text);
        wv.expansion = 0.0;
    }
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v
}
