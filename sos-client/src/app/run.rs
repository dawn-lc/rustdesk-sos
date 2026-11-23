//! 进程入口逻辑：CLI 解析、模式分发、主进程装配、SYSTEM 子进程看门狗。
//!
//! 这里是原来的 `main.rs` 主体；拆进库以后 `main.rs` 只剩 `#[tokio::main]` 三行，
//! 这段逻辑因此能被 `tests/*.rs` 当普通库代码使用（不再靠 `#[path]` 直接包含源文件）。

use clap::Parser;
use hbb_common::ResultType;

/// RustDesk SOS 精简受控端
#[derive(Parser)]
#[command(
    name = "rustdesk-sos",
    version,
    about = "RustDesk SOS 精简受控端 - 单文件远程协助客户端"
)]
struct Cli {
    /// 显示控制台窗口并输出调试日志
    #[arg(short = 'd', long, default_value_t = false)]
    debug: bool,

    /// 管理服务器地址（信令/ID 服务器），如 hbbs.example.com
    #[arg(short = 'm', long, default_value = "")]
    manager: String,

    /// 中继服务器地址（可选，未指定时由管理服务器自动下发）。
    /// 可用逗号分隔多个地址，按顺序逐个尝试。
    #[arg(short = 'r', long, default_value = "")]
    relay: String,

    /// 管理服务器公钥（Base64，可选，使用自定义服务器时必填）
    #[arg(short = 'k', long, default_value = "")]
    key: String,

    /// 固定密码（传入后永久使用，不断开刷新；不传入则每次连接后自动更换）
    #[arg(short = 'p', long, default_value = "")]
    password: String,

    /// 启动模式开关（内部使用：主进程以 SYSTEM 身份拉起自身时传入）
    #[arg(short = 's', long, default_value_t = false)]
    service: bool,

    /// 命名内核对象使用会话内命名空间（内部使用）
    ///
    /// 主进程启用 `SeCreateGlobalPrivilege` 失败时会退回会话内命名，
    /// 并通过本开关告知子进程，避免两侧名字不一致导致连不上。
    #[arg(long, default_value_t = false)]
    no_global: bool,

}

