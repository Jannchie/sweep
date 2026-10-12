//! 矩形树图：布局和光栅化都在后台线程完成，UI 线程只贴一张与屏幕像素一一对应的纹理，
//! 悬停、选中框和文件夹名字作为覆盖层另画。
//!
//! - 布局用 squarified 算法（Bruls 等），子节点已按大小降序排好，直接按顺序分行；
//! - 面积小于几个像素的子项合并成一块"其余小文件"，用点阵纹理表示，节点再多布局量也只和像素数有关；
//! - 所有块的边都吸附到整物理像素，相邻块共用同一条边：每块只在右、下让出一个缝宽，
//!   整张图按"宽 + 缝"布局，所以四周和块与块之间的缝处处一样宽；
//! - 平面色块加小圆角（太小的块不圆角），文件夹画成比外层略亮（浅色主题略暗）的分组底，
//!   四周留出内边距，够大的文件夹顶上留一条标题栏写名字；
//! - 只有根、数据、尺寸、主题、缩放或高亮的扩展名变化时才重新计算，平时每帧只画一个贴图四边形。

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions};
use sweep_scan::Tree;

use crate::theme::types;

/// 以下尺寸都按"缝宽单位"（`Key::unit`，约等于 1 个逻辑像素对应的物理像素数）缩放。
/// 小于这个面积（平方单位）的子项并入"其余"。
const MIN_AREA: f32 = 12.0;
/// 小于这个面积的文件夹不再展开，整块画成"其余"。
const MIN_DIR_AREA: f32 = 100.0;
/// 文件夹两边都不小于这个长度时画成分组（有底色和内边距）。
const GROUP_MIN: f32 = 16.0;
/// 分组的内边距。
const GROUP_PAD: f32 = 2.0;
/// 文件夹不小于这个宽、高时顶上留标题栏写名字。
const HEADER_MIN_W: f32 = 96.0;
const HEADER_MIN_H: f32 = 64.0;
/// 标题栏高度。
pub const HEADER_H: f32 = 18.0;
/// 色块圆角半径（太小的块按边长缩小）。
pub const RADIUS: f32 = 3.0;

/// 决定一次布局结果的全部输入；相同就不必重算。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    /// 数据版本（树被修改、口径切换时递增）。
    pub version: u64,
    pub root: u32,
    /// 物理像素尺寸。
    pub size: [usize; 2],
    pub dark: bool,
    /// 只突出显示这个扩展名（类型面板里点选）。
    pub highlight: Option<u16>,
    /// 缝宽单位（物理像素），即 `pixels_per_point` 取整、至少为 1。
    pub unit: u8,
}

pub struct Request {
    pub key: Key,
    pub tree: Arc<Tree>,
    /// 扩展名下标 → 调色板下标；`u8::MAX` 表示其他（灰色）。
    pub colors: Arc<Vec<u8>>,
    pub bg: Color32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TileKind {
    File,
    Dir,
    /// `node` 目录里合并显示的小文件。
    Rest,
}

#[derive(Clone, Copy, Debug)]
pub struct Tile {
    /// 画出来的区域，物理像素整数坐标：(x0, y0, x1, y1)，右、下边不含。
    pub rect: [f32; 4],
    pub node: u32,
    pub kind: TileKind,
    /// 文件夹：分组的层级（1 起），决定底色深浅，0 表示不画分组底。
    /// "其余"：它所在的分组层级（点阵画在这一层的底色上）。
    pub level: u8,
}

impl Tile {
    /// 这块的圆角半径（物理像素）。
    pub fn radius(&self, unit: u8) -> f32 {
        let side = (self.rect[2] - self.rect[0]).min(self.rect[3] - self.rect[1]);
        (RADIUS * unit as f32).min((side / 4.0).floor()).max(0.0)
    }
}

impl Tile {
    fn contains(&self, x: f32, y: f32) -> bool {
        let r = self.rect;
        x >= r[0] && x < r[2] && y >= r[1] && y < r[3]
    }
}

pub struct Layout {
    pub key: Key,
    /// 先文件夹后其内容，越靠后越深。
    pub tiles: Vec<Tile>,
    /// 有标题栏的文件夹：(块下标, 标题栏区域，物理像素)。
    pub headers: Vec<(usize, [f32; 4])>,
    image: Option<ColorImage>,
}

impl Layout {
    /// 物理像素坐标处最深的一块。
    pub fn hit(&self, x: f32, y: f32) -> Option<&Tile> {
        self.tiles.iter().rev().find(|t| t.contains(x, y))
    }

