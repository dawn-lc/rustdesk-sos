//! 码率上限：按**本机网卡最高链路速率**推导。
//!
//! 为什么需要上限：控制端的自定义画质最高可以开到 2000%（`custom_image_quality = 2000 << 8`），
//! 换算出的码率倍率是 40x（上游同样钳到 40）。对 1080p 再叠上充裕链路的 ×2 就是
//! **200 Mbps** —— 远超任何实际需要，也把编码器打崩过（见下）。
//!
//! 本模块只管**链路能承载多少**；「编码器能产出多少」由 `video::hwenc` 在启动时
//! 实测（`hwenc::max_bitrate_kbps`），不再写死常数：同一份代码在一台机器上 200 Mbps
//! 正常、在另一台上直接 `incompatible video parameters (-14)`，查表算不出来。
//!
//! 最终目标码率 = min(网络上限, 编码器实测上限)，两处各自钳制。

use std::sync::OnceLock;

/// 链路速率里可用于视频的比例：给协议开销、突发和同机其它流量留余量。
const LINK_FRACTION: f32 = 0.8;

/// 网卡速率读不到时的兜底上限（kbps）。
///
/// 这不是「编码器上限」，只是在拿不到链路信息时避免给出无意义的巨值；
/// 正常情况（能读到网卡速率）永远走 `LINK_FRACTION × 链路速率`。
const FALLBACK_CAP_KBPS: u32 = 40_000;

/// 上限的绝对下限（kbps）：网卡速率异常时不至于把码率压死。
const MIN_CAP_KBPS: u32 = 1_000;

#[cfg(windows)]
extern "C" {
    /// 所有已连通物理网卡里最高的发送链路速率（bit/s），0 = 查不到。
    /// 实现在 `platform/windows.cc`（`GetAdaptersAddresses`）。
    fn SosMaxLinkSpeedBps() -> u64;
}

/// 纯函数：链路速率（kbps）→ 码率上限（kbps）。
///
/// `link_kbps == 0`（查不到）时退回 [`FALLBACK_CAP_KBPS`]。
pub fn cap_kbps_for_link(link_kbps: u32) -> u32 {
    if link_kbps == 0 {
        return FALLBACK_CAP_KBPS;
    }
    let by_link = (link_kbps as f32 * LINK_FRACTION) as u32;
    by_link.max(MIN_CAP_KBPS)
}

/// 本机网卡最高链路速率（kbps）。查不到返回 `None`。
fn max_link_kbps() -> Option<u32> {
    #[cfg(windows)]
    {
        // SAFETY: 纯查询函数，不持有任何指针，返回值按 bit/s 解释。
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

/// 当前生效的网络侧上限（kbps）。首次调用时查询网卡，之后走缓存。
///
/// 网卡速率在会话期间不会变（换网卡/插拔网线会重建管线），所以只查一次；
/// 这同时避免了在每秒的码率计算路径上反复调用 IP Helper。
pub fn cap_kbps() -> u32 {
    static CAP: OnceLock<u32> = OnceLock::new();
    *CAP.get_or_init(|| {
        let link = max_link_kbps();
        let cap = cap_kbps_for_link(link.unwrap_or(0));
        match link {
            Some(link) => log::info!(
                "[BITRATE] link {} Mbps -> network cap {} kbps",
                link / 1000,
                cap
            ),
            None => log::info!(
                "[BITRATE] link speed unavailable -> fallback cap {} kbps",
                cap
            ),
        }
        cap
    })
}
