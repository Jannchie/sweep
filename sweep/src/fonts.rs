//! 界面字体：从已安装的字体里按家族名挑选，不打包任何字体。
//!
//! 已安装字体列在注册表 `SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts` 下（HKLM 是全机，
//! HKCU 是只为当前用户安装、文件在 `%LOCALAPPDATA%\Microsoft\Windows\Fonts` 的）。
//! 注册表里的显示名只用来粗筛文件，最终按文件里的家族名（name 表 ID 16，没有则 ID 1）、
//! 字重和是否斜体挑出具体的字形（.ttc 取对应的下标，可变字体用 `wght` 轴取字重）。
//!
//! 每个字体文件只内存映射一次并一直复用：切换语言时重新组合字体列表，不会重复映射。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use eframe::egui::epaint::text::{FontData, FontTweak, VariationCoords};
use eframe::egui::{self, FontFamily};
use skrifa::raw::{FileRef, FontRef, TableProvider};
use skrifa::{MetadataProvider, string::StringId};
use sweep_scan::i18n::Lang;

/// 一种字体的候选：家族名、正文字重、半粗体（标题、强调）字重。
type Pick = (&'static str, f32, f32);

/// 西文与数字（所有语言共用）。
const LATIN: &[Pick] = &[
    ("HarmonyOS Sans", 400.0, 500.0),
    ("Noto Sans", 400.0, 500.0),
    ("Segoe UI", 400.0, 600.0),
];

/// 简体中文字形。英文界面也用它显示文件名里的汉字。
const CJK_SC: &[Pick] = &[
    ("HarmonyOS Sans SC", 400.0, 500.0),
    ("Noto Sans SC", 400.0, 500.0),
    ("Noto Sans CJK SC", 400.0, 500.0),
    ("Microsoft YaHei UI", 400.0, 700.0),
    ("Microsoft YaHei", 400.0, 700.0),
    ("DengXian", 400.0, 700.0),
    ("SimHei", 400.0, 400.0),
];

/// 日文字形（汉字用日本的字形）。HarmonyOS Sans SC 只有中文字形，不能排在前面。
const CJK_JP: &[Pick] = &[
    ("Noto Sans JP", 400.0, 500.0),
    ("Noto Sans CJK JP", 400.0, 500.0),
    ("Yu Gothic UI", 400.0, 600.0),
    ("Meiryo UI", 400.0, 700.0),
    ("Meiryo", 400.0, 700.0),
];

/// 选中的一个字形。
#[derive(Clone)]
pub struct Face {
    pub family: String,
    pub file: PathBuf,
    pub index: u32,
    /// 静态字体的字重；可变字体为请求的字重。
    pub weight: f32,
    /// 可变字体：要设置的 `wght` 坐标。
    pub wght: Option<f32>,
    data: &'static [u8],
}

impl Face {
    fn font_data(&self) -> FontData {
        let mut d = FontData::from_static(self.data);
        d.index = self.index;
        if let Some(w) = self.wght {
            d.tweak = FontTweak {
                coords: VariationCoords::new([(b"wght", w)]),
                ..Default::default()
            };
        }
        d
    }

    fn key(&self) -> String {
        format!("{}#{}@{}", self.file.display(), self.index, self.weight)
    }
}

/// 一种语言下选中的字体：西文、本语言的汉字、兜底（各自的正文与半粗体）。
pub struct Selection {
    pub latin: Option<(Face, Face)>,
    pub cjk: Option<(Face, Face)>,
    /// 兜底：另一套汉字字形（覆盖本语言字体里没有的字）和 Segoe UI（符号）。
    pub fallback: Vec<Face>,
}

pub fn select(lang: Lang) -> Selection {
    let pair = |list: &[Pick]| {
        list.iter().find_map(|&(family, regular, semibold)| {
            let r = find(family, regular)?;
            let s = find(family, semibold).unwrap_or_else(|| r.clone());
            Some((r, s))
        })
    };
    let latin = pair(LATIN);
    let (own, other) = match lang {
        Lang::Ja => (CJK_JP, CJK_SC),
        Lang::Zh | Lang::En => (CJK_SC, CJK_JP),
    };
    let cjk = pair(own);
    let mut fallback = Vec::new();
    // 另一种语言的字体放在后面：中文界面里的假名、日文界面里只有简体才有的字。
    if let Some((r, _)) = pair(other) {
        fallback.push(r);
    }
    if latin.as_ref().is_none_or(|l| l.0.family != "Segoe UI")
        && let Some(f) = find("Segoe UI", 400.0)
    {
        fallback.push(f);
    }
    Selection {
        latin,
        cjk,
        fallback,
    }
}

/// 按语言组合字体并交给 egui。切换语言时再调用一次即可。
pub fn install(ctx: &egui::Context, lang: Lang) {
    let sel = select(lang);
    let mut fonts = egui::FontDefinitions::default();
    let mut regular = Vec::new();
    let mut semibold = Vec::new();
    let mut add = |face: &Face, list: &mut Vec<String>| {
        let key = face.key();
        fonts
            .font_data
            .entry(key.clone())
            .or_insert_with(|| Arc::new(face.font_data()));
        if !list.contains(&key) {
            list.push(key);
        }
    };
    for (r, s) in sel.latin.iter().chain(&sel.cjk) {
        add(r, &mut regular);
        add(s, &mut semibold);
    }
    for f in &sel.fallback {
        add(f, &mut regular);
    }
    let defaults = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut prop = regular.clone();
    prop.extend(defaults);
    semibold.extend(
        prop.iter()
            .filter(|k| !semibold.contains(k))
            .cloned()
            .collect::<Vec<_>>(),
    );
    // 等宽字体里的汉字、假名也按语言取。
    if let Some((r, _)) = &sel.cjk {
        let key = r.key();
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .push(key);
    }
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(crate::theme::semibold(), semibold);
    ctx.set_fonts(fonts);
}

// ---------------------------------------------------------------- 查找

/// 注册表里的一项：显示名拆开后的各个名字（小写）与文件路径。
struct Entry {
    names: Vec<String>,
    path: PathBuf,
}

fn installed() -> &'static [Entry] {
    static LIST: OnceLock<Vec<Entry>> = OnceLock::new();
    LIST.get_or_init(|| {
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
        let fonts_dir =
            PathBuf::from(std::env::var_os("WINDIR").unwrap_or("C:\\Windows".into())).join("Fonts");
        let mut out = Vec::new();
        // 当前用户的排在前面：同名时用户自己装的版本优先。
        for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
            for (name, file) in registry_values(root) {
                let path = PathBuf::from(&file);
                let path = if path.is_absolute() {
                    path
                } else {
                    fonts_dir.join(path)
                };
                out.push(Entry {
                    names: display_names(&name),
                    path,
                });
            }
        }
        out
    })
}

