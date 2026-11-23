//! 网络：会话主循环、信令、中继与文件传输。

pub mod connection;
pub mod file_transfer;
pub mod relay;
pub mod rtt;
pub mod rendezvous;
/// WebRTC 入站：控制端走 WebRTC 传输时的 answerer 与 ICE 路由。
pub mod webrtc_in;
/// UDP 打洞 + KCP 入站（`PunchHole.udp_port > 0`）。
pub mod kcp_in;
