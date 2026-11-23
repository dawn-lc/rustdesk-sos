//! 进程生命周期与应用装配。
//!
//! 日志初始化、注册表配置、常量、CPU 采样、托盘、SYSTEM 子进程管理，以及
//! 真正的入口逻辑（[`run`]，即原来的 `main.rs` 主体）。

pub mod bootstrap;
pub mod console;
pub mod config;
pub mod constants;
#[cfg(windows)]
pub mod cpu;
mod run;
pub mod secure_watch;
pub mod system;
pub mod tray;
pub mod watchdog;

pub use run::run;
