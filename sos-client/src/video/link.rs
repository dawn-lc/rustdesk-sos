//! 链路档位与码率预算
//!
//! 从 `video` 拆出（域根本身仍持有采集→编码→发送循环与 `CaptureState`）。
//! 父模块 `pub use` 本模块的公开项，所以 `crate::video::xxx` 的调用点不用改。

use std::sync::Mutex;

// ── 链路延迟（由 `net::connection` 的 TestDelay 测量更新） ──
//
// ── 链路延迟（由 `net::connection` 的 TestDelay 测量更新） ──
//
// 档位初始为保守（见 [`crate::video::vpx::LinkTier::new`]）：还没测到任何样本时
// 守码率，而不是因为初值是 0 被误判成「链路极好」而立刻超发。

/// 链路档位的迟滞状态（见 [`crate::video::vpx::LinkTier`]）。
///
/// 必须用**状态机**而不是「本次样本是否充裕」：档位一翻就是码率 ×2 或 ÷2、
/// 丢帧开关同时切换，实测被单个迟到回显在编码器重建瞬间把 5208kbps 砍到
/// 2604kbps 并丢了 3 帧（见 `LinkTier` 的文档）。
pub static LINK_TIER: Mutex<crate::video::vpx::LinkTier> = Mutex::new(crate::video::vpx::LinkTier::new());

/// 加锁访问链路档位。锁内只做纯计算，不碰 IO、不嵌套加锁。
pub fn with_link_tier<R>(f: impl FnOnce(&mut crate::video::vpx::LinkTier) -> R) -> R {
    match LINK_TIER.lock() {
        Ok(mut guard) => f(&mut guard),
        // 中毒只可能来自本模块内部的 panic，状态本身仍是普通数据
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

/// 由 `net::connection` 每次 TestDelay 往返后调用**一次**（已经过 min-filter）。
///
/// 延迟与排队必须一起喂：拆成两个 setter 会让一次探测被记成两个样本，
/// 迟滞的「连续 N 次」就等价于 N/2 秒，去抖强度会悄悄减半。
///
/// `queue = raw - floor`：floor 是 min-filter 的 RTT 下界，
/// raw 是本次实际往返时间，差值就是视频帧在本机出口 / 路由缓冲里造成的等待。
pub fn note_link_sample(rtt: u32, queue: u32) {
    let changed = with_link_tier(|t| t.sample(Some(rtt), Some(queue)));
    if changed {
        let generous = link_is_generous();
        log::info!(
            "[VIDEO] link tier -> {} (rtt={}ms queue={}ms)",
            if generous { "generous" } else { "constrained" },
            rtt,
            queue
        );
    }
}

/// 链路是否充裕（RTT 低 **且** 没有排队），**已过滤抖动**
///
/// 单样本判据见 [`crate::video::vpx::link_is_generous`]；这里返回的是
/// [`crate::video::vpx::LinkTier`] 迟滞之后的档位，读写两侧看到的永远是稳定值。
pub fn link_is_generous() -> bool {
    with_link_tier(|t| t.generous())
}

/// 当前应使用的丢帧水位
///
/// 实测 1080p 高复杂度内容 @1675kbps（`tests/vpx_feed_rate.rs`）：
///
/// | dropframe_thresh | 出帧 | 丢帧率 | 实际码率 |
/// |---|---|---|---|
/// | 20（守码率） | 10/60 | 83% | 1853 kbps |
/// | 0（守帧数） | 60/60 | 0% | 3294 kbps |
///
/// 也就是说这是一个纯粹的带宽取舍：要么流畅度极差，要么码率翻倍。
/// 链路充裕时选后者；串行链路超发会养出 bufferbloat，必须守码率。
pub fn dropframe_thresh_for_link() -> u32 {
    if link_is_generous() {
        // 链路充裕：不再为了省带宽而丢帧
        0
    } else {
        // 链路未知或已出现排队：守码率，容忍丢帧
        crate::video::vpx::RC_DROPFRAME_THRESH
    }
}

/// 当前的码率预算档位（与丢帧水位共用同一个链路判据）
pub fn link_budget() -> crate::video::vpx::LinkBudget {
    if link_is_generous() {
        crate::video::vpx::LinkBudget::Generous
    } else {
        crate::video::vpx::LinkBudget::Constrained
    }
}
