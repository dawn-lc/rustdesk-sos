//! 编码格式选择的决策表。
//!
//! 生产问题一：控制端把编码切到 VP8，日志里 `codec` 一直是 VP9，切换完全无效。
//! 根因是 [`choose_codec`] 的前身只看了 `ability_vp8` / `ability_vp9`，
//! **完全忽略 `SupportedDecoding.prefer`**。而 RustDesk 客户端切换编码时
//! 只改 `prefer`（两个 ability 恒为 true），所以偏好被直接丢弃。
//!
//! 生产问题二：`Auto` 走「线程少 → VP8」的启发式，但实测**不支持**这个假设
//! （见 `tests/codec_cost.rs`：同码率桌面内容下，VP8 在 1 线程只快 5%~33%，
//! 2 线程起反而慢 21%~33%，且量化器高 14%~38%）。
//! 所以 `Auto` 现在直接选 VP9，「编不动」交给运行时实测判定。
//!
//! 运行：cargo test --release --test codec_prefer -- --nocapture

#![cfg(all(windows, sos_vpx))]

use rustdesk_sos::video::vpx as sos_vpx;

use sos_vpx::{choose_codec, codec_with_degradation, CodecPrefer, SosCodec};
use rustdesk_sos::video::pick_codec;

/// 最关键的一条：能力都支持时，`prefer` 必须生效。
#[test]
fn explicit_preference_wins_over_default() {
    assert_eq!(
        choose_codec(true, true, CodecPrefer::Vp8),
        SosCodec::Vp8,
        "用户显式选 VP8 时必须用 VP8，不能被默认值覆盖"
    );
    assert_eq!(
        choose_codec(true, true, CodecPrefer::Vp9),
        SosCodec::Vp9,
        "用户显式选 VP9 时必须用 VP9"
    );
}

/// `Auto` 与显式偏好可以给出不同结果 —— 若相同说明 `prefer` 又被忽略了。
#[test]
fn auto_and_explicit_can_differ() {
    let auto = choose_codec(true, true, CodecPrefer::Auto);
    let vp8 = choose_codec(true, true, CodecPrefer::Vp8);
    println!("Auto -> {:?}, 显式 VP8 -> {:?}", auto, vp8);
    assert_eq!(auto, SosCodec::Vp9);
    assert_eq!(vp8, SosCodec::Vp8);
    assert_ne!(auto, vp8, "若两者相同，说明 prefer 又被忽略了");
}

/// 能力约束优先于偏好：对端解不了还推过去会黑屏。
#[test]
fn capability_constraint_beats_preference() {
    // 对端只有 VP8，却希望 VP9
    assert_eq!(
        choose_codec(true, false, CodecPrefer::Vp9),
        SosCodec::Vp8,
        "对端不支持 VP9，即使偏好 VP9 也只能发 VP8"
    );
    assert_eq!(
        choose_codec(true, false, CodecPrefer::Auto),
        SosCodec::Vp8,
        "对端只支持 VP8 时 Auto 也必须选 VP8"
    );
    // 对端只有 VP9，却希望 VP8
    assert_eq!(
        choose_codec(false, true, CodecPrefer::Vp8),
        SosCodec::Vp9,
        "对端不支持 VP8，即使偏好 VP8 也只能发 VP9"
    );
}

/// `Auto` 必须是 VP9，且**不再接受任何按线程数/分辨率的启发式输入**。
///
/// 这个测试是「不要把线程数塞回来」的守卫：签名里已经不含
/// `threads` / `pixels`，编译期就保证了无法再按它们分支。
#[test]
fn auto_is_always_vp9() {
    assert_eq!(
        choose_codec(true, true, CodecPrefer::Auto),
        SosCodec::Vp9,
        "Auto 必须选 VP9：2 线程及以上 VP8 又慢 27%~127% 又糊 14%~38%"
    );
}

