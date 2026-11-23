//! 编码成本自适应与采集档位降级
//!
//! 从 `video` 拆出（域根本身仍持有采集→编码→发送循环与 `CaptureState`）。
//! 父模块 `pub use` 本模块的公开项，所以 `crate::video::xxx` 的调用点不用改。

use super::*;
use std::sync::atomic::{Ordering, AtomicBool, AtomicU32, AtomicU8, AtomicUsize};
use std::time::Duration;
use crate::video::vpx::CodecPrefer;

// ── 编码成本自适应 ──

/// 连续多少个统计窗口「编码超预算」才降级。
/// 3 个窗口 = 15s：拖窗/播视频的瞬时高峰不会误判，真的编不动时也能及时自救。
pub const ENC_OVERLOAD_WINDOWS: u32 = 3;

/// 该显示器的采集帧间隔（受 QoS 控制，但不低于 [`MIN_CAPTURE_FPS`]）
pub fn capture_spf(display: &str) -> Duration {
    let qos_spf = crate::video::qos::spf(display);
    let floor = Duration::from_millis(1000 / MIN_CAPTURE_FPS as u64);
    qos_spf.min(floor)
}

/// 每个显示器当前允许的最低档，运行时可上调回 [`TIER_DXGI`]。
///
/// 必须按屏独立：一块屏的 DXGI 不可用（虚拟显示器、RDP 输出）不代表
/// 另一块主屏也不可用。
pub static MIN_TIER: [AtomicUsize; MAX_DISPLAY_SLOTS] =
    [const { AtomicUsize::new(TIER_DXGI) }; MAX_DISPLAY_SLOTS];

pub fn min_tier(display: usize) -> usize {
    MIN_TIER[display.min(MAX_DISPLAY_SLOTS - 1)].load(Ordering::Relaxed)
}

pub fn set_min_tier(display: usize, tier: usize) {
    MIN_TIER[display.min(MAX_DISPLAY_SLOTS - 1)].store(tier, Ordering::Relaxed);
}

/// 判断重建失败是否属于「桌面暂时不可捕获」这类**可自愈**的环境状态。
///
/// 这类错误不该以 ERROR 级别刷屏，也不该触发降档 —— 换个采集后端同样拿不到显示器。
///
/// 触发场景（实测）：会话锁屏、切到 UAC 安全桌面、RDP 断开、
/// 显示器正在重枚举。此时 `Display::all()` 同时失去 GDI（`EnumDisplayDevices`
/// 拿不到输出）与 DXGI 两条路径，返回空表。恢复后自行可用。
///
/// `SOS SHMEM not registered` 也归入此类：它只可能在「main 已判定安全桌面、
/// 但首帧尚未到达」的竞态窗口里出现，属于过渡态而非故障。正常路径下
/// 状态由 main 统一推进，这条分支基本不会走到，但绝不能让它刷 ERROR。
pub fn is_no_display_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("no display") || msg.contains("SHMEM not registered")
}

// ── 编码成本自适应 ──

/// 编码成本降级级别：0 = 默认配置；1 = 用最快的 `cpu-used`；2 = 改用 VP8。
///
/// 为什么需要它：控制端的**显式偏好**（`prefer=Vp9`，即画质选「最佳」）
/// 优先级高于本机启发式，这在强 CPU 上完全正确，但在低功耗 4 核机器上
/// 会选出编不动的配置。实测（真实机器，1600x900，DXGI 可用）：
///
/// ```text
/// SosVpxEncoder: Vp9 1600x900 threads=2 cpu_used=7 bitrate=1163kbps
/// [VIDEO:PERF] loop=19.6fps it=51.1ms cap=5.0ms conv=2.0ms enc=43.5ms spf=33ms skip=0
/// ```
///
/// 采集只要 5ms、转换 2ms，编码却要 43.5ms —— 超过 30fps 的 33ms 预算，
/// 循环被钉在 19.6fps。此时 VP9 相对 VP8 的画质优势完全被掉帧抵消，
/// 用户看到的是「更清晰但更卡」，而远程桌面里卡顿的代价远大于编码格式。
///
/// 判据只采用**实测编码耗时**，不含任何机器型号/核数硬编码：
/// 耗时超过整帧预算即证明这一档配置在本机不可行，逐级退到能实时编码为止。
///
/// 按显示器独立：两块屏可以有不同的分辨率和不同的 GPU，必须分别判定。
pub static ENC_LEVEL: [AtomicU8; MAX_DISPLAY_SLOTS] = [const { AtomicU8::new(0) }; MAX_DISPLAY_SLOTS];
/// 当前编码成本降级级别
pub fn enc_level(display: usize) -> u8 {
    ENC_LEVEL[display.min(MAX_DISPLAY_SLOTS - 1)].load(Ordering::Relaxed)
}

/// 换到 VP8 **之前**那一档的实测每帧编码耗时（毫秒，`f32` 位模式）。
///
/// 换 VP8 是否值得只能靠对比：实测表明 VP8 并不总比 VP9 快（同一内容
/// 3 线程下反而慢 40%，换一种内容又可能快数倍，见
/// [`crate::video::vpx::choose_codec`] 的表格）。所以升级到 VP8 时把当时的
/// 耗时记下来，下一个窗口用 [`crate::video::vpx::vp8_trial_worth_keeping`]
/// 判断它到底有没有帮上忙。
pub static ENC_BASELINE_MS: [AtomicU32; MAX_DISPLAY_SLOTS] =
    [const { AtomicU32::new(0) }; MAX_DISPLAY_SLOTS];

