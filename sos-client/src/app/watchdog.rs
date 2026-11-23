//! SYSTEM 子进程存活看门狗。
//!
//! 基于进程句柄的 `WaitForSingleObject`，零轮询、零心跳；子进程死亡时自动重建
//! 管道 + 重启。启动期失败用指数退避压制重启频率（见常量注释）。

// ── SYSTEM 子进程看门狗参数 ──
//
// 命名沿用 `video.rs` 约定的 `*_UPTIME` / `*_BACKOFF` / `MAX_*`：
// 单位在名字里，不带单位的数值不写常量。

/// 子进程「活着多久才算正常启动」。低于它视为启动期失败。
const SUBPROC_MIN_UPTIME: std::time::Duration = std::time::Duration::from_secs(10);

/// 启动期失败后首次重启前的等待，之后逐次翻倍。
const SUBPROC_RESTART_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// 重启退避上限（也决定了最大翻倍次数：1s→30s 约 5 次）。
const SUBPROC_MAX_RESTART_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// 退避指数的上限。`1 << 5 = 32`，配合 1s 基数恰好触到 30s 封顶，
/// 再大的移位只会溢出而无意义。
const SUBPROC_MAX_BACKOFF_SHIFT: u32 = 5;

#[cfg(windows)]
pub fn spawn(sub_proc_handle: isize, debug: bool) {
    if sub_proc_handle == 0 {
        return;
    }
    std::thread::spawn(move || {
        // FFI 声明（看门狗专用，避免引入额外 crate 依赖）
        extern "system" {
            fn WaitForSingleObject(h: isize, ms: u32) -> u32;
            fn CloseHandle(h: isize) -> i32;
        }
        const INFINITE: u32 = 0xFFFFFFFF;
        const WAIT_OBJECT_0: u32 = 0;

        let mut current_handle = sub_proc_handle;
        let mut started = std::time::Instant::now();
        let mut rapid_restarts = 0u32;
        loop {
            log::info!(
                "Watchdog: waiting on sub-process handle 0x{:x}",
                current_handle
            );
            let ret = unsafe { WaitForSingleObject(current_handle, INFINITE) };
            if ret != WAIT_OBJECT_0 {
                log::error!("Watchdog: WaitForSingleObject failed, ret={}", ret);
                break;
            }
            unsafe {
                CloseHandle(current_handle);
            }
            let lived = started.elapsed();
            log::warn!("SYSTEM sub-process exited after {:?}, restarting...", lived);

            // 子进程活着的时间太短，说明它在**启动阶段**就失败了
            // （拿不到 winlogon 令牌、管道被另一个实例占住、缺少权限……）。
            // 此时立刻重启只会把同一个失败刷成每秒一轮的循环，而每轮
            // 都要 OpenProcess(winlogon) + CreateProcessWithTokenW，
            // 足以让安全软件的进程/令牌防护持续告警。指数退避把频率压下去，
            // 同时也让日志里能看清真正的错误而不再被重启刷屏。
            if lived < SUBPROC_MIN_UPTIME {
                rapid_restarts = (rapid_restarts + 1).min(SUBPROC_MAX_BACKOFF_SHIFT);
            } else {
                rapid_restarts = 0;
            }
            if rapid_restarts > 0 {
                let factor = 1u32 << (rapid_restarts - 1);
                let delay = (SUBPROC_RESTART_BACKOFF * factor).min(SUBPROC_MAX_RESTART_DELAY);
                log::warn!(
                    "Watchdog: restart streak={}, backing off {:?} before relaunch",
                    rapid_restarts,
                    delay
                );
                std::thread::sleep(delay);
            }

            // 重建输入管道
            let (pt, _) = crate::ipc::pipe::create_pipe_server(
                crate::app::constants::INPUT_PIPE,
                crate::ipc::pipe::PIPE_ACCESS_OUTBOUND,
            );
            crate::ipc::pipe::set_down_tx(pt);

            // 启动新的 SYSTEM 子进程
            let new_handle = crate::app::system::launch_system_sub_process(
                debug,
                crate::ipc::shmem::uses_global_namespace(),
            );
            if new_handle == 0 {
                log::error!("Watchdog: failed to restart sub-process, exiting");
                break;
            }
            current_handle = new_handle;
            started = std::time::Instant::now();
        }
        log::warn!("Watchdog: exited");
    });
}
