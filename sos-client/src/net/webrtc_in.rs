//! WebRTC 入站（受控端）。
//!
//! 控制端要用 WebRTC 直连时，把 SDP offer 塞在 `PunchHole.webrtc_sdp_offer` 里；
//! 本端据此建 answerer，把 SDP answer 回填进 `PunchHoleSent.webrtc_sdp_answer`
//! （走中继时回填 `RelayResponse.webrtc_sdp_answer`），双方再用 `IceCandidate`
//! 消息互发候选地址。ICE 打通后得到一个 `Stream::WebRTC`，交给
//! [`crate::net::connection::handle`] —— 从这一刻起它和 TCP 会话没有区别。
//!
//! 三条不能省的细节（都是实测/上游踩出来的，改之前先读）：
//!
//! 1. **answer 与 ICE 候选必须走加密通道。** 候选里是本机每块网卡的地址，
//!    明文发出去等于自曝内网拓扑。上游为此单独开一条 TCP 连接并强制
//!    `secure_tcp_required`（"The answer goes out only on a channel that is
//!    actually encrypted"），这里照做：拿不到加密通道就放弃本次 WebRTC，
//!    让控制端回退到别的传输。
//! 2. **重复的 offer 不能再建 answerer。** 控制端每次打洞重试都会重发同一个
//!    offer，而 `WebRTCStream::new` 对相同 offer 有共享缓存；它的
//!    `take_local_ice_rx()` 每个 pc 只会成功一次。第二次拿到 `None` 就说明
//!    已经有 answerer 在跑，此时只回 answer、绝不再起连接任务 —— 否则两个
//!    任务抢读同一条 data channel，握手会互相踩烂。
//! 3. **Relay-only 需要 TURN 服务器。** 没有 TURN 就别答（上游：
//!    `!relay_only || has_turn_server()`），否则建出一个永远连不上的 pc，
//!    等它超时反而拖慢了控制端的回退。
//!
//! 与上游 `rendezvous_mediator.rs::spawn_webrtc_answerer` 的差异：
//! - 上游用 `AnswererSlot` 做"在飞 answerer 数"限流；这里用 `Semaphore`，
//!   语义相同（超限直接拒答，不排队）。
//! - 上游的 ICE 发送任务会在这条连接失败时重连一次；这里同样处理。

use crate::app::config::SosConfig;
use hbb_common::rendezvous_proto::{IceCandidate, PunchHole, RendezvousMessage};
use hbb_common::webrtc::WebRTCStream;
use hbb_common::ResultType;
use std::collections::HashMap;
use std::sync::OnceLock;
use tokio::sync::{mpsc, Mutex, Semaphore};

/// 同时在飞的 answerer 上限。
///
/// 每个 answerer 持有一个 peer connection（若干 socket + 后台任务），而 PunchHole
/// 是控制端可重复触发的；不限流等于让对方决定我们开多少 pc。超限直接拒答
/// （回空 answer），控制端会走别的传输。
const MAX_ANSWERERS: usize = 4;

/// 每个会话缓存多少条还没喂给 ICE agent 的对端候选。
///
/// 候选条数由发送方决定，而消费一条要解析 JSON 再抢 ICE agent 的锁；无界队列
/// 等于让能触达这条会话的人随意撑大常驻服务进程的内存。满了就丢最新的一条，
/// 控制端会重发。
const MAX_PENDING_REMOTE_ICE: usize = 32;

/// 同一会话键的在飞注册表：session_key → 对端候选的投喂口。
///
/// session_key 是 offer 里的 DTLS 指纹（同一 offer 的各次重试相同），所以它天然
/// 能把「重试」和「换了一个会话」区分开。
fn ice_routes() -> &'static Mutex<HashMap<String, mpsc::Sender<String>>> {
    static ROUTES: OnceLock<Mutex<HashMap<String, mpsc::Sender<String>>>> = OnceLock::new();
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn answerer_slots() -> &'static Semaphore {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| Semaphore::new(MAX_ANSWERERS))
}

