//! 界面语言：简体中文 / 英文 / 日文，运行时切换。
//!
//! 文案不放资源文件，调用处用 [`t!`](crate::t) / [`tf!`](crate::tf) 把三种语言写在一起，
//! 按当前语言取其一；少写一种语言就编译不过。当前语言是进程级全局状态，UI 线程和扫描线程都能读。

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
    Ja,
}

static CURRENT: AtomicU8 = AtomicU8::new(Lang::Zh as u8);

/// 当前界面语言。
pub fn lang() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        0 => Lang::Zh,
        1 => Lang::En,
        _ => Lang::Ja,
    }
}

pub fn set_lang(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

/// 由 LANGID 的主语言决定：日文为 `Ja`，中文为 `Zh`，其余一律 `En`。
pub fn lang_of(langid: u16) -> Lang {
    // LANGID 低 10 位是主语言。
    const LANG_CHINESE: u16 = 0x04;
    const LANG_JAPANESE: u16 = 0x11;
    match langid & 0x3ff {
        LANG_JAPANESE => Lang::Ja,
        LANG_CHINESE => Lang::Zh,
        _ => Lang::En,
    }
}

/// 系统界面语言（Windows 显示语言）。
pub fn system_lang() -> Lang {
    use windows::Win32::Globalization::GetUserDefaultUILanguage;
    lang_of(unsafe { GetUserDefaultUILanguage() })
}

/// 按当前语言取中文、英文或日文的 `&'static str`：`t!("取消", "Cancel", "キャンセル")`。
#[macro_export]
macro_rules! t {
    ($zh:literal, $en:literal, $ja:literal $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::Zh => $zh,
            $crate::i18n::Lang::En => $en,
            $crate::i18n::Lang::Ja => $ja,
        }
    };
}

/// 带格式化的 [`t!`]，三个分支各自 `format!`，所以可以直接内联捕获变量：
/// `tf!("扫描 {root}", "Scan {root}", "{root} をスキャン")`。
#[macro_export]
macro_rules! tf {
    ($zh:literal, $en:literal, $ja:literal $(, $arg:expr)* $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::Zh => format!($zh $(, $arg)*),
            $crate::i18n::Lang::En => format!($en $(, $arg)*),
            $crate::i18n::Lang::Ja => format!($ja $(, $arg)*),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_language() {
        let n = 3;
        set_lang(Lang::En);
        assert_eq!(t!("文件", "Files", "ファイル"), "Files");
        assert_eq!(tf!("{n} 个", "{n} items", "{n} 個"), "3 items");
        set_lang(Lang::Ja);
        assert_eq!(t!("文件", "Files", "ファイル"), "ファイル");
        assert_eq!(tf!("{} 个", "{} items", "{} 個", n), "3 個");
        set_lang(Lang::Zh);
        assert_eq!(t!("文件", "Files", "ファイル"), "文件");
        assert_eq!(tf!("{} 个", "{} items", "{} 個", n), "3 个");
    }

    #[test]
    fn maps_system_languages() {
        assert_eq!(lang_of(0x0411), Lang::Ja); // ja-JP
        assert_eq!(lang_of(0x0804), Lang::Zh); // zh-CN
        assert_eq!(lang_of(0x0404), Lang::Zh); // zh-TW
        assert_eq!(lang_of(0x0409), Lang::En); // en-US
        assert_eq!(lang_of(0x0407), Lang::En); // de-DE
    }
}
