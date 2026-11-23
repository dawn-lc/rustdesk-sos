//! 连接确认弹窗（"同意受控"）
//!
//! 密码校验通过之后、发送 `LoginResponse` 之前，由**本机用户点击「同意」**才放行
//! 这次远控。上游把这一步做在连接管理器 UI 里（Flutter / Sciter 子窗口 +
//! `ui_cm_interface` 的待确认队列），SOS 没有 UI 框架，这里直接用 Win32
//! 创建一个置顶顶层窗口，在独立线程里跑消息循环。
//!
//! 与上游语义对齐的部分：
//!
//! * 确认发生在**密码之后**。密码错就不弹窗 —— 否则任何人只要连上来就能给用户
//!   弹一个框，那是一个现成的骚扰/探测信道。
//! * 拒绝后**立刻关闭连接**，不让对端继续重试，避免「拒绝 → 重连 → 再弹窗」的
//!   循环。
//!
//! 三条 SOS 特有的约束：
//!
//! 1. **不能阻塞 tokio worker**：窗口消息循环与 `MessageBox` 都是线程阻塞的，
//!    所以调用方必须走 `confirm`（内部 `spawn_blocking`），不能直接调 `show`。
//! 2. **必须能被外部收场**：无人值守的机器上没人点按钮，窗口不能一直挂着占住连接，
//!    所以有 `CONSENT_TIMEOUT` 兜底；另外控制端可能在等待期间就断开，调用方要能用
//!    `cancel_pending` 立刻收窗（见 `crate::net::connection::request_local_approval`）。
//!    等待期间给控制端重发「等待接受」由调用方负责，本模块只管窗口。
//! 3. **默认开启，且没有开关**：每次连接都要本机用户点一下。但「要不要弹」由
//!    **调用方**判断（`crate::net::connection::local_user_can_approve`）：锁屏 / UAC 安全
//!    桌面期间窗口会建在用户 Default 桌面上，而用户看到的是 Winlogon 桌面 ——
//!    既看不见也点不到，那种时候**不弹**，直接退回「密码即授权」。
//!
//! 所以本模块不关心当前桌面状态，它只负责「弹一个窗，等一个决定」。

use hbb_common::ResultType;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, TryLockError};
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, RECT,
    WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, GetDC, GetDeviceCaps, GetObjectW, GetStockObject, GetSysColor,
    GetSysColorBrush, ReleaseDC, LOGPIXELSX,
    SetBkMode, SetTextColor, COLOR_GRAYTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, DEFAULT_GUI_FONT,
    HBRUSH, HDC, HFONT, HGDIOBJ, LOGFONTW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// 弹窗等待时长：与上游 `LOGIN_GRACE`（180s）对齐。
///
/// 能等这么久是因为等待期间**一直在给控制端发包**（见 `WAIT_REFRESH`）：控制端
/// 的 io_loop 有一个 30s「无数据」看门狗（`src/client/io_loop.rs:337`，
/// `if last_recv_time.elapsed() >= SEC30 { msgbox("Timeout"); break }`），
/// 只要 30s 内没收到任何字节就直接报超时断开 —— 与它是否正在显示「等待对方接受」
/// 无关。`crate::net::connection::request_local_approval` 每 `WAIT_REFRESH` 重发一次上游
/// 标准的「等待接受」`LoginResponse`，既喂住看门狗，也顺带探测控制端是否已经
/// 断开。
pub const CONSENT_TIMEOUT: Duration = Duration::from_secs(180);

/// 等待期间给控制端发保活包的间隔。
///
/// 必须明显小于控制端 30s 的无数据看门狗（取 10s 留三次余量）。
pub const WAIT_REFRESH: Duration = Duration::from_secs(10);

/// 本机用户的决定
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 用户点了「同意」
    Accepted,
    /// 用户点了「拒绝」，或关掉了窗口
    Rejected,
    /// 弹窗被 `cancel_pending()` 收掉（控制端已断开，或它的密码先通过了）。
    ///
    /// **不是用户的决定**：日志里与 `Rejected` 分开记，否则排查时会把「对端走了」
    /// 读成「本机用户拒绝了」。
    Cancelled,
    /// 超时无人操作
    TimedOut,
    /// 已有一个确认窗在等待，本次直接拒绝（防止并发请求堆出弹窗风暴）
    Busy,
    /// 窗口创建不出来（无交互桌面 / 资源耗尽 / win32 调用失败）。
    ///
    /// **不等于拒绝**：弹窗只是密码的替代路径，调用方收到它应当退回「只认密码」，
    /// 而不是把连接挡在门外。
    Unavailable,
}