/// 本次 offer 是否要求 ICE 只能走 TURN 中继。
///
/// 控制端 `force_relay` 单独出现时说不清是「策略要求」还是「传输被迫」（例如它
/// 走 WebSocket，只能靠中继）；真正的答案在 offer 的信封里：`ice_policy: "all"`
/// 表示中继是被传输逼的，此时可以正常答 full ICE，让直连成型。
pub fn relay_only(ph: &PunchHole) -> bool {
    ph.force_relay && !WebRTCStream::endpoint_declares_all_ice(&ph.webrtc_sdp_offer)
}

/// 是否该答这个 offer。
pub fn viable(ph: &PunchHole) -> bool {
    if ph.webrtc_sdp_offer.is_empty() {
        return false;
    }
    // 代理模式下本机出口 IP 是用户的隐私边界，ICE 会绕过代理把它暴露出去 ——
    // 上游同样在 `Config::is_proxy()` 时拒绝作答。
    if hbb_common::config::Config::is_proxy() {
        return false;
    }
    if relay_only(ph) && !WebRTCStream::has_turn_server() {
        return false;
    }
    true
}

/// 拒答理由（仅在 `viable()` 为 false 且控制器确实发了 offer 时有意义）。
///
/// 没有这行日志时，「控制器没发 offer」和「发了但因策略拒答」在日志里长得一模一样 ——
/// 实测排查时全靠猜。
pub fn decline_reason(ph: &PunchHole) -> &'static str {
    if ph.webrtc_sdp_offer.is_empty() {
        return "no offer in the request";
    }
    if hbb_common::config::Config::is_proxy() {
        return "proxy mode: ICE would leak the real IP";
    }
    if relay_only(ph) && !WebRTCStream::has_turn_server() {
        return "offer is Relay-only (force_relay without ice_policy=all) and no TURN server is configured";
    }
    ""
}

/// 为一条 PunchHole 建 answerer，返回要塞进 `webrtc_sdp_answer` 的 SDP。
///
/// 失败或无资格作答时返回空串（调用方据此让控制端回退到别的传输），不返回错误：
/// WebRTC 是可选传输，答不上来不该影响这条连接的其它路径。
pub async fn answer(
    ph: PunchHole,
    config: SosConfig,
    password_refresh_tx: tokio::sync::mpsc::UnboundedSender<()>,
) -> String {
    match spawn_answerer(ph, config, password_refresh_tx).await {
        Ok(answer) => answer,
        Err(err) => {
            log::warn!("[WEBRTC] failed to build answer: {}", err);
            String::new()
        }
    }
}

