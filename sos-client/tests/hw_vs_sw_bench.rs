//! 硬件 H.264 与软件 VP9 的同机 A/B 基准。
//!
//! 目的：回答「换硬件编码到底能省多少」以及「关键帧/码率会变成什么样」，
//! 为 A 方案的取舍提供数据（不看机器型号，只比同一份内容下的编码耗时）。
//!
//! 只在 `--features hwcodec` 时编译（默认构建没有 `scrap::hwcodec`）。
//!
//! 运行（必须 release，debug 下编码器慢一个量级）：
//!
//! ```powershell
//! cargo test --release --test hw_vs_sw_bench -- --nocapture
//! ```
//!
//! 内容用 `support/desktop.rs`：大面积平坦背景 + 细密文字 + 逐帧平移，
//! 两种编码器吃的是**同一份 Y 平面**，色度统一填中性值，因此可比较。

#![cfg(feature = "hwcodec")]

use rustdesk_sos::video::hwenc as sos_hwenc;

use rustdesk_sos::video::vpx as sos_vpx;

use scrap::codec::base_bitrate;
use scrap::EncodeYuvFormat;
use sos_vpx::{SosCodec, SosVpxConfig, SosVpxEncoder};
use std::time::Instant;

#[path = "support/desktop.rs"]
mod desktop;

/// 按编码器给出的布局把 Y 平面 + 中性色度填进去。
///
/// 必须用编码器自己的 `yuvfmt()`：VP9 要 I420 + 64 字节对齐，
/// 硬件要 NV12 + 编码器给的 stride，两者布局不同。
fn fill_yuv(fmt: &EncodeYuvFormat, y: &[u8], w: usize, h: usize) -> Vec<u8> {
    let stride_y = fmt.stride[0];
    let stride_uv = *fmt.stride.get(1).unwrap_or(&stride_y);
    let end_u = fmt.u + stride_uv * (h / 2).max(1);
    let end_v = fmt.v + stride_uv * (h / 2).max(1);
    let mut buf = vec![0u8; end_u.max(end_v).max(stride_y * h)];
    // Y
    for row in 0..h {
        let dst = row * stride_y;
        let src = row * w;
        if dst + w <= buf.len() && src + w <= y.len() {
            buf[dst..dst + w].copy_from_slice(&y[src..src + w]);
        }
    }
    // 色度：NV12 是 U/V 交错的一段（v=0），I420 是两段独立平面
    // 中性色度：I420 是两个独立平面、NV12 是交错的一段，但「全 128」对两者都成立。
    let chroma = vec![128u8; stride_uv * (h / 2).max(1)];
    if fmt.u != 0 || fmt.v != 0 {
        let pu = fmt.u;
        if pu + chroma.len() <= buf.len() {
            buf[pu..pu + chroma.len()].copy_from_slice(&chroma);
        }
    }
    if fmt.v != 0 {
        let pv = fmt.v;
        if pv + chroma.len() <= buf.len() {
            buf[pv..pv + chroma.len()].copy_from_slice(&chroma);
        }
    }
    buf
}

struct Stat {
    name: String,
    enc_ms: Vec<f64>,
    kbps: u32,
    frames: usize,
    keyframes: usize,
    max_key_bytes: usize,
    max_bytes: usize,
    first: String,
}

fn report(s: &Stat) {
    if s.enc_ms.is_empty() {
        println!("[{:22}] no frames", s.name);
        return;
    }
    let n = s.enc_ms.len() as f64;
    let avg = s.enc_ms.iter().sum::<f64>() / n;
    let max = s.enc_ms.iter().cloned().fold(0.0f64, f64::max);
    println!(
        "[{:22}] enc avg {:6.2}ms  max {:6.2}ms  out {:6.1} kbps  frames {}  kf {}/{}  max_kf {:.1}KB  max_frame {:.1}KB  ({})",
        s.name,
        avg,
        max,
        s.kbps as f64,
        s.frames,
        s.keyframes,
        s.frames,
        s.max_key_bytes as f64 / 1024.0,
        s.max_bytes as f64 / 1024.0,
        s.first,
    );
}

