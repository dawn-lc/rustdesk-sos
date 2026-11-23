//! 主进程 ↔ SYSTEM 子进程的本机通信。
//!
//! [`pipe`] 走控制信令，[`shmem`] 走帧数据。二者都是跨会话对象，
//! 名字必须带 `Global\` 前缀（见 `app::constants`）。

pub mod pipe;
pub mod shmem;