async fn spawn_answerer(
    ph: PunchHole,
    config: SosConfig,
    password_refresh_tx: tokio::sync::mpsc::UnboundedSender<()>,
) -> ResultType<String> {
    let Ok(_slot) = answerer_slots().try_acquire() else {
        log::warn!(
            "[WEBRTC] declined an offer: {} answerers already in flight",
            MAX_ANSWERERS
        );
        return Ok(String::new());
    };
    let relay_only_ice = relay_only(&ph);
    let mut stream = WebRTCStream::new(
        &ph.webrtc_sdp_offer,
        relay_only_ice,
        crate::app::constants::CONNECT_TIMEOUT,
    )
    .await?;
    let answer = stream.local_endpoint().to_owned();
    let session_key = stream.session_key().to_owned();

    // 重复的 offer 会命中 `WebRTCStream::new` 的共享缓存：answer 相同，
    // 但本地候选接收端只能取走一次 —— 拿不到就说明已有 answerer 在跑，
    // 只回 answer，不再起第二个连接任务（见模块注释第 2 条）。
    let Some(mut local_ice_rx) = stream.take_local_ice_rx() else {
        log::debug!("[WEBRTC] duplicate offer, reusing the answerer already in flight");
        return Ok(answer);
    };

    let (remote_ice_tx, mut remote_ice_rx) = mpsc::channel::<String>(MAX_PENDING_REMOTE_ICE);
    let own_ice_tx = remote_ice_tx.clone();
    {
        let mut routes = ice_routes().lock().await;
        routes.insert(session_key.clone(), remote_ice_tx);
    }

    // 对端候选 → ICE agent
    let stream_for_remote_ice = stream.clone();
    tokio::spawn(async move {
        while let Some(candidate) = remote_ice_rx.recv().await {
            if let Err(err) = stream_for_remote_ice
                .add_remote_ice_candidate(&candidate)
                .await
            {
                log::warn!("[WEBRTC] failed to add remote ICE candidate: {}", err);
            }
        }
    });

    // 本端候选 → 信令服务器（必须加密，见模块注释第 1 条）
    {
        let config_for_ice = config.clone();
        let session_key_for_ice = session_key.clone();
        let id = config.id.clone();
        tokio::spawn(async move {
            send_local_ice(config_for_ice, id, session_key_for_ice, &mut local_ice_rx).await;
        });
    }

    // 连上之后交给会话层；失败就立即释放 pc，不等 ICE agent 自己超时（约 30s）
    let session_key_for_cleanup = session_key.clone();
    tokio::spawn(async move {
        let connected = stream
            .wait_connected(crate::app::constants::CONNECT_TIMEOUT)
            .await;
        {
            // 只摘自己这一条：同一 session_key 的重试可能已经换上新的投喂口，
            // 盲目删除会把活跃会话的候选通道一起删掉。
            let mut routes = ice_routes().lock().await;
            if routes
                .get(&session_key_for_cleanup)
                .is_some_and(|tx| tx.same_channel(&own_ice_tx))
            {
                routes.remove(&session_key_for_cleanup);
            }
        }
        if let Err(err) = connected {
            log::warn!("[WEBRTC] wait_connected failed: {}", err);
            stream.close_detached();
            return;
        }
        log::info!("[WEBRTC] ICE connected, handing the session over");

        let peer_addr = hbb_common::AddrMangle::decode(&ph.socket_addr);
        let stream_for_cleanup = stream.clone();
        if let Err(err) = crate::net::connection::handle(
            hbb_common::Stream::WebRTC(stream),
            peer_addr,
            config,
            Some(&password_refresh_tx),
        )
        .await
        {
            log::warn!("[WEBRTC] session failed: {}", err);
        }
        // 会话正常结束也必须显式关 pc：ICE 的终态回调在干净关闭时未必触发，
        // 不关就会一直留在 SESSIONS 里，泄漏 pc、channel 与 socket。
        stream_for_cleanup.close().await;
    });

    Ok(answer)
}

