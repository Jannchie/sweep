//! 一体化标题栏：客户区延伸到原标题栏位置，页签和窗口按钮画在同一行。
//!
//! 窗口仍保留系统边框（阴影、圆角、左右下三边调整大小），只是通过子类过程去掉了顶部非客户区：
//! - `WM_NCCALCSIZE`：客户区顶边不再让出标题栏；最大化时窗口会超出屏幕一个边框宽，要补回来。
//! - `WM_NCHITTEST`：顶边几像素返回 HTTOP 以便拖动调整高度；标题行空白处返回 HTCAPTION，
//!   拖动、双击最大化、右键系统菜单、Aero Snap 都由系统处理；最大化按钮返回 HTMAXBUTTON，
//!   这样 Windows 11 悬停时会弹出贴靠布局。
//! - 最大化按钮落在非客户区，鼠标事件不会进 egui，悬停 / 按下 / 点击都在这里处理。
//!
//! 标题行各区域的位置由 UI 每帧通过 [`set_layout`] 交过来。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use eframe::egui::{self, Pos2, Rect};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, HTCAPTION, HTCLIENT, HTMAXBUTTON, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed,
    NCCALCSIZE_PARAMS, SM_CXPADDEDBORDER, SM_CYFRAME, SW_MAXIMIZE, SW_RESTORE, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos, ShowWindow, WM_NCCALCSIZE,
    WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE,
    WM_NCMOUSEMOVE,
};

/// 标题行的布局，逻辑坐标（point），相对客户区左上角。
struct Layout {
    pixels_per_point: f32,
    /// 整个标题行，其中不在 `holes` 里的部分可拖动窗口。
    caption: Rect,
    /// 标题行里可交互的控件（页签、按钮）。
    holes: Vec<Rect>,
    max_button: Rect,
}

static LAYOUT: Mutex<Option<Layout>> = Mutex::new(None);
static CTX: OnceLock<egui::Context> = OnceLock::new();
static MAX_HOVER: AtomicBool = AtomicBool::new(false);
static MAX_PRESSED: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// 主窗口句柄，作为系统对话框（删除确认、属性、选择文件夹）的父窗口。
static HWND_VALUE: AtomicIsize = AtomicIsize::new(0);

/// 接管窗口的标题栏；失败时（非 Windows 句柄等）保留系统标题栏，返回 false。
pub fn install(cc: &eframe::CreationContext<'_>) -> bool {
    let Ok(handle) = cc.window_handle() else {
        return false;
    };
    let RawWindowHandle::Win32(w) = handle.as_raw() else {
        return false;
    };
    let hwnd = HWND(w.hwnd.get() as *mut _);
    HWND_VALUE.store(w.hwnd.get(), Ordering::Relaxed);
    let _ = CTX.set(cc.egui_ctx.clone());
    set_window_icon(hwnd);
    unsafe {
        if !SetWindowSubclass(hwnd, Some(subclass), 1, 0).as_bool() {
            return false;
        }
        // 让系统按新的 WM_NCCALCSIZE 重新计算边框。
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    INSTALLED.store(true, Ordering::Relaxed);
    true
}

/// 窗口与任务栏图标改用 exe 里的图标资源（build.rs 生成，含 16–256 多个尺寸），
/// 按当前 DPI 取合适的尺寸，比 eframe 用一张位图缩放清晰。
fn set_window_icon(hwnd: HWND) {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, SM_CXICON, SM_CXSMICON,
        SendMessageW, WM_SETICON,
    };
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            return;
        };
        let dpi = GetDpiForWindow(hwnd);
        for (kind, metric) in [(ICON_SMALL, SM_CXSMICON), (ICON_BIG, SM_CXICON)] {
            let px = GetSystemMetricsForDpi(metric, dpi);
            // 资源 ID 1（见 build.rs 生成的 .rc），即 MAKEINTRESOURCE(1)：用整数充当指针。
            if let Ok(icon) = LoadImageW(
                Some(module.into()),
                windows::core::PCWSTR(std::ptr::without_provenance(1)),
                IMAGE_ICON,
                px,
                px,
                LR_DEFAULTCOLOR,
            ) {
                SendMessageW(
                    hwnd,
                    WM_SETICON,
                    Some(WPARAM(kind as usize)),
                    Some(LPARAM(icon.0 as isize)),
                );
            }
        }
    }
}

