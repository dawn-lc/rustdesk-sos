//! 常驻的「设备编号」窗口。
//!
//! 受控端常交到不懂电脑的人手里。编号其实一直在托盘图标里（悬停、双击、右键都能
//! 看到），但「托盘图标」这个概念本身就是一道门槛 —— Win10/11 默认把新图标折叠进
//! 溢出区，用户连找都找不到。这个窗口在**注册上管理服务器之后**把编号直接摆到屏幕
//! 中央，用户不需要知道任何东西，照着念给技术人员即可。
//!
//! 与 `ui::consent` 一致的三条约定：
//!
//! * **不阻塞调用方**：窗口在自己的线程里跑消息循环，`show()` 立即返回，
//!   所以可以直接从 tokio 任务里调。
//! * **与本 crate 其它模块零依赖**：编号由调用方传入，本模块不读配置。
//!   与 `src/lib.rs` 的依赖方向表一致：`ui` 只认 win32 / std / `hbb_common`。
//! * 只认 win32 / std / `hbb_common`。
//!
//! 与授权弹窗刻意不同的两点：
//!
//! * **不自动关闭**：它是给用户看的常驻提示，不是一次性的决定。只有用户自己关掉，
//!   或控制端来了（`close()`，见 `net::connection` 的入站连接）才收。
//! * **只弹一次**：`SHOWN` 保证一个进程生命周期内只建一次窗口。管理服务器重连、
//!   信令重注册都不会再弹 —— 否则每次网络抖动都糊一个新窗口上来。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, RECT,
    WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, GetDC, GetDeviceCaps, GetObjectW, GetStockObject, GetSysColor,
    GetSysColorBrush, ReleaseDC, SetBkMode, SetTextColor, COLOR_GRAYTEXT, COLOR_WINDOW,
    COLOR_WINDOWTEXT, DEFAULT_GUI_FONT, HBRUSH, HDC, HFONT, HGDIOBJ, LOGFONTW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// 客户区尺寸（96 DPI 下的逻辑值，实际创建时按系统 DPI 缩放）。
const CLIENT_W: i32 = 420;
const CLIENT_H: i32 = 232;
const PAD: i32 = 16;
const BTN_W: i32 = 88;
const BTN_H: i32 = 28;
const ID_H: i32 = 64;

const CLASS_NAME: &str = "RustDeskSOS_DeviceId";
const WINDOW_TITLE: &str = "RustDesk SOS";

/// 静态文本控件样式：`SS_CENTER`（居中）、`SS_ETCHEDHORZ`（画一条横线）。
/// windows crate 没有导出它们，与 `ui::consent` 一样就地定义。
const SS_CENTER: u32 = 0x0001;
const SS_ETCHEDHORZ: u32 = 0x0010;

/// 大号编号控件的 ID（`WM_CTLCOLORSTATIC` 用它区分颜色）。
const ID_VALUE: u32 = 200;
/// 说明文字的控件 ID（灰色）。
const ID_HINT: u32 = 201;
const ID_TITLE: u32 = 202;

/// 当前窗口句柄（0 = 没开），只供 `close()` 从任意线程投 `WM_CLOSE`。
static CURRENT_HWND: AtomicIsize = AtomicIsize::new(0);

/// 本进程是否已经弹过一次。
static SHOWN: AtomicBool = AtomicBool::new(false);

/// 是否已经被要求在显示之前就关掉。
///
/// `show()` 到窗口真正建出来之间有个小窗口期；控制端恰好在这期间连进来时
/// `close()` 会因为 `CURRENT_HWND` 还是 0 而扑空，随后窗口才弹出来，正好压在
/// 授权弹窗前面。这个标记让窗口在建好后立刻自我了断。
static CLOSE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 显示「设备编号」窗口（不阻塞）。
///
/// 由 `net::rendezvous` 在**注册上管理服务器之后**调用：那一刻编号才是最终的
/// （`UUID_MISMATCH` 会改号，早于此显示可能让用户念出一个已经作废的号码）。
///
/// 重复调用只会把已经存在的窗口提到前台，不会再建一个。
pub fn show(device_id: &str) {
    if SHOWN.swap(true, Ordering::AcqRel) {
        // 已经弹过：可能还开着，也可能被用户关了。两种情况都不重建，
        // 只把还开着的那扇窗提到前台，避免重连时反复糊窗口。
        focus_existing();
        return;
    }
    log::info!("[DEVICE_ID] showing the device id window");
    let id = device_id.to_string();
    std::thread::spawn(move || run_window(id));
}

