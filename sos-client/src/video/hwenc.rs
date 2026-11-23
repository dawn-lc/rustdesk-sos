//! H.264 硬件编码后端（FFmpeg 的 nvenc / qsv / amf，走 CPU 的 NV12 输入）。
//!
//! 为什么单独一个模块：`video::vpx` 是 libvpx 专用实现（VP8/VP9、毫秒级 rc 缓冲、
//! `rc_dropframe_thresh`、0-63 量化器），而硬件编码是**另一套语义**：
//!
//! | 维度 | libvpx（`video::vpx`） | 硬件（本模块） |
//! | --- | --- | --- |
//! | 编码格式 | VP8 / VP9 | **只有 H.264**（上游 `hwcodec` 不提供 VP8/VP9 硬件编码） |
//! | 输入布局 | I420（`STRIDE_ALIGN=64`） | **NV12**，stride 由编码器自己给 |
//! | 码率控制 | `rc_buf_*=1000ms` + `rc_dropframe_thresh` | 只有绝对码率；**没有「编码器内部丢帧」** |
//! | 量化器 | `VP8E_GET_LAST_QUANTIZER_64` 可回读 | 不可回读 |
//! | 强制关键帧 | `VPX_EFLAG_FORCE_KF` | **没有接口** ⇒ 用「重建编码器」代替（新序列首帧必是 IDR） |
//!
//! ## 为什么直接依赖 `hwcodec` crate，而不是打开 `rustdesk/hwcodec`
//!
//! 后者会把 hwcodec 编进上游的 `librustdesk`（连 cdylib 一起），于是那几条 FFmpeg
//! 静态库的链接参数必须加在上游 `libs/scrap/build.rs` 里 —— 那是改上游，不做。
//! 直接依赖后只有 sos 自己的 bin/tests 链接 hwcodec，链接参数由 `build.rs` 提供，
//! 上游一行都不用动。
//!
//! 代价是不再复用 `scrap::hwcodec::HwRamEncoder`（它依赖 `scrap` 的 `hwcodec`
//! feature），所以探测与编码上下文在本模块里自己搭，语义照抄上游，未做改动。

// 依赖 `hwcodec` crate 才有的真实实现。call site 不需要任何 cfg：
// 关闭 feature 时下面那个桩提供**同一套 API**，于是 `video` / `net::connection`
// 在两种构建下是同一份代码。
#[cfg(feature = "hwcodec")]
mod enabled {
    use hbb_common::ResultType;
    use hwcodec::{
        common::{Quality, RateControl},
        ffmpeg::AVPixelFormat,
        ffmpeg_ram::{
            encode::{EncodeContext, Encoder},
            CodecInfo,
        },
    };
    use scrap::{codec::base_bitrate, EncodeYuvFormat, Pixfmt, HW_STRIDE_ALIGN};
    use std::sync::atomic::{AtomicU8, Ordering};


    /// 与上游一致的编码参数（见 `scrap::hwcodec::HwRamEncoder::new`）。
    const DEFAULT_FPS: i32 = 30;
    const DEFAULT_THREADS: i32 = 4;

    const UNKNOWN: u8 = 0;
    const AVAILABLE: u8 = 1;
    const UNAVAILABLE: u8 = 2;

