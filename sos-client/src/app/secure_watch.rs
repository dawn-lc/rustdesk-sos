//! 安全桌面（锁屏 / UAC）监听线程。
//!
//! 从 `app::run` 抽出：这段是全项目最容易踩坑的一处 —— 钩子只是加速器，
//! 必须保留有限超时轮询；`secure_desktop()` 检查的是**调用线程**的桌面，
//! 所以本线程永远不能调 `SetThreadDesktop`。详见各常量与下文注释。

/// 桌面切换钩子不可用时的轮询间隔。
///
/// `SosWaitForDesktopSwitch` 在钩子注册失败时会**立刻**返回 -1，
/// 调用方若直接在循环里重试就变成忙等 —— 实测这种状态下单个线程能吃掉
/// 整个核心（4 核机器上正好 25%），并且每轮还附带一次全进程快照。
const UAC_FALLBACK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// 安全桌面激活后等待 SYSTEM 子进程首帧的单次时长。
const SECURE_READY_WAIT_MS: u32 = 500;

/// watcher 线程等待桌面切换的**上限**。
///
/// 钩子（`EVENT_SYSTEM_DESKTOPSWITCH`）只作为「加速器」：事件到了立刻醒，
/// 没事件也必须每秒醒一次重新评估。
///
/// 现场日志证明只靠事件会失效：机器在 570s 左右锁屏，`video` 在
/// 583s 已经判定安全桌面，而 watcher 一直静默到 606s（客户端断开时才收到
/// 事件）—— 25 秒内 SYSTEM 采集从未启动，控制端全程黑屏。
///
/// 会话被锁 / RDP 断开后，本会话不再收到 WinEvent，这是可复现的行为，
/// 不是偶发丢事件。所以这里的上限是**必须**的，不能退回 `INFINITE`。
const SECURE_POLL_INTERVAL_MS: u32 = 1000;

