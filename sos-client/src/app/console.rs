//! 调试控制台挂接（`--debug` 时把 stdout/stderr 接到控制台）。
//!
//! 从 `app::run` 抽出：Release 构建是 GUI 子系统（无控制台），这里用
//! `AttachConsole` + `CreateFile("CONOUT$")` + `SetStdHandle` 手工重定向 ——
//! `AttachConsole` 挂上父控制台后标准句柄仍是无效的，必须自己打开 `CONOUT$`。

use std::io::Write;

#[cfg(windows)]
pub fn setup(debug: bool, service: bool) {
    if !debug || service {
        return;
    }
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::*;
    use windows::Win32::System::Console::*;
    unsafe {
        let has_parent = AttachConsole(ATTACH_PARENT_PROCESS).is_ok();
        if !has_parent {
            let _ = AllocConsole();
        }
        // CreateFileW 的 dwDesiredAccess 是 u32，不能用 FILE_GENERIC_WRITE（类型不匹配）
        const GENERIC_WRITE: u32 = 0x40000000;
        const GENERIC_READ: u32 = 0x80000000;
        let con_out: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let handle = CreateFileW(
            PCWSTR(con_out.as_ptr()),
            GENERIC_WRITE | GENERIC_READ,
            FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        );
        if let Ok(h) = handle {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
            let _ = SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
    // 确保 Rust stdio 重定向到新句柄
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}
