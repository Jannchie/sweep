//! 文件类型图标：后台线程向 Shell 按扩展名要图标（不读文件本身，所以很快），UI 线程缓存成纹理。
//! 同一扩展名只取一次；文件夹用一个共用的键。

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, HBITMAP, HDC,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES,
    SHGetFileInfoW,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use windows::core::PCWSTR;

/// 文件夹图标的键（扩展名里不会出现尖括号）。
pub const FOLDER: &str = "<dir>";

/// 后台取图标的结果：(键, (边长, RGBA))。
type Loaded = (String, Option<(usize, Vec<u8>)>);

enum Slot {
    Pending,
    Ready(TextureHandle),
    Missing,
}

pub struct FileIcons {
    requests: Sender<String>,
    results: Receiver<Loaded>,
    slots: HashMap<String, Slot>,
}

impl FileIcons {
    pub fn new(ctx: &egui::Context) -> Self {
        let (req_tx, req_rx) = channel::<String>();
        let (res_tx, res_rx) = channel();
        let ctx = ctx.clone();
        // 150% 以上缩放用 32px 大图标缩小显示，否则 16px 小图标放大会糊。
        let large = ctx.pixels_per_point() >= 1.5;
        std::thread::Builder::new()
            .name("file-icons".into())
            .spawn(move || {
                unsafe {
                    let _ = windows::Win32::System::Com::CoInitializeEx(
                        None,
                        windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                    );
                }
                for key in req_rx {
                    let icon = shell_icon(&key, large);
                    if res_tx.send((key, icon)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn icon worker");
        Self {
            requests: req_tx,
            results: res_rx,
            slots: HashMap::new(),
        }
    }

    /// 把后台取到的图标上传为纹理。每帧调用一次。
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((key, icon)) = self.results.try_recv() {
            let slot = match icon {
                Some((size, rgba)) => Slot::Ready(ctx.load_texture(
                    format!("ficon-{key}"),
                    ColorImage::from_rgba_unmultiplied([size, size], &rgba),
                    TextureOptions::LINEAR,
                )),
                None => Slot::Missing,
            };
            self.slots.insert(key, slot);
        }
    }

    /// `key` 是小写扩展名（没有扩展名为空串）或 [`FOLDER`]。
    pub fn get(&mut self, key: &str) -> Option<&TextureHandle> {
        if !self.slots.contains_key(key) {
            let _ = self.requests.send(key.to_string());
            self.slots.insert(key.to_string(), Slot::Pending);
        }
        match self.slots.get(key) {
            Some(Slot::Ready(t)) => Some(t),
            _ => None,
        }
    }
}

fn shell_icon(key: &str, large: bool) -> Option<(usize, Vec<u8>)> {
    let (name, attrs) = if key == FOLDER {
        ("folder".to_string(), FILE_ATTRIBUTE_DIRECTORY)
    } else if key.is_empty() {
        ("file".to_string(), FILE_ATTRIBUTE_NORMAL)
    } else {
        (format!("file.{key}"), FILE_ATTRIBUTE_NORMAL)
    };
    let w = crate::os::wide(&name);
    let mut info = SHFILEINFOW::default();
    let size_flag = if large {
        SHGFI_LARGEICON
    } else {
        SHGFI_SMALLICON
    };
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            attrs,
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_USEFILEATTRIBUTES | size_flag,
        )
    };
    if ok == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let out = to_rgba(info.hIcon, if large { 32 } else { 16 });
    unsafe {
        let _ = DestroyIcon(info.hIcon);
    }
    out
}

fn to_rgba(icon: HICON, size: i32) -> Option<(usize, Vec<u8>)> {
    let mut info = ICONINFO::default();
    unsafe { GetIconInfo(icon, &mut info) }.ok()?;
    let dc = unsafe { CreateCompatibleDC(None) };
    let color = read_bits(dc, info.hbmColor, size);
    let result = color.map(|mut px| {
        // 老式图标没有 alpha 通道，用 AND 掩码补：掩码为 0 的像素不透明。
        if px.chunks_exact(4).all(|p| p[3] == 0) {
            let mask = read_bits(dc, info.hbmMask, size);
            for (i, p) in px.chunks_exact_mut(4).enumerate() {
                p[3] = match &mask {
                    Some(m) if m[i * 4] != 0 => 0,
                    _ => 255,
                };
            }
        }
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2); // BGRA → RGBA
        }
        (size as usize, px)
    });
    unsafe {
        let _ = DeleteDC(dc);
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor.into());
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(info.hbmMask.into());
        }
    }
    result
}

fn read_bits(dc: HDC, bitmap: HBITMAP, size: i32) -> Option<Vec<u8>> {
    if bitmap.is_invalid() {
        return None;
    }
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size, // 自上而下
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut px = vec![0u8; (size * size * 4) as usize];
    let lines = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            size as u32,
            Some(px.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    (lines == size).then_some(px)
}
