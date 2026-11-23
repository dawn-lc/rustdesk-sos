//! 中继地址解析（纯函数，独立成模块以便单测）
//!
//! # 协议事实（已核对 hbbs 源码，勿再凭直觉改写下面的结论）
//!
//! 1. 协议里 `relay_server` 是**单值** `string`
//!    （`rendezvous.proto` 的 `PunchHole.relay_server` /
//!    `PunchHoleResponse.relay_server` / `RequestRelay.relay_server`），
//!    **不是**列表，也没有"逗号分隔"的约定。
//!
//! 2. 多中继是**服务端侧**的配置概念：hbbs 用
//!    `relay-servers=`（Pro 控制台 / OSS 命令行）或 `RELAY=` 环境变量配置
//!    逗号分隔的多台，由 `src/common.rs::get_servers()` 的 `split(',')` 解析，
//!    存进 `relay_servers: Vec<String>`。
//!
//! 3. hbbs **自己选一台**再下发，客户端拿到的永远是单个主机名
//!    （下面是 hbbs 的源码摘录，不是本 crate 的示例，故标记为 `text`）：
//!
//!    ```text
//!    // rustdesk-server/src/rendezvous_server.rs
//!    fn get_relay_server(&self, _pa: IpAddr, _pb: IpAddr) -> String {
//!        if self.relay_servers.is_empty() { return "".to_owned(); }
//!        else if self.relay_servers.len() == 1 { return self.relay_servers[0].clone(); }
//!        let i = ROTATION_RELAY_SERVER.fetch_add(1, Ordering::SeqCst)
//!                % self.relay_servers.len();
//!        self.relay_servers[i].clone()
//!    }
//!    ```
//!
//!    OSS 版是**轮询**（`ROTATION_RELAY_SERVER`），Pro 版按 GeoLite2
//!    选地理最近的（见官方文档 *Configure Relay Servers*）。
//!    所以客户端**没有**在多台中继之间挑选的余地 —— 上游 RustDesk 不做切分
//!    是**正确的**，不是缺陷。
//!
//! # 那这个模块为什么还存在
//!
//! 因为 `--relay` 是**本客户端自己的**命令行选项，不是协议字段。
//! 运维可能希望本机也配几台后备（hbbs 只给一台，而那一台可能正好挂了，
//! 或者 hbbs 本身配置错了）。这属于本地策略，协议没有约束。
//!
//! 因此这里只做两件事，都不依赖"服务器会下发多个"这个错误假设：
//!
//! - [`relay_candidates`]：把**任意来源**的串按逗号切成候选。
//!   服务器下发实际上永远只有一项，切分对它只是无害的防御。
//! - [`resolve_relay_candidates`]：本地 `--relay` 优先于服务器下发。
//!   这一条与上游 `rendezvous_mediator::get_relay_server()` 一致。

/// 候选中继之间的分隔符
///
/// 与 hbbs 解析 `relay-servers=` 时用的分隔符保持相同（`get_servers`）。
const RELAY_SEPARATOR: char = ',';

/// 把中继串解析成去重后的候选列表（保持原顺序，顺序即优先级）。
///
/// - 逗号分隔，两侧空白与空项丢弃；
/// - 主机名大小写不敏感地去重（同一台写两遍不该重复拨号）；
/// - IPv6 字面量内部是冒号不是逗号，切分不会破坏它；
/// - 全部为空时返回空 `Vec`，由调用方决定兜底。
pub fn relay_candidates(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(RELAY_SEPARATOR) {
        let host = part.trim();
        if host.is_empty() {
            continue;
        }
        if !out.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            out.push(host.to_string());
        }
    }
    out
}

/// 决定本次连接要用的中继候选列表。
///
/// `cli_relay` 是本地配置（`--relay`），`provided` 是服务器下发的原始串。
///
/// 优先级与上游 `rendezvous_mediator::get_relay_server()` 一致：
/// 本地配置非空就整体取代服务器下发（哪怕本地也写了多台）。
/// 这样内网部署可以强制走指定中继，而不必改服务器配置。
///
/// `provided` 实际上永远只有一项（hbbs 已替我们选好），
/// 这里仍走同一套切分逻辑只是为了不为它写特例。
///
/// 兜底项（信令服务器 +1）由调用方拼接 —— 这里不引入 `hbb_common` 依赖，
/// 保持本模块可独立单测。
pub fn resolve_relay_candidates(cli_relay: &str, provided: &str) -> Vec<String> {
    let from_cli = relay_candidates(cli_relay);
    if !from_cli.is_empty() {
        return from_cli;
    }
    relay_candidates(provided)
}

// ── NAT 类型判定与中继决策 ──
//
// 与上游 `src/common.rs::test_nat_type_` / `rendezvous_mediator` 逐项对齐。
// 提成纯函数是为了让 `tests/p2p_logic.rs` 直接测**生产实现**，而不是复刻一份。

/// 由两次 `TestNat` 拿到的映射端口判定 NAT 类型。
///
/// 两个端口相同 = 端点无关映射（`ASYMMETRIC`，可以打洞）；不同 = 对称 NAT
/// （每个目标一个新端口，打洞无用，必须中继）。任一端口为 0 表示探测失败，
/// 此时返回 `None`，调用方保留上一次结论而不是写进一个错误值。
pub fn classify_nat(ports: [i32; 2]) -> Option<hbb_common::rendezvous_proto::NatType> {
    use hbb_common::rendezvous_proto::NatType;
    if ports[0] <= 0 || ports[1] <= 0 {
        None
    } else if ports[0] == ports[1] {
        Some(NatType::ASYMMETRIC)
    } else {
        Some(NatType::SYMMETRIC)
    }
}

/// 是否必须放弃打洞、改走中继。
///
/// 只有四种确定的情况才放弃：对端要求强制中继、任一侧是对称 NAT、
/// 或对端端口未知（`0`）。其余一律先尝试打洞 —— 打洞失败还有超时兜底，
/// 而误判成「必须中继」会白白把流量压到中继上。
pub fn must_relay(
    force_relay: bool,
    remote: hbb_common::rendezvous_proto::NatType,
    ours: hbb_common::rendezvous_proto::NatType,
    peer_port: u16,
) -> bool {
    use hbb_common::rendezvous_proto::NatType;
    force_relay
        || remote == NatType::SYMMETRIC
        || ours == NatType::SYMMETRIC
        || peer_port == 0
}