/// "Yu Gothic Bold & Yu Gothic UI Semibold (TrueType)" → ["yu gothic bold", "yu gothic ui semibold"]
fn display_names(name: &str) -> Vec<String> {
    let name = match name.rfind(" (") {
        Some(i) if name.ends_with(')') => &name[..i],
        _ => name,
    };
    name.split(" & ").map(|s| s.trim().to_lowercase()).collect()
}

/// 显示名粗筛：某个名字是家族名本身，或以"家族名 + 空格"开头（后面是字重等）。
fn may_contain(names: &[String], family: &str) -> bool {
    names.iter().any(|n| {
        n.strip_prefix(family)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    })
}

fn registry_values(root: windows::Win32::System::Registry::HKEY) -> Vec<(String, String)> {
    use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        HKEY, KEY_READ, REG_SZ, RegCloseKey, RegEnumValueW, RegOpenKeyExW,
    };
    use windows::core::{PWSTR, w};
    let mut out = Vec::new();
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            root,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts"),
            None,
            KEY_READ,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return out;
        }
        let mut name = vec![0u16; 1024];
        let mut data = vec![0u8; 4096];
        for i in 0.. {
            let mut name_len = name.len() as u32;
            let mut data_len = data.len() as u32;
            let mut ty = 0u32;
            let r = RegEnumValueW(
                key,
                i,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut ty),
                Some(data.as_mut_ptr()),
                Some(&mut data_len),
            );
            if r == ERROR_MORE_DATA {
                continue;
            }
            if r != ERROR_SUCCESS {
                break;
            }
            if ty != REG_SZ.0 {
                continue;
            }
            let wide: Vec<u16> = data[..data_len as usize]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| u16::from_le_bytes(c))
                .take_while(|&c| c != 0)
                .collect();
            out.push((
                String::from_utf16_lossy(&name[..name_len as usize]),
                String::from_utf16_lossy(&wide),
            ));
        }
        let _ = RegCloseKey(key);
    }
    out
}

/// 已映射的字体文件（打不开的记为 `None`，不再重试）。
type Maps = HashMap<PathBuf, Option<&'static [u8]>>;

/// 内存映射字体文件，每个文件只映射一次（映射一直保留，供 egui 以 `'static` 引用）。
/// 字体文件页可与其他进程共享，不计入私有内存。
fn map(path: &Path) -> Option<&'static [u8]> {
    static MAPS: OnceLock<Mutex<Maps>> = OnceLock::new();
    let mut maps = MAPS.get_or_init(Default::default).lock().unwrap();
    *maps.entry(path.to_path_buf()).or_insert_with(|| {
        let file = std::fs::File::open(path).ok()?;
        let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        let map: &'static memmap2::Mmap = Box::leak(Box::new(map));
        Some(&map[..])
    })
}