    #[cfg(test)]
    pub fn rect_of(&self, node: u32) -> Option<[f32; 4]> {
        self.tiles.iter().find(|t| t.node == node).map(|t| t.rect)
    }
}

pub struct Treemap {
    tx: Sender<Request>,
    rx: Receiver<Layout>,
    pub current: Option<Layout>,
    pub texture: Option<TextureHandle>,
    requested: Option<Key>,
    /// 选中项对应的块（下标），按 (布局, 选中项版本) 缓存，平时不必每帧扫一遍所有块。
    pub selected_tiles: Option<((Key, u64), Vec<usize>)>,
}

impl Treemap {
    pub fn new(ctx: &egui::Context) -> Self {
        let (tx, req_rx) = channel::<Request>();
        let (res_tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("treemap".into())
            .spawn(move || {
                while let Ok(mut req) = req_rx.recv() {
                    // 拖动窗口大小时请求会连续到来，只做最新的那个。
                    while let Ok(newer) = req_rx.try_recv() {
                        req = newer;
                    }
                    let layout = compute(&req);
                    drop(req); // 尽快放开对树的引用，UI 线程才能原地修改它。
                    if res_tx.send(layout).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn treemap");
        Self {
            tx,
            rx,
            current: None,
            texture: None,
            requested: None,
            selected_tiles: None,
        }
    }

    /// 需要时发出新的布局请求（与上次请求相同则什么都不做）。
    pub fn request(&mut self, key: Key, make: impl FnOnce() -> Request) {
        if self.requested != Some(key) {
            self.requested = Some(key);
            let _ = self.tx.send(make());
        }
    }

    /// 收取后台结果并上传纹理。每帧调用一次。
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(mut layout) = self.rx.try_recv() {
            if let Some(image) = layout.image.take() {
                match &mut self.texture {
                    Some(t) => t.set(image, TextureOptions::NEAREST),
                    None => {
                        self.texture =
                            Some(ctx.load_texture("treemap", image, TextureOptions::NEAREST))
                    }
                }
            }
            self.current = Some(layout);
        }
    }

    /// 数据整体换掉（新的扫描）时清空。
    pub fn reset(&mut self) {
        self.current = None;
        self.requested = None;
    }
}

fn compute(req: &Request) -> Layout {
    let [w, h] = req.key.size;
    let mut lay = Layouter {
        tree: &req.tree,
        tiles: Vec::new(),
        headers: Vec::new(),
        u: req.key.unit.max(1) as f32,
    };
    if w > 0 && h > 0 {
        // 每块只在右、下让出一个缝宽；整张图按"宽 + 缝"布局，最右、最下一列正好贴边。
        let g = lay.u;
        lay.dir(req.key.root, [0.0, 0.0, w as f32 + g, h as f32 + g], 0, 0);
    }
    let image = render(req, &lay.tiles);
    Layout {
        key: req.key,
        tiles: lay.tiles,
        headers: lay.headers,
        image: Some(image),
    }
}

struct Layouter<'a> {
    tree: &'a Tree,
    tiles: Vec<Tile>,
    headers: Vec<(usize, [f32; 4])>,
    /// 缝宽单位（物理像素）。
    u: f32,
}

impl Layouter<'_> {
    /// `cell` 是这个文件夹占的格子（整数坐标，含右、下的缝）；`under` 是它所在的分组层级。
    fn dir(&mut self, node: u32, cell: [f32; 4], depth: u32, under: u8) {
        let u = self.u;
        let g = u;
        let r = [cell[0], cell[1], cell[2] - g, cell[3] - g];
        let (w, h) = (r[2] - r[0], r[3] - r[1]);
        if w < 1.0 || h < 1.0 {
            return;
        }
        if depth > 0 && w * h < MIN_DIR_AREA * u * u {
            self.push(r, node, TileKind::Dir, 0);
            self.push(r, node, TileKind::Rest, under);
            return;
        }
        let group = depth > 0 && w >= GROUP_MIN * u && h >= GROUP_MIN * u;
        let level = if group { depth.min(255) as u8 } else { 0 };
        let index = self.push(r, node, TileKind::Dir, level);
        let inside = if group { level } else { under };
        let pad = if group { GROUP_PAD * u } else { 0.0 };
        let header = if group && w >= HEADER_MIN_W * u && h >= HEADER_MIN_H * u {
            let hh = HEADER_H * u;
            self.headers
                .push((index, [r[0] + pad, r[1], r[2] - pad, r[1] + hh]));
            hh - pad
        } else {
            0.0
        };
        let inner = [r[0] + pad, r[1] + pad + header, r[2] - pad, r[3] - pad];
        let total = self.tree.value(node) as f64;
        if total <= 0.0 || inner[2] <= inner[0] || inner[3] <= inner[1] {
            return;
        }
        // 子项在 inner 里按"宽 + 缝"布局（同根节点的做法），面积按这个外扩后的区域换算。
        let area_rect = [inner[0], inner[1], inner[2] + g, inner[3] + g];
        let area = ((area_rect[2] - area_rect[0]) * (area_rect[3] - area_rect[1])) as f64;
        let scale = area / total;
        let min_area = (MIN_AREA * u * u) as f64;
        let mut items: Vec<(u32, f64)> = Vec::new();
        let mut rest = 0.0;
        for c in self.tree.children(node) {
            let a = self.tree.value(c) as f64 * scale;
            if a <= 0.0 {
                break; // 降序：后面都是 0
            }
            if rest > 0.0 || a < min_area {
                rest += a;
            } else {
                items.push((c, a));
            }
        }
        if rest > 0.0 {
            items.push((u32::MAX, rest));
        }
        for (child, c) in squarify(&items, area_rect) {
            // 吸附到整像素。squarify 里相邻块的共用边是同一个数，取整后仍然重合。
            let c = c.map(f32::round);
            if child == u32::MAX {
                let r = [c[0], c[1], c[2] - g, c[3] - g];
                if r[2] > r[0] && r[3] > r[1] {
                    self.push(r, node, TileKind::Rest, inside);
                }
            } else if self.tree.node(child).is_dir() {
                self.dir(child, c, depth + 1, inside);
            } else {
                let r = [c[0], c[1], c[2] - g, c[3] - g];
                if r[2] > r[0] && r[3] > r[1] {
                    self.push(r, child, TileKind::File, 0);
                }
            }
        }
    }

    fn push(&mut self, rect: [f32; 4], node: u32, kind: TileKind, level: u8) -> usize {
        self.tiles.push(Tile {
            rect,
            node,
            kind,
            level,
        });
        self.tiles.len() - 1
    }
}

/// squarified 布局：`items` 的面积已按比例换算、降序排列，总和等于 `rect` 的面积。
/// 每次沿短边放一行，只要加入下一项不会让这一行最差的长宽比变得更差就继续加。
pub fn squarify(items: &[(u32, f64)], rect: [f32; 4]) -> Vec<(u32, [f32; 4])> {
    let mut out = Vec::with_capacity(items.len());
    let (mut x0, mut y0, x1, y1) = (
        rect[0] as f64,
        rect[1] as f64,
        rect[2] as f64,
        rect[3] as f64,
    );
    let mut i = 0;
    while i < items.len() {
        let (w, h) = (x1 - x0, y1 - y0);
        if w <= 0.0 || h <= 0.0 {
            break;
        }
        let short = w.min(h);
        let worst = |sum: f64, max: f64, min: f64| {
            let s2 = short * short;
            (max * s2 / (sum * sum)).max(sum * sum / (s2 * min))
        };
        let mut j = i;
        let mut sum = 0.0;
        let mut best = f64::INFINITY;
        while j < items.len() {
            let next = sum + items[j].1;
            let r = worst(next, items[i].1, items[j].1);
            if j > i && r > best {
                break;
            }
            best = r;
            sum = next;
            j += 1;
        }
        // 最后一行吃掉剩余空间，消除浮点误差。
        let last = j == items.len();
        let thick = if last {
            if w >= h { w } else { h }
        } else {
            sum / short
        };
        // 相邻块的共用边用同一个数（上一块的终点就是下一块的起点），取整后不会错开。
        let (row_x1, row_y1) = if w >= h {
            (if last { x1 } else { x0 + thick }, y1)
        } else {
            (x1, if last { y1 } else { y0 + thick })
        };
        let mut start = if w >= h { y0 } else { x0 };
        for (k, &(node, a)) in items[i..j].iter().enumerate() {
            let end = if k + 1 == j - i {
                if w >= h { y1 } else { x1 }
            } else {
                start + a / thick
            };
            let r = if w >= h {
                // 竖着一列放在左边。
                [x0, start, row_x1, end]
            } else {
                // 横着一行放在上边。
                [start, y0, end, row_y1]
            };
            out.push((node, r.map(|v| v as f32)));
            start = end;
        }
        if w >= h {
            x0 = row_x1;
        } else {
            y0 = row_y1;
        }
        i = j;
    }
    out
}

/// 文件夹分组的底色：深色主题逐层略亮，浅色主题逐层略暗。
pub fn group_color(bg: Color32, dark: bool, level: u8) -> Color32 {
    let k = level.min(4) as f32;
    if dark {
        mix(bg, Color32::WHITE, 0.045 * k)
    } else {
        mix(bg, Color32::BLACK, 0.035 * k)
    }
}

fn render(req: &Request, tiles: &[Tile]) -> ColorImage {
    let [w, h] = req.key.size;
    let mut px = vec![req.bg; w * h];
    let dark = req.key.dark;
    let unit = req.key.unit.max(1);
    let other = if dark {
        types::OTHER
    } else {
        Color32::from_rgb(0x9a, 0x9f, 0xab)
    };
    for t in tiles {
        let r = t.rect.map(|v| v as i32);
        let radius = t.radius(unit);
        match t.kind {
            TileKind::Dir => {
                if t.level > 0 {
                    let c = group_color(req.bg, dark, t.level);
                    fill_round(&mut px, w, h, r, radius, c);
                }
            }
            TileKind::File => {
                let ext = req.tree.node(t.node).ext;
                let idx = req.colors.get(ext as usize).copied().unwrap_or(u8::MAX);
                let mut base = types::PALETTE.get(idx as usize).copied().unwrap_or(other);
                if dark {
                    // 深色背景上收一点亮度，不刺眼。
                    base = mix(base, req.bg, 0.08);
                }
                if req.key.highlight.is_some_and(|e| e != ext) {
                    base = mix(base, req.bg, 0.78);
                }
                fill_round(&mut px, w, h, r, radius, base);
            }
            TileKind::Rest => {
                // 合并的小文件：点阵纹理，与界面里其他点阵同一种语言。点按整像素网格排，处处对齐。
                let under = if t.level > 0 {
                    group_color(req.bg, dark, t.level)
                } else {
                    req.bg
                };
                let mut dot = mix(other, under, 0.35);
                if req.key.highlight.is_some() {
                    dot = mix(dot, under, 0.6);
                }
                let step = 3 * unit as i32;
                let size = 2 * unit as i32;
                for y in r[1].max(0)..r[3].min(h as i32) {
                    for x in r[0].max(0)..r[2].min(w as i32) {
                        if x % step < size && y % step < size {
                            px[y as usize * w + x as usize] = dot;
                        }
                    }
                }
            }
        }
    }
    ColorImage {
        size: [w, h],
        pixels: px,
        source_size: egui::vec2(w as f32, h as f32),
    }
}

/// 填充圆角矩形（整数像素坐标，右、下不含）。四个角按像素中心到圆弧的距离做抗锯齿，
/// 与下面已有的颜色混合；其余部分实色填充。
fn fill_round(px: &mut [Color32], w: usize, h: usize, r: [i32; 4], radius: f32, c: Color32) {
    let [x0, y0, x1, y1] = [
        r[0].max(0),
        r[1].max(0),
        r[2].min(w as i32),
        r[3].min(h as i32),
    ];
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let rad = radius.max(0.0);
    let ri = rad.ceil() as i32;
    for y in y0..y1 {
        let row = &mut px[y as usize * w..(y as usize + 1) * w];
        let in_band = ri > 0 && (y < r[1] + ri || y >= r[3] - ri);
        if !in_band {
            row[x0 as usize..x1 as usize].fill(c);
            continue;
        }
        // 这一行落在上 / 下圆角带里：两端各 ri 个像素按覆盖率混合，中间实色。
        let cy = if y < r[1] + ri {
            r[1] as f32 + rad
        } else {
            r[3] as f32 - rad
        };
        for x in x0..x1 {
            let corner = x < r[0] + ri || x >= r[2] - ri;
            if !corner {
                row[x as usize] = c;
                continue;
            }
            let cx = if x < r[0] + ri {
                r[0] as f32 + rad
            } else {
                r[2] as f32 - rad
            };
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            // 只有在圆心外侧的象限里才按圆弧裁切。
            let outside_x = if x < r[0] + ri { dx < 0.0 } else { dx > 0.0 };
            let outside_y = if y < r[1] + ri { dy < 0.0 } else { dy > 0.0 };
            let cover = if outside_x && outside_y {
                (rad + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0)
            } else {
                1.0
            };
            if cover >= 1.0 {
                row[x as usize] = c;
            } else if cover > 0.0 {
                row[x as usize] = mix(row[x as usize], c, cover);
            }
        }
    }
}

pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sweep_scan::{Entry, flags};