/// 关闭窗口（任意线程可调，非阻塞）。没有窗口时为空操作。
///
/// 由 `net::connection` 在收到入站控制请求时调用：这时编号的任务已经完成，
/// 窗口不该再压在授权弹窗前面。
pub fn close() {
    CLOSE_REQUESTED.store(true, Ordering::Release);
    let raw = CURRENT_HWND.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    log::info!("[DEVICE_ID] closing the device id window (control request)");
    unsafe {
        let _ = PostMessageW(
            Some(HWND(raw as *mut core::ffi::c_void)),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

fn focus_existing() {
    let raw = CURRENT_HWND.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    unsafe {
        let _ = SetForegroundWindow(HWND(raw as *mut core::ffi::c_void));
    }
}

/// 编号分组：`297474596` → `297 474 596`。
///
/// 用户要在电话里把这串数字念给技术人员，三位一组既不容易漏位，也方便对方核对。
/// 非纯数字（理论上不该发生）原样返回，不在这里猜测格式。
fn group_digits(id: &str) -> String {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return id.to_string();
    }
    let bytes = id.as_bytes();
    let mut out = String::with_capacity(id.len() + id.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(*b as char);
    }
    out
}

/// 创建窗口、跑消息循环；窗口销毁后线程退出。
fn run_window(device_id: String) {
    if let Err(e) = unsafe { create_and_pump(&device_id) } {
        log::error!("[DEVICE_ID] cannot show device id window: {}", e);
        // 建不出来就把名额还回去，下次注册还有机会（例如当时没有交互桌面）。
        SHOWN.store(false, Ordering::Release);
    }
}

unsafe fn create_and_pump(device_id: &str) -> hbb_common::ResultType<()> {
    let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();
    let class = wide(CLASS_NAME);

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: LoadIconW(Some(hinstance), PCWSTR(1 as *const u16)).unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as isize as *mut core::ffi::c_void),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class.as_ptr()),
    };
    if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
        anyhow::bail!("RegisterClassW failed");
    }

    let dpi = system_dpi();
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let ex_style = WS_EX_TOPMOST | WS_EX_DLGMODALFRAME;
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: scale(CLIENT_W, dpi),
        bottom: scale(CLIENT_H, dpi),
    };
    AdjustWindowRectEx(&mut frame, style, false, ex_style)?;
    let (w, h) = (frame.right - frame.left, frame.bottom - frame.top);
    // 居中到主屏（进程是系统 DPI 感知，GetSystemMetrics 已是缩放后坐标）
    let x = (GetSystemMetrics(SM_CXSCREEN) - w) / 2;
    let y = (GetSystemMetrics(SM_CYSCREEN) - h) / 2;

    let title = wide(WINDOW_TITLE);
    let hwnd = CreateWindowExW(
        ex_style,
        PCWSTR(class.as_ptr()),
        PCWSTR(title.as_ptr()),
        style,
        x,
        y,
        w,
        h,
        None,
        None,
        Some(hinstance),
        None,
    )?;
    CURRENT_HWND.store(hwnd.0 as isize, Ordering::Release);

    // 显示之前就被要求关闭（控制端抢先连进来了）：建好即销毁，不要弹出来。
    if CLOSE_REQUESTED.load(Ordering::Acquire) {
        CURRENT_HWND.store(0, Ordering::Release);
        let _ = DestroyWindow(hwnd);
        return Ok(());
    }

    let font = body_font();
    let font_title = bold_font(font, 1);
    let font_id = bold_font(font, 3);
    let built = populate(hwnd, hinstance, device_id, dpi, font, font_title, font_id);

    if built.is_ok() {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        // 只调层级，不动位置（漏掉 SWP_NOMOVE 会把居中坐标改成 (0,0)）
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOSIZE | SWP_NOMOVE | SWP_SHOWWINDOW,
        );
        pump_messages(hwnd);
    }

    // 字体是 GDI 对象，消息循环结束后释放；`bold_font` 失败时会返回原句柄，去重防双删。
    let mut fonts = vec![font];
    if font_title.0 != font.0 {
        fonts.push(font_title);
    }
    if font_id.0 != font.0 && font_id.0 != font_title.0 {
        fonts.push(font_id);
    }
    for f in fonts {
        let _ = DeleteObject(HGDIOBJ(f.0));
    }
    CURRENT_HWND.store(0, Ordering::Release);
    built
}