pub async fn run() -> ResultType<()> {
    // 捕获所有 panic，输出到 stderr 防止静默崩溃
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let payload = info.payload();
        let msg = if let Some(s) = payload.downcast_ref::<&str>() {
            *s
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.as_str()
        } else {
            "unknown payload"
        };
        eprintln!("!!! PANIC at {}: {}", loc, msg);
        default_hook(info);
    }));

    let cli = Cli::parse();

    // ── 控制台管理 ──
    // Release 构建使用 GUI 子系统（无控制台），仅 --debug 时显示控制台。
    // AttachConsole 挂接父控制台后标准句柄仍是无效的，需要手动通过
    // CreateFile("CONOUT$") 打开控制台并用 SetStdHandle 重定向。
    crate::app::console::setup(cli.debug, cli.service);

    let log_level = if cli.debug { "info" } else { "warn" };
    // 初始化日志（所有模式都需要）
    if cli.service {
        if cli.debug {
            // SYSTEM 子进程：追加写入 rustdesk-sos-system.log
            crate::app::bootstrap::init_system_logger(log_level);
        } else {
            crate::app::bootstrap::init_logger(log_level);
        }
    } else if cli.debug {
        // 主进程 --debug：同时写控制台与 exe 同目录的 rustdesk-sos.log
        crate::app::bootstrap::init_debug_logger(log_level);
    } else {
        crate::app::bootstrap::init_logger(log_level);
    }

    // ── 模式分发：服务模式 ──
    if cli.service {
        // 与主进程保持一致：主进程启用全局命名空间失败时会传 --no-global
        crate::ipc::shmem::set_global_namespace(!cli.no_global);
        return crate::app::system::run_service();
    }

    // ── 主进程模式 ──

    // 0. 硬性检查：必须以管理员权限运行
    if !crate::app::bootstrap::is_admin() {
        crate::app::bootstrap::show_admin_required_and_exit();
    }

    // 0b. 单实例保护。必须放在创建任何**全局命名对象**之前
    //     （管道 \\.\pipe\input、共享内存 RustDeskSOS_sos、捕获启停事件）。
    //     否则第二个实例会抢同一个管道，两个 SYSTEM 子进程互踢，
    //     看门狗把失败的重启刷成高频循环 —— 详见 acquire_single_instance 的文档。
    #[cfg(windows)]
    if !crate::app::bootstrap::acquire_single_instance() {
        crate::app::bootstrap::show_already_running_and_exit(cli.debug);
    }

    log::info!(
        "{} v{} starting...",
        crate::app::constants::APP_NAME,
        crate::app::constants::VERSION
    );

    // 连接确认：每次连接都要本机用户点「同意」（与密码二选一，谁先完成谁生效）。
    // 锁屏 / UAC / 会话断开时用户看不到窗口，那种情况由
    // `crate::net::connection::local_user_can_approve` 判掉、退回「密码即授权」。
    log::info!(
        "Approval mode: every remote request waits for a local click ({}s timeout), \
         skipped on the secure desktop / disconnected session",
        crate::ui::consent::CONSENT_TIMEOUT.as_secs()
    );

    // 硬件编码器探测：后台线程，不影响启动。探测完成后 `video` 才会在
    // 协商里声明并能选用 H.264；探测没完成时一律按「没有硬件」处理（走 VP9）。
    crate::video::hwenc::probe_async();

    // 1a. 输出主进程令牌诊断
    #[cfg(windows)]
    crate::app::system::log_token_info("MAIN_INIT");

    // 2. 读取/初始化注册表配置
    //    ID 优先级: 注册表 > MAC 地址自动生成
    //    管理服务器优先级: --manager > 注册表 > 默认
    let config = crate::app::config::SosConfig::from_cli_and_registry(
        &cli.manager,
        &cli.relay,
        &cli.key,
        &cli.password,
    )?;
    log::info!("Device ID: {}", config.id);
    log::info!("Rendezvous server: {}", config.rendezvous_server);

    // CPU 使用率采样。
    //
    // 这里**不用**上游的 `start_cpu_performance_monitor()`：它走 PDH 的
    // `\Processor(_total)\% Processor Time`，需要 `PdhCollectQueryDataEx`
    // 定时采集，会进 pdh.dll → perfos.dll 的查询链路。
    //
    // （早期版本在这里写了「每次查询 41ms / 计数器注册损坏」，那是**算错的**：
    //  两阶段 A/B 的实际时长不同（16.3s vs 15.0s），却直接拿原始 ms 差除以
    //  采集次数。做了基准差分后实测为 0.0ms，两台机器一致。
    //  PDH 本身没有问题，不要为此执行 `lodctr /R`。）
    //
    // 换掉它的真实理由只剩两个，与那 41ms 无关：
    //   1. `GetSystemTimes` 是单次内核调用，不碰 pdh.dll 的查询对象，
    //      也没有「查询句柄泄漏就永久失效」这类失效模式；
    //   2. 上游实现把结果放在全局队列里，还需要额外的补种线程。
    //
    // 采样周期仍为 2 秒，并继续把结果同步进上游缓存，
    // 保证任何依赖 `cpu_uage_one_minute()` 的代码行为不变。
    #[cfg(windows)]
    crate::app::cpu::start_cpu_monitor();

    // 编码资源配置诊断。
    //
    // 注意：不能再用 `scrap::codec_thread_num()` 作为参考值。它的算法是
    // `min(cpu/2, available_memory/2)`，而 `available_memory` 是整数 GB，
    // 4GB 虚拟机上报 1G 时 `1/2 = 0`，于是恒定返回 1 线程。
    // 我们的自建编码器不用这个启发式，实测线程数对内存几乎无影响，
    // 真正的开销是分辨率决定的基础工作集（见 video::vpx 的实测表）。
    #[cfg(sos_vpx)]
    {
        let threads = crate::video::vpx::SosVpxConfig::recommended_threads();
        log::info!(
            "encoder: threads={} cpus={} total_mem={}MB avail_mem={}MB (scrap heuristic would say {})",
            threads,
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0),
            crate::video::vpx::total_phys_mb(),
            crate::video::vpx::avail_phys_mb(),
            scrap::codec::codec_thread_num(64),
        );
    }

    // 密码策略：
    //   --password 传入 → 永久固定，不保存注册表，不断开刷新
    //   未传入       → 自动生成临时密码，每次连接断开后刷新
    let cli_password_provided = !config.password.is_empty();
    let temp_pwd = if cli_password_provided {
        config.password.clone()
    } else {
        crate::app::config::generate_temp_password()
    };
    crate::app::config::set_current_password(temp_pwd.clone());
    if cli_password_provided {
        log::info!("Using fixed CLI password (not regenerated on disconnect)");
    } else {
        log::info!(
            "Temporary password (will refresh each connection): {}",
            temp_pwd
        );
    }

    // 4. 创建 SOS 自有 SHMEM（固定名 "sos"，SYSTEM 子进程常驻写帧）
    let _shmem = match crate::ipc::shmem::create_shmem(crate::capture::display::primary_size()) {
        Ok(m) => m,
        Err(e) => {
            log::error!("Failed to create SOS SHMEM: {}", e);
            return Err(e);
        }
    };

    // 创建输入管道服务器（Main → System 输入事件）
    let (pipe_tx, _) =
        crate::ipc::pipe::create_pipe_server(crate::app::constants::INPUT_PIPE, crate::ipc::pipe::PIPE_ACCESS_OUTBOUND);
    crate::ipc::pipe::set_down_tx(pipe_tx);

    // 创建捕获启停命名事件（替代原 CONTROL_PIPE，统一为内核事件模型）
    let (capture_start_event, capture_stop_event) = unsafe {
        extern "system" {
            fn CreateEventW(sa: *const u8, manual: i32, initial: i32, name: *const u16) -> isize;
        }
        let to_wide = |s: &str| {
            s.encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>()
        };
        let start = CreateEventW(
            std::ptr::null(),
            0,
            0,
            to_wide(&crate::ipc::shmem::scoped_name(crate::app::constants::CAPTURE_START_EVENT)).as_ptr(),
        );
        let stop = CreateEventW(
            std::ptr::null(),
            0,
            0,
            to_wide(&crate::ipc::shmem::scoped_name(crate::app::constants::CAPTURE_STOP_EVENT)).as_ptr(),
        );
        (start, stop)
    };

    // 5. 以 SYSTEM 身份启动子进程（常驻），获取进程句柄用于看门狗
    #[cfg(windows)]
    let global_ns = crate::ipc::shmem::uses_global_namespace();
    #[cfg(windows)]
    let sub_proc_handle = crate::app::system::launch_system_sub_process(cli.debug, global_ns);

    // 5a. SYSTEM 子进程存活看门狗：基于进程句柄的 WaitForSingleObject 监控
    //     零轮询、零心跳 — 纯事件驱动。子进程死亡时自动重建管道 + 重启子进程。
    crate::app::watchdog::spawn(sub_proc_handle, cli.debug);

    // 6. 启动 SOS 自建视频链路（替代上游 video_service）
    //    采集 → I420 转换 → 自建 libvpx 编码 → 通道发送。
    //    绕开 scrap 的 codec_thread_num 线程钳制与每帧 flush。
    crate::video::start();
    log::info!("SOS video pipeline started ({})", crate::video::describe());

    // 6a. UAC 监控：SosWaitForDesktopSwitch 内部封装 SetWinEventHook + 消息泵
    //     调用线程阻塞等待桌面切换，零轮询、零 sleep。
    crate::app::secure_watch::spawn(capture_start_event, capture_stop_event);

    // 5b. 启动上游 clipboard_service（剪贴板同步，含文件剪贴板）
    log::info!("Starting upstream clipboard service...");
    let clip_svc = librustdesk::server::clipboard_service::new("clipboard".into());
    crate::app::config::set_clipboard_service(clip_svc);
    log::info!("Clipboard service started (upstream)");

    // 5c. 初始化 cliprdr 上下文（OLE 文件剪贴板引擎，用于远程文件粘贴）
    #[cfg(windows)]
    {
        log::info!("Initializing cliprdr context for file clipboard...");
        clipboard::ContextSend::enable(true);
        if clipboard::ContextSend::is_enabled() {
            log::info!("Cliprdr context initialized successfully");
        } else {
            log::warn!("Cliprdr context initialization failed - file clipboard may not work");
        }
    }

    // 6. 创建托盘（tokio unbounded channel，支持异步 recv，无需轮询）
    let (tray_tx, mut tray_rx) = tokio::sync::mpsc::unbounded_channel::<crate::app::tray::TrayCommand>();
    let tray_tx_for_rendezvous = tray_tx.clone();
    std::thread::spawn(move || {
        if let Err(e) = crate::app::tray::run(tray_tx) {
            log::error!("Tray thread error: {}", e);
        }
    });

    // 8. 创建密码刷新通道（仅未传入 --password 时才需要断开后刷新）
    let (pwd_refresh_tx, mut pwd_refresh_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    // 8. 启动信令注册任务（后台持续运行）
    let rendezvous_config = config.clone();
    let rendezvous_handle = tokio::spawn(async move {
        if let Err(e) =
            crate::net::rendezvous::run(rendezvous_config, tray_tx_for_rendezvous, pwd_refresh_tx).await
        {
            log::error!("Rendezvous service error: {}", e);
        }
    });

    log::info!(
        "SOS client is ready, ID: {}, waiting for incoming connections...",
        config.id
    );

    // 9. 主事件循环：接受托盘命令 + 密码刷新通知 + 保持运行
    loop {
        tokio::select! {
            // 密码刷新通知：连接断开后重新生成临时密码（仅当未通过 --password 固定时生效）
            Some(()) = pwd_refresh_rx.recv() => {
                if cli_password_provided {
                    log::trace!("CLI password is fixed, ignoring password refresh trigger");
                } else {
                    let new_pwd = crate::app::config::generate_temp_password();
                    crate::app::config::set_current_password(new_pwd.clone());
                    log::info!("Connection closed, regenerated temporary password: {}", new_pwd);
                }
            }

            // 托盘命令（事件驱动，零轮询）
            cmd = tray_rx.recv() => {
                match cmd {
                    Some(crate::app::tray::TrayCommand::Exit) | None => {
                        log::info!("Exit requested via tray");
                        break;
                    }
                    Some(crate::app::tray::TrayCommand::ShowInfo) => {
                        log::debug!("Show info requested via tray");
                        crate::app::tray::show_info();
                    }
                }
            }
        }
    }

    // 8. 清理：通知信令任务退出
    rendezvous_handle.abort();

    log::info!("SOS client exiting.");
    Ok(())
}
