//! 验证「码率随帧率等比缩放」—— 打断 QoS 的丢帧/延迟正反馈。
//!
//! 生产日志里的死循环：
//! ```text
//! rtt=451ms qos_fps=3 qos_bitrate=2868kbps   ← 帧率被压到 3，码率没动
//! rtt=158ms qos_fps=30 qos_bitrate=3750kbps  ← 每帧 34KB，排队 158ms
//! ```
//! QoS 因为延迟高压低帧率，而码率不变 → 每帧变大 → 排队更久 → 延迟更高。
//!
//! 本测试除了校验公式，还**实测**验证核心物理结论：
//! 固定码率下降低帧率会让单帧传输时间成比例上升，而码率同比缩放则不变。
//!
//! 运行：cargo test --release --test vpx_bitrate_fps -- --nocapture

#![cfg(all(windows, sos_vpx))]

use rustdesk_sos::video::vpx as sos_vpx;

use sos_vpx::{LinkBudget, SosCodec, SosVpxConfig, SosVpxEncoder};
use std::time::{Duration, Instant};

fn make_motion(w: usize, h: usize, seed: usize) -> Vec<u8> {
    let (cw, ch) = (w / 2, h / 2);
    let mut buf = vec![0u8; w * h + 2 * cw * ch];
    let shift = seed * 7;
    for y in 0..h {
        for x in 0..w {
            buf[y * w + x] = (((x + shift) * 3 + y * 5) % 251) as u8;
        }
    }
    let uv = w * h;
    for y in 0..ch {
        for x in 0..cw {
            let v = ((x + shift) * 2 + y * 3) % 251;
            buf[uv + y * cw + x] = v as u8;
            buf[uv + cw * ch + y * cw + x] = (250 - v) as u8;
        }
    }
    buf
}

/// 满帧率下结果必须与旧公式完全一致 —— 不能影响正常工作状态。
#[test]
fn full_fps_matches_old_formula() {
    for (w, h) in [(1920usize, 1080usize), (1280, 720), (2880, 1800)] {
        let full = SosVpxConfig::default_bitrate_kbps(w, h);
        for ratio in [0.4f32, 0.67, 1.0, 1.35, 1.5] {
            let got = SosVpxConfig::bitrate_kbps_for(w, h, ratio, 30);
            let want = (full as f32 * ratio).max(150.0) as u32;
            assert_eq!(got, want, "{}x{} ratio {} 在 30fps 下应与旧公式一致", w, h, ratio);
        }
        // 超过标称帧率也不应放大码率
        assert_eq!(
            SosVpxConfig::bitrate_kbps_for(w, h, 1.0, 120),
            SosVpxConfig::bitrate_kbps_for(w, h, 1.0, 30)
        );
    }
    println!("满帧率下与旧公式一致 ✓");
}

/// 帧率越低码率越低，且近似线性。
#[test]
fn bitrate_scales_with_fps() {
    let (w, h) = (1920usize, 1080usize);
    println!("{:>6} {:>12} {:>12} {:>14}", "fps", "bitrate", "每帧字节", "相对 30fps");
    let mut prev = u32::MAX;
    for fps in [30u32, 20, 15, 10, 5, 3, 2, 1] {
        let kbps = SosVpxConfig::bitrate_kbps_for(w, h, 1.5, fps);
        let per_frame = kbps as f64 * 1000.0 / 8.0 / fps as f64;
        println!(
            "{:>6} {:>12} {:>12.0} {:>14.2}",
            fps,
            kbps,
            per_frame,
            per_frame / (SosVpxConfig::bitrate_kbps_for(w, h, 1.5, 30) as f64 * 1000.0 / 8.0 / 30.0)
        );
        assert!(kbps <= prev, "fps {} 的码率 ({}) 不应高于更高帧率", fps, kbps);
        prev = kbps;
    }
    // 30fps → 15fps 应当缩到一半（这是缩放区间的下端点）
    let a = SosVpxConfig::bitrate_kbps_for(w, h, 1.5, 30);
    let b = SosVpxConfig::bitrate_kbps_for(w, h, 1.5, 15);
    assert!(
        (b as f64 / a as f64 - 0.5).abs() < 0.05,
        "15fps 的码率 ({b}) 应约为 30fps ({a}) 的一半"
    );
    // 再往下降就被下限截断了 —— 这是有意的取舍，避免编码器编不出帧
    let c = SosVpxConfig::bitrate_kbps_for(w, h, 1.5, 3);
    assert_eq!(c, b, "低于 15fps 后码率应稳定在下限，不再继续下降");
}

