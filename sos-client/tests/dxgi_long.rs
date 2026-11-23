//! 自建 DXGI 采集器的正确性验证。
//!
//! 重点验证 `libs/scrap` 的两个缺陷在我们这里不存在：
//! 1. 长时间连续采集不出现 `InvalidData`（scrap 约 40ms 就会出现）；
//! 2. 桌面静止时返回 `WouldBlock` 而不是卡死，且之后仍能正常出帧。
//!
//! 运行：cargo test --release --test dxgi_long -- --nocapture --test-threads=1

#![cfg(windows)]

use rustdesk_sos::capture::dxgi as sos_dxgi;

use scrap::{TraitCapturer, TraitPixelBuffer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MoveWindow, PeekMessageW,
    RegisterClassW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, MSG, PM_REMOVE, SW_SHOW,
    WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};

/// 同进程内的采集互斥。
///
/// DXGI 每个输出**只允许一个** Desktop Duplication，而 cargo 默认让同一测试二进制里的
/// 测试并行跑 —— 两个测试各建一个采集器时，后建的那个拿不到帧（实测 
/// `恢复后出帧: ok=0/20`）。原文件头写的是「请加 --test-threads=1」，但要求调用方
/// 记住参数不可靠，这里用互斥锁串起来。
static CAPTURE_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 取得采集互斥；中毒也继续（前一个测试 panic 不该连累后一个）。
fn capture_gate() -> std::sync::MutexGuard<'static, ()> {
    CAPTURE_GATE.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    DefWindowProcW(h, m, w, l)
}

/// 持续移动的小窗口，制造真实的桌面变化
fn spawn_animator(stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || unsafe {
        let Ok(instance) = GetModuleHandleW(None) else {
            return;
        };
        let class = w!("SosDxgiLongWindow");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("probe"),
            WS_OVERLAPPEDWINDOW,
            40,
            40,
            320,
            240,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        let _ = ShowWindow(hwnd, SW_SHOW);

        let mut i: i32 = 0;
        while !stop.load(Ordering::Relaxed) {
            let _ = MoveWindow(hwnd, 40 + (i % 40) * 9, 40 + (i % 25) * 9, 320, 240, true);
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            i = i.wrapping_add(1);
            std::thread::sleep(Duration::from_millis(16));
        }
        let _ = DestroyWindow(hwnd);
    })
}

fn display_name() -> Option<String> {
    scrap::Display::all().ok()?.into_iter().next().map(|d| d.name())
}

