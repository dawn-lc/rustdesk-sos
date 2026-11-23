//! RTT 采样滤波（min-filter）。
//!
//! 从 `net::connection` 的 `Session::on_probe_echo` 提出来，让
//! `tests/rtt_filter.rs` 能直接测**生产实现**（此前测试里复刻了一份同样的窗口逻辑）。

use std::collections::VecDeque;

/// 一次探测往返的合法上限。超过它说明回显迟到（对端主线程忙），不能当网络延迟用。
pub const MAX_TRUSTED_RTT_MS: u32 = 2000;

/// min-filter 窗口长度。取 5：容忍连续 4 个异常样本。
///
/// 回显时间不等于网络延迟。控制端主线程忙（比如正在解码）时，回显可能迟到
/// 一秒以上；实测日志里出现过 `rtt=882ms`，而同一时刻视频链路一切正常。
/// 单靠上限不行：实测尖峰可达 490-725ms，远低于任何合理的“异常”阈值；
/// 中值滤波也不够：尖峰常常连续 3 个以上（解码大帧时主线程会忙好几秒）。
///
/// 关键洞察：**排队延迟永远为正**，真实往返时间是下界。所以用窗口最小值
/// （min-filter）估计 —— 只有窗口内所有样本都变高，才认为网络真的恶化。
/// 代价是恶化要走满一个窗口才反映，但远程桌面场景下「反应慢」远比
/// 「误判拥塞」安全。
pub const RTT_WINDOW: usize = 5;

/// 滑动窗口最小值滤波。
#[derive(Default)]
pub struct RttFilter {
    window: VecDeque<u32>,
}

impl RttFilter {
    /// 送入一次原始往返耗时，返回 `(rtt 下界, 排队延迟)`。
    ///
    /// 返回 `None` 表示样本不可信（0 或超过 [`MAX_TRUSTED_RTT_MS`]），已丢弃 ——
    /// 调用方不能把它当网络延迟用，也不能用它推进拥塞判定。
    pub fn push(&mut self, raw: u32) -> Option<(u32, u32)> {
        if raw == 0 || raw > MAX_TRUSTED_RTT_MS {
            return None;
        }
        if self.window.len() >= RTT_WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(raw);
        let floor = self.window.iter().copied().min().unwrap_or(raw);
        // 排队延迟 = 本次往返 - RTT 下界。它恒为正，直接反映本地出口是否积压，
        // 与路径长短无关 —— 用它区分「有线内网」与「低延迟 Wi-Fi」。
        Some((floor, raw.saturating_sub(floor)))
    }

    /// 当前窗口内的样本数（诊断用）。
    pub fn len(&self) -> usize {
        self.window.len()
    }
}
