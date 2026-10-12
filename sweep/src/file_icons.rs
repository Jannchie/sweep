//! 文件类型图标：后台线程向 Shell 按扩展名要图标（不读文件本身，所以很快），UI 线程缓存成纹理。
//! 同一扩展名只取一次；文件夹用一个共用的键。程序图标（[`app_key`]）从 exe / ico 文件本身读取。

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

/// 程序图标的键：注册表 DisplayIcon 的 `路径,序号`。
pub fn app_key(icon: &str) -> String {
    format!("@{icon}")
}

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
    if let Some(spec) = key.strip_prefix('@') {
        return file_icon(spec, large);
    }
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

/// 从 exe / dll / ico 里取图标。`spec` 是 `路径` 或 `路径,序号`（负数是资源 ID）。
fn file_icon(spec: &str, large: bool) -> Option<(usize, Vec<u8>)> {
    let spec = spec.trim().trim_matches('"');
    let (path, index) = match spec.rsplit_once(',') {
        Some((p, i)) if i.trim().parse::<i32>().is_ok() => {
            (p.trim().trim_matches('"'), i.trim().parse().unwrap())
        }
        _ => (spec, 0),
    };
    let w = crate::os::wide(path);
    let (mut big, mut small) = (HICON::default(), HICON::default());
    let n = unsafe {
        windows::Win32::UI::Shell::ExtractIconExW(
            PCWSTR(w.as_ptr()),
            index,
            Some(&mut big),
            Some(&mut small),
            1,
        )
    };
    if n == 0 || n == u32::MAX {
        return None;
    }
    let (icon, other, size) = if large {
        (big, small, 32)
    } else {
        (small, big, 16)
    };
    let out = if icon.is_invalid() {
        None
    } else {
        to_rgba(icon, size)
    };
    unsafe {
        for h in [icon, other] {
            if !h.is_invalid() {
                let _ = DestroyIcon(h);
            }
        }
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
        if px.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
            let mask = read_bits(dc, info.hbmMask, size);
            for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                p[3] = match &mask {
                    Some(m) if m[i * 4] != 0 => 0,
                    _ => 255,
                };
            }
        }
        for p in px.as_chunks_mut::<4>().0 {
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