/// 该屏已实测「换 VP8 不划算」，不再尝试。
/// 新控制端接入时由 [`reset_enc_level`] 清除（对端能力/负载可能变了）。
pub static VP8_REJECTED: [AtomicBool; MAX_DISPLAY_SLOTS] =
    [const { AtomicBool::new(false) }; MAX_DISPLAY_SLOTS];

pub fn enc_baseline_ms(display: usize) -> f64 {
    let bits = ENC_BASELINE_MS[display.min(MAX_DISPLAY_SLOTS - 1)].load(Ordering::Relaxed);
    f32::from_bits(bits) as f64
}

pub fn set_enc_baseline_ms(display: usize, ms: f64) {
    let v = if ms.is_finite() && ms > 0.0 {
        ms as f32
    } else {
        0.0
    };
    ENC_BASELINE_MS[display.min(MAX_DISPLAY_SLOTS - 1)].store(v.to_bits(), Ordering::Relaxed);
}

pub fn vp8_rejected(display: usize) -> bool {
    VP8_REJECTED[display.min(MAX_DISPLAY_SLOTS - 1)].load(Ordering::Relaxed)
}

/// 新控制端接入时复位该显示器的编码降级结论。返回是否真的变了。
///
/// 连「VP8 被否决」一起清掉：新对端可能有不同的解码能力，负载也可能变了，
/// 旧的实测结论不一定还成立。
pub fn reset_enc_level(display: usize) -> bool {
    let slot = display.min(MAX_DISPLAY_SLOTS - 1);
    let was_rejected = VP8_REJECTED[slot].swap(false, Ordering::Relaxed);
    ENC_BASELINE_MS[slot].store(0, Ordering::Relaxed);
    let changed = ENC_LEVEL[slot].swap(0, Ordering::Relaxed) != 0;
    if changed {
        log::info!("[VIDEO] encoder level reset for display #{}", display);
    }
    changed || was_rejected
}

/// 清掉所有显示器的实测降级结论。
///
/// 控制端**显式**改变编码偏好时调用：降级阶梯是针对某个具体格式标定的，
/// 换了格式（或从 `Auto` 变成显式）之后旧结论不再成立。
pub fn reset_all_enc_levels() {
    for d in 0..MAX_DISPLAY_SLOTS {
        reset_enc_level(d);
    }
}

/// 降一级编码成本。返回 true 表示配置确实变了，调用方需要重建编码器。
///
/// `enc_avg_ms` 是**当前档位**的实测每帧编码耗时，换到 VP8 时用它做基线。
pub fn step_down_encoder(display: usize, enc_avg_ms: f64) -> bool {
    let cur = enc_level(display);
    let (peer_vp8, _, _, prefer) = peer_ability();
    // 已实测「VP8 不划算」就不再试，直接停在当前档位。
    //
    // 显式偏好下也**不**做 VP8 试验：用户已经指定了格式，实测降级只应影响
    // cpu-used（第 1 级），不该悄悄把格式换掉。
    let allow_vp8 = peer_vp8 && prefer == CodecPrefer::Auto && !vp8_rejected(display);
    let Some(next) = crate::video::vpx::next_enc_level(cur, allow_vp8) else {
        return false;
    };
    if next == 2 {
        // 记下升级前的耗时，下一个窗口用它判断 VP8 是否真的更快。
        set_enc_baseline_ms(display, enc_avg_ms);
    }
    ENC_LEVEL[display.min(MAX_DISPLAY_SLOTS - 1)].store(next, Ordering::Relaxed);
    log::warn!(
        "[VIDEO] display #{} encoder cannot keep up with the frame budget, level {} ({})",
        display,
        next,
        if next == 1 {
            "fastest cpu-used".to_string()
        } else {
            format!("VP8 trial, baseline {:.1}ms", enc_avg_ms)
        }
    );
    true
}

/// 已在 VP8 上仍然超预算：用实测对比决定是保留还是回退。
///
/// 返回 true 表示已回退到默认档位，调用方需要重建编码器。
pub fn revert_vp8_if_useless(display: usize, enc_avg_ms: f64) -> bool {
    let baseline = enc_baseline_ms(display);
    if crate::video::vpx::vp8_trial_worth_keeping(baseline, enc_avg_ms) {
        // VP8 确实更快（或者基线数据无效）：保留。
        return false;
    }
    let slot = display.min(MAX_DISPLAY_SLOTS - 1);
    VP8_REJECTED[slot].store(true, Ordering::Relaxed);
    ENC_BASELINE_MS[slot].store(0, Ordering::Relaxed);
    ENC_LEVEL[slot].store(0, Ordering::Relaxed);
    log::warn!(
        "[VIDEO] display #{} VP8 is not faster here ({:.1}ms vs baseline {:.1}ms), \
         reverting to the default codec and keeping the quality",
        display,
        enc_avg_ms,
        baseline
    );
    true
}

// ── 安全桌面状态 ──
