//! H.264 / H.265 硬件编码后端（FFmpeg 的 nvenc / qsv / amf，走 CPU 的 NV12 输入）。
//!
//! 为什么单独一个模块：`video::vpx` 是 libvpx 专用实现（VP8/VP9、毫秒级 rc 缓冲、
//! `rc_dropframe_thresh`、0-63 量化器），而硬件编码是**另一套语义**：
//!
//! | 维度 | libvpx（`video::vpx`） | 硬件（本模块） |
//! | --- | --- | --- |
//! | 编码格式 | VP8 / VP9 | **H.264 / H.265**（上游 `hwcodec` 不提供 VP8/VP9 硬件编码） |
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
    use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};


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
    static H264_STATE: AtomicU8 = AtomicU8::new(UNKNOWN);

    /// 探测到的编码器名字（`h264_amf` / `h264_nvenc` / `h264_qsv`），未探测时为空。
    static H264_NAME: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

    /// H.265 那一套（`hevc_amf` / `hevc_nvenc` / `hevc_qsv`）。
    ///
    /// 两种编码器**各自独立**地判可用：机器上可能有 HEVC 而没有 H.264（或反之），
    /// 混用一个开关会让其中一种被错误地整体禁用。
    static H265_STATE: AtomicU8 = AtomicU8::new(UNKNOWN);
    static H265_NAME: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

    fn is_available(state: &AtomicU8) -> bool {
        state.load(Ordering::Relaxed) == AVAILABLE
    }

    fn name_of(name: &std::sync::Mutex<String>) -> String {
        name.lock().map(|n| n.clone()).unwrap_or_default()
    }

    /// 当前是否可用（H.264）。**探测完成前一律返回 false**（保守：宁可用软件编码，也不要赌）。
    pub fn available() -> bool {
        is_available(&H264_STATE)
    }

    /// H.265 硬件编码是否可用（同样是探测结论，未探测时为 false）。
    pub fn available_h265() -> bool {
        is_available(&H265_STATE)
    }

    /// 会话级关闭 H.264 硬件编码（初始化失败时调用），之后不再尝试。
    pub fn mark_unavailable() {
        H264_STATE.store(UNAVAILABLE, Ordering::Relaxed);
    }

    /// 会话级关闭 H.265 硬件编码（初始化失败时调用）。不影响 H.264。
    pub fn mark_unavailable_h265() {
        H265_STATE.store(UNAVAILABLE, Ordering::Relaxed);
    }

    /// 选中的 H.264 硬件编码器名字（诊断用，例如 `h264_amf`）。
    pub fn encoder_name() -> String {
        name_of(&H264_NAME)
    }

    /// 选中的 H.265 硬件编码器名字（诊断用，例如 `hevc_amf`）。
    pub fn encoder_name_h265() -> String {
        name_of(&H265_NAME)
    }

    // ── 码率上限：实测得到，不用常数 ──
    //
    // 「编码器能吃多少码率」因机器/驱动而异，无法查表：同一份代码在一台机器上
    // 200 Mbps 正常、在另一台上直接 `incompatible video parameters (-14)`
    // （`tests/hw_bitrate_limit` 记录了两种机器的实测差异）。所以这里在启动时
    // 让编码器自己投票：二分搜索出「建得出**并且**能编出一帧」的最高码率。
    //
    // 只建不编不够 —— 现场那次崩溃正发生在 `avcodec_send_frame`，编码器是建成功的。
    static H264_MAX_KBPS: AtomicU32 = AtomicU32::new(0);
    static H265_MAX_KBPS: AtomicU32 = AtomicU32::new(0);
    /// 建不出来 / 编不出的最低码率（kbps）。生效上限取它的下一个可用值。
    static H264_BAD_KBPS: AtomicU32 = AtomicU32::new(0);
    static H265_BAD_KBPS: AtomicU32 = AtomicU32::new(0);

    /// 探测的搜索区间（kbps）。这是**搜索范围**，不是策略上限 ——
    /// 真正生效的值由编码器自己决定。
    const MAX_BITRATE_SEARCH_LOW: u32 = 1_000;
    const MAX_BITRATE_SEARCH_HIGH: u32 = 200_000;

    fn max_slot(codec: crate::video::vpx::SosCodec) -> Option<&'static AtomicU32> {
        use crate::video::vpx::SosCodec;
        match codec {
            SosCodec::H264 => Some(&H264_MAX_KBPS),
            SosCodec::H265 => Some(&H265_MAX_KBPS),
            _ => None,
        }
    }

    fn bad_slot(codec: crate::video::vpx::SosCodec) -> Option<&'static AtomicU32> {
        use crate::video::vpx::SosCodec;
        match codec {
            SosCodec::H264 => Some(&H264_BAD_KBPS),
            SosCodec::H265 => Some(&H265_BAD_KBPS),
            _ => None,
        }
    }

    /// 该编码器实测的码率上限（kbps）。`None` = 还不知道，调用方不做钳制。
    ///
    /// 结果同时受两个来源约束：探测到的最高可用值，以及运行中失败的更低值
    /// （建不出来或编不出时记录，见 [`Self::record_rejected_kbps`]）。
    pub fn max_bitrate_kbps(codec: crate::video::vpx::SosCodec) -> Option<u32> {
        let ok = max_slot(codec)
            .map(|a| a.load(Ordering::Relaxed))
            .filter(|v| *v > 0);
        let bad = bad_slot(codec)
            .map(|a| a.load(Ordering::Relaxed))
            .filter(|v| *v > 0);
        match (ok, bad) {
            // 失败点比已知可用点更低 ⇒ 以失败点为准（留一格）
            (Some(ok), Some(bad)) => Some(ok.min(bad.saturating_sub(1))),
            (Some(ok), None) => Some(ok),
            (None, Some(bad)) => Some(bad.saturating_sub(1)),
            (None, None) => None,
        }
    }

    /// 任一已探测编码器的最高可用码率，用于「网卡速率读不出来」时兜底。
    ///
    /// 为什么不用一个常数：那等于再猜一个值。编码器上限是我们**实测**出来的，
    /// 在没有任何链路信息时，它是这台机器唯一可信的产出能力依据。
    /// 两个编码器都没探出来则返回 `None`（调用方不做钳制）。
    pub fn max_bitrate_kbps_any() -> Option<u32> {
        [H264_MAX_KBPS.load(Ordering::Relaxed), H265_MAX_KBPS.load(Ordering::Relaxed)]
            .into_iter()
            .filter(|v| *v > 0)
            .max()
    }

    /// 记录一个**失败**的码率：生效上限会被压到它下面。
    fn record_rejected_kbps(codec: crate::video::vpx::SosCodec, kbps: u32) {
        if let Some(slot) = bad_slot(codec) {
            let mut cur = slot.load(Ordering::Relaxed);
            while cur == 0 || kbps < cur {
                match slot.compare_exchange_weak(cur, kbps, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(actual) => cur = actual,
                }
            }
        }
    }

    /// 某个码率能不能真的用起来：建得出编码器**并且**能编出一帧。
    fn bitrate_works(name: &str, width: usize, height: usize, kbps: u32) -> bool {
        let Ok(mut encoder) = SosHwEncoder::build(name, width, height, kbps, 60) else {
            return false;
        };
        // 按编码器自己给的布局造一帧 NV12（Y 有渐变、色度中性），编出去看结果。
        let stride_y = encoder.linesize[0] as usize;
        let stride_uv = *encoder.linesize.get(1).unwrap_or(&(stride_y as i32)) as usize;
        let end_u = encoder.offset[0] as usize + stride_uv * (height / 2).max(1);
        let end_v = encoder.offset[1] as usize + stride_uv * (height / 2).max(1);
        let mut buf = vec![128u8; end_u.max(end_v).max(stride_y * height).max(width * height)];
        for row in 0..height {
            let dst = row * stride_y;
            if dst + width <= buf.len() {
                buf[dst..dst + width].fill((row % 251) as u8);
            }
        }
        encoder.encode(&buf, 33).is_ok()
    }

    /// 二分搜索该编码器能吃下的最高码率，成功后写入 [`Self::max_bitrate_kbps`] 的状态。
    fn probe_max_bitrate(codec: crate::video::vpx::SosCodec, name: &str, width: usize, height: usize) {
        if name.is_empty() {
            return;
        }
        let start = std::time::Instant::now();
        let mut lo = MAX_BITRATE_SEARCH_LOW;
        if !bitrate_works(name, width, height, lo) {
            // 连最低档都用不了，说明这台机器的失败与码率无关，不发布上限。
            log::warn!("[HWENC] {} rejected even {}kbps, skip bitrate probe", name, lo);
            return;
        }
        let mut hi = MAX_BITRATE_SEARCH_HIGH;
        if bitrate_works(name, width, height, hi) {
            lo = hi;
        } else {
            // 区间收敛到 2 Mbps 就够用了：再细分对码率选择没有意义。
            while hi - lo > 2_000 {
                let mid = lo + (hi - lo) / 2;
                if bitrate_works(name, width, height, mid) {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
        }
        if let Some(slot) = max_slot(codec) {
            slot.store(lo, Ordering::Relaxed);
        }
        log::info!(
            "[HWENC] {} max bitrate probed {}kbps at {}x{} (took {:?})",
            name,
            lo,
            width,
            height,
            start.elapsed()
        );
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
                // 一次探测得出两个结论：`available_encoders` 本来就同时枚举 h264 与 hevc，
                // 没必要为了 H.265 再建一遍编码器（那是几百毫秒到数秒的开销）。
                let infos = CodecInfo::prioritized(Encoder::available_encoders(ctx, None));
                let report = |label: &str,
                                  info: Option<&CodecInfo>,
                                  state: &AtomicU8,
                                  name: &std::sync::Mutex<String>| {
                    match info {
                        Some(info) => {
                            if let Ok(mut slot) = name.lock() {
                                *slot = info.name.clone();
                            }
                            state.store(AVAILABLE, Ordering::Relaxed);
                            log::info!(
                                "[HWENC] {} available (probe took {:?})",
                                info.name,
                                start.elapsed()
                            );
                        }
                        None => {
                            state.store(UNAVAILABLE, Ordering::Relaxed);
                            log::info!(
                                "[HWENC] no hardware {} encoder (probe took {:?})",
                                label,
                                start.elapsed()
                            );
                        }
                    }
                };
                report("H.264", infos.h264.as_ref(), &H264_STATE, &H264_NAME);
                report("H.265", infos.h265.as_ref(), &H265_STATE, &H265_NAME);

                // 码率上限放在**可用性发布之后**再测：那要额外建/编十几次编码器
                // （数百毫秒到数秒），不能让「硬件编码什么时候生效」等它。
                // 测完之前 `max_bitrate_kbps()` 返回 None，调用方不做钳制，
                // 真遇到吃不下时还有 `SosHwEncoder::new` 的减半重试兜底。
                let probe_size = (1920usize, 1080usize);
                if let Some(info) = infos.h264.as_ref() {
                    probe_max_bitrate(
                        crate::video::vpx::SosCodec::H264,
                        &info.name,
                        probe_size.0,
                        probe_size.1,
                    );
                }
                if let Some(info) = infos.h265.as_ref() {
                    probe_max_bitrate(
                        crate::video::vpx::SosCodec::H265,
                        &info.name,
                        probe_size.0,
                        probe_size.1,
                    );
                }
            });
        if let Err(e) = spawned {
            log::warn!("[HWENC] failed to spawn probe thread: {}", e);
        }
    }

    /// 硬件编码器（H.264 或 H.265，由 `codec` 区分）。
    pub struct SosHwEncoder {
        /// 本实例产出的码流格式。
        ///
        /// 必须随实例一起保存：上层要用它决定把码流放进 `VideoFrame` 的哪个字段
        /// （`h264s` / `h265s`）。放错字段控制端会按错误的格式解码。
        codec: crate::video::vpx::SosCodec,
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
            codec: crate::video::vpx::SosCodec,
            width: usize,
            height: usize,
            kbps: u32,
            keyframe_interval: usize,
        ) -> ResultType<Self> {
            use crate::video::vpx::SosCodec;
            let (name, label) = match codec {
                SosCodec::H264 => (encoder_name(), "H.264"),
                SosCodec::H265 => (encoder_name_h265(), "H.265"),
                // 只有这两种走硬件后端（`video::ActiveEncoder` 保证）
                _ => anyhow::bail!("{:?} is not a hardware codec", codec),
            };
            if name.is_empty() {
                anyhow::bail!("no {} hardware encoder available", label);
            }
            // 与 VP9 保持一致的周期 IDR（数的是**送进编码器的帧数**，不是墙钟）。
            // 不设会让 GOP 变成 `i32::MAX`：只有「重建编码器」那一刻才出关键帧，
            // 中途接入的控制端要等到下一次重建才拿得到 IDR。
            let gop = keyframe_interval.max(1) as i32;
            // 钳到实测上限（可能还没测出来，那就是 None = 不钳）
            let mut kbps = match max_bitrate_kbps(codec) {
                Some(max) if kbps > max => {
                    log::info!(
                        "[HWENC] {} requested {}kbps above probed max {}kbps, clamping",
                        name,
                        kbps,
                        max
                    );
                    max
                }
                _ => kbps,
            };
            // 吃不下就减半重试：探测是**在 1080p 上做的**，实际分辨率/帧率不同
            // 时上限也会变，所以运行期还得留一条兜底路径。
            let mut attempt = 0;
            let encoder = loop {
                match Self::build(&name, width, height, kbps, gop) {
                    Ok(enc) => break enc,
                    Err(e) => {
                        record_rejected_kbps(codec, kbps);
                        if attempt >= 3 || kbps <= 1_000 {
                            return Err(e);
                        }
                        let next = (kbps / 2).max(1_000);
                        log::warn!(
                            "[HWENC] {} cannot run at {}kbps ({}), retrying at {}kbps",
                            name,
                            kbps,
                            e,
                            next
                        );
                        kbps = next;
                        attempt += 1;
                    }
                }
            };
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
                codec,
                name,
                width,
                height,
                gop,
                encoder,
                kbps,
            })
        }

        /// 本实例产出的码流格式（上层据此选 `h264s` / `h265s`）。
        pub fn codec(&self) -> crate::video::vpx::SosCodec {
            self.codec
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
            // 与 `new` 同一套钳制：运行期的码率更新走的是这里，不钳制等于上限失效。
            let kbps = match max_bitrate_kbps(self.codec) {
                Some(max) if kbps > max => max,
                _ => kbps,
            };
            if kbps == self.kbps {
                return Ok(());
            }
            self.kbps = kbps;
            self.encoder.set_bitrate(kbps as i32).map_err(|_| {
                // 设置失败也说明这个码率用不了：记下来，后续创建/更新都会被压低。
                record_rejected_kbps(self.codec, kbps);
                anyhow::anyhow!("failed to set bitrate {}kbps", kbps)
            })
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

    pub fn available_h265() -> bool {
        false
    }

    pub fn mark_unavailable() {}

    pub fn mark_unavailable_h265() {}

    pub fn encoder_name_h265() -> String {
        String::new()
    }

    pub fn encoder_name() -> String {
        String::new()
    }

    pub fn probe_async() {
        log::info!("[HWENC] built without the `hwcodec` feature, software encoding only");
    }

    /// 与真实实现同名同签名；构造永远失败，调用方会回退 VP9。
    pub struct SosHwEncoder;

        /// 没有硬件编码器 ⇒ 没有码率上限可言（调用方不做钳制）。
    pub fn max_bitrate_kbps(_codec: crate::video::vpx::SosCodec) -> Option<u32> {
        None
    }

    /// 同上：没有硬件编码器时没有可用的兜底值。
    pub fn max_bitrate_kbps_any() -> Option<u32> {
        None
    }

    impl SosHwEncoder {
        pub fn new(
            _codec: crate::video::vpx::SosCodec,
            _width: usize,
            _height: usize,
            _kbps: u32,
            _keyframe_interval: usize,
        ) -> ResultType<Self> {
            anyhow::bail!("built without the `hwcodec` feature")
        }

        pub fn codec(&self) -> crate::video::vpx::SosCodec {
            crate::video::vpx::SosCodec::H264
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
pub use enabled::{
    available, available_h265, encoder_name, encoder_name_h265, mark_unavailable,
    mark_unavailable_h265, max_bitrate_kbps, max_bitrate_kbps_any, probe_async, SosHwEncoder,
};
#[cfg(not(feature = "hwcodec"))]
pub use disabled::{
    available, available_h265, encoder_name, encoder_name_h265, mark_unavailable,
    mark_unavailable_h265, max_bitrate_kbps, max_bitrate_kbps_any, probe_async, SosHwEncoder,
};