    /// 探测结论（**必须缓存**）。
    ///
    /// `Encoder::available_encoders` 会真的去建一遍编码器（试编码一帧），单次几百毫秒到
    /// 数秒。而 `available()` 会被 `[VID_FPS]` 这类周期性日志路径间接调用，不缓存就等于
    /// 每隔几秒重探一次。
    static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);

    /// 探测到的编码器名字（`h264_amf` / `h264_nvenc` / `h264_qsv`），未探测时为空。
    static NAME: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

    /// 当前是否可用。**探测完成前一律返回 false**（保守：宁可用软件编码，也不要赌）。
    pub fn available() -> bool {
        STATE.load(Ordering::Relaxed) == AVAILABLE
    }

    /// 会话级关闭硬件编码（初始化失败时调用），之后不再尝试。
    pub fn mark_unavailable() {
        STATE.store(UNAVAILABLE, Ordering::Relaxed);
    }

    /// 选中的硬件编码器名字（诊断用，例如 `h264_amf`）。
    pub fn encoder_name() -> String {
        NAME.lock().map(|n| n.clone()).unwrap_or_default()
    }

    /// 启动时探测可用硬件编码器（后台线程，不阻塞启动）。
    ///
    /// 上游把探测放在**独立子进程**里跑（`--check-hwcodec-config`），因为「试编码一次」
    /// 在某些驱动上可能卡住几十秒。SOS 没有那个子进程模式（clap 会拒绝未知参数，而且
    /// 启动路径上还有管理员检查与单实例互斥），所以改在本进程的后台线程里跑，并把它
    /// 当作**可选加速**：探测没完成、失败或超时，`available()` 就是 false，链路照旧走
    /// VP9 软件编码，功能不受影响。
    pub fn probe_async() {
        let spawned = std::thread::Builder::new()
            .name("sos-hwprobe".into())
            .spawn(|| {
                let start = std::time::Instant::now();
                // 尺寸只影响「能否建出编码器」，用常见的 720p 即可。
                let ctx = EncodeContext {
                    name: String::new(),
                    mc_name: None,
                    width: 1280,
                    height: 720,
                    pixfmt: AV_PIX_FMT_NV12,
                    align: HW_STRIDE_ALIGN as i32,
                    fps: DEFAULT_FPS,
                    gop: i32::MAX,
                    rc: RateControl::RC_CBR,
                    quality: Quality::Quality_Default,
                    kbs: 1000,
                    q: -1,
                    thread_count: DEFAULT_THREADS,
                };
                let best = CodecInfo::prioritized(Encoder::available_encoders(ctx, None));
                match best.h264 {
                    Some(info) => {
                        if let Ok(mut name) = NAME.lock() {
                            *name = info.name.clone();
                        }
                        STATE.store(AVAILABLE, Ordering::Relaxed);
                        log::info!(
                            "[HWENC] {} available (probe took {:?})",
                            info.name,
                            start.elapsed()
                        );
                    }
                    None => {
                        STATE.store(UNAVAILABLE, Ordering::Relaxed);
                        log::info!("[HWENC] no hardware H.264 encoder (probe took {:?})", start.elapsed());
                    }
                }
            });
        if let Err(e) = spawned {
            log::warn!("[HWENC] failed to spawn probe thread: {}", e);
        }
    }

    /// 硬件 H.264 编码器。
    pub struct SosHwEncoder {
        name: String,
        width: usize,
        height: usize,
        gop: i32,
        encoder: Encoder,
        /// 当前目标码率（kbps）。
        kbps: u32,
    }

    impl SosHwEncoder {
        pub fn new(
            width: usize,
            height: usize,
            kbps: u32,
            keyframe_interval: usize,
        ) -> ResultType<Self> {
            let name = encoder_name();
            if name.is_empty() {
                anyhow::bail!("no H.264 hardware encoder available");
            }
            // 与 VP9 保持一致的周期 IDR（数的是**送进编码器的帧数**，不是墙钟）。
            // 不设会让 GOP 变成 `i32::MAX`：只有「重建编码器」那一刻才出关键帧，
            // 中途接入的控制端要等到下一次重建才拿得到 IDR。
            let gop = keyframe_interval.max(1) as i32;
            let encoder = Self::build(&name, width, height, kbps, gop)?;
            log::info!(
                "[HWENC] {} {}x{} {}kbps gop={} stride={:?}",
                name,
                width,
                height,
                kbps,
                gop,
                encoder.linesize
            );
            Ok(Self {
                name,
                width,
                height,
                gop,
                encoder,
                kbps,
            })
        }

        fn build(
            name: &str,
            width: usize,
            height: usize,
            kbps: u32,
            gop: i32,
        ) -> ResultType<Encoder> {
            let ctx = EncodeContext {
                name: name.to_owned(),
                mc_name: None,
                width: width as i32,
                height: height as i32,
                pixfmt: AV_PIX_FMT_NV12,
                align: HW_STRIDE_ALIGN as i32,
                fps: DEFAULT_FPS,
                gop,
                rc: RateControl::RC_CBR,
                quality: Quality::Quality_Default,
                kbs: kbps as i32,
                q: -1,
                thread_count: DEFAULT_THREADS,
            };
            Encoder::new(ctx).map_err(|_| anyhow::anyhow!("failed to create encoder {}", name))
        }

        /// 供 `Frame::to()` 使用的目标布局（NV12 + 编码器给出的 stride/偏移）。
        pub fn yuvfmt(&self) -> EncodeYuvFormat {
            let pixfmt = if self.encoder.ctx.pixfmt == AV_PIX_FMT_NV12 {
                Pixfmt::NV12
            } else {
                Pixfmt::I420
            };
            EncodeYuvFormat {
                pixfmt,
                w: self.encoder.ctx.width as _,
                h: self.encoder.ctx.height as _,
                stride: self.encoder.linesize.iter().map(|x| *x as usize).collect(),
                u: self.encoder.offset[0] as _,
                v: if pixfmt == Pixfmt::NV12 {
                    0
                } else {
                    self.encoder.offset[1] as _
                },
            }
        }

        pub fn encode(&mut self, yuv: &[u8], ms: i64) -> ResultType<Vec<(Vec<u8>, bool)>> {
            let frames = self
                .encoder
                .encode(yuv, ms)
                .map_err(|code| anyhow::anyhow!("hw encode failed: {}", code))?;
            Ok(frames
                .iter()
                .map(|f| (f.data.clone(), f.key == 1))
                .collect())
        }

        /// 运行期改码率（硬件编码器直接吃绝对 kbps，不需要换算成 `quality` 比例）。
        pub fn tune(&mut self, kbps: u32) -> ResultType<()> {
            if kbps == self.kbps {
                return Ok(());
            }
            self.kbps = kbps;
            self.encoder
                .set_bitrate(kbps as i32)
                .map_err(|_| anyhow::anyhow!("failed to set bitrate {}kbps", kbps))
        }

        /// 请求下一帧为关键帧。
        ///
        /// `hwcodec` 没有「强制 IDR」接口，所以这里重建编码器 —— 新序列的第一帧
        /// 必然是带 SPS/PPS 的 IDR，语义上等价于 `force_keyframe`。重建失败时旧编码器
        /// 保持不变（`?` 在赋值前返回），调用方只需记一条日志。
        pub fn force_keyframe(&mut self) -> ResultType<()> {
            self.encoder = Self::build(&self.name, self.width, self.height, self.kbps, self.gop)?;
            Ok(())
        }

        /// 硬件编码器不会在内部丢帧（没有 `rc_dropframe_thresh` 的等价物），恒为 0。
        /// 保留接口是为了让调用方对两种后端保持同一形状。
        pub fn take_dropped_frames(&mut self) -> usize {
            0
        }

        pub fn bitrate_kbps(&self) -> u32 {
            self.kbps
        }
    }

    /// 目标 kbps → `quality` 比例。硬件路径用绝对码率，这里只为日志/兼容保留。
    #[allow(dead_code)]
    fn quality_for(width: usize, height: usize, kbps: u32) -> f32 {
        let base = base_bitrate(width as _, height as _);
        if base == 0 {
            return 1.0;
        }
        (kbps as f32 / base as f32).clamp(0.05, 8.0)
    }

    const AV_PIX_FMT_NV12: AVPixelFormat = AVPixelFormat::AV_PIX_FMT_NV12;
}

