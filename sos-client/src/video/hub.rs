//! 订阅表、广播与每显示器控制标志
//!
//! 从 `video` 拆出（域根本身仍持有采集→编码→发送循环与 `CaptureState`）。
//! 父模块 `pub use` 本模块的公开项，所以 `crate::video::xxx` 的调用点不用改。

use super::*;
use base::message_proto::Message;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{Ordering, AtomicU64};

// ── 订阅者（多连接 × 多显示器） ──
//
// 一个连接同时只看一块屏（与 RustDesk 协议一致），但**可以有多块屏
// 被同时采集**，也可以有多个控制端同时观看同一块屏。所以订阅表是
// `(conn_id, 显示器索引)` 的列表，而不是单个槽位。

#[derive(Clone)]
pub struct Subscriber {
    pub conn_id: i32,
    /// 该订阅者要看的显示器索引（`Display::all()` 的顺序）
    pub display: usize,
    pub tx: Sender,
    pub tx_video: VideoSender,
}

pub static SUBSCRIBERS: Mutex<Vec<Subscriber>> = Mutex::new(Vec::new());

/// 控制端登录成功后订阅指定显示器。
///
/// 同一 `conn_id` 重复订阅会替换旧条目（重连、切换显示器都走这条路径）。
/// 返回该显示器的管线是否为新启动。
pub fn subscribe(conn_id: i32, display: usize, tx: Sender, tx_video: VideoSender) -> bool {
    let first_subscriber = {
        let mut list = SUBSCRIBERS.lock().unwrap();
        let was_empty = list.is_empty();
        list.retain(|s| s.conn_id != conn_id);
        list.push(Subscriber {
            conn_id,
            display,
            tx,
            tx_video,
        });
        log::info!(
            "[VIDEO] subscriber #{} attached on display #{} ({} total)",
            conn_id,
            display,
            list.len()
        );
        was_empty
    };
    // 全新会话：上一个控制端测出的链路结论不能继承过来。
    // 它可能走的是完全不同的路径，而 Generous 档位意味着码率 ×2 且关掉
    // 丢帧保护，正好是「错判代价最大」的方向。迟滞会在 5 个探测样本
    // （约 5s）后重新给出结论。多控制端场景**不**重置：那时链路仍在使用中。
    if first_subscriber {
        with_link_tier(|t| t.reset());
        log::info!("[VIDEO] link tier reset for new session (constrained until measured)");
    }
    // 新控制端接入：上一轮因本机编码能力不足而做的降级不该继承过来。
    // 只影响**该显示器**的编码档位，其他屏的结论照旧。
    if reset_enc_level(display) {
        request_refresh_display(display);
    }
    ensure_pipeline(display)
}

/// 连接断开时取消订阅。返回该显示器是否已无人观看（管线会自行退出）。
pub fn unsubscribe(conn_id: i32) {
    let mut list = SUBSCRIBERS.lock().unwrap();
    let before = list.len();
    list.retain(|s| s.conn_id != conn_id);
    log::info!(
        "[VIDEO] subscriber #{} detached ({} -> {} total)",
        conn_id,
        before,
        list.len()
    );
}

/// 切换该连接观看的显示器。返回 `true` 表示需要启动新管线。
pub fn set_subscriber_display(conn_id: i32, display: usize) -> bool {
    let changed = {
        let mut list = SUBSCRIBERS.lock().unwrap();
        match list.iter_mut().find(|s| s.conn_id == conn_id) {
            Some(s) if s.display != display => {
                log::info!(
                    "[VIDEO] subscriber #{} switches display #{} -> #{}",
                    conn_id,
                    s.display,
                    display
                );
                s.display = display;
                true
            }
            _ => false,
        }
    };
    if changed {
        ensure_pipeline(display);
    }
    changed
}

/// 该显示器当前的订阅者快照
pub fn subscribers_for(display: usize) -> Vec<Subscriber> {
    SUBSCRIBERS
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.display == display)
        .cloned()
        .collect()
}