    fn area(r: [f32; 4]) -> f32 {
        (r[2] - r[0]) * (r[3] - r[1])
    }

    #[test]
    fn squarify_tiles_exactly() {
        let items: Vec<(u32, f64)> = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0]
            .iter()
            .enumerate()
            .map(|(i, &a)| (i as u32, a * 1000.0))
            .collect();
        let total: f64 = items.iter().map(|i| i.1).sum();
        // 600×400 = 240000，与合计 24000 成 10 倍比例。
        let scaled: Vec<(u32, f64)> = items
            .iter()
            .map(|&(n, a)| (n, a * 240000.0 / total))
            .collect();
        let out = squarify(&scaled, [0.0, 0.0, 600.0, 400.0]);
        assert_eq!(out.len(), 7);
        let sum: f32 = out.iter().map(|o| area(o.1)).sum();
        assert!((sum - 240000.0).abs() < 1.0);
        for (k, (_, r)) in out.iter().enumerate() {
            assert!((area(*r) as f64 - scaled[k].1).abs() < 1.0, "面积按比例");
            assert!(r[0] >= 0.0 && r[1] >= 0.0 && r[2] <= 600.001 && r[3] <= 400.001);
            let ratio = (r[2] - r[0]).max(r[3] - r[1]) / (r[2] - r[0]).min(r[3] - r[1]);
            assert!(ratio < 4.0, "长宽比合理：{ratio}");
        }
        // 两两不重叠。
        for a in 0..out.len() {
            for b in a + 1..out.len() {
                let (p, q) = (out[a].1, out[b].1);
                let ix = (p[2].min(q[2]) - p[0].max(q[0])).max(0.0);
                let iy = (p[3].min(q[3]) - p[1].max(q[1])).max(0.0);
                assert!(ix * iy < 0.01);
            }
        }
    }

