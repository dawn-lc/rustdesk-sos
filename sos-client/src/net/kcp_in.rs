//! UDP 打洞 + KCP 入站（受控端）。
//!
//! 控制端也可以要求走 UDP：`PunchHole.udp_port > 0` 表示它愿意在这条连接上做 UDP
//! 打洞，打通后数据走 KCP（一种建立在 UDP 上的可靠有序传输）。
//!
//! 流程与上游 `rendezvous_mediator.rs` 的 `punch_udp_hole` + `udp_nat_listen` 对齐：
//!
//! 1. 新建一个直连 hbbs 的 UDP socket，通过它发 `PunchHoleSent`；
//! 2. **关键**：hbbs 对这个包做「源地址观测」—— 它从包的源地址学到本端 NAT 映射，
//!    再转告控制端来打这个洞。所以这条 `PunchHoleSent` 必须从打洞用的**同一个**
//!    socket 发出，不能走中转的 UDP 信令通道，否则 hbbs 学到的是另一个端口；
//! 3. 重发两次（随机 10-30ms 间隔）：第一次可能丢在还没建好的映射上；
//! 4. `connect(peer)` + 双向探针（`common::punch_udp`）打开两侧 NAT；
//! 5. `KcpStream::accept` 等到控制端的 KCP SYN，拿到一个 `Stream`；
//! 6. 交给 [`crate::net::connection::handle`] —— 与 TCP 会话同一套逻辑。
//!
//! 与上游的差异只有一个：上游用 `UDP_PUNCHES` 这个池做「在飞打洞数」限流，
//! 这里用 `Semaphore`，语义相同（超限直接放弃，不排队）。

use crate::app::config::SosConfig;
use hbb_common::rendezvous_proto::{PunchHole, PunchHoleSent, NatType, RendezvousMessage};
use hbb_common::ResultType;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Semaphore;

/// 同时在飞的 UDP 打洞上限。
///
/// 每个打洞占一个 UDP socket 并持续发探针（最长 3 秒），而 PunchHole 是控制端可重复
/// 触发的；不限流等于让对方决定我们开多少 socket、发多少包。
const MAX_UDP_PUNCHES: usize = 2;

/// 控制端是否要求走 UDP 打洞。
pub fn is_udp_punch(ph: &PunchHole) -> bool {
    ph.udp_port > 0
}

fn punch_slots() -> &'static Semaphore {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| Semaphore::new(MAX_UDP_PUNCHES))
}

/// UDP 打洞并接收 KCP 会话。
///
/// 失败只记日志、返回错误由调用方吞掉：UDP 打洞本来就「能通就快、不通回退中继」，
/// 它失败不该影响这条连接的其它传输。
pub async fn serve(
    ph: PunchHole,
    config: SosConfig,
    password_refresh_tx: tokio::sync::mpsc::UnboundedSender<()>,
) -> ResultType<()> {
    let Ok(_slot) = punch_slots().try_acquire() else {
        log::warn!(
            "[KCP] declined a UDP punch: {} punches already in flight",
            MAX_UDP_PUNCHES
        );
        return Ok(());
    };

    // 控制端给的公网地址，加上它愿意收 KCP 的 UDP 端口
    let mut peer_addr = hbb_common::AddrMangle::decode(&ph.socket_addr);
    peer_addr.set_port(ph.udp_port as u16);

    let host = hbb_common::socket_client::check_port(
        &config.rendezvous_server,
        crate::app::constants::RENDEZVOUS_PORT,
    );
    let (socket, hbbs_addr) = hbb_common::socket_client::new_direct_udp_for(&host).await?;

    // PunchHoleSent 必须从这个打洞 socket 发出（见模块注释第 2 条）
    let mut msg = RendezvousMessage::new();
    msg.set_punch_hole_sent(make_punch_hole_sent(&ph, &config, String::new()));
    let data = {
        use hbb_common::protobuf::Message as _;
        msg.write_to_bytes()?
    };
    socket.send_to(&data, hbbs_addr).await?;
    log::info!(
        "[KCP] PunchHoleSent sent from {:?} to hbbs {} (peer={})",
        socket.local_addr().ok(),
        hbbs_addr,
        peer_addr
    );

    // 重发两次：第一次可能落在尚未建立的 NAT 映射上。用同一个 socket，hbbs 只会
    // 看到同一个源地址，重发不会污染它的观测。
    {
        let socket = socket.clone();
        let data = data.clone();
        tokio::spawn(async move {
            for _ in 0..2 {
                let ms = (hbb_common::time_based_rand() % 20 + 10) as f32 / 1000.;
                hbb_common::sleep(ms).await;
                socket.send_to(&data, hbbs_addr).await.ok();
            }
        });
    }

    // 打开两侧 NAT：先 connect 固定对端，再双向发探针
    socket.connect(peer_addr).await?;
    let init_packet = librustdesk::common::punch_udp(socket.clone(), true).await?;

    let (_kcp, stream) = librustdesk::kcp_stream::KcpStream::accept(
        socket.clone(),
        Duration::from_millis(crate::app::constants::CONNECT_TIMEOUT as _),
        init_packet,
    )
    .await?;
    log::info!("[KCP] KCP session established with {}", peer_addr);

    crate::net::connection::handle(stream, peer_addr, config, Some(&password_refresh_tx)).await
}

/// 构造回给 hbbs 的 `PunchHoleSent`。
///
/// WebRTC 路径复用它填 `webrtc_sdp_answer`；UDP 路径传空串。
pub(crate) fn make_punch_hole_sent(
    ph: &PunchHole,
    config: &SosConfig,
    webrtc_sdp_answer: String,
) -> PunchHoleSent {
    use hbb_common::protobuf::Enum;
    let our_nat = NatType::from_i32(crate::app::config::get_nat_type()).unwrap_or(NatType::UNKNOWN_NAT);
    PunchHoleSent {
        socket_addr: ph.socket_addr.clone(),
        id: config.id.clone(),
        relay_server: ph.relay_server.clone(),
        nat_type: our_nat.into(),
        version: crate::app::constants::VERSION.to_string(),
        socket_addr_v6: ph.socket_addr_v6.clone(),
        webrtc_sdp_answer,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::is_udp_punch;
    use hbb_common::rendezvous_proto::PunchHole;

    /// 控制端用 `udp_port` 表达「愿意走 UDP」：0 表示没有这个意愿（走 TCP）。
    /// 负数属异常值，同样按「不走 UDP」处理 —— 不能因为一个坏字段就去开 socket。
    #[test]
    fn udp_punch_only_when_port_is_positive() {
        let ph = |port: i32| PunchHole {
            udp_port: port,
            ..Default::default()
        };
        assert!(!is_udp_punch(&ph(0)));
        assert!(!is_udp_punch(&ph(-1)));
        assert!(is_udp_punch(&ph(21116)));
    }

    #[test]
    fn udp_punch_slots_are_bounded() {
        assert_eq!(
            super::MAX_UDP_PUNCHES,
            super::punch_slots().available_permits()
        );
    }
}
