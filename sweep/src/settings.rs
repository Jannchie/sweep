//! 用户设置：保存在 %APPDATA%\Sweep\settings.ini（简单的 key=value）。

use std::path::PathBuf;

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
            ThemeChoice::System => "跟随系统",
            ThemeChoice::Light => "浅色",
            ThemeChoice::Dark => "深色",
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
            Basis::Allocated => "占用空间",
            Basis::Logical => "文件大小",
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
            EngineChoice::Auto => "自动（优先读取 MFT）",
            EngineChoice::Walk => "总是遍历目录",
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

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub basis: Basis,
    pub engine: EngineChoice,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
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
            "theme={}\nbasis={}\nengine={}\n",
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
        let s = Settings {
            theme: ThemeChoice::Dark,
            basis: Basis::Logical,
            engine: EngineChoice::Walk,
        };
        assert_eq!(Settings::parse(&s.serialize()), s);
        assert_eq!(Settings::parse("garbage\nbasis=nope"), Settings::default());
    }
}