/// 向单个订阅者发送普通消息（非视频帧）
pub fn send_msg(sub: &Subscriber, msg: Message) {
    let _ = sub.tx.send((tokio::time::Instant::now(), Arc::new(msg)));
}

/// 向单个订阅者发送视频帧（背压点：通道满时阻塞）
pub fn send_video(sub: &Subscriber, msg: &Arc<Message>) -> bool {
    match sub
        .tx_video
        .blocking_send((tokio::time::Instant::now(), msg.clone()))
    {
        Ok(()) => true,
        Err(_) => {
            // 接收端已关闭（连接断开），交给主循环回收订阅
            false
        }
    }
}

/// 把一帧发给该显示器的**所有**订阅者。
///
/// 返回仍然存活的订阅数。多个控制端同时观看同一块屏时共用同一个编码器
/// 输出 —— 这正是我们需要的：一次采集、一次编码、N 份发送。
pub fn broadcast_video(subs: &[Subscriber], msg: &Arc<Message>) -> usize {
    let mut alive = 0;
    for s in subs {
        if send_video(s, msg) {
            alive += 1;
        }
    }
    alive
}

// ── 每显示器的控制标志 ──
//
// 刷新、关键帧、采集档位、编码档位都必须是**按显示器独立**的：
// 两块屏可能一块走 DXGI、一块走 GDI（不同 GPU / 不同会话），
// 一块屏的编码器编不动不代表另一块也编不动。
// 用位掩码而不是 HashMap：读写都在每帧路径上，且显示器数量天然有上限。

/// 请求重建捕获器与编码器
pub static REFRESH_MASK: AtomicU64 = AtomicU64::new(0);
/// 请求下一帧编为关键帧
pub static KEYFRAME_MASK: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn slot_bit(display: usize) -> u64 {
    1u64 << (display.min(MAX_DISPLAY_SLOTS - 1) as u64)
}

/// 请求重建**所有**显示器的捕获器与编码器（UAC 切换、编码格式变化等全局事件）。
pub fn request_refresh() {
    REFRESH_MASK.fetch_or(u64::MAX, Ordering::SeqCst);
}

/// 请求重建**指定**显示器的捕获器与编码器。
pub fn request_refresh_display(display: usize) {
    REFRESH_MASK.fetch_or(slot_bit(display), Ordering::SeqCst);
}

/// 取出并清除该显示器的重建请求
pub fn take_refresh(display: usize) -> bool {
    REFRESH_MASK.fetch_and(!slot_bit(display), Ordering::SeqCst) & slot_bit(display) != 0
}

/// 请求下一帧编为关键帧（不重建编码器）。
///
/// 用于两类场景：
/// - 控制端发来 `Misc::refresh_video`（它自己解码出错，等一个关键帧重同步）；
/// - 本端发送队列溢出丢掉了一个已编码的帧（参考链断了）。
///
/// `kf_mode = VPX_KF_DISABLED` 意味着**除了首帧再也不会自动出关键帧**，
/// 所以没有这条路径的话，一次丢帧就是永久花屏。
///
/// 请求对所有屏生效：`Misc::refresh_video` 不携带显示器索引，
/// 而多发一个关键帧的代价（约 9 倍帧大小）远小于对端一直花屏。
pub fn request_keyframe() {
    KEYFRAME_MASK.fetch_or(u64::MAX, Ordering::SeqCst);
}

/// 取出并清除该显示器的关键帧请求（保留逻辑：窗口内不丢，留给下一帧生效）
pub fn has_keyframe_request(display: usize) -> bool {
    KEYFRAME_MASK.load(Ordering::SeqCst) & slot_bit(display) != 0
}

/// 消费该显示器的关键帧请求
pub fn take_keyframe(display: usize) {
    KEYFRAME_MASK.fetch_and(!slot_bit(display), Ordering::SeqCst);
}