    #[test]
    fn aggregates_tiny_items_and_hits() {
        let mut t = Tree::new(r"C:\");
        let d = t.push(
            t.root,
            "big",
            Entry {
                flags: flags::DIR,
                ..Default::default()
            },
        );
        let f = t.push(
            d,
            "a.iso",
            Entry {
                size: 1_000_000,
                ..Default::default()
            },
        );
        for k in 0..500 {
            t.push(
                t.root,
                &format!("tiny{k}.txt"),
                Entry {
                    size: 40,
                    ..Default::default()
                },
            );
        }
        t.finish();
        let req = Request {
            key: Key {
                version: 1,
                root: t.root,
                size: [200, 100],
                dark: true,
                highlight: None,
                unit: 1,
            },
            tree: Arc::new(t),
            colors: Arc::new(vec![u8::MAX; 4]),
            bg: Color32::BLACK,
        };
        let l = compute(&req);
        // 根、big 目录、a.iso、一块"其余"。
        assert_eq!(l.tiles.len(), 4);
        assert_eq!(
            l.tiles.iter().filter(|t| t.kind == TileKind::Rest).count(),
            1
        );
        assert_eq!(l.hit(100.0, 50.0).map(|t| t.node), Some(f));
        assert!(l.rect_of(d).is_some());
        let img = l.image.unwrap();
        assert_eq!(img.size, [200, 100]);
    }

    /// 相邻块共用整像素的边：任意两块不重叠，同一文件夹里的块之间正好隔一个缝宽。
    #[test]
    fn tiles_snap_to_pixels_with_uniform_gaps() {
        let mut t = Tree::new(r"C:\");
        let mut sizes = 7u64;
        for d in 0..6 {
            let dir = t.push(
                t.root,
                &format!("d{d}"),
                Entry {
                    flags: flags::DIR,
                    ..Default::default()
                },
            );
            for k in 0..40 {
                sizes = sizes.wrapping_mul(6364136223846793005) % 1_000_003;
                t.push(
                    dir,
                    &format!("f{k}.bin"),
                    Entry {
                        size: 1000 + sizes,
                        ..Default::default()
                    },
                );
            }
        }
        t.finish();
        for unit in [1u8, 2] {
            let req = Request {
                key: Key {
                    version: 1,
                    root: t.root,
                    size: [613, 287],
                    dark: true,
                    highlight: None,
                    unit,
                },
                tree: Arc::new(t.clone()),
                colors: Arc::new(vec![0; 4]),
                bg: Color32::BLACK,
            };
            let l = compute(&req);
            let files: Vec<&Tile> = l.tiles.iter().filter(|t| t.kind != TileKind::Dir).collect();
            assert!(files.len() > 100);
            let g = unit as f32;
            for a in &files {
                assert!(
                    a.rect.iter().all(|v| v.fract() == 0.0),
                    "整像素：{:?}",
                    a.rect
                );
                assert!(a.rect[2] <= 613.0 && a.rect[3] <= 287.0);
            }
            for (i, a) in files.iter().enumerate() {
                for b in &files[i + 1..] {
                    let (p, q) = (a.rect, b.rect);
                    let ix = p[2].min(q[2]) - p[0].max(q[0]);
                    let iy = p[3].min(q[3]) - p[1].max(q[1]);
                    assert!(ix <= 0.0 || iy <= 0.0, "不重叠");
                    let parent_same = t.node(a.node).parent == t.node(b.node).parent;
                    // 左右相邻（纵向有重叠）时，缝正好一个单位。
                    if parent_same && iy > 0.0 && ix <= 0.0 && ix > -(g + 0.5) {
                        assert_eq!(ix, -g, "左右缝宽 {:?} {:?}", p, q);
                    }
                    if parent_same && ix > 0.0 && iy <= 0.0 && iy > -(g + 0.5) {
                        assert_eq!(iy, -g, "上下缝宽 {:?} {:?}", p, q);
                    }
                }
            }
        }
    }
}
