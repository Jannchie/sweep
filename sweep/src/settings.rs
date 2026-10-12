//! 用户设置：保存在 %APPDATA%\Sweep\settings.ini（简单的 key=value）。

use std::path::PathBuf;

use sweep_scan::i18n::{self, Lang};
use sweep_scan::t;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeChoice {
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => t!("跟随系统", "System", "システム"),
            ThemeChoice::Light => t!("浅色", "Light", "ライト"),
            ThemeChoice::Dark => t!("深色", "Dark", "ダーク"),
        }
    }

    pub fn preference(self) -> eframe::egui::ThemePreference {
        match self {
            ThemeChoice::System => eframe::egui::ThemePreference::System,
            ThemeChoice::Light => eframe::egui::ThemePreference::Light,
            ThemeChoice::Dark => eframe::egui::ThemePreference::Dark,
        }
    }

    fn key(self) -> &'static str {
        match self {
            ThemeChoice::System => "system",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        }
    }
}

/// 列表排序、矩形树图面积、百分比按哪种大小计算。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Basis {
    /// 磁盘上实际占用：删掉能腾出多少空间。云盘占位文件、稀疏文件、压缩文件都按实际占用算。
    Allocated,
    /// 文件的逻辑大小（资源管理器"大小"一栏）。
    Logical,
}

impl Basis {
    pub const ALL: [Basis; 2] = [Basis::Allocated, Basis::Logical];

    pub fn label(self) -> &'static str {
        match self {
            Basis::Allocated => t!("占用空间", "Size on disk", "ディスク上のサイズ"),
            Basis::Logical => t!("文件大小", "Size", "サイズ"),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Basis::Allocated => "allocated",
            Basis::Logical => "logical",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EngineChoice {
    /// 能读 MFT 就读。
    Auto,
    /// 总是遍历目录（与其他工具对照时有用）。
    Walk,
}

impl EngineChoice {
    pub const ALL: [EngineChoice; 2] = [EngineChoice::Auto, EngineChoice::Walk];

    pub fn label(self) -> &'static str {
        match self {
            EngineChoice::Auto => t!(
                "自动（优先读取 MFT）",
                "Automatic (MFT first)",
                "自動 (MFT 優先)"
            ),
            EngineChoice::Walk => t!("总是遍历目录", "Always walk folders", "常にフォルダー走査"),
        }
    }

    pub fn engine(self) -> sweep_scan::Engine {
        match self {
            EngineChoice::Auto => sweep_scan::Engine::Auto,
            EngineChoice::Walk => sweep_scan::Engine::Walk,
        }
    }

    fn key(self) -> &'static str {
        match self {
            EngineChoice::Auto => "auto",
            EngineChoice::Walk => "walk",
        }
    }
}

/// 界面语言。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Language {
    /// 跟随 Windows 显示语言。
    System,
    Zh,
    En,
    Ja,
}

impl Language {
    pub const ALL: [Language; 4] = [Language::System, Language::Zh, Language::En, Language::Ja];

    /// 语言名用各自的语言写，只有"跟随系统"随当前语言变。
    pub fn label(self) -> &'static str {
        match self {
            Language::System => t!("跟随系统", "System", "システム"),
            Language::Zh => "中文",
            Language::En => "English",
            Language::Ja => "日本語",
        }
    }

    pub fn resolve(self) -> Lang {
        match self {
            Language::System => i18n::system_lang(),
            Language::Zh => Lang::Zh,
            Language::En => Lang::En,
            Language::Ja => Lang::Ja,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Language::System => "system",
            Language::Zh => "zh",
            Language::En => "en",
            Language::Ja => "ja",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub language: Language,
    pub theme: ThemeChoice,
    pub basis: Basis,
    pub engine: EngineChoice,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: Language::System,
            theme: ThemeChoice::System,
            basis: Basis::Allocated,
            engine: EngineChoice::Auto,
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("APPDATA")?)
            .join("Sweep")
            .join("settings.ini"),
    )
}

fn pick<T: Copy>(all: &[T], key: impl Fn(T) -> &'static str, v: &str, default: T) -> T {
    all.iter()
        .copied()
        .find(|&x| key(x) == v)
        .unwrap_or(default)
}

impl Settings {
    pub fn load() -> Self {
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return Self::default();
        };
        Self::parse(&text)
    }

    fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "language" => s.language = pick(&Language::ALL, Language::key, v, s.language),
                "theme" => s.theme = pick(&ThemeChoice::ALL, ThemeChoice::key, v, s.theme),
                "basis" => s.basis = pick(&Basis::ALL, Basis::key, v, s.basis),
                "engine" => s.engine = pick(&EngineChoice::ALL, EngineChoice::key, v, s.engine),
                _ => {}
            }
        }
        s
    }

    fn serialize(&self) -> String {
        format!(
            "language={}\ntheme={}\nbasis={}\nengine={}\n",
            self.language.key(),
            self.theme.key(),
            self.basis.key(),
            self.engine.key(),
        )
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, self.serialize());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for language in Language::ALL {
            let s = Settings {
                language,
                theme: ThemeChoice::Dark,
                basis: Basis::Logical,
                engine: EngineChoice::Walk,
            };
            assert_eq!(Settings::parse(&s.serialize()), s);
        }
        assert_eq!(Settings::parse("garbage\nbasis=nope"), Settings::default());
    }

    #[test]
    fn loads_files_without_language() {
        // 加入语言设置之前保存的文件：其余设置照常读取，语言跟随系统。
        let s = Settings::parse("theme=dark\nbasis=logical\nengine=walk\n");
        assert_eq!(s.language, Language::System);
        assert_eq!(s.theme, ThemeChoice::Dark);
        assert_eq!(s.basis, Basis::Logical);
        assert_eq!(s.engine, EngineChoice::Walk);
        assert_eq!(Settings::parse("language=ja\n").language, Language::Ja);
        assert_eq!(Settings::parse("language=fr\n").language, Language::System);
    }
}