impl Decision {
    pub fn is_accepted(self) -> bool {
        matches!(self, Decision::Accepted)
    }
}

/// 弹窗里展示的请求方信息（由 `LoginRequest` 填充）。
///
/// 用自有结构而不是直接引用 protobuf 类型：本模块因此不依赖 `base::protos`，
/// 也就能被 `tests/*.rs` 单独 `#[path]` 引入做单测（与项目里其他模块一致）。
#[derive(Debug, Clone, Default)]
pub struct PeerHint {
    /// 控制端 ID（`LoginRequest.my_id`）
    pub id: String,
    /// 控制端名称（`LoginRequest.my_name`，旧客户端可能为空）
    pub name: String,
    /// 控制端平台（`LoginRequest.my_platform`）
    pub platform: String,
    /// 网络来源地址
    pub addr: String,
    /// 这条连接是「文件传输」而不是远程桌面
    pub file_transfer: bool,
}

// ── 对外入口 ──

/// 请求本机用户确认这次连接。
///
/// **是否需要弹窗由调用方判断**（见 `crate::net::connection::local_user_can_approve`）：
/// 锁屏 / UAC 安全桌面期间用户根本看不到窗口，那时调用方不会走到这里。
/// 本模块只管「弹窗并等一个决定」，在阻塞线程池里跑，**不阻塞** tokio worker。
pub async fn confirm(peer: PeerHint) -> Decision {
    match tokio::task::spawn_blocking(move || show(peer)).await {
        Ok(decision) => decision,
        Err(e) => {
            // 阻塞任务 panic 或被取消：宁可拒绝，也不要在「确认」这条安全路径上
            // 放行一个没被确认过的连接。
            log::error!("[CONSENT] approval task failed: {}", e);
            Decision::Unavailable
        }
    }
}

/// 同一时刻只允许一个确认窗。
///
/// 并发来的第二个请求不排队等待（否则攻击者可以堆出一串弹窗，或者把阻塞线程
/// 池占满），直接拒绝并让对端看到明确原因。
static GATE: Mutex<()> = Mutex::new(());

/// 本次弹窗是否被 `cancel_pending()` 收掉。
///
/// 用来把「取消」和「用户点拒绝」区分开：两者都走 `IDCANCEL`，但语义完全不同。
static CANCELLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 当前确认窗的 HWND（0 = 没有）。
///
/// 只用于 `cancel_pending`：从任意线程给窗口投一条「拒绝」，让阻塞在消息循环里的
/// 弹窗立刻收场。`PostMessageW` 本身是线程安全的，真正的状态修改仍发生在窗口自己
/// 的线程上（`WM_COMMAND` → `resolve`）。
static CURRENT_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 取消当前正在等待的确认窗（等价于用户点了「拒绝」）。
///
/// 在控制端已经断开时使用：让阻塞的弹窗立刻收场，不再白等到超时，也不占着
/// `GATE` 挡住后续连接。没有弹窗在等待时是空操作。
pub fn cancel_pending() {
    let hwnd = CURRENT_HWND.load(Ordering::Acquire);
    if hwnd == 0 {
        return;
    }
    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    log::info!("[CONSENT] cancelling the pending approval dialog");
    CANCELLED.store(true, Ordering::Release);
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            WM_COMMAND,
            WPARAM(IDCANCEL.0 as usize),
            LPARAM(0),
        );
    }
}

/// 同步弹出确认窗并等待用户操作（**阻塞**调用线程，必须在阻塞线程里跑）。
fn show(peer: PeerHint) -> Decision {
    let _gate = match GATE.try_lock() {
        Ok(g) => g,
        Err(TryLockError::WouldBlock) => {
            log::warn!(
                "[CONSENT] another request is already waiting for approval, rejecting id={} addr={}",
                peer.id,
                peer.addr
            );
            return Decision::Busy;
        }
        Err(TryLockError::Poisoned(_)) => {
            log::error!("[CONSENT] approval gate poisoned, rejecting");
            return Decision::Unavailable;
        }
    };

    CANCELLED.store(false, Ordering::Release);
    let decision = match unsafe { run_dialog(&peer) } {
        Ok(d) => d,
        Err(e) => {
            log::error!("[CONSENT] cannot show approval dialog: {}", e);
            Decision::Unavailable
        }
    };
    log::info!(
        "[CONSENT] decision={:?} id={} addr={} file_transfer={}",
        decision,
        peer.id,
        peer.addr,
        peer.file_transfer
    );
    decision
}

