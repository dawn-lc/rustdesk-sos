//! 临时性能基准：测量不同编码线程数 / 分辨率 / 画面复杂度下的软件编码吞吐
//!
//! 运行：cargo test --release --test enc_bench -- --ignored --nocapture
//!
//! 用**生产链路**的 `video::vpx` 编码器测，不用上游 `scrap::codec::Encoder`：
//! 上游那套在硬内容 + 高码率下会按 `rc_dropframe_thresh` 丢帧，被丢的帧不参与编码
//! 却照样计入耗时，测出的 fps 是被丢帧抬高的。改用自建编码器就不必改上游代码。

#![cfg(all(windows, sos_vpx))]

use rustdesk_sos::video::vpx as sos_vpx;

use scrap::codec::base_bitrate;
use scrap::EncodeYuvFormat;
use sos_vpx::{SosCodec, SosVpxConfig, SosVpxEncoder};
use std::time::Instant;

fn make_i420(w: u32, h: u32, frame_idx: u32, noise: bool) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut buf = vec![0u8; w * h * 3 / 2];
    let (y, rest) = buf.split_at_mut(w * h);
    let (u, v) = rest.split_at_mut(w * h / 4);
    let mut seed = frame_idx.wrapping_mul(2654435761).wrapping_add(1);
    if noise {
        // 高细节内容（文字/纹理桌面），逐帧变化，编码器无法偷懒
        for p in y.iter_mut().chain(u.iter_mut()).chain(v.iter_mut()) {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *p = (seed >> 24) as u8;
        }
    } else {
        // 平滑滚动内容（动画/视频），运动估计容易
        let shift = (frame_idx as usize * 7) % w.max(1);
        for row in 0..h {
            for col in 0..w {
                let x = (col + shift) % w.max(1);
                y[row * w + col] = ((x * 255 / w.max(1)) ^ (row * 3)) as u8;
            }
        }
        for row in 0..h / 2 {
            for col in 0..w / 2 {
                u[row * (w / 2) + col] = ((col + shift) % 256) as u8;
                v[row * (w / 2) + col] = ((row * 5) % 256) as u8;
            }
        }
    }
    buf
}

/// 把紧凑 I420（`make_i420` 的产物）铺成编码器要求的 stride/平面偏移布局。
///
/// 绝大多数尺寸下 `stride[0] == 宽度`、`stride[1] == 宽度/2`，可直接透传；
/// 只有宽度不是对齐倍数时才需要逐行拷贝。
fn to_layout(fmt: &EncodeYuvFormat, i420: &[u8], w: usize, h: usize) -> Vec<u8> {
    let sy = fmt.stride[0];
    let su = *fmt.stride.get(1).unwrap_or(&sy);
    let cw = (w / 2).max(1);
    let ch = (h / 2).max(1);
    if sy == w && su == cw && fmt.u == w * h && fmt.v == w * h + cw * ch {
        return i420.to_vec();
    }
    let mut buf = vec![0u8; (fmt.v + su * ch).max(sy * h)];
    for row in 0..h {
        buf[row * sy..row * sy + w].copy_from_slice(&i420[row * w..row * w + w]);
    }
    for row in 0..ch {
        let su_src = w * h + row * cw;
        let sv_src = w * h + cw * ch + row * cw;
        let du = fmt.u + row * su;
        let dv = fmt.v + row * su;
        buf[du..du + cw].copy_from_slice(&i420[su_src..su_src + cw]);
        buf[dv..dv + cw].copy_from_slice(&i420[sv_src..sv_src + cw]);
    }
    buf
}

fn bench(name: &str, w: u32, h: u32, codec: SosCodec, frames: u32, noise: bool) {
    // 关掉丢帧、把 CBR 缓冲放大到 1000ms：测的是**编码吞吐**，不是码控行为。
    // 不这样做的话硬内容会触发 `rc_dropframe_thresh` 丢帧（实测 40 帧只出 1 个包），
    // 而丢掉的帧不参与编码却照样计入耗时，fps 会被抬高 —— 这正是以前去改上游
    // `libs/scrap/src/common/vpxcodec.rs` 的原因。现在用自己的诊断钩子就够了。
    sos_vpx::set_rc_override(Some((1000, 0)));
    let cfg = SosVpxConfig {
        width: w as usize,
        height: h as usize,
        codec,
        threads: SosVpxConfig::recommended_threads(),
        cpu_used: SosVpxConfig::default_cpu_used(codec, w as usize, h as usize),
        bitrate_kbps: base_bitrate(w as _, h as _) as u32,
        keyframe_interval: None,
    };
    let mut enc = match SosVpxEncoder::new(cfg) {
        Ok(e) => e,
        Err(e) => {
            println!("[{name}] create failed: {e}");
            return;
        }
    };
    let fmt = enc.yuvfmt();
    let mut total_bytes = 0usize;
    let mut encoded = 0u32;
    let start = Instant::now();
    for i in 0..frames {
        let yuv = make_i420(w, h, i, noise);
        // 采集器给的是 BGRA→I420 的产物，这里直接喂同样布局的 I420
        let data = to_layout(&fmt, &yuv, w as usize, h as usize);
        if let Ok(packets) = enc.encode(&data, (i * 33) as i64) {
            for (d, _key) in packets {
                total_bytes += d.len();
                encoded += 1;
            }
        }
    }
    let total = start.elapsed().as_secs_f64();
    println!(
        "[{name}] {w}x{h} fps={:.1} frame_ms={:.1} avg_kb={:.0} encoded={encoded}/{frames}",
        frames as f64 / total,
        total * 1000.0 / frames as f64,
        total_bytes as f64 / encoded.max(1) as f64 / 1024.0
    );
}
#[test]
#[ignore = "实机基准，默认不跑；cargo test -- --ignored 或 --test <name> -- --ignored"]
fn bench_software_encoders() {
    let frames = 40;
    println!(
        "=== codec threads (scrap heuristic): {} ===",
        scrap::codec::codec_thread_num(64)
    );
    for (w, h, tag) in [(1920u32, 1080u32, "1080p"), (1280, 720, "720p")] {
        for (codec, ctag) in [
            (SosCodec::Vp9, "vp9"),
            (SosCodec::Vp8, "vp8"),
        ] {
            bench(&format!("{ctag}-{tag}-text"), w, h, codec, frames, true);
            bench(&format!("{ctag}-{tag}-scroll"), w, h, codec, frames, false);
        }
    }
}
