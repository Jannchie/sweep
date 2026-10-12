//! 矩形树图：布局和光栅化都在后台线程完成，UI 线程只贴一张与屏幕像素一一对应的纹理。
//!
//! - 布局用 squarified 算法（Bruls 等），子节点已按大小降序排好，直接按顺序分行；
//! - 面积小于几个像素的子项合并成一块"其余小文件"，用点阵纹理表示，节点再多布局量也只和像素数有关；
//! - 文件块之间留 1px 缝，文件夹之间多留 1px，看得出层级；每块有一点自上而下的明暗渐变；
//! - 只有根、数据、尺寸、主题或高亮的扩展名变化时才重新计算，平时每帧只画一个贴图四边形。

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureOptions};
use sweep_scan::Tree;

use crate::theme::types;

/// 小于这个面积（平方像素）的子项并入"其余"。
const MIN_AREA: f32 = 12.0;
/// 小于这个面积的文件夹不再展开，整块画成"其余"。
const MIN_DIR_AREA: f32 = 100.0;
/// 文件夹两边都不小于这个长度时，四周留 1px 缝。
const DIR_PAD_MIN: f32 = 16.0;

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
    /// 物理像素：(x0, y0, x1, y1)。
    pub rect: [f32; 4],
    pub node: u32,
    pub kind: TileKind,
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
    image: Option<ColorImage>,
}

impl Layout {
    /// 物理像素坐标处最深的一块。
    pub fn hit(&self, x: f32, y: f32) -> Option<&Tile> {
        self.tiles.iter().rev().find(|t| t.contains(x, y))
    }

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
    let mut tiles = Vec::new();
    if w > 0 && h > 0 {
        let mut lay = Layouter {
            tree: &req.tree,
            tiles: &mut tiles,
        };
        lay.dir(req.key.root, [0.0, 0.0, w as f32, h as f32], true);
    }
    let image = render(req, &tiles);
    Layout {
        key: req.key,
        tiles,
        image: Some(image),
    }
}

struct Layouter<'a> {
    tree: &'a Tree,
    tiles: &'a mut Vec<Tile>,
}

impl Layouter<'_> {
    fn dir(&mut self, node: u32, rect: [f32; 4], top: bool) {
        let (w, h) = (rect[2] - rect[0], rect[3] - rect[1]);
        self.tiles.push(Tile {
            rect,
            node,
            kind: TileKind::Dir,
        });
        if w * h < MIN_DIR_AREA && !top {
            self.tiles.push(Tile {
                rect,
                node,
                kind: TileKind::Rest,
            });
            return;
        }
        // 文件夹四周留 1px，与兄弟文件夹之间就有 2px 的缝。
        let inner = if !top && w >= DIR_PAD_MIN && h >= DIR_PAD_MIN {
            [rect[0] + 1.0, rect[1] + 1.0, rect[2] - 1.0, rect[3] - 1.0]
        } else {
            rect
        };
        let total = self.tree.value(node) as f64;
        if total <= 0.0 {
            return;
        }
        let area = ((inner[2] - inner[0]) * (inner[3] - inner[1])) as f64;
        let scale = area / total;
        let mut items: Vec<(u32, f64)> = Vec::new();
        let mut rest = 0.0;
        for c in self.tree.children(node) {
            let a = self.tree.value(c) as f64 * scale;
            if a <= 0.0 {
                break; // 降序：后面都是 0
            }
            if rest > 0.0 || a < MIN_AREA as f64 {
                rest += a;
            } else {
                items.push((c, a));
            }
        }
        if rest > 0.0 {
            items.push((u32::MAX, rest));
        }
        for (child, r) in squarify(&items, inner) {
            if child == u32::MAX {
                self.tiles.push(Tile {
                    rect: r,
                    node,
                    kind: TileKind::Rest,
                });
            } else if self.tree.node(child).is_dir() {
                self.dir(child, r, false);
            } else {
                self.tiles.push(Tile {
                    rect: r,
                    node: child,
                    kind: TileKind::File,
                });
            }
        }
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
        let mut pos = 0.0;
        for (k, &(node, a)) in items[i..j].iter().enumerate() {
            let len = if k + 1 == j - i {
                short - pos
            } else {
                a / thick
            };
            let r = if w >= h {
                // 竖着一列放在左边。
                [x0, y0 + pos, x0 + thick, y0 + pos + len]
            } else {
                // 横着一行放在上边。
                [x0 + pos, y0, x0 + pos + len, y0 + thick]
            };
            out.push((node, r.map(|v| v as f32)));
            pos += len;
        }
        if w >= h {
            x0 += thick;
        } else {
            y0 += thick;
        }
        i = j;
    }
    out
}

fn render(req: &Request, tiles: &[Tile]) -> ColorImage {
    let [w, h] = req.key.size;
    let mut px = vec![req.bg; w * h];
    let dark = req.key.dark;
    let other = if dark {
        types::OTHER
    } else {
        Color32::from_rgb(0x9a, 0x9f, 0xab)
    };
    for t in tiles {
        // 吸附到像素：左上取整、右下取整后各让出 1px 缝（太窄的块不留缝）。
        let (x0, y0) = (t.rect[0].round() as usize, t.rect[1].round() as usize);
        let (mut x1, mut y1) = (
            (t.rect[2].round() as usize).min(w),
            (t.rect[3].round() as usize).min(h),
        );
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        if x1 - x0 >= 3 {
            x1 -= 1;
        }
        if y1 - y0 >= 3 {
            y1 -= 1;
        }
        match t.kind {
            TileKind::Dir => {}
            TileKind::File => {
                let ext = req.tree.node(t.node).ext;
                let idx = req.colors.get(ext as usize).copied().unwrap_or(u8::MAX);
                let mut base = types::PALETTE.get(idx as usize).copied().unwrap_or(other);
                if req.key.highlight.is_some_and(|e| e != ext) {
                    base = mix(base, req.bg, 0.78);
                }
                fill_shaded(&mut px, w, [x0, y0, x1, y1], base);
            }
            TileKind::Rest => {
                // 合并的小文件：点阵纹理，与界面里其他点阵同一种语言。
                let mut dot = mix(other, req.bg, 0.35);
                if req.key.highlight.is_some() {
                    dot = mix(dot, req.bg, 0.6);
                }
                for y in y0..y1 {
                    for x in x0..x1 {
                        if (x % 3 != 2) && (y % 3 != 2) {
                            px[y * w + x] = dot;
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

/// 从上到下、从左到右略微变暗，像一块微微隆起的方块。
fn fill_shaded(px: &mut [Color32], stride: usize, r: [usize; 4], base: Color32) {
    let [x0, y0, x1, y1] = r;
    let (bw, bh) = ((x1 - x0).max(1) as f32, (y1 - y0).max(1) as f32);
    let [br, bg, bb, _] = base.to_array();
    for y in y0..y1 {
        let ty = (y - y0) as f32 / bh;
        let row = &mut px[y * stride + x0..y * stride + x1];
        for (k, p) in row.iter_mut().enumerate() {
            let tx = k as f32 / bw;
            let f = 1.12 - 0.12 * tx - 0.22 * ty;
            let c = |v: u8| (v as f32 * f).clamp(0.0, 255.0) as u8;
            *p = Color32::from_rgb(c(br), c(bg), c(bb));
        }
    }
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
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
                    size: 1,
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
}