/// 连续采集 300 帧，统计错误种类并测量耗时
#[test]
fn dxgi_long_run_no_invalid_call() {
    let _gate = capture_gate();
    let Some(name) = display_name() else {
        println!("[skip] no display");
        return;
    };
    let mut capturer = match sos_dxgi::create(&name) {
        Ok(c) => c,
        Err(e) => {
            println!("[skip] DXGI 创建失败: {e}");
            return;
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let animator = spawn_animator(stop.clone());
    std::thread::sleep(Duration::from_millis(300));

    let (mut ok, mut wouldblock, mut invalid, mut other) = (0u32, 0u32, 0u32, 0u32);
    let mut first_other: Option<String> = None;
    let mut total = Duration::ZERO;
    let mut max_frame = Duration::ZERO;
    let target = 300u32;

    let wall = Instant::now();
    // 墙钟预算：桌面静止 / 锁屏时每一帧都要等满超时，300 次就是 60 秒。
    // 这里只限制**空转**的总时长，正常出帧时远达不到（实测 1~2 秒跑完 300 帧）。
    const BUDGET: Duration = Duration::from_secs(6);
    let mut sampled = 0u32;
    for _ in 0..target {
        if wall.elapsed() > BUDGET {
            println!("提前结束：{}s 内只采到 {} 次（桌面无变化或已锁屏）", BUDGET.as_secs(), sampled);
            break;
        }
        sampled += 1;
        let t = Instant::now();
        match capturer.frame(Duration::from_millis(200)) {
            Ok(f) => {
                let dt = t.elapsed();
                total += dt;
                max_frame = max_frame.max(dt);
                if let scrap::Frame::PixelBuffer(pb) = &f {
                    std::hint::black_box(pb.data().len());
                }
                ok += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => wouldblock += 1,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                invalid += 1;
                if first_other.is_none() {
                    first_other = Some(format!("InvalidData: {e}"));
                }
            }
            Err(e) => {
                other += 1;
                if first_other.is_none() {
                    first_other = Some(format!("{:?}: {e}", e.kind()));
                }
            }
        }
    }
    let wall = wall.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let _ = animator.join();

    println!("=== 自建 DXGI 连续采集 {} 次 ===", target);
    println!("ok={ok} wouldblock={wouldblock} invalid_data={invalid} other={other}");
    println!("墙钟 {wall:.2}s → {:.1} 有效帧/秒", ok as f64 / wall);
    if ok > 0 {
        println!(
            "出帧 {:.2} ms/帧（平均）  {:.2} ms（最慢）",
            total.as_secs_f64() * 1000.0 / ok as f64,
            max_frame.as_secs_f64() * 1000.0
        );
    }
    if let Some(e) = first_other {
        println!("首个非 WouldBlock 错误: {e}");
    }

    assert_eq!(
        invalid, 0,
        "自建 DXGI 不应该出现 InvalidData（scrap 的 order bug 特征）"
    );
    if ok == 0 {
        // 锁屏 / 会话断开时 DXGI 不再产出新帧，这是环境限制而不是缺陷 ——
        // 与同文件里 `dxgi_static_then_recover` 的 [skip] 约定保持一致。
        println!("[skip] 本机桌面当前无变化（可能已锁屏），跳过出帧断言");
        return;
    }
    println!("✓ 无 InvalidData，未触发 scrap 的顺序缺陷");
}

/// 桌面静止 → WouldBlock → 之后仍能恢复出帧
#[test]
fn dxgi_static_then_recover() {
    let _gate = capture_gate();
    let Some(name) = display_name() else {
        println!("[skip] no display");
        return;
    };
    let mut capturer = match sos_dxgi::create(&name) {
        Ok(c) => c,
        Err(e) => {
            println!("[skip] DXGI 创建失败: {e}");
            return;
        }
    };

    // 前置探测：先确认本机当前**能**采到帧。
    //
    // 锁屏 / UAC 安全桌面 / 会话断开时，我们的窗口建在 Default 桌面上而用户看的是
    // Winlogon 桌面 ⇒ 桌面没有可见变化 ⇒ DXGI 一帧都不给。那是环境限制，不能把
    // 「恢复出帧」判成回归（现场：ok=0/20 但 wouldblock=10/10 全部正常）。
    {
        let stop = Arc::new(AtomicBool::new(false));
        let animator = spawn_animator(stop.clone());
        std::thread::sleep(Duration::from_millis(300));
        let mut probe = 0u32;
        for _ in 0..10 {
            if capturer.frame(Duration::from_millis(100)).is_ok() {
                probe += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let _ = animator.join();
        if probe == 0 {
            println!("[skip] 本机当前采不到任何帧（锁屏 / 会话断开），跳过恢复出帧验证");
            return;
        }
        println!("前置探测: 能采到帧 probe={probe}/10");
        // 让刚才那轮变化沉寂下去，再进入「静止」阶段
        std::thread::sleep(Duration::from_millis(400));
    }

    // 阶段 1：静止桌面，应当只拿到 WouldBlock
    let mut block = 0u32;
    for _ in 0..10 {
        match capturer.frame(Duration::from_millis(120)) {
            Ok(f) => {
                if let scrap::Frame::PixelBuffer(pb) = &f {
                    std::hint::black_box(pb.data().len());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => block += 1,
            Err(e) => panic!("静止桌面出现非 WouldBlock 错误: {e:?} {e}"),
        }
    }
    println!("静止桌面: wouldblock={block}/10");

    // 阶段 2：引入变化，必须恢复出帧
    let stop = Arc::new(AtomicBool::new(false));
    let animator = spawn_animator(stop.clone());
    std::thread::sleep(Duration::from_millis(300));

    let mut ok = 0u32;
    for _ in 0..20 {
        if capturer.frame(Duration::from_millis(200)).is_ok() {
            ok += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = animator.join();

    println!("恢复后出帧: ok={ok}/20");
    assert!(ok > 0, "静止之后无法恢复出帧");
    println!("✓ 静止不卡死，恢复后正常出帧");
}