/// 没打开 `hwcodec` feature 时的桩：API 形状一致，但永远报告「没有硬件编码器」。
///
/// 这样默认构建既不引入 FFmpeg 静态库依赖，也不需要在上层写 `#[cfg]`。
#[cfg(not(feature = "hwcodec"))]
mod disabled {
    use hbb_common::ResultType;
    use scrap::EncodeYuvFormat;

    pub fn available() -> bool {
        false
    }

    pub fn mark_unavailable() {}

    pub fn probe_async() {
        log::info!("[HWENC] built without the `hwcodec` feature, software encoding only");
    }

    /// 与真实实现同名同签名；构造永远失败，调用方会回退 VP9。
    pub struct SosHwEncoder;

    impl SosHwEncoder {
        pub fn new(
            _width: usize,
            _height: usize,
            _kbps: u32,
            _keyframe_interval: usize,
        ) -> ResultType<Self> {
            anyhow::bail!("built without the `hwcodec` feature")
        }

        /// 桩永远不会被 `Frame::to()` 真正使用（构造必然失败），返回一个空布局即可。
        pub fn yuvfmt(&self) -> EncodeYuvFormat {
            EncodeYuvFormat {
                pixfmt: scrap::Pixfmt::I420,
                w: 0,
                h: 0,
                stride: Vec::new(),
                u: 0,
                v: 0,
            }
        }

        pub fn encode(&mut self, _yuv: &[u8], _ms: i64) -> ResultType<Vec<(Vec<u8>, bool)>> {
            anyhow::bail!("built without the `hwcodec` feature")
        }

        pub fn tune(&mut self, _kbps: u32) -> ResultType<()> {
            Ok(())
        }

        pub fn force_keyframe(&mut self) -> ResultType<()> {
            Ok(())
        }

        pub fn take_dropped_frames(&mut self) -> usize {
            0
        }

        pub fn bitrate_kbps(&self) -> u32 {
            0
        }
    }
}

#[cfg(feature = "hwcodec")]
pub use enabled::{available, encoder_name, mark_unavailable, probe_async, SosHwEncoder};
#[cfg(not(feature = "hwcodec"))]
pub use disabled::{available, mark_unavailable, probe_async, SosHwEncoder};
