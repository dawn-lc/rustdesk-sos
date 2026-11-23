//! 控制端编码能力与当前编码格式描述
//!
//! 从 `video` 拆出（域根本身仍持有采集→编码→发送循环与 `CaptureState`）。
//! 父模块 `pub use` 本模块的公开项，所以 `crate::video::xxx` 的调用点不用改。

use super::*;
use std::sync::Mutex;
use crate::video::vpx::{SosCodec, SosVpxConfig, CodecPrefer};

// ── 控制端编码能力 ──

/// (vp8 可用, vp9 可用, h264 可用, 偏好)
pub static PEER_ABILITY: Mutex<(i32, i32, i32, i32)> = Mutex::new((0, 0, 0, 0));

/// 标记硬件编码器不可用（会话级降级，直到进程重启）。
///
/// 初始化失败时调用：之后 `current_codec_for` 不再选 H.264，避免每条管线都去
/// 撞一次初始化失败。
pub fn set_hw_unavailable() {
    crate::video::hwenc::mark_unavailable();
}

/// 当前能否真正使用 H.264 硬件编码。
///
/// 结论来自启动探测（见 `crate::video::hwenc::probe_async`），**探测完成前恒为 false** ——
/// 宁可先用软件编码，也不要赌一个还没验证过的硬件路径。
pub fn hw_available() -> bool {
    crate::video::hwenc::available()
}

/// 记录控制端声明的解码能力与**编码偏好**。
///
/// 编码格式真的发生变化时会请求重建编码器 —— `vpx_codec_ctx_t` 绑定具体 codec，
/// 无法原地切换。这也是之前切 VP8 无效的另一个原因：就算选对了格式，
/// 也不会重建，最后还是继续推 VP9 的码流。
pub fn set_peer_codec_ability(vp8: bool, vp9: bool, h264: bool, prefer: CodecPrefer) {
    let new_state = (vp8 as i32, vp9 as i32, h264 as i32, prefer.to_i32());
    let prev_prefer = {
        let guard = PEER_ABILITY.lock().unwrap();
        if *guard == new_state {
            // 控制端的每个 `Misc::Option` 都带 supported_decoding，
            // 值没变时必须直接返回，否则每次都会重建一次编码器。
            return;
        }
        CodecPrefer::from_i32(guard.3)
    };
    let before = current_codec();
    *PEER_ABILITY.lock().unwrap() = new_state;
    let after = current_codec();
    log::info!(
        "[VIDEO] peer codec: vp8={} vp9={} h264={} prefer={:?} -> encoder {:?}",
        vp8,
        vp9,
        h264,
        prefer,
        after
    );
    // 偏好在 `Auto` 与显式值之间切换时，按**旧偏好**测出来的降级结论不再适用。
    //
    // 典型死档：某次负载高峰把某屏推到 `enc_level = 2`（VP8 试验）之后，
    // 用户显式选了 VP9。`next_enc_level(2)` 返回 `None`，既换不回 VP9，
    // 也不会再降 cpu-used，永久停在既非用户选择、也不自适应的状态。
    let prefer_changed = prev_prefer != prefer;
    if prefer_changed {
        reset_all_enc_levels();
    }
    if before != after {
        log::info!(
            "[VIDEO] codec changed {:?} -> {:?}, rebuilding encoder",
            before,
            after
        );
        request_refresh();
    } else if prefer_changed {
        // 格式没变，但降级档位被清掉了：必须重建一次才能把 cpu-used 复位，
        // 否则新偏好会沿用上一个偏好时的编码配置。
        log::info!(
            "[VIDEO] codec preference {:?} -> {:?}, rebuilding at default level",
            prev_prefer,
            prefer
        );
        request_refresh();
    }
}

