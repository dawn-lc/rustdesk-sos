//! 显示器枚举、命名与切换消息
//!
//! 从 `video` 拆出（域根本身仍持有采集→编码→发送循环与 `CaptureState`）。
//! 父模块 `pub use` 本模块的公开项，所以 `crate::video::xxx` 的调用点不用改。

use super::*;
use base::message_proto::{message, Message, DisplayInfo};
use hbb_common::ResultType;
use std::sync::Mutex;

/// 按显示器设备名查询当前尺寸（与 [`crate::capture::display::size_by_name`] 同义）
pub fn display_size_by_name(name: &str) -> Option<(usize, usize)> {
    crate::capture::display::size_by_name(name)
}

// ── 显示器选择 ──
//
// 显示器是**每个连接独立选择**的：订阅表里记录每个连接看哪块屏
// （`Subscriber.display`），QoS 里按显示器名聚合并行采集所需的帧率与码率。
// 所以这里不再有「当前显示器」这样的全局单值 —— 多个连接可以看不同的屏，
// 每块屏都有独立的采集管线。

/// 枚举所有显示器，返回 `(索引, 宽, 高, 设备名, 原点x, 原点y)`。
///
/// `Display::all()` 开销较大（`CreateDXGIFactory1` + 枚举输出），
/// 只能在重建/切换显示器时调用，**不能**放进每秒的检查里。
pub fn list_displays() -> ResultType<Vec<(usize, usize, usize, String, i32, i32)>> {
    let list =
        crate::capture::display::list().map_err(|e| anyhow::anyhow!("enumerate displays: {}", e))?;
    Ok(list
        .into_iter()
        .map(|d| (d.index, d.width, d.height, d.name, d.origin.0, d.origin.1))
        .collect())
}

/// 校验显示器索引是否可用，并返回该屏的 `(宽, 高, 设备名)`。
///
/// 切换是低频操作，允许付一次枚举开销来做校验。
pub fn display_info(idx: usize) -> Option<(usize, usize, String)> {
    match list_displays() {
        Ok(list) => match list.get(idx) {
            Some((_, w, h, name, _, _)) => Some((*w, *h, name.clone())),
            None => {
                log::warn!(
                    "[VIDEO] display #{} unavailable: only {} display(s) present",
                    idx,
                    list.len()
                );
                None
            }
        },
        Err(e) => {
            log::warn!("[VIDEO] enumerate displays failed: {}", e);
            None
        }
    }
}

// ── 显示器真实设备名缓存 ──
//
// 为什么需要它：连接可能建立在**锁屏期间**（`Display::all()` 返回空表），
// 此时 `display_info()` 拿不到名字，`Session` 只能记下空串；解锁后采集线程
// 才拿到真实名（`\\.\DISPLAY1`）。两边不一致会让 QoS 按屏聚合彻底失配：
// 采集线程用真实名查 `fps()`/`bitrate_kbps()` 查不到任何观看者，于是帧率
// 恒为默认值、码率上报为 0。
//
// 这里由采集线程写入真实名，`Session` 侧读取，保证两边用同一个 key。
// 读失败/未设置时由调用方回退到自己缓存的名字。

pub static DISPLAY_NAMES: [Mutex<String>; MAX_DISPLAY_SLOTS] =
    [const { Mutex::new(String::new()) }; MAX_DISPLAY_SLOTS];

/// 记录该屏的真实设备名（由采集线程在捕获器建成后调用）。
pub fn set_display_name(display: usize, name: &str) {
    if name.is_empty() {
        return;
    }
    let slot = display.min(MAX_DISPLAY_SLOTS - 1);
    if let Ok(mut g) = DISPLAY_NAMES[slot].lock() {
        if g.as_str() != name {
            g.clear();
            g.push_str(name);
            log::info!("[VIDEO] display #{} name cached as '{}'", display, name);
        }
    }
}

/// 该屏当前的真实设备名；从未成功采集过时返回 `None`。
///
/// 不在这里回退到 `display_info()`：那会调 `Display::all()`（数十毫秒），
/// 而本函数在每秒的 QoS 同步路径上被调用。
pub fn cached_display_name(display: usize) -> Option<String> {
    let slot = display.min(MAX_DISPLAY_SLOTS - 1);
    match DISPLAY_NAMES[slot].lock() {
        Ok(g) if !g.is_empty() => Some(g.clone()),
        _ => None,
    }
}

pub fn make_switch_display_msg(
    index: usize,
    width: usize,
    height: usize,
    display_name: &str,
    origin: (i32, i32),
) -> Option<Message> {
    let mut info = DisplayInfo::new();
    // 多显示器下原点不是 (0,0)。控制端靠它把鼠标坐标映射回正确的屏幕。
    info.x = origin.0;
    info.y = origin.1;
    info.width = width as i32;
    info.height = height as i32;
    info.name = display_name.to_string();
    info.online = true;
    // Windows 上光标由 mouse_cursor 服务单独发送，与上游 is_cursor_embedded() 一致
    info.cursor_embedded = false;
    let mut msg = librustdesk::video_service::make_display_changed_msg(
        // 这个索引就是 `SwitchDisplay.display`，控制端靠它把画面归到某块屏。
        // 之前硬编码 0，切到副屏后控制端仍以为是 0 号屏。
        index,
        Some(info),
        librustdesk::video_service::VideoSource::Monitor,
    )?;
    // make_display_changed_msg 可能附带 SwitchDisplay，这里补上光标标志
    if let Some(message::Union::Misc(ref mut misc)) = msg.union {
        if let Some(base::message_proto::misc::Union::SwitchDisplay(ref mut sd)) = misc.union {
            sd.cursor_embedded = false;
        }
    }
    Some(msg)
}

// ── 采集后端选择 ──

/// 当前进程是否运行在**远程桌面会话**里（`SM_REMOTESESSION`）。
///
/// 这个状态解释了一个最常见的困惑：同一台机器，"本地登录时 `cap=5ms`、
/// 通过 RDP 连进去后 `cap=50-80ms`"。原因是 **RDP 会话里 Windows 不提供
/// Desktop Duplication**（虚拟显示输出没有真实 GPU 输出），
/// `DuplicateOutput` 恒定返回 `E_UNEXPECTED 0x8000FFFF`，
/// 于是只能退回 GDI 的 BitBlt —— 单次全屏拷贝 50-80ms 是物理下限。
///
/// 这不是缺陷，也没有软件层的绕过办法：RDP 客户端的画面本身就是
/// RDP 协议自己编码的，我们拿到的是二次捕获。要高速率必须在
/// **物理控制台会话**里运行（无人值守远程协助的常态）。
#[cfg(windows)]
pub fn is_remote_session() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}
