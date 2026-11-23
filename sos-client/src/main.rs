//! RustDesk SOS 精简受控端 —— 二进制入口。
//!
//! 这里只保留 `#[tokio::main]` 与 GUI 子系统声明；全部逻辑在库里的
//! [`rustdesk_sos::app::run`]，这样测试可以直接使用库而不必重新编译源文件。

// Release 构建使用 Windows GUI 子系统，不显示控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use hbb_common::ResultType;

#[tokio::main]
async fn main() -> ResultType<()> {
    rustdesk_sos::app::run().await
}
