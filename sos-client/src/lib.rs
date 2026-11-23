//! RustDesk SOS 精简受控端 —— 库根。
//!
//! 二进制入口在 `main.rs`（只有 `#[tokio::main]` 三行），全部逻辑在这里，
//! 因此 `tests/*.rs` 可以直接 `use rustdesk_sos::…`，不再需要用
//! `#[path = "../src/…"] mod …;` 把源文件重新编译进每个测试二进制。
//!
//! # 模块分层
//!
//! ```text
//! app      进程生命周期：日志/配置/常量/CPU/托盘/SYSTEM 子进程/入口
//! capture  屏幕采集：采集线程、DXGI、GDI、显示器枚举、安全桌面判定
//! video    采集→编码→发送：视频循环、libvpx、硬件编码、QoS
//! net      网络：会话、信令、中继、文件传输
//! ipc      进程间：命名管道（控制）、共享内存（帧）
//! input    输入注入
//! ui       面向本机用户的界面（连接确认弹窗）
//! ```
//!
//! # 依赖方向（按实际调用统计，不是按设想）
//!
//! ```text
//! app     → 其余六个域（装配层，入口在这里）
//! net     → video（约 30 处）、capture（约 5 处，`uac`/`display`）
//! video   → capture（约 9 处）
//! capture → ipc（写帧）
//! ipc     → 无出边（只用 windows crate）
//! ui      → 无出边（弹窗只认 win32 / std / hbb_common）
//! ```
//!
//! `ipc` 必须保持零出边：它只提供「命名共享内存原语」，帧缓冲区有多大由调用方
//! 算好传进来（`ipc::shmem::create` 收尺寸参数），不要再让它回头调 `capture`。

pub mod app;
pub mod capture;
pub mod input;
pub mod ipc;
pub mod net;
pub mod ui;
pub mod video;

// 自建视频链路依赖由 vcpkg 的 libvpx 生成的绑定，缺少时 `build.rs` 不会置
// `cfg(sos_vpx)`。与其让编译器在几十个引用点抛「找不到模块」，不如在这里直接说明。
#[cfg(not(sos_vpx))]
compile_error!(
    "libvpx bindings are required: install vcpkg and build vpx (x64-windows-static), \
     then ensure VCPKG_ROOT is set before `cargo build`"
);