pub fn peer_ability() -> (bool, bool, bool, CodecPrefer) {
    let (vp8, vp9, h264, prefer) = *PEER_ABILITY.lock().unwrap();
    let prefer = CodecPrefer::from_i32(prefer);
    if vp8 == 0 && vp9 == 0 && h264 == 0 {
        // 未声明时按 VP8/VP9 都支持处理（RustDesk 客户端两者都支持）；
        // H.264 不在此列 —— 老客户端可能真的解不了，不能替它假设。
        (true, true, false, prefer)
    } else {
        (vp8 > 0, vp9 > 0, h264 > 0, prefer)
    }
}
/// 依据控制端能力/偏好与当前环境选择编码格式。
///
/// 决策顺序见 [`crate::video::vpx::choose_codec`]：能力约束 > 显式偏好 > `Auto`。
///
/// `Auto` **不做任何基于线程数/分辨率的猜测**：实测表明 VP8 相对 VP9 的
/// 速度关系会随内容反转（同一内容 3 线程下慢 40%，另一种内容下快数倍），
/// 任何静态启发式都无法成立，而画质方向是确定的（VP8 同码率量化器更高）。
///
/// 唯一的例外是**实测出来的**编码成本降级（见 [`ENC_LEVEL`]）：默认配置
/// 在此机器上编不进帧预算时，才逐级降档，且 VP8 那一档要经过实测对比
/// 决定去留（见 [`revert_vp8_if_useless`]）。
///
/// 编码格式是**全局**的（对端能力与偏好对所有屏相同），
/// 只有降级结论按屏独立 —— 所以这里取**任一屏**都还没降级时的默认格式，
/// 实际的每屏降级由 [`current_codec_for`] 处理。
pub fn current_codec() -> SosCodec {
    current_codec_for_impl(None)
}

/// 指定显示器的编码格式（考虑该屏的降级级别）
pub fn current_codec_for(slot: usize) -> SosCodec {
    current_codec_for_impl(Some(slot))
}

pub fn current_codec_for_impl(slot: Option<usize>) -> SosCodec {
    let (peer_vp8, peer_vp9, peer_h264, prefer) = peer_ability();
    // 硬件 H.264 的优先级最高：它对 CPU 几乎零成本，而这正是 SOS 最缺的资源。
    //
    // 条件缺一不可：对端能解（`ability_h264 > 0`）、本机有可用编码器、
    // 用户没有显式要求别的格式。这里**不看** `ENC_LEVEL` —— 那套阶梯是按
    // 「软件编码吃 CPU」标定的，对 GPU 编码没有意义（见 `video::hwenc` 的说明）。
    if peer_h264 && hw_available() && matches!(prefer, CodecPrefer::Auto | CodecPrefer::H264) {
        return SosCodec::H264;
    }
    // 用户点名要 H.264 但本机没有硬件编码器：退回软件规则，按 `Auto` 处理，
    // 否则 `codec_with_degradation` 会把 H264 当成未知值。
    let sw_prefer = if prefer == CodecPrefer::H264 {
        CodecPrefer::Auto
    } else {
        prefer
    };
    // 该屏的实测降级档位（仅在该屏真的在采集时有意义）
    let level = slot.map(enc_level).unwrap_or(0);
    // 规则本身在 [`codec_with_degradation`] 里，那里有详细的优先级说明，
    // 也有对应的回归测试（`tests/codec_prefer.rs`）。
    crate::video::vpx::codec_with_degradation(peer_vp8, peer_vp9, sw_prefer, level)
}

/// 供诊断日志使用：当前链路的编码格式与线程数
pub fn describe() -> String {
    format!(
        "codec={:?} threads={}",
        current_codec(),
        SosVpxConfig::recommended_threads()
    )
}

/// 当前链路实际使用的编码格式名（避免日志读取 `scrap` 的全局状态而误报）
pub fn codec_name() -> &'static str {
    match current_codec() {
        SosCodec::Vp8 => "VP8",
        SosCodec::Vp9 => "VP9",
        SosCodec::H264 => "H264(hw)",
    }
}