// ── 窗口实现 ──

const CLASS_NAME: &str = "RustDeskSOS_Consent";
const WINDOW_TITLE: &str = "RustDesk SOS - 远程控制请求";
const TIMER_ID: usize = 1;
/// 客户区尺寸（96dpi 下的逻辑值）。按钮/文本按这个坐标系布局，实际像素由
/// `scale()` 按窗口 DPI 换算；窗口外框由 `AdjustWindowRectEx` 反推，因此标题栏
/// 高度变化不会挤坏布局。
const CLIENT_W: i32 = 480;
const CLIENT_H: i32 = 300;

/// 内边距（逻辑像素）。
const PAD: i32 = 20;
/// 信息行的行高。
const ROW_H: i32 = 24;
/// 标签列宽。
const LABEL_W: i32 = 84;
/// 按钮尺寸。
const BTN_W: i32 = 96;
const BTN_H: i32 = 32;

/// 控件 ID：用来在 `WM_CTLCOLORSTATIC` 里区分「灰色说明文字」与「黑色取值」。
const ID_LABEL_BASE: u32 = 2000;
const ID_VALUE_BASE: u32 = 3000;
const ID_HINT: u32 = 4000;
/// 静态文本控件里 `SS_ETCHEDHORZ`（画一条横线）的值。
const SS_ETCHEDHORZ: u32 = 0x0010;

/// 系统 DPI（每英寸像素数）。进程是「系统 DPI 感知」的，窗口不会被逐显示器缩放，
/// 所以这里取屏幕 DC 的 LOGPIXELSX 与窗口实际 DPI 一致。
///
/// 不用 `GetDpiForSystem`：它在本项目锁定的 windows crate 里要求 `Win32_UI_HiDpi`
/// feature，为一个小功能开整块 API 面不划算。
fn system_dpi() -> u32 {
    unsafe {
        let dpi = match GetDC(None) {
            dc if dc.is_invalid() => 0,
            dc => {
                let d = GetDeviceCaps(Some(dc), LOGPIXELSX);
                ReleaseDC(None, dc);
                d
            }
        };
        if dpi > 0 {
            dpi as u32
        } else {
            96
        }
    }
}

/// 96dpi 逻辑像素 → 实际像素。
fn scale(v: i32, dpi: u32) -> i32 {
    (v as i64 * dpi as i64 / 96) as i32
}

struct DialogState {
    decided: Option<Decision>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 弹窗里的「标签 / 取值」两列信息（纯函数，单测直接打这里）。
///
/// 空字段整行省略：旧客户端可能不带名称 / 平台，显示空值只会让人以为哪里没填。
pub fn info_rows(peer: &PeerHint) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if !peer.id.is_empty() {
        rows.push(("请求方 ID", peer.id.clone()));
    }
    if !peer.name.is_empty() {
        rows.push(("名称", peer.name.clone()));
    }
    if !peer.platform.is_empty() {
        rows.push(("平台", peer.platform.clone()));
    }
    rows.push(("来源地址", peer.addr.clone()));
    rows.push((
        "请求类型",
        if peer.file_transfer {
            "文件传输"
        } else {
            "远程桌面控制"
        }
        .to_string(),
    ));
    rows
}

/// 底部的说明文字（纯函数，单测直接打这里）。
pub fn compose_hint() -> String {
    format!(
        "点击「同意」后，对方可以查看并操作本机屏幕、键盘与鼠标。\n{} 秒内无人操作将自动拒绝。",
        CONSENT_TIMEOUT.as_secs()
    )
}