/// 主窗口句柄（还没创建时为 0）。
pub fn hwnd() -> isize {
    HWND_VALUE.load(Ordering::Relaxed)
}

/// 是否由 Sweep 自己绘制标题栏。
pub fn installed() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

pub fn set_layout(ctx: &egui::Context, caption: Rect, holes: Vec<Rect>, max_button: Rect) {
    *LAYOUT.lock().unwrap() = Some(Layout {
        pixels_per_point: ctx.pixels_per_point(),
        caption,
        holes,
        max_button,
    });
}

/// 最大化按钮的 (悬停, 按下) 状态。
pub fn max_button_state() -> (bool, bool) {
    (
        MAX_HOVER.load(Ordering::Relaxed),
        MAX_PRESSED.load(Ordering::Relaxed),
    )
}

fn set_flag(flag: &AtomicBool, on: bool) {
    if flag.swap(on, Ordering::Relaxed) != on
        && let Some(ctx) = CTX.get()
    {
        ctx.request_repaint();
    }
}

/// 顶部可调整大小的边框厚度（物理像素）。
fn frame_thickness(hwnd: HWND) -> i32 {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

fn hit_test(hwnd: HWND, lparam: LPARAM) -> Option<u32> {
    let mut pt = POINT {
        x: (lparam.0 & 0xffff) as i16 as i32,
        y: ((lparam.0 >> 16) & 0xffff) as i16 as i32,
    };
    unsafe {
        let _ = ScreenToClient(hwnd, &mut pt);
    }
    let maximized = unsafe { IsZoomed(hwnd).as_bool() };
    let border = frame_thickness(hwnd);
    if !maximized && pt.y < border {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
        }
        return Some(if pt.x < border * 2 {
            HTTOPLEFT
        } else if pt.x >= rc.right - border * 2 {
            HTTOPRIGHT
        } else {
            HTTOP
        });
    }
    let layout = LAYOUT.lock().unwrap();
    let l = layout.as_ref()?;
    let p = Pos2::new(
        pt.x as f32 / l.pixels_per_point,
        pt.y as f32 / l.pixels_per_point,
    );
    if l.max_button.contains(p) {
        Some(HTMAXBUTTON)
    } else if l.caption.contains(p) && !l.holes.iter().any(|h| h.contains(p)) {
        Some(HTCAPTION)
    } else {
        None
    }
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCALCSIZE if wparam.0 != 0 => {
                let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
                let top = params.rgrc[0].top;
                let r = DefSubclassProc(hwnd, msg, wparam, lparam);
                params.rgrc[0].top = top;
                if IsZoomed(hwnd).as_bool() {
                    params.rgrc[0].top += frame_thickness(hwnd);
                }
                return r;
            }
            WM_NCHITTEST => {
                let hit = DefSubclassProc(hwnd, msg, wparam, lparam);
                if hit.0 != HTCLIENT as isize {
                    return hit;
                }
                return match hit_test(hwnd, lparam) {
                    Some(code) => LRESULT(code as isize),
                    None => hit,
                };
            }
            WM_NCMOUSEMOVE => {
                let over = wparam.0 == HTMAXBUTTON as usize;
                set_flag(&MAX_HOVER, over);
                if over {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE | TME_NONCLIENT,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                } else {
                    set_flag(&MAX_PRESSED, false);
                }
            }
            WM_NCMOUSELEAVE => {
                set_flag(&MAX_HOVER, false);
                set_flag(&MAX_PRESSED, false);
            }
            // 交给默认处理会画出 Windows 经典样式的按钮，必须吞掉。
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam.0 == HTMAXBUTTON as usize => {
                set_flag(&MAX_PRESSED, true);
                return LRESULT(0);
            }
            WM_NCLBUTTONUP if wparam.0 == HTMAXBUTTON as usize => {
                if MAX_PRESSED.swap(false, Ordering::Relaxed) {
                    let cmd = if IsZoomed(hwnd).as_bool() {
                        SW_RESTORE
                    } else {
                        SW_MAXIMIZE
                    };
                    let _ = ShowWindow(hwnd, cmd);
                }
                return LRESULT(0);
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }
}