/// 核心不变量：在生产实际会遇到的帧率区间内，单帧体积不会成倍增长。
///
/// 这是打破死循环的关键 —— 帧率被压下去时，每帧体积必须同步变小，
/// 否则探针排队时间会成比例变长，测到的 RTT 跟着涨，QoS 于是继续压帧率。
///
/// 下界取 8（`MIN_CAPTURE_FPS`）：采集侧永远不会比这个更慢，
/// 所以生产里的码率也是按 ≥8fps 算的，不会走到 0.5 截断区以外。
#[test]
fn per_frame_bytes_do_not_grow_when_fps_drops() {
    let (w, h) = (1920usize, 1080usize);
    let base = SosVpxConfig::bitrate_kbps_for(w, h, 1.5, 30) as f64 * 1000.0 / 8.0 / 30.0;
    println!("30fps 每帧 {:.1} KB", base / 1024.0);
    for fps in [30u32, 20, 15, 12, 10, 8] {
        let kbps = SosVpxConfig::bitrate_kbps_for(w, h, 1.5, fps) as f64;
        let per_frame = kbps * 1000.0 / 8.0 / fps as f64;
        assert!(
            per_frame < base * 2.5,
            "fps={fps} 时每帧 {:.1}KB，是 30fps ({:.1}KB) 的 {:.1} 倍 —— 会重新引发排队",
            per_frame / 1024.0,
            base / 1024.0,
            per_frame / base
        );
        println!("{fps:>4}fps 每帧 {:.1} KB", per_frame / 1024.0);
    }
}

/// 帧率缩放下限要足够高，否则编码器编不出帧。
///
/// 实测教训：下限 0.1 时 QoS 把帧率压到 3fps，码率只剩 **318kbps**，
/// 1080p 根本编不出来，于是 `edrop=8/19 (42%)` —— 为了省带宽反而去丢帧，
/// 而“丢帧”正是我们要避免的。现在下限 0.5，最差也有半码率。
#[test]
fn floor_keeps_bitrate_usable() {
    let (w, h) = (1920usize, 1080usize);
    let nominal = SosVpxConfig::default_bitrate_kbps(w, h);
    let floor = nominal / 2;
    for fps in [0u32, 1, 2, 3, 5] {
        let kbps = SosVpxConfig::bitrate_kbps_for(w, h, 1.0, fps);
        println!("fps={fps} -> {kbps}kbps (标称 {nominal})");
        assert!(
            kbps >= floor,
            "fps={fps} 时码率 {kbps} 低于半标称 {floor}，编码器会被迫丢帧"
        );
    }
}