/// 创建窗口、跑消息循环、返回用户决定。
///
/// # Safety
///
/// 必须在有消息循环能力的线程上调用；窗口的所有 Win32 调用（创建、定时器、
/// 销毁）都发生在同一个线程内。
unsafe fn run_dialog(peer: &PeerHint) -> ResultType<Decision> {
    let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();
    let class = wide(CLASS_NAME);
    let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();

    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(consent_window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        // 复用 exe 里嵌入的图标（与托盘同一份资源，id = 1）
        hIcon: LoadIconW(Some(hinstance), PCWSTR(1 as *const u16)).unwrap_or_default(),
        hCursor: cursor,
        // 背景只用到系统色，不引入额外画刷资源
        hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as isize as *mut core::ffi::c_void),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class.as_ptr()),
    };
    if RegisterClassW(&wc) == 0 {
        let err = GetLastError();
        // 同一进程内第二次弹窗时类已存在，属正常情况
        if err != ERROR_CLASS_ALREADY_EXISTS {
            anyhow::bail!("RegisterClassW failed: {:?}", err);
        }
    }

    // 全程使用屏幕 DPI：进程是「系统 DPI 感知」的，窗口不会被逐显示器缩放，
    // 所以这里取系统 DPI 与后面实际创建窗口时一致。
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

    // 居中到主屏；系统缩放下 GetSystemMetrics 已是缩放后的坐标，与窗口一致
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

    let state = Box::into_raw(Box::new(DialogState { decided: None }));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    // 发布给 `cancel_pending`（控制端断开时用它收窗）
    CURRENT_HWND.store(hwnd.0 as isize, Ordering::Release);

    let font = body_font();
    let font_title = title_font(font);
    let result = populate(hwnd, hinstance, peer, dpi, font, font_title).and_then(|()| {
        let _ = ShowWindow(hwnd, SW_SHOW);
        // 尽量把窗口提到前台：后台进程抢占前台会被系统拒绝，此时用任务栏闪烁
        // 提示用户。两者都失败也不致命 —— 用户看任务栏仍能找到窗口。
        let _ = SetForegroundWindow(hwnd);
        // 只调层级，**不动位置**：漏掉 SWP_NOMOVE 会把刚算好的居中坐标改成 (0,0)
        // （实测窗口跑到左上角）。
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOSIZE | SWP_NOMOVE | SWP_SHOWWINDOW,
        );
        let flash = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: FLASHW_ALL,
            uCount: 5,
            dwTimeout: 0,
        };
        let _ = FlashWindowEx(&flash);
        SetTimer(
            Some(hwnd),
            TIMER_ID,
            CONSENT_TIMEOUT.as_millis() as u32,
            None,
        );
        pump_messages(hwnd);
        Ok(())
    });

    // 字体是 GDI 对象，必须在消息循环结束后释放（窗口本身不持有它们的生命周期）
    if font_title.0 != font.0 {
        let _ = DeleteObject(HGDIOBJ(font_title.0));
    }
    let _ = DeleteObject(HGDIOBJ(font.0));

    // 无论成功失败都要回收：窗口可能已销毁，这里只清理自有内存
    CURRENT_HWND.store(0, Ordering::Release);
    let decision = (*state).decided;
    drop(Box::from_raw(state));
    if let Err(e) = result {
        let _ = DestroyWindow(hwnd);
        return Err(e);
    }
    // 没有决定就退出消息循环（理论上不会发生）时按不可用处理，**不**放行
    Ok(decision.unwrap_or(Decision::Unavailable))
}

/// 正文/按钮字体：取系统「消息框」字体（现代系统上是 Segoe UI 9pt）。
///
/// 用 `DEFAULT_GUI_FONT` 是弹窗「看着很旧」的主因：那是 8pt 的 MS Shell Dlg。
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
    if font.is_invalid() {
        stock()
    } else {
        font
    }
}

/// 标题字体：在正文字体上「加粗 + 放大 4/3」，比自己指定字号更贴合系统设置。
unsafe fn title_font(base: HFONT) -> HFONT {
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
    lf.lfHeight = lf.lfHeight * 4 / 3;
    let font = CreateFontIndirectW(&lf);
    if font.is_invalid() {
        base
    } else {
        font
    }
}