fn bench_sw(w: usize, h: usize, frames: usize) -> Stat {
    // 软件侧用**生产链路自己的编码器**（`video::vpx`），不用上游 `scrap::codec::Encoder`。
    //
    // 这样做的两个理由：
    // 1. 硬件要对比的就是「换掉生产软编能省多少」，拿生产实现当参照更有意义；
    // 2. 上游编码器在硬内容 + 高码率下会按 `rc_dropframe_thresh` 丢帧，被丢的帧
    //    不参与编码、`enc_ms` 却照样计入 —— 测出来的软编耗时是偏快的，A/B 无效。
    //    以前靠改上游（把 dropframe 关掉、放大 CBR 缓冲）来消除这个偏差，
    //    现在改成用自建编码器：它按链路档位选丢帧策略，参数完全在 `SosVpxConfig` 里，
    //    不需要动上游一行代码。
    // 关丢帧 + 1000ms CBR 缓冲：A/B 要对比**编码耗时**，不能让码控把帧丢掉。
    // 丢掉的帧不进编码却照样计时，会把软件侧测快，硬件对比就失真了。
    // （以前靠改上游 `vpxcodec.rs` 关掉丢帧；现在用本 crate 的诊断钩子，不动上游。）
    sos_vpx::set_rc_override(Some((1000, 0)));
    let kbps = base_bitrate(w as _, h as _);
    let cfg = SosVpxConfig {
        width: w,
        height: h,
        codec: SosCodec::Vp9,
        threads: SosVpxConfig::recommended_threads(),
        cpu_used: SosVpxConfig::default_cpu_used(SosCodec::Vp9, w, h),
        bitrate_kbps: kbps,
        keyframe_interval: Some(60),
    };
    let mut enc = match SosVpxEncoder::new(cfg) {
        Ok(e) => e,
        Err(e) => {
            println!("[vp9] create failed: {e}");
            return Stat {
                name: "vp9".into(),
                enc_ms: vec![],
                kbps: 0,
                frames: 0,
                keyframes: 0,
                max_key_bytes: 0,
                max_bytes: 0,
                first: String::new(),
            };
        }
    };
    let fmt = enc.yuvfmt();
    let mut st = Stat {
        name: format!("vp9 {}x{}", w, h),
        enc_ms: Vec::new(),
        kbps: 0,
        frames: 0,
        keyframes: 0,
        max_key_bytes: 0,
        max_bytes: 0,
        first: format!("fmt {:?}", fmt.pixfmt),
    };
    let mut total = 0usize;
    for i in 0..frames {
        let y = desktop::desktop_y(w, h, i);
        let buf = fill_yuv(&fmt, &y, w, h);
        let t = Instant::now();
        let res = enc.encode(&buf, (i * 33) as i64);
        let el = t.elapsed().as_secs_f64() * 1000.0;
        st.enc_ms.push(el);
        if let Ok(packets) = res {
            for (data, key) in packets {
                total += data.len();
                st.frames += 1;
                if key {
                    st.keyframes += 1;
                    st.max_key_bytes = st.max_key_bytes.max(data.len());
                }
                st.max_bytes = st.max_bytes.max(data.len());
            }
        }
    }
    st.kbps = (total as f64 * 8.0 / (frames as f64 * 0.033) / 1000.0) as u32;
    st
}
/// 走**生产路径**的探测（`sos_hwenc::probe_async` 是后台线程，这里等它出结论）。
fn ensure_probe() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        sos_hwenc::probe_async();
        let start = Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(10) {
            if sos_hwenc::available() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}

fn bench_hw(w: usize, h: usize, frames: usize) -> Stat {
    ensure_probe();
    if !sos_hwenc::available() {
        println!("[hw] no H.264 hardware encoder on this machine");
        return Stat {
            name: "hw-h264".into(),
            enc_ms: vec![],
            kbps: 0,
            frames: 0,
            keyframes: 0,
            max_key_bytes: 0,
            max_bytes: 0,
            first: "unavailable".into(),
        };
    }
    let name = sos_hwenc::encoder_name();
    // 目标码率取 `base_bitrate`（相当于上游的 quality=1.0 基准），与软件侧对齐。
    let mut enc = match sos_hwenc::SosHwEncoder::new(w, h, base_bitrate(w as _, h as _), 60) {
        Ok(e) => e,
        Err(e) => {
            println!("[hw] create failed: {e}");
            return Stat {
                name: "hw-h264".into(),
                enc_ms: vec![],
                kbps: 0,
                frames: 0,
                keyframes: 0,
                max_key_bytes: 0,
                max_bytes: 0,
                first: String::new(),
            };
        }
    };
    let fmt = enc.yuvfmt();
    let mut st = Stat {
        name: format!("{} {}x{}", name, w, h),
        enc_ms: Vec::new(),
        kbps: 0,
        frames: 0,
        keyframes: 0,
        max_key_bytes: 0,
        max_bytes: 0,
        first: format!("fmt {:?}", fmt.pixfmt),
    };
    let mut total = 0usize;
    for i in 0..frames {
        let y = desktop::desktop_y(w, h, i);
        let buf = fill_yuv(&fmt, &y, w, h);
        let t = Instant::now();
        let res = enc.encode(&buf, (i * 33) as i64);
        let el = t.elapsed().as_secs_f64() * 1000.0;
        st.enc_ms.push(el);
        if let Ok(frames_out) = res {
            for (data, key) in frames_out.iter() {
                total += data.len();
                st.frames += 1;
                if *key {
                    st.keyframes += 1;
                    st.max_key_bytes = st.max_key_bytes.max(data.len());
                }
                st.max_bytes = st.max_bytes.max(data.len());
            }
        }
    }
    st.kbps = (total as f64 * 8.0 / (frames as f64 * 0.033) / 1000.0) as u32;
    st
}

struct PrintLogger;

impl log::Log for PrintLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        println!("[{} {}] {}", r.level(), r.target(), r.args());
    }
    fn flush(&self) {}
}

static PRINT_LOGGER: PrintLogger = PrintLogger;
/// 打印本机真实可用的硬件编码器（走生产路径的探测）。
#[test]
#[ignore = "实机基准，默认不跑；cargo test -- --ignored 或 --test <name> -- --ignored"]
fn probe_hardware_encoders() {
    let _ = log::set_logger(&PRINT_LOGGER);
    log::set_max_level(log::LevelFilter::Trace);
    ensure_probe();
    println!("=== sos_hwenc probe ===");
    println!("available: {}", sos_hwenc::available());
    println!("encoder: {}", sos_hwenc::encoder_name());
}
#[test]
#[ignore = "实机基准，默认不跑；cargo test -- --ignored 或 --test <name> -- --ignored"]
fn hardware_h264_vs_software_vp9() {
    println!("base_bitrate 1600x900 = {}", base_bitrate(1600, 900));
    println!("base_bitrate 1920x1080 = {}", base_bitrate(1920, 1080));
    for (w, h) in [(1600usize, 900usize), (1920, 1080)] {
        let sw = bench_sw(w, h, 120);
        report(&sw);
        let hw = bench_hw(w, h, 120);
        report(&hw);
        println!("---");
    }
}