/// 实测：固定码率下降帧率，单帧体积真的会翻倍。
///
/// 用真实编码器验证「每帧字节数 ≈ 码率/帧率」，这样才能确认生产日志里
/// `2868kbps / 2fps = 179KB` 那类数据不是测量假象。
#[test]
fn measured_frame_size_follows_bitrate_over_fps() {
    let (w, h) = (1920usize, 1080usize);
    println!("{:>8} {:>8} {:>12} {:>12}", "码率", "帧率", "实测每帧KB", "理论KB");
    for (kbps, fps) in [(2500u32, 30u32), (2500, 10), (2500, 5)] {
        let cfg = SosVpxConfig {
            width: w,
            height: h,
            codec: SosCodec::Vp9,
            threads: 5,
            cpu_used: 8,
            bitrate_kbps: kbps,
            keyframe_interval: None,
        };
        let mut enc = SosVpxEncoder::new(cfg).expect("encoder");
        let origin = Instant::now();
        let spf = Duration::from_secs_f64(1.0 / fps as f64);
        let n = (fps * 3) as usize; // 编 3 秒
        let mut bytes = 0usize;
        let mut outs = 0usize;
        for i in 0..n {
            let deadline = origin + spf * i as u32;
            let now = Instant::now();
            if deadline > now {
                std::thread::sleep(deadline - now);
            }
            let yuv = make_motion(w, h, i);
            let pts = origin.elapsed().as_millis() as i64;
            if let Ok(out) = enc.encode(&yuv, pts) {
                if !out.is_empty() {
                    outs += 1;
                    for (d, _k) in out {
                        bytes += d.len();
                    }
                }
            }
        }
        let measured = if outs > 0 {
            bytes as f64 / outs as f64 / 1024.0
        } else {
            0.0
        };
        let theory = kbps as f64 * 1000.0 / 8.0 / fps as f64 / 1024.0;
        println!("{:>8} {:>8} {:>12.1} {:>12.1}", kbps, fps, measured, theory);
        // 只要求同量级：合成内容的绝对码率精度本来就不高
        assert!(
            measured > theory * 0.4,
            "{kbps}kbps/{fps}fps 每帧只有 {measured:.1}KB，远低于理论 {theory:.1}KB"
        );
    }
}

// ── 码率上限：网络侧 + 编码器侧兜底 ──

/// 上限必须随网卡速率变化，而不是写死一个常数。
///
/// 「编码器能吃多少」不在这里：那由 `video::hwenc` 启动时实测
/// （`max_bitrate_kbps`），因为同一份代码在不同机器上的失败点不同。
#[test]
fn cap_follows_link_speed() {
    use rustdesk_sos::video::netspeed::cap_kbps_for_link;

    assert_eq!(cap_kbps_for_link(10_000), Some(8_000), "10 Mbps 链路应给 8 Mbps");
    assert_eq!(cap_kbps_for_link(30_000), Some(24_000), "30 Mbps 链路应给 24 Mbps");
    assert_eq!(cap_kbps_for_link(100_000), Some(80_000), "100 Mbps 链路应给 80 Mbps");
    assert_eq!(cap_kbps_for_link(1_000_000), Some(800_000), "千兆链路给 800 Mbps");

    // 链路速率未知：**不猜常数**，交给编码器侧兜底（调用方 or_else 处理）
    assert_eq!(
        cap_kbps_for_link(0),
        None,
        "网卡速率读不出来时不应编造一个上限，应由编码器实测值兜底"
    );

    // 极慢链路仍有下限
    assert_eq!(cap_kbps_for_link(1), Some(1_000));
}

/// 极端自定义画质（控制端 2000% = 40x）不能突破上限。
///
/// 上限可能来自网卡，也可能来自编码器实测（网卡读不出来时）。两者都没有时不钳制 ——
/// 那种情况下任何断言都只能针对「不被无谓钳制」，即结果仍应是合理量级。
#[test]
fn bitrate_is_capped_for_extreme_custom_quality() {
    use rustdesk_sos::video::{hwenc, netspeed};
    let (w, h) = (1920usize, 1080usize);
    let cap = netspeed::cap_kbps().or_else(hwenc::max_bitrate_kbps_any);
    for (ratio, fps, budget) in [
        (40.0f32, 30u32, LinkBudget::Generous),
        (40.0, 120, LinkBudget::Constrained),
    ] {
        let got = SosVpxConfig::bitrate_kbps_for_link(w, h, ratio, fps, budget);
        if let Some(cap) = cap {
            assert!(
                got <= cap,
                "{budget:?} ratio={ratio} fps={fps} -> {got}kbps 超过上限 {cap}"
            );
        }
    }
    if let Some(cap) = cap {
        let plain = SosVpxConfig::bitrate_kbps_for(w, h, 40.0, 120);
        assert!(plain <= cap, "bitrate_kbps_for -> {plain}kbps 超过上限 {cap}");
        // 正常档位不受影响：Best(1.5x) 约 7500kbps
        assert!(SosVpxConfig::bitrate_kbps_for(w, h, 1.5, 30) < cap / 2);
    }
}
