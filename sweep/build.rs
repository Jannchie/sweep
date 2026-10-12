//! 生成 Sweep 的 .ico（绘制代码与程序共用 `src/brand_art.rs`），并作为资源嵌入 exe，
//! 资源管理器、开始菜单、任务栏固定的图标都来自这里。

use std::path::PathBuf;

include!("src/brand_art.rs");

fn main() {
    println!("cargo:rerun-if-changed=src/brand_art.rs");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("sweep.ico");
    std::fs::write(&ico, brand_ico(&[16, 20, 24, 32, 40, 48, 64, 256])).unwrap();
    // .rc 里的路径要转义反斜杠。
    let rc = out.join("sweep.rc");
    let path = ico.display().to_string().replace('\\', "\\\\");
    std::fs::write(&rc, format!("1 ICON \"{path}\"\n")).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}

/// 打包成 .ico：每个尺寸一张 32 位 BMP（自下而上的 BGRA + 全零的 AND 掩码）。
fn brand_ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&size| {
            let rgba = brand_rgba(size);
            let mut bmp = Vec::new();
            // BITMAPINFOHEADER：高度是图像 + 掩码，所以乘 2。
            bmp.extend_from_slice(&40u32.to_le_bytes());
            bmp.extend_from_slice(&(size as i32).to_le_bytes());
            bmp.extend_from_slice(&(size as i32 * 2).to_le_bytes());
            bmp.extend_from_slice(&1u16.to_le_bytes());
            bmp.extend_from_slice(&32u16.to_le_bytes());
            bmp.extend_from_slice(&[0u8; 24]);
            for y in (0..size).rev() {
                for x in 0..size {
                    let i = ((y * size + x) * 4) as usize;
                    bmp.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
                }
            }
            let mask_row = (size as usize).div_ceil(32) * 4;
            bmp.extend(std::iter::repeat_n(0u8, mask_row * size as usize));
            bmp
        })
        .collect();

    let mut ico = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&size, img) in sizes.iter().zip(&images) {
        let dim = if size >= 256 { 0 } else { size as u8 };
        ico.extend_from_slice(&[dim, dim, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(img.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        ico.extend_from_slice(&img);
    }
    ico
}
