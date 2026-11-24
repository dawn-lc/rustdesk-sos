//! 目标码率上限：按**本机网卡最高链路速率**定，再叠加编码器物理上限。
//!
//! 为什么需要上限：控制端的自定义画质最高可以开到 2000%（`custom_image_quality = 2000 << 8`），
//! 换算出的码率倍率是 40x（上游同样钳到 40）。对 1080p 再叠上充裕链路的 ×2，就是
//! **200 Mbps** —— 实测直接把硬件编码器打崩：
//!
//! ```text
//! [VIDEO] display #0 ratio 40.00 ... -> bitrate 200000kbps
//! [ERROR] Error during resetting: incompatible video parameters (-14)
//! [ERROR] [FFMPEG_RAM_ENC] avcodec_send_frame failed, ret = Invalid argument
//! ```
//!
//! 之后的每次重建都带着同一个不可能的码率，画面持续黑屏。
//!
//! 上限取三者较小值：
//!
//! 1. **网卡最高链路速率 × [`LINK_FRACTION`]** —— 超过物理链路的码率没有意义；
//!    百兆网（100 Mbps）与千兆网的可用带宽本来就不同；
//! 2. **编码器等级上限** [`ENCODER_CEILING_KBPS`] —— 与网卡无关：HEVC Level 4.1 在
//!    1080p 上就规定了 50 Mbps 的上界，编码器会直接拒绝更高的码率（`-14`）；
//! 3. 下限 [`MIN_CAP_KBPS`] —— 防止异常环境（速率读成 0/极小）把码率压死。
//!
//! 千兆网 + 1080p 时实际生效的是编码器上限；百兆网时网卡先成为瓶颈 —— 正是本模块要的效果。

use std::sync::OnceLock;

/// 链路速率里可用于视频的比例：给协议开销、突发和同机其它流量留余量。
const LINK_FRACTION: f32 = 0.8;

/// 编码器上限（kbps）。
///
/// 1080p 默认工作点（2500kbps）的 16 倍，4K 高质量场景也够用。
///
/// **为什么需要一个与网卡无关的上限**：现场一台 QSV 机器在收到 200 Mbps 时崩了 ——
///
/// ```text
/// [ERROR] Error during resetting: incompatible video parameters (-14)
/// [ERROR] [FFMPEG_RAM_ENC] avcodec_send_frame failed, ret = Invalid argument
/// ```
///
/// 而 `tests/hw_bitrate_limit` 在开发机上实测 h264_qsv / hevc_qsv 连 200 Mbps 都接受 ——
/// 说明这是**机器/驱动相关**的失败，不是某个统一的标准上限，也无法靠查表提前算出。
/// 所以取一个保守值：网卡再快也不越线，宁可少用带宽也不能让编码器进不去。
/// 将来若确认某类编码器的真实上界，调这个常量即可（两个数量级都留了余量）。
pub const ENCODER_CEILING_KBPS: u32 = 40_000;

/// 上限的绝对下限（kbps）：网卡速率读不出来或异常时不至于把码率压死。
const MIN_CAP_KBPS: u32 = 1_000;

#[cfg(windows)]
extern "C" {
    /// 所有已连通物理网卡里最高的发送链路速率（bit/s），0 = 查不到。
    /// 实现在 `platform/windows.cc`（`GetAdaptersAddresses`）。
    fn SosMaxLinkSpeedBps() -> u64;
}

/// 纯函数：链路速率（kbps）→ 目标码率上限（kbps）。
///
/// `link_kbps == 0`（查不到）时只保留编码器上限，这是最保守且可用的选择。
pub fn cap_kbps_for_link(link_kbps: u32) -> u32 {
    if link_kbps == 0 {
        return ENCODER_CEILING_KBPS;
    }
    let by_link = (link_kbps as f32 * LINK_FRACTION) as u32;
    by_link.min(ENCODER_CEILING_KBPS).max(MIN_CAP_KBPS)
}

/// 本机网卡最高链路速率（kbps）。查不到返回 `None`。
fn max_link_kbps() -> Option<u32> {
    #[cfg(windows)]
    {
        // SAFETY: 纯查询函数，不持有任何指针，返回值由调用方按位/s 解释。
        let bps = unsafe { SosMaxLinkSpeedBps() };
        if bps == 0 {
            return None;
        }
        // bit/s → kbit/s，注意别溢出 u32（10 Gbps = 1e10 bit/s > u32::MAX）
        Some((bps / 1000).min(u32::MAX as u64) as u32)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 当前生效的码率上限（kbps）。首次调用时查询网卡，之后走缓存。
///
/// 网卡速率在会话期间不会变（换网卡/插拔网线重启机会重建管线），
/// 所以只查一次；这同时避免了在每秒的码率计算路径上反复调用 IP Helper。
pub fn cap_kbps() -> u32 {
    static CAP: OnceLock<u32> = OnceLock::new();
    *CAP.get_or_init(|| {
        let link = max_link_kbps();
        let cap = cap_kbps_for_link(link.unwrap_or(0));
        match link {
            Some(link) => log::info!(
                "[BITRATE] link {} Mbps -> cap {} kbps (encoder ceiling {} kbps)",
                link / 1000,
                cap,
                ENCODER_CEILING_KBPS
            ),
            None => log::info!(
                "[BITRATE] link speed unavailable -> cap {} kbps (encoder ceiling)",
                cap
            ),
        }
        cap
    })
}