unsafe fn populate(
    hwnd: HWND,
    hinstance: HINSTANCE,
    device_id: &str,
    dpi: u32,
    font: HFONT,
    font_title: HFONT,
    font_id: HFONT,
) -> hbb_common::ResultType<()> {
    let s = |v: i32| scale(v, dpi);

    let title = create_child(
        hwnd,
        hinstance,
        w!("STATIC"),
        "您的设备编号",
        (WS_CHILD | WS_VISIBLE).0,
        Some(ID_TITLE),
        s(PAD),
        s(PAD),
        s(CLIENT_W - PAD * 2),
        s(26),
    )?;
    // 分隔线：SS_ETCHEDHORZ 自带一条 1px 线，与授权弹窗同款
    let _sep = create_child(
        hwnd,
        hinstance,
        w!("STATIC"),
        "",
        (WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_ETCHEDHORZ)).0,
        None,
        s(PAD),
        s(PAD + 34),
        s(CLIENT_W - PAD * 2),
        s(2),
    )?;
    let value = create_child(
        hwnd,
        hinstance,
        w!("STATIC"),
        &group_digits(device_id),
        (WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_CENTER)).0,
        Some(ID_VALUE),
        s(PAD),
        s(PAD + 44),
        s(CLIENT_W - PAD * 2),
        s(ID_H),
    )?;
    let hint_y = s(PAD + 44 + ID_H + 6);
    let btn_y = s(CLIENT_H - PAD - BTN_H);
    let hint = create_child(
        hwnd,
        hinstance,
        w!("STATIC"),
        "技术人员远程协助时，请把这个编号告诉他。\n关掉此窗口后，仍可在屏幕右下角托盘图标上查看。",
        (WS_CHILD | WS_VISIBLE).0,
        Some(ID_HINT),
        s(PAD),
        hint_y,
        s(CLIENT_W - PAD * 2),
        (btn_y - s(10) - hint_y).max(s(30)),
    )?;
    // BS_DEFPUSHBUTTON 是 i32，与 WINDOW_STYLE 的 u32 不同型，显式转换
    let button_style = (WS_CHILD | WS_VISIBLE | WS_TABSTOP).0
        | u32::try_from(BS_DEFPUSHBUTTON).unwrap_or(0);
    let close_btn = create_child(
        hwnd,
        hinstance,
        w!("BUTTON"),
        "关闭",
        button_style,
        // 用 IDCANCEL 作控件 ID：IsDialogMessageW 才会把 Esc 映射成关闭
        Some(IDCANCEL.0 as u32),
        s(CLIENT_W - PAD - BTN_W),
        btn_y,
        s(BTN_W),
        s(BTN_H),
    )?;

    for (child, f) in [
        (title, font_title),
        (value, font_id),
        (hint, font),
        (close_btn, font),
    ] {
        SendMessageW(
            child,
            WM_SETFONT,
            Some(WPARAM(f.0 as usize)),
            Some(LPARAM(1)),
        );
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as u32;
            if id == IDCANCEL.0 as u32 {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
        }
        // 标题栏 X、Alt+F4、以及 `close()` 投来的 WM_CLOSE 都走这里
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        // 0x0138 == WM_CTLCOLORSTATIC：静态控件默认不继承父窗口前景色，
        // 不接管这条消息说明文字会是黑的。
        0x0138 => {
            let hdc = HDC(lparam.0 as *mut core::ffi::c_void);
            let hctrl = HWND(wparam.0 as *mut core::ffi::c_void);
            let id = GetDlgCtrlID(hctrl) as u32;
            let gray = id == ID_HINT;
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(
                hdc,
                COLORREF(GetSysColor(if gray { COLOR_GRAYTEXT } else { COLOR_WINDOWTEXT })),
            );
            return LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize);
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 标准消息泵：`IsDialogMessageW` 负责 Tab 切换与 Esc 关闭。
unsafe fn pump_messages(hwnd: HWND) {
    let mut msg = MSG::default();
    loop {
        let ret = GetMessageW(&mut msg, None, 0, 0);
        if ret.0 <= 0 {
            break;
        }
        if IsDialogMessageW(hwnd, &msg).as_bool() {
            continue;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// 96dpi 逻辑像素 → 实际像素。
fn scale(v: i32, dpi: u32) -> i32 {
    (v as i64 * dpi as i64 / 96) as i32
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 不用 `GetDpiForSystem`：它要求 `Win32_UI_HiDpi` feature（与 `ui::consent` 同因）。
fn system_dpi() -> u32 {
    use windows::Win32::Graphics::Gdi::LOGPIXELSX;
    unsafe {
        let dpi = match GetDC(None) {
            dc if dc.is_invalid() => 0,
            dc => {
                let d = GetDeviceCaps(Some(dc), LOGPIXELSX);
                ReleaseDC(None, dc);
                d
            }
        };
        if dpi > 0 { dpi as u32 } else { 96 }
    }
}

/// 正文/按钮字体：系统「消息框」字体（现代系统上是 Segoe UI 9pt）。
unsafe fn body_font() -> HFONT {
    let stock = || HFONT(GetStockObject(DEFAULT_GUI_FONT).0);
    let mut ncm = NONCLIENTMETRICSW::default();
    ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
    let ok = SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        Some(&mut ncm as *mut _ as *mut core::ffi::c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_ok();
    if !ok {
        return stock();
    }
    let font = CreateFontIndirectW(&ncm.lfMessageFont);
    if font.is_invalid() { stock() } else { font }
}

/// 在正文字体上加粗并放大 `factor` 倍（`factor = 1` 与授权弹窗标题同款）。
unsafe fn bold_font(base: HFONT, factor: i32) -> HFONT {
    let mut lf = LOGFONTW::default();
    let got = GetObjectW(
        HGDIOBJ(base.0),
        std::mem::size_of::<LOGFONTW>() as i32,
        Some(&mut lf as *mut _ as *mut core::ffi::c_void),
    );
    if got == 0 {
        return base;
    }
    lf.lfWeight = 700;
    lf.lfHeight = lf.lfHeight * factor;
    let font = CreateFontIndirectW(&lf);
    if font.is_invalid() { base } else { font }
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_child(
    parent: HWND,
    hinstance: HINSTANCE,
    class: PCWSTR,
    text: &str,
    style: u32,
    id: Option<u32>,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> hbb_common::ResultType<HWND> {
    let text = wide(text);
    let menu = id.map(|id| HMENU(id as isize as *mut core::ffi::c_void));
    Ok(CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class,
        PCWSTR(text.as_ptr()),
        WINDOW_STYLE(style),
        x,
        y,
        w,
        h,
        Some(parent),
        menu,
        Some(hinstance),
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::group_digits;

    /// 现场编号是 9 位；三位一组是给「在电话里念」用的。
    #[test]
    fn digits_are_grouped_in_threes_from_the_right() {
        assert_eq!(group_digits("297474596"), "297 474 596");
        assert_eq!(group_digits("1234567890"), "1 234 567 890");
        assert_eq!(group_digits("12345"), "12 345");
    }

    /// 短编号不该被塞空格；非数字原样返回，不在这里猜测格式。
    #[test]
    fn short_or_non_numeric_ids_are_left_alone() {
        assert_eq!(group_digits("123"), "123");
        assert_eq!(group_digits(""), "");
        assert_eq!(group_digits("abc"), "abc");
        assert_eq!(group_digits("12 34"), "12 34");
    }
}