#[cfg(windows)]
pub fn spawn(capture_start_event: isize, capture_stop_event: isize) {
    let uac_w = {
        scrap::Display::all()
            .unwrap_or_default()
            .first()
            .map(|d| d.width() as usize)
            .unwrap_or(1280)
    };
    let uac_h = {
        scrap::Display::all()
            .unwrap_or_default()
            .first()
            .map(|d| d.height() as usize)
            .unwrap_or(720)
    };

    // 创建命名自动重置事件：捕获线程写完首帧后 SetEvent
    let event_name_wide: Vec<u16> = crate::ipc::shmem::scoped_name(crate::app::constants::CAPTURE_READY_EVENT)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let ready_event = unsafe {
        extern "system" {
            fn CreateEventW(
                sa: *const u8,
                manual: i32,
                initial: i32,
                name: *const u16,
            ) -> isize;
        }
        CreateEventW(std::ptr::null(), 0, 0, event_name_wide.as_ptr())
    };

    std::thread::spawn(move || {
        extern "system" {
            fn WaitForSingleObject(h: isize, ms: u32) -> u32;
            fn SetEvent(h: isize) -> i32;
        }
        const WAIT_OBJECT_0: u32 = 0;

        // 判据是 `crate::capture::uac::secure_desktop()`：
        // 1. 主判据 `desktop_changed()`（本线程桌面名 != 输入桌面名），
        //    与上游一致，锁屏与 UAC 一并覆盖；
        // 2. 兜底 `consent_running()`，也就是自建进程快照。上游这里写的是
        //    `cmd /C tasklist | findstr consent.exe` —— 每次要创建
        //    cmd.exe + tasklist.exe + findstr.exe 并等它们退出，实测单次
        //    205ms；快照版 1-3ms。
        //
        // 为什么这件事不只是"慢"：每次进程创建/销毁都会产生一次
        // 进程实例事件，系统上任何注册了 `Win32_Process` 变更通知的
        // 组件（杀毒、EDR、SCCM/Intune、OEM 管理软件）都会被唤醒，
        // 这些回调在 WmiPrvSE.exe 里执行。桌面切换事件在锁屏/解锁、
        // UAC 弹出、RDP 连接与断开时都会触发，反复拉进程会让
        // WmiPrvSE 的 CPU 占用明显抬头。
        //
        // 注意 `secure_desktop()` 检查的是**调用线程**的桌面，所以这个
        // 线程永远不能调用 `SetThreadDesktop`。
        let mut was_uac = crate::capture::uac::secure_desktop();
        // 是否已经把视频切到「SYSTEM 子进程写 SHMEM」这条通路。
        // 必须等首帧真的到位之后才切 —— 否则视频会取到一张永不更新的
        // 共享内存帧，画面直接冻结（比停在旧画面上更糟）。
        let mut secure_ready = false;
        // 钩子是否处于可用状态。诊断用的坑，但也是节流闸门：
        // 不可用时只打一条日志，之后退化成定时轮询。
        let mut hook_ok = true;
        // 「等待首帧」日志只打一次，避免锁屏长时间期间刷屏
        let mut secure_wait_logged = false;

        // 启动时就已处于安全桌面（开机自启 + 机器锁着）：
        // 这个状态下不会再发生「桌面切换」，钩子永远等不到事件，
        // 所以必须在这里主动请求一次。
        if was_uac {
            log::info!(
                "[SECURE] 启动时已处于安全桌面 → SetEvent(CAPTURE_START) ({})",
                crate::capture::uac::desktop_state()
            );
            unsafe { SetEvent(capture_start_event) };
        }

        loop {
            // **永远**用有限超时，不能再用 INFINITE。
            //
            // 钩子只是「加速器」，不能当唯一信号源。只靠事件会漏掉两种
            // 真实情况，而且都发生在最需要采集的时候：
            //
            // 1. 进程启动时就已经是安全桌面 —— 切换事件早就过去了；
            // 2. **会话被锁 / RDP 断开后，本会话不再收到任何 WinEvent**。
            //
            // 现场日志就是第 2 种：`video` 在 583s 已经判定
            // `secure desktop active = true`，而这里的钩子一直静默，
            // 直到 606s 客户端断开后才第一次触发 —— 25 秒内 SHMEM 从没
            // 被注册，控制端全程黑屏（见下面 "SOS SHMEM not registered" 循环）。
            //
            // 代价：每秒一次 `desktop_changed()`（几个句柄操作）+ 最坏
            // 每秒一次进程快照（已限流到 1s）。远小于一次白跑的显示器枚举。
            let sw = unsafe { crate::app::system::SosWaitForDesktopSwitch(SECURE_POLL_INTERVAL_MS) };
            if sw < 0 {
                if hook_ok {
                    hook_ok = false;
                    log::error!(
                        "[UAC] 桌面切换钩子不可用，退化为 {:?} 定时轮询（安全桌面仍可检测）",
                        UAC_FALLBACK_POLL_INTERVAL
                    );
                }
                std::thread::sleep(UAC_FALLBACK_POLL_INTERVAL);
            } else if !hook_ok {
                hook_ok = true;
                log::info!("[UAC] 桌面切换钩子已恢复");
            }

            let uac = crate::capture::uac::secure_desktop();

            if uac && !was_uac {
                log::info!(
                    "[SECURE] 安全桌面激活（锁屏 / UAC）→ SetEvent(CAPTURE_START) ({})",
                    crate::capture::uac::desktop_state()
                );
                was_uac = true;
                // 通知 SYSTEM 子进程开始往 SHMEM 写帧
                unsafe { SetEvent(capture_start_event) };
            } else if !uac && was_uac {
                log::info!(
                    "[SECURE] 安全桌面结束 → SetEvent(CAPTURE_STOP) ({})",
                    crate::capture::uac::desktop_state()
                );
                was_uac = false;
                unsafe { SetEvent(capture_stop_event) };
                if secure_ready {
                    secure_ready = false;
                    #[cfg(sos_vpx)]
                    crate::video::set_uac_active(false);
                }
            }

            // 安全桌面期间：等 SYSTEM 子进程的首帧，到了再把视频切过去。
            // 没到就下一轮继续等（子进程那边会重试，不会退出）。
            if uac && !secure_ready {
                // 重复 SetEvent 是幂等的（子进程侧只是把 active 置 true），
                // 但可以覆盖「子进程刚被看门狗重启，错过了最初的 START 信号」
                // 这种情况 —— 否则新子进程会一直暂停在锁屏界面。
                unsafe { SetEvent(capture_start_event) };
                let ret = unsafe { WaitForSingleObject(ready_event, SECURE_READY_WAIT_MS) };
                if ret != WAIT_OBJECT_0 {
                    // 等不到首帧是「子进程没起来 / 采不到 / 在别的 session」
                    // 的直接证据，必须打到 INFO 并带上桌面状态，
                    // 否则现场只能看到控制端黑屏而日志一片安静。
                    if !secure_wait_logged {
                        secure_wait_logged = true;
                        log::warn!(
                            "[SECURE] 已请求 SYSTEM 采集但首帧未到，继续等待（{}）",
                            crate::capture::uac::desktop_state()
                        );
                    }
                    continue;
                }
                secure_wait_logged = false;
                match crate::ipc::shmem::SosShmem::open_existing(crate::ipc::shmem::SHMEM_NAME) {
                    Ok(shmem) => {
                        crate::ipc::shmem::register_shmem(shmem, uac_w, uac_h);
                        #[cfg(sos_vpx)]
                        crate::video::set_uac_active(true);
                        secure_ready = true;
                        log::info!("[SECURE] SYSTEM 子进程首帧就绪 → 切换到共享内存采集");
                    }
                    Err(e) => {
                        log::warn!("[SECURE] SHMEM 打开失败({}), 保持本地采集", e);
                    }
                }
            } else if !uac {
                secure_wait_logged = false;
            }
        }
    });
}