/// 偏好值的存取必须可逆（它被存进静态变量的 i32 槽位）。
#[test]
fn prefer_roundtrip() {
    for p in [
        CodecPrefer::Auto,
        CodecPrefer::Vp8,
        CodecPrefer::Vp9,
        CodecPrefer::H264,
        CodecPrefer::H265,
    ] {
        assert_eq!(CodecPrefer::from_i32(p.to_i32()), p);
    }
    // 未知值一律回落到 Auto
    assert_eq!(CodecPrefer::from_i32(-1), CodecPrefer::Auto);
    assert_eq!(CodecPrefer::from_i32(99), CodecPrefer::Auto);
    // 默认值是 Auto
    assert_eq!(CodecPrefer::default(), CodecPrefer::Auto);
}

// ── 实测降级与显式偏好的优先级 ──

/// 生产问题三：控制端切编码协议无效。
///
/// 现场日志（1600x900 / 4 核 / 3 线程）：
///
/// ```text
/// [INFO] encoder cannot keep up ... level 2 (VP8 trial, baseline 110.7ms)
/// [INFO] codec changed Vp8 -> Vp9, rebuilding encoder
/// [INFO] SosVpxEncoder: Vp8 1600x900 threads=3 cpu_used=12     ← 说好的 Vp9 呢？
/// ```
///
/// 根因：`enc_level >= 2`（VP8 试验档）无条件压过了控制端的显式偏好，
/// 而且该档位一旦置上就不会自行清除。
#[test]
fn explicit_preference_beats_measured_degradation() {
    // 降级档位再高，用户显式选的格式也必须生效
    for level in [0u8, 1, 2, 3, 255] {
        assert_eq!(
            codec_with_degradation(true, true, CodecPrefer::Vp9, level),
            SosCodec::Vp9,
            "enc_level={} 时显式偏好 VP9 仍必须生效，否则切换编码协议就是无效的",
            level
        );
        assert_eq!(
            codec_with_degradation(true, true, CodecPrefer::Vp8, level),
            SosCodec::Vp8,
            "enc_level={} 时显式偏好 VP8 也必须生效",
            level
        );
    }
}

/// 只有 `Auto` 才允许实测降级改格式 —— 那是「没表态」时的兜底。
#[test]
fn auto_yields_to_measured_degradation() {
    // 0/1 档不换格式：第 1 档只改 cpu-used
    for level in [0u8, 1] {
        assert_eq!(
            codec_with_degradation(true, true, CodecPrefer::Auto, level),
            SosCodec::Vp9,
            "enc_level={} 还没到换格式的档位",
            level
        );
    }
    // 2 档才换 VP8
    assert_eq!(
        codec_with_degradation(true, true, CodecPrefer::Auto, 2),
        SosCodec::Vp8,
        "Auto 下实测编不动时应当换到 VP8"
    );
    // 对端解不了 VP8 时不能换，否则对端黑屏
    assert_eq!(
        codec_with_degradation(false, true, CodecPrefer::Auto, 2),
        SosCodec::Vp9,
        "对端没有 VP8 解码能力时，降级不能把编码换成 VP8"
    );
}

/// 降级前后，`Auto` 与显式的差异必须可观察 —— 若三者恒等说明优先级又被写错了。
#[test]
fn degradation_only_affects_auto() {
    let auto_hi = codec_with_degradation(true, true, CodecPrefer::Auto, 2);
    let auto_lo = codec_with_degradation(true, true, CodecPrefer::Auto, 0);
    assert_ne!(auto_hi, auto_lo, "Auto 在两档之间必须给出不同结果");

    let explicit_hi = codec_with_degradation(true, true, CodecPrefer::Vp9, 2);
    let explicit_lo = codec_with_degradation(true, true, CodecPrefer::Vp9, 0);
    assert_eq!(explicit_hi, explicit_lo, "显式偏好不应随降级档位改变");
}

// ── 硬件编码的选码决策（H.264 / H.265）──