/// 创建子控件、设置字体并完成布局（尺寸都按 DPI 换算）。
unsafe fn populate(
    hwnd: HWND,
    hinstance: HINSTANCE,
    peer: &PeerHint,
    dpi: u32,
    font: HFONT,
    font_title: HFONT,
) -> ResultType<()> {
    let s = |v: i32| scale(v, dpi);

    let title = create_child(
        hwnd,
        hinstance,
        w!("STATIC"),
        "有人请求远程控制本机",
        (WS_CHILD | WS_VISIBLE).0,
        None,
        s(PAD),
        s(PAD),
        s(CLIENT_W - PAD * 2),
        s(28),
    )?;

    // 标题与内容之间的分隔线（SS_ETCHEDHORZ 自带一条 1px 线，不用自绘）
    let _sep = create_child(
        hwnd,
        hinstance,
        w!("STATIC"),
        "",
        (WS_CHILD | WS_VISIBLE | WINDOW_STYLE(SS_ETCHEDHORZ)).0,
        None,
        s(PAD),
        s(PAD + 36),
        s(CLIENT_W - PAD * 2),
        s(2),
    )?;

    let mut children: Vec<(HWND, HFONT)> = vec![(title, font_title)];

    // 「标签 / 取值」两列：比一整块「字段：值」更容易扫读
    let rows = info_rows(peer);
    let top = s(PAD + 48);
    for (i, (label, value)) in rows.iter().enumerate() {
        let y = top + s(ROW_H) * i as i32;
        let l = create_child(
            hwnd,
            hinstance,
            w!("STATIC"),
            label,
            (WS_CHILD | WS_VISIBLE).0,
            Some(ID_LABEL_BASE + i as u32),
            s(PAD),
            y,
            s(LABEL_W),
            s(ROW_H - 4),
        )?;
        let v = create_child(
            hwnd,
            hinstance,
            w!("STATIC"),
            value,
            (WS_CHILD | WS_VISIBLE).0,
            Some(ID_VALUE_BASE + i as u32),
            s(PAD + LABEL_W + 8),
            y,
            s(CLIENT_W - PAD * 2 - LABEL_W - 8),
            s(ROW_H - 4),
        )?;
        children.push((l, font));
        children.push((v, font));
    }

    // 底部说明：放在信息行与按钮之间，灰色小字（颜色见 WM_CTLCOLORSTATIC）
    let hint_y = top + s(ROW_H) * rows.len() as i32 + s(8);
    let btn_y = s(CLIENT_H - PAD - BTN_H);
    let hint_h = (btn_y - s(8) - hint_y).max(s(30));
    let hint = create_child(
        hwnd,
        hinstance,
        w!("STATIC"),
        &compose_hint(),
        (WS_CHILD | WS_VISIBLE).0,
        Some(ID_HINT),
        s(PAD),
        hint_y,
        s(CLIENT_W - PAD * 2),
        hint_h,
    )?;
    children.push((hint, font));

    let button_style = |extra: i32| {
        WINDOW_STYLE((WS_CHILD | WS_VISIBLE | WS_TABSTOP).0 | u32::try_from(extra).unwrap_or(0))
    };
    // 用 IDOK / IDCANCEL 作控件 ID，`IsDialogMessageW` 才会把 Enter / Esc 映射成
    // 「同意」/「拒绝」
    let accept_x = s(CLIENT_W - PAD - BTN_W);
    let reject_x = accept_x - s(12) - s(BTN_W);
    let reject = create_child(
        hwnd,
        hinstance,
        w!("BUTTON"),
        "拒绝",
        button_style(BS_PUSHBUTTON).0,
        Some(IDCANCEL.0 as u32),
        reject_x,
        btn_y,
        s(BTN_W),
        s(BTN_H),
    )?;
    let accept = create_child(
        hwnd,
        hinstance,
        w!("BUTTON"),
        "同意",
        button_style(BS_DEFPUSHBUTTON).0,
        Some(IDOK.0 as u32),
        accept_x,
        btn_y,
        s(BTN_W),
        s(BTN_H),
    )?;
    children.push((reject, font));
    children.push((accept, font));

    for (child, f) in children {
        SendMessageW(
            child,
            WM_SETFONT,
            Some(WPARAM(f.0 as usize)),
            Some(LPARAM(1)),
        );
    }
    Ok(())
}

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
) -> ResultType<HWND> {
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

/// 标准消息泵。`IsDialogMessageW` 负责 Tab 切换焦点、Enter 触发默认按钮、
/// Esc 触发 `IDCANCEL`。
unsafe fn pump_messages(hwnd: HWND) {
    let mut msg = MSG::default();
    loop {
        let ret = GetMessageW(&mut msg, None, 0, 0);
        // 0 = WM_QUIT，-1 = 错误；两者都退出（错误时由调用方按未决定处理）
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

unsafe extern "system" fn consent_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as u32;
            if id == IDOK.0 as u32 {
                resolve(hwnd, Decision::Accepted);
                return LRESULT(0);
            }
            if id == IDCANCEL.0 as u32 {
                resolve(hwnd, Decision::Rejected);
                return LRESULT(0);
            }
        }
        WM_TIMER => {
            resolve(hwnd, Decision::TimedOut);
            return LRESULT(0);
        }
        // 关窗口（标题栏 X、Alt+F4）等价于拒绝
        WM_CLOSE => {
            resolve(hwnd, Decision::Rejected);
            return LRESULT(0);
        }
        // 0x0138 == WM_CTLCOLORSTATIC：标签与底部说明用系统灰，取值用正文色。
        // 静态控件默认不会继承父窗口的前景色，不接管这条消息就全是黑的。
        0x0138 => {
            let hdc = HDC(lparam.0 as *mut core::ffi::c_void);
            let hctrl = HWND(wparam.0 as *mut core::ffi::c_void);
            let id = GetDlgCtrlID(hctrl) as u32;
            let gray = id == ID_HINT || (ID_LABEL_BASE..ID_LABEL_BASE + 16).contains(&id);
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(
                hdc,
                COLORREF(GetSysColor(if gray { COLOR_GRAYTEXT } else { COLOR_WINDOWTEXT })),
            );
            return LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize);
        }
        WM_DESTROY => {
            KillTimer(Some(hwnd), TIMER_ID).ok();
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 记下决定并销毁窗口（首个决定生效，后续事件忽略）
unsafe fn resolve(hwnd: HWND, decision: Decision) {
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DialogState;
    if !state.is_null() && (*state).decided.is_none() {
    // 取消走的是和「拒绝」同一个 IDCANCEL，靠标记区分语义
    let decision = if decision == Decision::Rejected && CANCELLED.load(Ordering::Acquire) {
        Decision::Cancelled
    } else {
        decision
    };
        (*state).decided = Some(decision);
    }
    let _ = DestroyWindow(hwnd);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> PeerHint {
        PeerHint {
            id: "123456789".into(),
            name: "alice".into(),
            platform: "Windows".into(),
            addr: "10.0.0.5:51234".into(),
            file_transfer: false,
        }
    }

    #[test]
    fn rows_list_peer_identity_and_request_kind() {
        let rows = info_rows(&peer());
        let flat = format!("{:?}", rows);
        assert!(flat.contains("123456789"), "{}", flat);
        assert!(flat.contains("10.0.0.5:51234"), "{}", flat);
        assert!(flat.contains("远程桌面控制"), "{}", flat);
    }

    #[test]
    fn rows_mark_file_transfer_and_skip_empty_fields() {
        let mut p = peer();
        p.file_transfer = true;
        p.id = String::new();
        p.name = String::new();
        p.platform = String::new();
        let rows = info_rows(&p);
        let flat = format!("{:?}", rows);
        assert!(flat.contains("文件传输"), "{}", flat);
        assert!(!flat.contains("请求方 ID"), "{}", flat);
        assert!(!flat.contains("名称"), "{}", flat);
        assert!(!flat.contains("平台"), "{}", flat);
    }

    #[test]
    fn hint_states_the_timeout() {
        let hint = compose_hint();
        assert!(
            hint.contains(&format!("{} 秒", CONSENT_TIMEOUT.as_secs())),
            "{}",
            hint
        );
    }

    /// 只有「同意」算放行。`Unavailable` 也不放行，但调用方会把它当成「没弹窗」
    /// 退回只认密码，而不是拒绝连接（见 `crate::net::connection::Approval`）。
    #[test]
    fn only_accepted_lets_the_connection_in() {
        assert!(Decision::Accepted.is_accepted());
        for d in [
            Decision::Rejected,
            Decision::Cancelled,
            Decision::TimedOut,
            Decision::Busy,
            Decision::Unavailable,
        ] {
            assert!(!d.is_accepted(), "{:?}", d);
        }
    }
}