/// 在已安装字体里找 `family` 中最接近 `weight` 的正体字形。
pub fn find(family: &str, weight: f32) -> Option<Face> {
    let lower = family.to_lowercase();
    let mut best: Option<(f32, Face)> = None;
    let mut seen = Vec::new();
    for e in installed() {
        if !may_contain(&e.names, &lower) || seen.contains(&&e.path) {
            continue;
        }
        seen.push(&e.path);
        let Some(data) = map(&e.path) else { continue };
        let count = match FileRef::new(data) {
            Ok(FileRef::Font(_)) => 1,
            Ok(FileRef::Collection(c)) => c.len(),
            Err(_) => continue,
        };
        for index in 0..count {
            let Ok(font) = FontRef::from_index(data, index) else {
                continue;
            };
            let Some((score, face_weight, wght)) = matches(&font, family, weight) else {
                continue;
            };
            let face = Face {
                family: family.to_string(),
                file: e.path.clone(),
                index,
                weight: face_weight,
                wght,
                data,
            };
            // 分差相同时：静态字体优先于可变字体，再取较细的。
            let better = match &best {
                None => true,
                Some((s, b)) => {
                    score < *s
                        || (score == *s
                            && (b.wght.is_some() && wght.is_none()
                                || (b.wght.is_some() == wght.is_some() && face_weight < b.weight)))
                }
            };
            if better {
                best = Some((score, face));
            }
        }
    }
    best.map(|b| b.1)
}

/// 这个字形属于 `family` 且是正体时，返回 (与目标字重的差, 字重, 可变字体的 wght 坐标)。
fn matches(font: &FontRef, family: &str, weight: f32) -> Option<(f32, f32, Option<f32>)> {
    let name = |id| {
        font.localized_strings(id)
            .english_or_first()
            .map(|s| s.chars().collect::<String>())
    };
    let fam = name(StringId::TYPOGRAPHIC_FAMILY_NAME).or_else(|| name(StringId::FAMILY_NAME))?;
    if !fam.eq_ignore_ascii_case(family) {
        return None;
    }
    let attrs = font.attributes();
    if attrs.style != skrifa::attribute::Style::Normal {
        return None;
    }
    if let Some(axis) = font
        .axes()
        .iter()
        .find(|a| a.tag() == skrifa::Tag::new(b"wght"))
    {
        let w = weight.clamp(axis.min_value(), axis.max_value());
        return Some(((w - weight).abs(), w, Some(w)));
    }
    // 没有 fvar 表时 axes 为空；有些字体的 OS/2 表缺失则按常规体处理。
    let w = if font.os2().is_ok() {
        attrs.weight.value()
    } else {
        400.0
    };
    Some(((w - weight).abs(), w, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_names() {
        assert_eq!(
            display_names("Yu Gothic Bold & Yu Gothic UI Semibold & Yu Gothic UI Bold (TrueType)"),
            [
                "yu gothic bold",
                "yu gothic ui semibold",
                "yu gothic ui bold"
            ]
        );
        let names = display_names("HarmonyOS Sans SC Medium (TrueType)");
        assert!(may_contain(&names, "harmonyos sans"));
        assert!(may_contain(&names, "harmonyos sans sc"));
        assert!(!may_contain(&names, "harmonyos sans s"));
    }

    /// 打印本机选中的字体：`cargo test -p sweep fonts -- --nocapture`。
    #[test]
    fn selects_installed_fonts() {
        // Segoe UI 是每台 Windows 都有的。
        let segoe = find("Segoe UI", 400.0).expect("Segoe UI");
        assert_eq!(segoe.weight, 400.0);
        assert!(find("Segoe UI", 600.0).is_some_and(|f| f.weight == 600.0));
        for lang in [Lang::Zh, Lang::En, Lang::Ja] {
            let s = select(lang);
            let show = |f: &Face| {
                format!(
                    "{} {} ({}#{}{})",
                    f.family,
                    f.weight,
                    f.file.file_name().unwrap_or_default().to_string_lossy(),
                    f.index,
                    if f.wght.is_some() { ", variable" } else { "" }
                )
            };
            println!("{lang:?}");
            for (role, pair) in [("latin", &s.latin), ("cjk", &s.cjk)] {
                if let Some((r, b)) = pair {
                    println!("  {role}: {} / {}", show(r), show(b));
                }
            }
            for f in &s.fallback {
                println!("  fallback: {}", show(f));
            }
            assert!(s.latin.is_some());
        }
    }
}