/// 硬件格式的优先级与上游一致：上游源码注释原话是 `auto: h265 > h264 > av1/vp9/vp8`。
///
/// 选 H.265 的理由不是「更新」，而是**同硬件同画质下省约 25-40% 带宽**，
/// 编码成本与 H.264 相同（都是 GPU）。所以只要对端能解，Auto 就该选它。
#[test]
fn auto_prefers_h265_over_h264() {
    // 两种硬件都有、对端都能解 → 选 H.265
    assert_eq!(
        pick_codec(true, true, true, true, true, true, CodecPrefer::Auto, 0),
        SosCodec::H265,
        "Auto 下 H.265 应优先于 H.264（上游同款规则）"
    );
    // 只有 H.264 → 回落 H.264
    assert_eq!(
        pick_codec(true, true, true, true, true, false, CodecPrefer::Auto, 0),
        SosCodec::H264,
        "没有 HEVC 编码器时必须用 H.264"
    );
    // 都没有 → 软件 VP9
    assert_eq!(
        pick_codec(true, true, true, true, false, false, CodecPrefer::Auto, 0),
        SosCodec::Vp9,
        "没有硬件编码器时必须回软件"
    );
}

/// 对端解不了 H.265 时**绝不能**发 H.265：那会让控制端黑屏/花屏。
#[test]
fn peer_without_h265_never_gets_h265() {
    // 本机有 HEVC 编码器，但对端 ability_h265 == 0
    assert_ne!(
        pick_codec(true, true, true, false, true, true, CodecPrefer::Auto, 0),
        SosCodec::H265,
        "对端没有 H.265 解码能力时不能选 H.265"
    );
    // 即使控制端明确 prefer=H265（协议允许它这么声明），能力为 0 时也不能从
    assert_ne!(
        pick_codec(true, true, true, false, true, true, CodecPrefer::H265, 0),
        SosCodec::H265,
        "prefer 与 ability 冲突时以 ability 为准"
    );
}

/// 显式偏好优先于 Auto 的默认顺序。
#[test]
fn explicit_hw_preference_wins() {
    // 两种都有，但用户点名 H.264 → 用 H.264，不能被 Auto 的 H.265 覆盖
    assert_eq!(
        pick_codec(true, true, true, true, true, true, CodecPrefer::H264, 0),
        SosCodec::H264,
        "用户显式选 H.264 时必须用 H.264"
    );
    // 用户点名 H.265 且可用 → H.265
    assert_eq!(
        pick_codec(true, true, true, true, true, true, CodecPrefer::H265, 0),
        SosCodec::H265
    );
}

/// 点名的硬件格式不可用时退回 Auto 规则，不能停在不可用的选择上。
#[test]
fn unavailable_hw_preference_falls_back() {
    // 要 H.265 但没有 HEVC 编码器 → 退到 H.264（Auto 里次优的硬件）
    assert_eq!(
        pick_codec(true, true, true, true, true, false, CodecPrefer::H265, 0),
        SosCodec::H264,
        "H.265 不可用时应退到 H.264，而不是直接掉软件编码"
    );
    // 要 H.264 但没有 H.264 编码器，而 HEVC 可用 → Auto 规则给 H.265
    assert_eq!(
        pick_codec(true, true, true, true, false, true, CodecPrefer::H264, 0),
        SosCodec::H265,
        "H.264 不可用时应退到可用的 H.265"
    );
    // 两种硬件都没有 → 软件
    assert_eq!(
        pick_codec(true, true, true, true, false, false, CodecPrefer::H265, 0),
        SosCodec::Vp9
    );
}

/// 软件偏好不受硬件可用性影响（原有行为不能被 H.265 改动碰到）。
#[test]
fn software_preference_ignores_hw() {
    for (h264, h265) in [(false, false), (true, false), (false, true), (true, true)] {
        assert_eq!(
            pick_codec(true, true, true, true, h264, h265, CodecPrefer::Vp8, 0),
            SosCodec::Vp8,
            "显式 VP8 不该被硬件编码器抢走（h264={h264} h265={h265}）"
        );
        assert_eq!(
            pick_codec(true, true, true, true, h264, h265, CodecPrefer::Vp9, 0),
            SosCodec::Vp9,
            "显式 VP9 不该被硬件编码器抢走（h264={h264} h265={h265}）"
        );
    }
}
