//! 实测硬件编码器能接受的码率上限 —— 给 `video::netspeed::ENCODER_CEILING_KBPS` 定值。
//!
//! 背景：控制端自定义画质开到 2000% 时，码率会被算成 200 Mbps，实测直接把 `hevc_qsv`
//! 打崩（`Error during resetting: incompatible video parameters (-14)`），此后每次重建
//! 都带着同一个不可能的码率，画面持续黑屏。上限该定多少需要实测，不能凭直觉。
//!
//! 运行（debug 即可，只看「编码器能不能建起来 / 能不能出包」，不看速度）：
//!
//! ```powershell
//! cargo test --manifest-path d:\code\rustdesk\sos-client\Cargo.toml --test hw_bitrate_limit -- --ignored --nocapture
//! ```

#![cfg(feature = "hwcodec")]

use rustdesk_sos::video::hwenc as sos_hwenc;
use rustdesk_sos::video::vpx::SosCodec;

fn try_bitrate(codec: SosCodec, kbps: u32, w: usize, h: usize) -> (bool, String) {
    if !sos_hwenc::available() || (codec == SosCodec::H265 && !sos_hwenc::available_h265()) {
        return (false, "no encoder".into());
    }
    match sos_hwenc::SosHwEncoder::new(codec, w, h, kbps, 60) {
        Ok(mut enc) => {
            // 只建起来还不够：真正打崩现场的是 send_frame，必须走一帧。
            let fmt = enc.yuvfmt();
            let stride_y = fmt.stride[0];
            let stride_uv = *fmt.stride.get(1).unwrap_or(&stride_y);
            let end_u = fmt.u + stride_uv * (h / 2).max(1);
            let end_v = fmt.v + stride_uv * (h / 2).max(1);
            let mut buf = vec![128u8; end_u.max(end_v).max(stride_y * h).max(w * h)];
            for row in 0..h.min(buf.len() / stride_y.max(1)) {
                let dst = row * stride_y;
                if dst + w <= buf.len() {
                    buf[dst..dst + w].fill((row % 251) as u8);
                }
            }
            match enc.encode(&buf, 33) {
                Ok(_) => (true, "ok".into()),
                Err(e) => (false, format!("encode: {e}")),
            }
        }
        Err(e) => (false, format!("new: {e}")),
    }
}

#[test]
#[ignore = "实机基准：硬件编码器码率上限探测"]
fn hw_encoder_bitrate_ceiling() {
    // 探测在应用启动的后台线程里跑，测试环境要先手动触发并等它完成
    sos_hwenc::probe_async();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline
        && !sos_hwenc::available()
        && !sos_hwenc::available_h265()
    {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if !sos_hwenc::available() && !sos_hwenc::available_h265() {
        println!("[HW_BR] 本机没有可用硬件编码器，跳过");
        return;
    }

    let (w, h) = (1920usize, 1080usize);
    // 覆盖默认工作点(~2.5Mbps)到崩溃点(200Mbps)，重点看 40/60/100 这几个候选
    let probes = [
        2_500u32, 10_000, 20_000, 40_000, 60_000, 100_000, 200_000,
    ];

    // 探测线程会自己二分搜索码率上限（可用性发布之后才跑，可能要几秒）
    let deadline2 = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline2
        && (sos_hwenc::max_bitrate_kbps(SosCodec::H264).is_none()
            || sos_hwenc::max_bitrate_kbps(SosCodec::H265).is_none())
    {
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    println!(
        "[HW_BR] probed cap: H264={:?} H265={:?}",
        sos_hwenc::max_bitrate_kbps(SosCodec::H264),
        sos_hwenc::max_bitrate_kbps(SosCodec::H265)
    );
    println!("[HW_BR] resolution {w}x{h}");
    for codec in [SosCodec::H264, SosCodec::H265] {
        for kbps in probes {
            let (ok, detail) = try_bitrate(codec, kbps, w, h);
            println!(
                "[HW_BR] {:?} {:>7} kbps -> {} ({detail})",
                codec,
                kbps,
                if ok { "OK" } else { "FAIL" }
            );
        }
    }
}
