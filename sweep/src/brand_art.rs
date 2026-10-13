// Sweep 的图标：渐变圆角方块上的 5×5 点阵矩形树图。
//
// 和 Pulse（点阵心跳线）同一套语言：方块底色是渐变，上面一格格白点；点按矩形树图的分块着色，
// 越大的块越亮，一眼看得出是"按大小切分的方块"。
// 所有尺寸用同一个图案、同一套比例等比缩放，各尺寸看起来是同一个图标；边缘靠超采样抗锯齿。
// 本文件不依赖任何库：build.rs 用 `include!` 引入它来生成 exe 的 .ico 资源，程序本身作为模块使用
// （窗口图标、界面贴图），所以这里只放普通函数，不写 `use`、不写模块级文档注释。

/// 点阵图案：每个字母是一块，同一块的点亮度相同。
const BRAND_PATTERN: [&str; 5] = ["AAABB", "AAABB", "AAABB", "CCDDD", "CCEEE"];

/// 各块白点的不透明度：大块最亮，逐级变淡。
fn brand_block_alpha(block: u8) -> f32 {
    match block {
        b'A' => 1.0,
        b'B' => 0.78,
        b'C' => 0.6,
        b'D' => 0.48,
        b'E' => 0.32,
        _ => 0.0,
    }
}

/// 背景渐变：左上 → 右下（琥珀 → 玫红）。
const BRAND_FROM: [f32; 3] = [0xf5 as f32, 0x9e as f32, 0x0b as f32];
const BRAND_TO: [f32; 3] = [0xe1 as f32, 0x1d as f32, 0x48 as f32];

/// 方块四周的透明边、方块圆角、点阵宽度、点占点距的比例、点的圆角（都相对于各自的边长）。
/// 与 Pulse 的图标取同样的数值。
const BRAND_PAD: f32 = 1.0 / 32.0;
const BRAND_ROUND: f32 = 0.24;
const BRAND_CONTENT: f32 = 0.66;
const BRAND_DOT: f32 = 0.72;
const BRAND_DOT_ROUND: f32 = 0.22;

/// 点 (x, y) 是否落在左上角 (x0, y0)、边长 w、圆角 r 的圆角方块里。
fn brand_in_rounded(x: f32, y: f32, x0: f32, y0: f32, w: f32, r: f32) -> bool {
    if x < x0 || y < y0 || x > x0 + w || y > y0 + w {
        return false;
    }
    let cx = x.clamp(x0 + r, x0 + w - r);
    let cy = y.clamp(y0 + r, y0 + w - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

/// 渲染 `size`×`size` 的图标，返回非预乘的 RGBA。
pub fn brand_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let pad = s * BRAND_PAD;
    let side = s - 2.0 * pad;
    let radius = side * BRAND_ROUND;
    // 点阵：n 列，点距 pitch，点边长 dot；整个点阵在方块里居中。
    let n = BRAND_PATTERN.len() as f32;
    let content = side * BRAND_CONTENT;
    let pitch = content / (n - 1.0 + BRAND_DOT);
    let dot = pitch * BRAND_DOT;
    let g0 = pad + (side - content) / 2.0;
    let dot_r = dot * BRAND_DOT_ROUND;

    // 4×4 超采样抗锯齿。
    const SS: usize = 4;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = px as f32 + (sx as f32 + 0.5) / SS as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SS as f32;
                    if !brand_in_rounded(x, y, pad, pad, side, radius) {
                        continue;
                    }
                    let t = ((x - pad) + (y - pad)) / (2.0 * side);
                    let mut c = [
                        BRAND_FROM[0] + (BRAND_TO[0] - BRAND_FROM[0]) * t,
                        BRAND_FROM[1] + (BRAND_TO[1] - BRAND_FROM[1]) * t,
                        BRAND_FROM[2] + (BRAND_TO[2] - BRAND_FROM[2]) * t,
                    ];
                    // 落在哪个点上：按格子直接算，不用逐点比较。
                    let col = ((x - g0) / pitch).floor();
                    let row = ((y - g0) / pitch).floor();
                    if col >= 0.0 && row >= 0.0 && col < n && row < n {
                        let (dx, dy) = (g0 + col * pitch, g0 + row * pitch);
                        if brand_in_rounded(x, y, dx, dy, dot, dot_r) {
                            let block = BRAND_PATTERN[row as usize].as_bytes()[col as usize];
                            let alpha = brand_block_alpha(block);
                            for ch in &mut c {
                                *ch += (255.0 - *ch) * alpha;
                            }
                        }
                    }
                    acc[0] += c[0];
                    acc[1] += c[1];
                    acc[2] += c[2];
                    acc[3] += 1.0;
                }
            }
            let i = ((py * size + px) * 4) as usize;
            if acc[3] > 0.0 {
                out[i] = (acc[0] / acc[3]).round() as u8;
                out[i + 1] = (acc[1] / acc[3]).round() as u8;
                out[i + 2] = (acc[2] / acc[3]).round() as u8;
                out[i + 3] = (acc[3] / (SS * SS) as f32 * 255.0).round() as u8;
            }
        }
    }
    out
}