/// 把本端 ICE 候选发回信令服务器。
///
/// 与上游同样单开一条 TCP（而不是复用 UDP 信令 socket）：默认部署下信令通道是
/// UDP，而很多 hbbs 对外只开 TCP/WebSocket，UDP 端口不可达；候选必须可靠送达，
/// 所以走 TCP 的可靠性代替了上游早年的 400ms 重发。
async fn send_local_ice(
    config: SosConfig,
    id: String,
    session_key: String,
    local_ice_rx: &mut mpsc::UnboundedReceiver<String>,
) {
    let host = hbb_common::socket_client::check_port(
        &config.rendezvous_server,
        crate::app::constants::RENDEZVOUS_PORT,
    );
    let key = licence_key(&config);
    let mut conn: Option<hbb_common::Stream> = None;
    while let Some(candidate) = local_ice_rx.recv().await {
        let mut msg = RendezvousMessage::new();
        msg.set_ice_candidate(IceCandidate {
            id: id.clone(),
            session_key: session_key.clone(),
            candidate,
            ..Default::default()
        });
        // 每次发送最多重连一次：hbbs 重启或空闲断连后，第一次发送会撞在已失效的
        // stream 上。
        for _ in 0..2 {
            if conn.is_none() {
                match hbb_common::socket_client::connect_tcp(
                    host.clone(),
                    crate::app::constants::CONNECT_TIMEOUT,
                )
                .await
                {
                    // connect_tcp 已经返回可用的 Stream（走 SOCKS 时也适配好了）
                    Ok(mut stream) => {
                        // 加密失败就放弃本轮候选（不是放弃整个会话）：
                        // 明文发候选等于泄漏内网拓扑，宁可这条 WebRTC 没有候选。
                        match librustdesk::common::secure_tcp_required(&mut stream, &key).await {
                            Ok(()) => conn = Some(stream),
                            Err(err) => {
                                log::warn!(
                                    "[WEBRTC] ICE channel not secured, dropping candidates: {}",
                                    err
                                );
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        log::warn!("[WEBRTC] ICE channel connect failed: {}", err);
                        break;
                    }
                }
            }
            if let Some(stream) = conn.as_mut() {
                match stream.send(&msg).await {
                    Ok(()) => break,
                    Err(err) => {
                        log::debug!("[WEBRTC] ICE candidate send failed, reconnecting: {}", err);
                        conn = None;
                    }
                }
            }
        }
    }
}

/// 信令通道用的许可证密钥（hbbs 公钥）。
pub(crate) fn licence_key(config: &SosConfig) -> String {
    if !config.server_pub_key.is_empty() {
        config.server_pub_key.clone()
    } else {
        hbb_common::config::RS_PUB_KEY.to_string()
    }
}

/// 收到对端 `IceCandidate` 消息：投给对应会话的 ICE agent。
///
/// 认不出的 session_key 直接丢弃并节流记日志：那是别的会话（或已结束的会话）
/// 的候选，不值得为它开任何状态。
pub async fn on_ice_candidate(ice: IceCandidate) {
    let tx = {
        let routes = ice_routes().lock().await;
        routes.get(&ice.session_key).cloned()
    };
    match tx {
        Some(tx) => {
            if let Err(err) = tx.try_send(ice.candidate) {
                log::debug!("[WEBRTC] dropped a remote ICE candidate: {}", err);
            }
        }
        None => {
            log::debug!(
                "[WEBRTC] dropped an ICE candidate for unknown session key ({} bytes)",
                ice.session_key.len()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{relay_only, viable};
    use hbb_common::rendezvous_proto::PunchHole;

    fn offer(force_relay: bool, sdp_offer: &str) -> PunchHole {
        PunchHole {
            force_relay,
            webrtc_sdp_offer: sdp_offer.to_owned(),
            ..Default::default()
        }
    }

    /// 没有 offer 时绝不答：这是 TCP 打洞/中继路径的正常情况。
    #[test]
    fn no_offer_is_not_viable() {
        assert!(!viable(&offer(false, "")));
        assert!(!viable(&offer(true, "")));
    }

    /// `force_relay` 单独出现时，只有 offer 信封里声明了 `ice_policy: all` 才说明
    /// 中继是被传输逼的，可以按 full ICE 作答。解析不出信封一律按 Relay-only 处理
    /// —— 那是旧版控制端的行为，宁可保守。
    #[test]
    fn relay_only_follows_the_offer_envelope() {
        // 不是 force_relay：与信封无关
        assert!(!relay_only(&offer(false, "webrtc://AAAA")));
        // force_relay + 无法解析的信封（旧控制端）→ 按 Relay-only
        assert!(relay_only(&offer(true, "webrtc://AAAA")));
        // force_relay 但没有 offer：这条分支只在讨论 offer 时才有意义，仍应保守
        assert!(relay_only(&offer(true, "")));
    }

    /// 并发上限之外的 offer 必须直接拒答（回空 answer），不能排队 ——
    /// 排队等于让控制端用重连堆出 peer connection。
    #[test]
    fn answerer_slots_are_bounded() {
        assert_eq!(super::MAX_ANSWERERS, super::answerer_slots().available_permits());
    }
}
