//! SOS 精简连接处理器
//!
//! 替代 `src/server/connection.rs`，实现精简的事件循环：
//! 密钥交换 → 登录验证 → 视频/输入/剪贴板/文件 7 路并发

use crate::app::config::SosConfig;
use base::protos::message::*;
use hbb_common::protobuf::Message as ProtobufMessage;
use hbb_common::{ResultType, Stream};

/// 连接 ID 分配器。
///
/// 必须**进程内唯一**：上游服务（clipboard / cursor / position）与自建
/// QoS 都用这个 ID 区分订阅者。旧实现硬编码 `conn_id = 1`，第二个控制端
/// 接入时会覆盖第一个的订阅，两边互相踢掉消息。
static NEXT_CONN_ID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

fn alloc_conn_id() -> i32 {
    NEXT_CONN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// 处理单个远程连接
pub async fn handle(
    stream: Stream,
    peer_addr: std::net::SocketAddr,
    config: SosConfig,
    pwd_refresh_tx: Option<&tokio::sync::mpsc::UnboundedSender<()>>,
) -> ResultType<()> {
    let addr_str = peer_addr.ip().to_string();
    // 每个连接分配唯一 ID：多控制端同时在线时，上游服务与自建 QoS
    // 都靠它区分订阅者（旧实现硬编码为 1，第二个控制端会覆盖第一个）。
    let conn_id = alloc_conn_id();
    log::info!(
        "[AUTH] incoming addr={} conn_id={} id={} fixed_pwd={}",
        addr_str,
        conn_id,
        config.id,
        !config.password.is_empty()
    );
    // 有人来连了：编号的任务已经完成，收掉窗口 —— 否则它会一直压在授权弹窗前面。
    crate::ui::device_id::close();

    // 检查登录频率限制
    let wait = check_login_rate_limit(&addr_str);
    if wait > 0 {
        log::warn!("[AUTH] rate limited for {}, wait {}s", addr_str, wait);
        return Ok(());
    }

    // Phase 1: 密钥交换 + 加密层
    let mut stream = match setup_encrypted_stream(stream, &config.id).await {
        Ok(s) => {
            log::debug!("[AUTH] key exchange ok");
            s
        }
        Err(e) => {
            log::warn!("[AUTH] key exchange failed: {}", e);
            return Ok(());
        }
    };

    // Phase 2: 登录验证循环
    //
    // 日志约定：**绝不记录**密码、salt、challenge 或任何中间哈希。
    // 这些东西组合起来足以离线爆破甚至直接重放；日志文件在客户机器上
    // 是明文落盘的。只记「走了哪条分支」「第几次尝试」「结果如何」。

    // 生成 salt 和 challenge
    use hbb_common::rand::Rng;
    let salt = crate::app::config::RegistryConfig::get_password_salt();
    let challenge: String = (0..6)
        .map(|_| {
            hbb_common::rand::thread_rng().sample(hbb_common::rand::distributions::Alphanumeric)
                as char
        })
        .collect();

    // 发送 Hash 给客户端（仅一次）
    {
        let mut hash_msg = Hash::new();
        hash_msg.salt = salt.clone();
        hash_msg.challenge = challenge.clone();
        let mut msg = Message::new();
        msg.set_hash(hash_msg);
        stream.send(&msg).await?;
        log::debug!("[AUTH] sent salt+challenge");
    }

    let mut login_attempts = 0;
    const MAX_LOGIN_ATTEMPTS: u32 = 5;
    // 等控制端发 LoginRequest 的上限。实测过一次「0.16 秒就报 60 秒超时」的日志：
    // `next_timeout` 把「超时」和「对端关闭」都归成 None，不看耗时就分不开。
    const WAIT_LOGIN_MS: u64 = 60_000;

    // 授权路径与上游 `approve-mode=both` 一致：**密码正确** 或 **本机用户点「同意」**，
    // 两条路径谁先完成谁生效。密码没通过时弹窗就已经出现，而此时控制端还停在它自己的
    // 密码框上（上游 `connection.rs:3066` 的 `lr.password.is_empty()` 分支：只弹窗、
    // 什么都不回）—— 所以等待期间必须继续读这条连接，用户可能正在输密码。
    let authorized_req: LoginRequest = loop {
        log::debug!(
            "[AUTH] waiting for LoginRequest (attempt {})",
            login_attempts + 1
        );
        let wait_started = std::time::Instant::now();
        let login_data = match stream.next_timeout(WAIT_LOGIN_MS).await {
            Some(Ok(d)) => d,
            Some(Err(e)) => {
                log::info!("[AUTH] stream error while waiting for login: {}", e);
                return Ok(());
            }
            None => {
                // None 既可能是等满超时，也可能是对端在这期间关掉了流。用耗时区分：
                // 提前结束就说明控制端自己走了（它重连时常见），不是我们等太久。
                let waited = wait_started.elapsed();
                if waited.as_millis() as u64 + 500 < WAIT_LOGIN_MS {
                    log::info!(
                        "[AUTH] peer closed before sending LoginRequest ({}ms, not a timeout)",
                        waited.as_millis()
                    );
                } else {
                    log::info!("[AUTH] timeout waiting for login request ({}s)", WAIT_LOGIN_MS / 1000);
                }
                return Ok(());
            }
        };

        let login_msg = match Message::parse_from_bytes(&login_data) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("[AUTH] failed to parse protobuf: {}, skipping", e);
                continue;
            }
        };
        if !login_msg.has_login_request() {
            // 记下前几个字节：没有它就只能看到「不是 LoginRequest」，无法判断
            // 是对端发错了、还是协议里别的消息（例如保活空包）撞进了登录窗口。
            let head: String = login_data
                .iter()
                .take(12)
                .map(|b| format!("{:02x}", b))
                .collect();
            // union 的 Debug 就是变体名（可能带内容），按字符截断避免切开 UTF-8。
            let what: String = format!("{:?}", login_msg.union)
                .chars()
                .take(120)
                .collect();
            log::warn!(
                "[AUTH] message is not a LoginRequest ({} bytes, head={}, union={}), skipping",
                login_data.len(),
                head,
                what
            );
            continue;
        }

        let login_req = login_msg.login_request();
        let received_h2 = &login_req.password;

        let authorized = password_matches(&config, &salt, &challenge, received_h2);
        log::debug!(
            "[AUTH] attempt {} hash match={}",
            login_attempts + 1,
            authorized
        );
        if authorized {
            log::info!("[AUTH] authorized by password, conn_id={}", conn_id);
            break login_req.clone();
        }

        // 密码没通过。空密码说明控制端还停在它自己的密码框上（并不是输错了什么），
        // 不计入失败次数；真输错了才计数。
        if received_h2.is_empty() {
            log::info!(
                "[AUTH] empty password from {} (controller is at its password prompt)",
                addr_str
            );
        } else {
            login_attempts += 1;
            log::warn!(
                "Login authorization failed from {} (attempt {})",
                addr_str,
                login_attempts
            );
            record_login_failure(&addr_str);
            // 发送错误消息，不关闭连接（客户端会弹出重试对话框）
            send_login_error(&mut stream, "Wrong Password").await;
            if login_attempts >= MAX_LOGIN_ATTEMPTS {
                log::warn!("Too many login attempts from {}, closing", addr_str);
                send_close_reason(&mut stream, "Wrong Password").await;
                return Ok(());
            }
        }

        // 本机用户此刻**看不见**弹窗（锁屏 / UAC 安全桌面 / 会话已断开）：不弹，
        // 按「密码即授权」继续等控制端把密码输对。窗口建在用户 Default 桌面上而
        // 用户看到的是 Winlogon 桌面 —— 弹了也只能白等到超时。
        if !local_user_can_approve() {
            continue;
        }

        // 本机用户确认。弹窗出现的同时控制端仍停在它自己的密码框上，谁先完成谁授权。
        log::info!(
            "[AUTH] asking the local user to approve conn_id={}",
            conn_id
        );
        match wait_for_local_approval(
            &mut stream,
            &addr_str,
            conn_id,
            login_req,
            &config,
            &salt,
            &challenge,
        )
        .await?
        {
            Approval::Authorized(req) => {
                log::info!("[AUTH] authorized by the local user, conn_id={}", conn_id);
                break req;
            }
            // 弹窗建不出来（无交互桌面、资源耗尽、win32 调用失败）：弹窗只是密码的
            // **替代**路径，不是前置条件，所以退回「只认密码」继续等，不能因此拒绝 ——
            // 否则窗口出个小问题就把合法用户挡在门外。
            Approval::NoDialog => continue,
            Approval::Denied => return Ok(()),
        }
    };

    // 授权成功后的公共流程（密码路径与点击路径共用）
    log::info!("[AUTH] authorized conn_id={}", conn_id);
    clear_login_failures(&addr_str);
    let is_file_transfer = authorized_req.has_file_transfer();
    let video_ack_required = authorized_req.video_ack_required;
    log::info!(
        "LoginRequest: file_transfer={}, video_ack_required={}",
        is_file_transfer,
        video_ack_required
    );
    // 握手期的解码能力决定编码格式，可以立刻应用。
    //
    // 画质/帧率**不能**在这里应用：QoS 的连接条目要等 `run_service_loop` 里的
    // `on_connection_open` 才建立，早了会命中 `[QOS] image_quality from unknown conn`
    // 并被丢弃。所以只把整条 option 传下去，由那边在注册之后应用。
    register_peer_decoding(&authorized_req);
    // 控制端平台用于能力声明（见 `send_login_response` 里的
    // `has_file_clipboard`）。旧客户端不带这个字段，取到空串。
    let peer_platform = authorized_req.my_platform.clone();
    // 新连接默认看第 0 块屏，`SwitchDisplay` 会把它改到别的屏
    send_login_response(&mut stream, conn_id, 0, &peer_platform).await?;

    if is_file_transfer {
        log::info!("File transfer connection detected");
        // 上游 behavior: 不主动发送初始目录列表，等待客户端 ReadDir 请求
        // 客户端打开文件传输 UI 后会自动发送 ReadDir，我们在 handle_incoming_message 中即时响应
        let result = run_file_transfer_loop(stream).await;
        log::info!("Connection closed");
        if let Some(tx) = pwd_refresh_tx {
            let _ = tx.send(());
        }
        return result;
    }

    // Phase 3: 启动所有服务
    let result = run_service_loop(
        stream,
        config,
        conn_id,
        video_ack_required,
        authorized_req.option.to_owned().into_option(),
    )
    .await;

    // Phase 4: 连接清理
    log::info!("Connection closed");
    // 只在认证成功的连接关闭后才刷新密码（防止中间失败也刷新）
    if let Some(tx) = pwd_refresh_tx {
        let _ = tx.send(());
    }
    result
}

/// 密钥交换 + 设置加密层
///
/// 与 RustDesk `create_tcp_connection` 一致：
/// 1. 生成本次会话的临时 NaCl box 密钥对
/// 2. 构造 IdPk { id: 设备ID, pk: NaCl公钥 }，用 Ed25519 签名
/// 3. 发送 SignedId 消息（含签名后的 IdPk）
/// 4. 等待对端回复 PublicKey 消息
/// 5. 用 NaCl box 解密对称密钥，设置加密层
async fn setup_encrypted_stream(mut stream: Stream, device_id: &str) -> ResultType<Stream> {
    use hbb_common::sodiumoxide::crypto::box_;
    use hbb_common::sodiumoxide::crypto::sign;

    // 获取本机 Ed25519 密钥对（用于签名）
    let (sk_bytes, pk_bytes) = crate::app::config::RegistryConfig::get_key_pair();
    if sk_bytes.len() != sign::SECRETKEYBYTES || pk_bytes.len() != sign::PUBLICKEYBYTES {
        anyhow::bail!(
            "Invalid Ed25519 key pair: sk={}, pk={} (expected {},{})",
            sk_bytes.len(),
            pk_bytes.len(),
            sign::SECRETKEYBYTES,
            sign::PUBLICKEYBYTES
        );
    }
    let mut sk_arr = [0u8; sign::SECRETKEYBYTES];
    sk_arr.copy_from_slice(&sk_bytes);
    let ed_sk = sign::SecretKey(sk_arr);

    // 生成临时 NaCl box 密钥对
    let (na_pk, na_sk) = box_::gen_keypair();

    // WebRTC 传输自带 DTLS 加密，但必须把自己的 DTLS 证书指纹签进 `IdPk`：
    // 控制端会拿签名里的指纹和它实际协商出的 DTLS 通道比对
    // （`dtls_fingerprint_bound`），不一致说明 SDP 在途中被改过（信令服务器或
    // 中继换掉了指纹），控制端会直接拒绝这次连接。
    //
    // 非 WebRTC 传输上留空（`..Default::default()`）：TCP/中继的机密性由下面的
    // NaCl 流密钥负责，与 DTLS 无关。指纹取不到时按失败处理 —— 签一个空指纹只会
    // 让对端在浪费一个来回之后才失败。
    let dtls_fingerprint = stream.dtls_fingerprint(true).await.unwrap_or_default();
    if stream.is_webrtc() && dtls_fingerprint.is_empty() {
        anyhow::bail!("WebRTC local DTLS fingerprint unavailable");
    }

    // 构造 IdPk 并序列化
    //
    // `IdPk` 定义在 `hbb_common` 的 rendezvous 协议里（与信令服务器共用），
    // 不在客户端的 message 协议中。
    let id_pk_bytes = hbb_common::rendezvous_proto::IdPk {
        id: device_id.to_string(),
        pk: hbb_common::bytes::Bytes::from(na_pk.0.to_vec()),
        dtls_fingerprint,
        ..Default::default()
    }
    .write_to_bytes()?;

    // 用 Ed25519 签名
    let signed_id = sign::sign(&id_pk_bytes, &ed_sk);

    // 发送 SignedId 消息
    let mut msg_out = base::protos::message::Message::new();
    msg_out.set_signed_id(base::protos::message::SignedId {
        id: signed_id.into(),
        ..Default::default()
    });
    stream.send(&msg_out).await?;

    // 等待 PublicKey 响应
    let pk_data = stream
        .next_timeout(10_000)
        .await
        .ok_or_else(|| anyhow::anyhow!("Timeout waiting for PublicKey"))??;
    let pk_msg = base::protos::message::Message::parse_from_bytes(&pk_data)?;
    let public_key = pk_msg.public_key();

    // 如果 asymmetric_value 为空，表示对端不认识本端公钥
    // （相当于 RustDesk 的 "Force to update pk" 分支）
    if public_key.asymmetric_value.is_empty() {
        log::warn!("Remote peer doesn't know our public key, connection cannot be secured");
        // 不 bail，与 RustDesk create_tcp_connection 一致（继续但不加密）
        // 对端会主动断开连接，调用方应触发 RegisterPk 重新注册
        return Err(anyhow::anyhow!("Peer doesn't know our public key"));
    }

    // 用 NaCl box 解密对称密钥
    stream.set_key(hbb_common::tcp::Encrypt::decode(
        &public_key.symmetric_value,
        &public_key.asymmetric_value,
        &na_sk,
    )?);

    log::debug!("Encrypted stream established");
    Ok(stream)
}

/// 出站队列中视频帧的最大积压数。超过后丢弃新视频帧而不是继续缓冲。
///
/// 写队列本身无界（不能对有控制消息的通道做阻塞），所以必须在这里主动丢弃，
/// 否则网络变慢时帧会无限堆积，表现为延迟持续增大。
///
/// 取 6：30fps 下约 200ms，与编码器的码率控制窗口（`RC_BUFFER_MS`）对齐。
/// 远程桌面的体感延迟对排队深度极其敏感——积压 30 帧就是 1 秒的操作延迟，
/// 已经远超「画面略卡」的范畴，不如直接丢旧帧。
const MAX_VIDEO_BACKLOG: usize = 6;

/// 累计被丢弃的视频帧数（仅诊断用）
static DROPPED_VIDEO_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 登录失败追踪（指数退避暴力破解防护）
const BACKOFF_BASE_SECS: u64 = 2; // 2^failures 秒
const BACKOFF_MAX_SECS: u64 = 1800; // 30 分钟上限
const IDLE_RESET_SECS: u64 = 7200; // 2 小时无失败重置

struct LoginFailure {
    count: u32,
    last_fail: std::time::Instant,
}

static LOGIN_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, LoginFailure>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 检查该地址是否被限流。返回等待秒数（0=允许登录）。
fn check_login_rate_limit(peer_addr: &str) -> u64 {
    let map = LOGIN_FAILURES.lock().unwrap();
    if let Some(state) = map.get(peer_addr) {
        let elapsed = state.last_fail.elapsed().as_secs();
        if elapsed > IDLE_RESET_SECS {
            return 0; // 空闲超时，重置
        }
        let wait = BACKOFF_BASE_SECS.min(BACKOFF_MAX_SECS) << (state.count - 1);
        let wait = wait.min(BACKOFF_MAX_SECS);
        if elapsed < wait {
            return wait - elapsed;
        }
    }
    0
}

/// 记录登录失败
fn record_login_failure(peer_addr: &str) {
    let mut map = LOGIN_FAILURES.lock().unwrap();
    let state = map.entry(peer_addr.to_string()).or_insert(LoginFailure {
        count: 0,
        last_fail: std::time::Instant::now(),
    });
    state.count = (state.count + 1).min(20); // 上限 20 次
    state.last_fail = std::time::Instant::now();
    log::info!("Login failure #{} from {}", state.count, peer_addr);
}

/// 登录成功清除失败记录
fn clear_login_failures(peer_addr: &str) {
    let mut map = LOGIN_FAILURES.lock().unwrap();
    map.remove(peer_addr);
}

/// 校验 `LoginRequest.password`（协议是 `sha256(sha256(pwd || salt) || challenge)`）。
///
/// 抽成函数是因为它现在有两个调用点：登录主循环，以及「等本机用户确认」期间
/// 控制端可能补发的新密码。
///
/// 摘要比较用的是普通字节比较（非定长）。对本协议足够：攻击者能测到的时间差远
/// 小于网络抖动，且每次失败都计入频率限制。
fn password_matches(config: &SosConfig, salt: &str, challenge: &str, received: &[u8]) -> bool {
    use hbb_common::sha2::{Digest, Sha256};

    let password_to_check = if !config.password.is_empty() {
        config.password.clone()
    } else {
        let current = crate::app::config::get_current_password();
        if current.is_empty() {
            log::warn!("[AUTH] no password configured, denying access");
        }
        current
    };
    if password_to_check.is_empty() {
        return false;
    }

    let mut hasher = Sha256::new();
    hasher.update(password_to_check.as_bytes());
    hasher.update(salt.as_bytes());
    let h1 = hasher.finalize();

    let mut hasher2 = Sha256::new();
    hasher2.update(h1);
    hasher2.update(challenge.as_bytes());
    let expected_h2 = hasher2.finalize();

    received == expected_h2.as_slice()
}

/// 发一条空 `Message` 保活。
///
/// 控制端的 io_loop 有一个 30s「无数据」看门狗（`src/client/io_loop.rs:337`），
/// 只要 30s 内收不到任何字节就报 "Timeout" 断开 —— 与它正在显示什么 UI 无关。
/// 弹窗可能开满 `CONSENT_TIMEOUT`，所以等待期间必须定期喂包。
///
/// **必须是空消息**，不能重发任何业务响应：空 `Message` 不命中控制端
/// `handle_msg_from_peer` 的任何分支（`src/client/io_loop.rs:2588` 的 `_ => {}`），
/// 只在收包处刷新 `last_recv_time`（`:303`），所以对控制端 UI 零影响 ——
/// 它原本停在哪个界面（例如密码输入框）就还停在哪个界面。上游自己也用空
/// `Message` 当空包敲对端（`src/client.rs:1698` 等处）。
async fn send_keepalive(stream: &mut Stream) -> ResultType<()> {
    stream.send(&Message::new()).await
}

/// 本机用户确认这一步的结果。
enum Approval {
    /// 已授权（点「同意」，或等待期间控制端输对了密码）
    Authorized(LoginRequest),
    /// 被拒绝 / 超时 / 控制端已断开 —— 连接该结束
    Denied,
    /// 弹窗根本没能建出来 —— **不算拒绝**，退回「只认密码」继续等
    NoDialog,
}

/// 本机用户是否**看得见并能点到**确认弹窗。
///
/// 锁屏 / UAC 安全桌面期间活动桌面是 Winlogon，而我们的窗口建在用户 Default 桌面
/// 上 —— 用户既看不见也点不到，弹出来只能白等到 `CONSENT_TIMEOUT`（180s）再拒绝，
/// 等于把「有人在场」这件事误判成「无人值守」。会话断开（退出 RDP）同理：那一侧
/// 压根没人在看屏幕。这两种情况一律不弹，退回「密码即授权」。
fn local_user_can_approve() -> bool {
    let secure = crate::capture::uac::secure_desktop();
    let surface = crate::capture::uac::session_state().may_have_surface();
    if !secure && !surface {
        log::trace!(
            "[AUTH] skipping the approval dialog: lock screen / UAC / disconnected session"
        );
    }
    !secure && surface
}

/// 等本机用户点「同意」，期间继续读这条连接并定时保活。
///
/// 语义与上游 `approve-mode=both` 一致：**点击是密码的替代，不是附加条件**。
/// 所以这里同时等两件事，谁先完成谁生效：
///
/// * 控制端补发一条密码正确的 `LoginRequest`（用户正在它自己的密码输入框里输入）；
/// * 本机用户在弹窗里点「同意」。
///
/// 返回 `Some(req)` = 已授权（`req` 是**触发授权的那条** LoginRequest：点「同意」
/// 时是 `seed`，输对密码时是那条新请求）；返回 `None` = 拒绝 / 超时 / 控制端断开，
/// 连接该做的事（发不接受原因、记失败次数）都已做完，调用方直接结束。
async fn wait_for_local_approval(
    stream: &mut Stream,
    addr: &str,
    conn_id: i32,
    seed: &LoginRequest,
    config: &SosConfig,
    salt: &str,
    challenge: &str,
) -> ResultType<Approval> {
    let hint = crate::ui::consent::PeerHint {
        id: seed.my_id.clone(),
        // 新客户端带 `my_name`；旧客户端只有 `username`（主机名/账号）
        name: if seed.my_name.is_empty() {
            seed.username.clone()
        } else {
            seed.my_name.clone()
        },
        platform: seed.my_platform.clone(),
        addr: addr.to_string(),
        file_transfer: seed.has_file_transfer(),
    };

    let mut dialog = Box::pin(crate::ui::consent::confirm(hint));
    // 控制端是否已断开。断开后不再发保活，但仍然等弹窗收场（`cancel_pending`
    // 会让它立刻返回），避免把阻塞任务丢在后台。
    let mut peer_gone = false;

    // 一次 select 的产出：控制端来了数据 / 本机用户给了决定
    enum Step {
        Msg(Option<std::result::Result<hbb_common::bytes::BytesMut, std::io::Error>>),
        Decided(crate::ui::consent::Decision),
    }

    let decision = loop {
        // 超时时长就是保活间隔：`next_timeout` 返回 None 即到了该发保活的时候。
        // 这样不需要额外的 interval future，也就不会和 stream 的可变借用打架。
        let step = tokio::select! {
            msg = stream.next_timeout(crate::ui::consent::WAIT_REFRESH.as_millis() as u64) => {
                Step::Msg(msg)
            }
            decided = &mut dialog => Step::Decided(decided),
        };

        match step {
            Step::Decided(decision) => break decision,
            Step::Msg(Some(Ok(data))) => {
                // 控制端可能在它的密码输入框里输对了密码 —— 那时无需本机用户再点。
                if let Ok(msg) = Message::parse_from_bytes(&data) {
                    if msg.has_login_request() {
                        let req = msg.login_request();
                        if password_matches(config, salt, challenge, &req.password) {
                            log::info!(
                                "[AUTH] password accepted while waiting for local approval, conn_id={}",
                                conn_id
                            );
                            crate::ui::consent::cancel_pending();
                            return Ok(Approval::Authorized(req.clone()));
                        }
                        log::debug!(
                            "[AUTH] still waiting for local approval (password not accepted)"
                        );
                    }
                }
                // 其它消息（控制端此时通常只可能在等响应）直接忽略。
            }
            Step::Msg(Some(Err(e))) => {
                log::info!(
                    "[AUTH] stream error while waiting for local approval: {}, conn_id={}",
                    e,
                    conn_id
                );
                peer_gone = true;
                crate::ui::consent::cancel_pending();
            }
            Step::Msg(None) => {
                // 这一片静默到期：喂一个保活包，顺便探测控制端是否还活着。
                if !peer_gone && send_keepalive(stream).await.is_err() {
                    log::info!(
                        "[AUTH] peer disconnected while waiting for local approval, conn_id={}",
                        conn_id
                    );
                    peer_gone = true;
                    crate::ui::consent::cancel_pending();
                }
            }
        }
    };

    if peer_gone {
        return Ok(Approval::Denied);
    }

    if decision.is_accepted() {
        return Ok(Approval::Authorized(seed.clone()));
    }

    if matches!(decision, crate::ui::consent::Decision::Unavailable) {
        log::warn!(
            "[AUTH] approval dialog could not be shown: conn_id={} addr={} (falling back \
             to password-only auth)",
            conn_id,
            addr
        );
        return Ok(Approval::NoDialog);
    }

    log::warn!(
        "[AUTH] not approved by the local user: conn_id={} addr={} decision={:?}",
        conn_id,
        addr,
        decision
    );
    // 记一次失败：否则「拒绝」可以被无成本地重连反复触发
    record_login_failure(addr);
    send_close_reason(stream, "").await;
    Ok(Approval::Denied)
}


/// 发送登录错误（不关闭连接，让客户端重试）
async fn send_login_error(stream: &mut Stream, err: &str) {
    let mut msg = Message::new();
    let mut res = base::protos::message::LoginResponse::new();
    res.set_error(err.to_string());
    msg.set_login_response(res);
    let _ = stream.send(&msg).await;
}

/// 发送登录成功响应
///
/// 填充 PeerInfo 以告知控制端本机信息，
/// 与 RustDesk 的 send_logon_response_and_keep_alive() 对齐。
async fn send_login_response(
    stream: &mut Stream,
    _conn_id: i32,
    display: usize,
    peer_platform: &str,
) -> ResultType<()> {
    let mut peer_info = PeerInfo::new();
    // 获取主机名
    let hostname = get_hostname();
    if !hostname.is_empty() {
        peer_info.username = hostname;
    }
    peer_info.version = crate::app::constants::VERSION.to_string();
    // 必须与上游同一取值（`hbb_common::whoami::platform()` → "Windows"），
    // **不能**用 `std::env::consts::OS`（"windows"，小写）。
    // 控制端大量判断是 `pi.platform == kPeerPlatformWindows`（首字母大写），
    // 大小写不一致会让那些分支静默走错。
    peer_info.platform = hbb_common::whoami::platform().to_string();

    // 填充显示器信息（RustDesk 客户端需要此信息来显示画面）
    //
    // 必须包在独立作用域里：`scrap::Display` 内部持有 DXGI/COM 裸指针，
    // **不是 `Send`**，一旦活过后面的 `.await`，整个 future 就不再 `Send`，
    // 外层 `tokio::spawn` 会编译失败。
    {
        let displays = scrap::Display::all().unwrap_or_default();
        if displays.is_empty() {
            // 锁屏 / UAC 安全桌面 / RDP 断开时 `Display::all()` 返回空表（拿不到输出）。
            //
            // 上游 `handle_peer_info` 对**空 displays 是直接报错中止会话**
            // （"No displays"），于是用户根本连不上锁屏中的机器 —— 而「机器重启后
            // 无人登录」正是远程协助最常见的场景。
            //
            // 这里上报一个占位显示器让会话能建立：尺寸取 `GetSystemMetrics`
            // （`crate::capture::display::primary_size()`，不经过 DXGI，锁屏时仍可用）。
            // 真实几何会在桌面重新可捕获后由 `video` 的 `SwitchDisplay` 纠正 ——
            // `CaptureState::new` 成功时会广播真实尺寸与设备名。
            //
            // 不填 `resolutions`：此时并不知道真实的模式列表，让控制端的分辨率
            // 菜单保持空态，比给一份错的列表安全。
            let (w, h) =
                crate::capture::display::primary_size().unwrap_or(crate::capture::display::FALLBACK_SIZE);
            let mut info = DisplayInfo::new();
            info.width = w as i32;
            info.height = h as i32;
            // 设备名留空：不假装知道真实名字。`size_by_name("")` 会回退到
            // `primary_size()`，与本处的取值一致，不会引起尺寸误判。
            info.name = String::new();
            info.online = true;
            info.cursor_embedded = false;
            peer_info.displays.push(info);
            log::warn!(
            "[VIDEO] no display available (locked screen / UAC / RDP detached?), reporting placeholder {}x{} so the session can be established",
            w,
            h
        );
        } else {
            for display in displays.iter() {
                let mut info = DisplayInfo::new();
                // 原点必须上报真实值：多显示器下副屏的原点不是 (0,0)，
                // 控制端靠它把鼠标坐标映射回正确的屏幕。
                let (x, y) = display.origin();
                info.x = x;
                info.y = y;
                info.width = display.width() as i32;
                info.height = display.height() as i32;
                // 必须用真实设备名，不能用 "Display {i}"：控制端可能按名字匹配显示器，
                // 而 `SwitchDisplay` 里发的也是真实名，两处不一致会显示混乱。
                info.name = display.name();
                info.online = true;
                info.cursor_embedded = false;
                peer_info.displays.push(info);
            }
            // 填充支持的分辨率列表（控制端据此显示"更改分辨率"菜单）
            if let Some(d) = displays.first() {
                use base::message_proto::SupportedResolutions;
                peer_info.resolutions = Some(SupportedResolutions {
                    resolutions: librustdesk::platform::resolutions(&d.name()),
                    ..Default::default()
                })
                .into();
            }
        }
        // 控制端连接时看到的屏 = 该连接实际订阅的屏。
        // 占位显示器只有一个，索引必然是 0。
        peer_info.current_display = if displays.is_empty() {
            0
        } else {
            display as i32
        };
        log::info!(
            "Sent display info: {} displays, current=#{}",
            peer_info.displays.len(),
            peer_info.current_display
        );
    }

    // 上报功能支持（客户端据此决定输入模式等行为）
    use base::message_proto::Features;
    peer_info.features = hbb_common::protobuf::MessageField::some(Features {
        privacy_mode: false,
        terminal: false,
        ..Default::default()
    });
    let mut encoding = scrap::codec::Encoder::supported_encoding();
    encoding.av1 = false;
    // H.265 同理显式钉死。这行不是冗余：上游 `supported_encoding()` 里那句
    // `encoding.h265 |= HwRamEncoder::try_get(H265).is_some()` 目前**没有**编进来
    // （`scrap` 的 `hwcodec` feature 在 SOS 构建里未启用，我们直接依赖 hwcodec crate），
    // 所以现状恰好是 false；一旦有人给 scrap 打开那个 feature，本端就会开始谎报
    // 支持 H.265 —— 而 `SosCodec` 根本没有 H265 变体。显式写死让这个声明与实现解耦。
    // H.265 同样门控在本机真的有 HEVC 编码器：见 `video::hwenc::available_h265`。
    // 两种硬件编码器是独立探测的，所以这里用各自的结论，不要拿 h264 的结果顶替。
    encoding.h265 = crate::video::hw_available_h265();
    // H.264 只有硬件编码（见 `video::hwenc`）。只在**本机真的有可用编码器**时才声明：
    // 声明了却没有实现，控制端会显示 H.264 选项、用户选了却仍旧收到 VP9 码流。
    encoding.h264 = crate::video::hw_available();
    // 记一条「实际用哪个硬件编码器」：AMD / Intel / NVIDIA 机器上分别是 h264_amf /
    // h264_qsv / h264_nvenc，排障时一眼能看出走的是哪条后端。
    if encoding.h264 {
        log::info!(
            "[HWENC] announcing h264 via {}",
            crate::video::hwenc::encoder_name()
        );
    }
    if encoding.h265 {
        log::info!(
            "[HWENC] announcing h265 via {}",
            crate::video::hwenc::encoder_name_h265()
        );
    }
    // 我们的编码链路**只有 I420**（`video::vpx` 固定 `VPX_IMG_FMT_I420`），
    // 但 `scrap::Encoder::supported_encoding()` 是通用实现，会声称支持
    // VP9/AV1 的 4:4:4。谎报会让控制端认为可以协商真彩模式。
    //
    // 注意：**光清空这里并不能隐藏控制端的「真彩模式 (4:4:4)」菜单** ——
    // 那条菜单的可见性只看控制端自己实测的 `codec_format`（见下方说明），
    // 清空只是让协议声明与真实能力一致。
    if let Some(i444) = encoding.i444.as_mut() {
        *i444 = Default::default();
    }
    peer_info.encoding = hbb_common::protobuf::MessageField::some(encoding);

    // ── 能力声明（`platform_additions`）──
    //
    // 这里只放**上游已经定义好的标准键**，不发明自定义键：控制端对未知键
    // 只是存进 map，不会理解；而对标准键有确定的 UI 行为。
    //
    // 当前只声明 `has_file_clipboard`：
    // 控制端 `toolbarDisplayToggle()` 用
    // `pi.platformAdditions.containsKey("has_file_clipboard")` 决定是否显示
    // 「启用文件复制粘贴」。我们**确实**实现了文件剪贴板（cliprdr 通道 +
    // 上游 clipboard service 订阅，见 `run_service_loop`），但以前从没声明，
    // 于是这个本可用的开关一直被隐藏。
    //
    // 条件与上游一致：**双方都是 Windows** 才为真（上游 `is_both_windows`）。
    // 旧客户端不带 `my_platform`，取到空串 → 视为非 Windows → 不声明，
    // 与上游行为相同。
    if peer_platform.eq_ignore_ascii_case("windows") {
        use hbb_common::serde_json::json;
        let additions = json!({ "has_file_clipboard": true });
        peer_info.platform_additions = additions.to_string();
    }

    let mut login_resp = LoginResponse::new();
    login_resp.set_peer_info(peer_info);

    let mut msg = Message::new();
    msg.set_login_response(login_resp);
    stream.send(&msg).await?;
    log::debug!("LoginResponse sent");

    // ── 隐藏本端根本没有实现的功能 ──
    //
    // 控制端的可见性判断形如 `perms['audio'] != false`：**没收到权限消息时
    // 默认为「有」**。所以只靠 PeerInfo 里不声明是没用的，必须主动发一条
    // `PermissionInfo`，否则「静音」这类开关会一直显示且点了没有任何效果。
    //
    // 只能覆盖协议里有权限位的项：
    // | 菜单 | 协议开关 |
    // |---|---|
    // | 静音 | `PermissionInfo(Audio, false)` |
    // | 录制 | `PermissionInfo(Recording, false)` |
    // | 真彩 4:4:4 | **无**（控制端只看自己实测的 `codec_format`）|
    // | 会话结束后锁定 | **无**（只受 `keyboard` 权限影响）|
    // | 分辨率 / 显示质量监测 / 编解码 | **无**（无条件显示）|
    //
    // 后三项要隐藏只能改上游 UI，那不在本项目的改动范围（sos-client 只
    // 替换受控端逻辑，不改 `flutter/`、`libs/`、`src/`）。
    deny_permission(&mut *stream, permission_info::Permission::Audio).await?;
    deny_permission(&mut *stream, permission_info::Permission::Recording).await?;
    Ok(())
}

/// 通知控制端本端不具备某项能力（`Misc::PermissionInfo`）。
///
/// 与上游 `Connection::send_permission` 的语义一致：`enabled = false` 表示
/// 「不可用」，控制端据此隐藏或禁用对应开关。
async fn deny_permission(
    stream: &mut Stream,
    permission: permission_info::Permission,
) -> ResultType<()> {
    let mut misc = Misc::new();
    misc.set_permission_info(PermissionInfo {
        permission: permission.into(),
        enabled: false,
        ..Default::default()
    });
    let mut msg = Message::new();
    msg.set_misc(misc);
    stream.send(&msg).await
}

/// 获取本机主机名
fn get_hostname() -> String {
    #[cfg(windows)]
    {
        // Windows 上使用 COMPUTERNAME 环境变量
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOSTNAME").unwrap_or_default()
    }
}

// ── 连接收发分离 ──
//
// 旧实现把 `stream.send().await` 内联在 select! 分支中：一旦发送缓冲被大帧填满，
// 这个 await 会挂起整个事件循环，`stream.next()`（输入事件入口）随之停摆，
// 表现为画面仍在刷新但鼠标键盘失效的“假死”。
// 这里把 TCP 流拆成读/写两半：写半区交给独立 task 串行发送，主循环只投递消息；
// 读半区留在主循环，任何时刻都能第一时间取到控制消息。
// WebSocket/WebRTC 无法拆分，保持原有内联收发行为。

use hbb_common::bytes::Bytes;
use hbb_common::bytes_codec::BytesCodec;
use hbb_common::tcp::DynTcpStream;
use hbb_common::tokio::io::{ReadHalf, WriteHalf};
use hbb_common::tokio_util::codec::{Framed, FramedParts};
use std::sync::Arc;

/// 拆分后的 TCP 读半区（沿用原 codec 的解码状态与已读缓冲）
type TcpReader = Framed<ReadHalf<DynTcpStream>, BytesCodec>;
/// 拆分后的 TCP 写半区（由独立写任务持有）
type TcpWriter = Framed<WriteHalf<DynTcpStream>, BytesCodec>;

enum Conn {
    /// 已拆分：写任务独立运行，发送不阻塞事件循环
    Split {
        reader: TcpReader,
        decrypt: Option<hbb_common::tcp::Encrypt>,
        tx: tokio::sync::mpsc::UnboundedSender<Arc<Message>>,
        /// 已入队但尚未写完的消息数（用于诊断发送是否为瓶颈）
        pending: Arc<std::sync::atomic::AtomicUsize>,
        /// 已发出的字节数（诊断码率用）
        bytes_sent: Arc<std::sync::atomic::AtomicU64>,
        /// 其中视频帧的字节数。
        /// `sent_kbps` 里混着鼠标位置/光标形状/TestDelay 等控制消息，
        /// 不分开就无法判断「码率超发」到底是编码器还是控制流量造成的。
        video_bytes_sent: Arc<std::sync::atomic::AtomicU64>,
        writer_task: tokio::task::JoinHandle<()>,
    },
    /// WebSocket/WebRTC 等无法拆分：保持原有内联收发
    Whole(Stream),
}

impl Conn {
    /// 尝试把 TCP 流拆成独立的读/写通道；其他传输方式原样返回。
    ///
    /// # 为什么这里有一段 `unsafe`
    ///
    /// 上游给 `Stream` 加了 `Drop`（用来关闭 WebRTC 的对等连接），于是不能再
    /// 按值解构它 —— 即使只想取 `Tcp` 变体也会报 `E0509: cannot move out of
    /// type which implements the Drop trait`。
    ///
    /// 这里把整个值放进 `ManuallyDrop`（阻止它被 drop），再从 `&mut` 上把
    /// `Tcp` 变体里的 `FramedStream` 按位读出来：
    ///
    /// - `FramedStream` 自身没有 `Drop`、四个字段全是 `pub`，按位取出后由我们
    ///   负责持有并释放，不会泄漏;
    /// - `Stream::drop` 的全部作用就是 `close_webrtc()`，对非 WebRTC 变体是
    ///   空操作 —— 所以跳过这次 drop 不改变任何资源释放路径;
    /// - 取出后立即 `return`，`slot` 之后再也不会被访问。
    ///
    /// 非 TCP 变体走 `ManuallyDrop::into_inner` 交回所有权，后续正常 drop，
    /// WebRTC 的清理不受影响。
    fn split(stream: Stream) -> Self {
        let mut slot = std::mem::ManuallyDrop::new(stream);
        if let Stream::Tcp(framed_stream) = &mut *slot {
            // SAFETY: 见上方文档 —— 目标是 `Tcp` 变体时才会执行到这里，
            // 且取出后 `slot` 不再被使用也不会被 drop。
            let hbb_common::tcp::FramedStream(framed, _addr, encrypt, send_timeout) =
                unsafe { std::ptr::read(framed_stream) };

            let parts = framed.into_parts();
            let (read_io, write_io) = hbb_common::tokio::io::split(parts.io);
            // 读半区必须保留已解码缓冲：登录应答之后对端可能已经在同一
            // TCP 段里发来了后续消息，丢弃会导致输入事件丢失、流错位。
            let mut read_parts = FramedParts::new::<Bytes>(read_io, parts.codec);
            read_parts.read_buf = parts.read_buf;
            let reader = Framed::from_parts(read_parts);
            let writer = Framed::new(write_io, BytesCodec::new());

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let bytes_sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let video_bytes_sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
            // Encrypt 的发送序号(.1)与接收序号(.2)互不相干，各复制一份即可
            // 让两个方向独立递增，不破坏加密协议。
            let writer_task = tokio::spawn(write_loop(
                writer,
                encrypt.clone(),
                send_timeout,
                rx,
                pending.clone(),
                bytes_sent.clone(),
                video_bytes_sent.clone(),
            ));
            return Conn::Split {
                reader,
                decrypt: encrypt,
                tx,
                pending,
                bytes_sent,
                video_bytes_sent,
                writer_task,
            };
        }
        Conn::Whole(std::mem::ManuallyDrop::into_inner(slot))
    }

    /// 投递一条出站消息（拆分后仅入队，不阻塞）
    async fn send(&mut self, msg: Arc<Message>) -> ResultType<()> {
        match self {
            Conn::Split { tx, pending, .. } => {
                // 写队列是无界的，若不设阀值，网络一旦跟不上帧就会无限堆在
                // 队列里（实测 `sendq` 到过 35 帧，相当于积累了 2 秒以上的画面），
                // 表现为“操作延迟越来越大”而不是丢帧。
                //
                // 实时视频的正确语义是**丢旧帧**而不是缓存：新帧到达后旧帧
                // 已经无意义。所以只对视频帧做丢帧，控制消息（鼠标/键盘/
                // 剪贴板）无论如何都入队，否则会出现输入丢失。
                let backlog = pending.load(std::sync::atomic::Ordering::Relaxed);
                if backlog > MAX_VIDEO_BACKLOG
                    && matches!(
                        msg.union,
                        Some(base::message_proto::message::Union::VideoFrame(_))
                    )
                {
                    DROPPED_VIDEO_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // 丢掉的是**已编码**的帧，编码器的参考链与解码端已经错位。
                    // 本链路 `kf_mode = VPX_KF_DISABLED`，不重新出关键帧的话
                    // 对端会一直花屏到重连。码率会很便宜地多花一次关键帧。
                    crate::video::request_keyframe();
                    return Ok(());
                }
                pending.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tx.send(msg).map_err(|_| {
                    pending.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    anyhow::anyhow!("connection writer task has exited")
                })
            }
            Conn::Whole(stream) => stream.send(&*msg).await,
        }
    }

    /// 出站队列积压的消息数（用于判断发送是否成为瓶颈）
    fn queue_len(&self) -> usize {
        match self {
            Conn::Split { pending, .. } => pending.load(std::sync::atomic::Ordering::Relaxed),
            Conn::Whole(_) => 0,
        }
    }

    /// 取走自上次调用以来已发送的字节数
    fn take_bytes_sent(&self) -> u64 {
        match self {
            Conn::Split { bytes_sent, .. } => {
                bytes_sent.swap(0, std::sync::atomic::Ordering::Relaxed)
            }
            Conn::Whole(_) => 0,
        }
    }

    /// 取走自上次调用以来已发送的**视频**字节数
    fn take_video_bytes_sent(&self) -> u64 {
        match self {
            Conn::Split {
                video_bytes_sent, ..
            } => video_bytes_sent.swap(0, std::sync::atomic::Ordering::Relaxed),
            Conn::Whole(_) => 0,
        }
    }

    /// 接收下一条入站消息
    async fn recv(&mut self) -> Option<Result<hbb_common::bytes::BytesMut, std::io::Error>> {
        match self {
            Conn::Split {
                reader, decrypt, ..
            } => {
                use hbb_common::futures::StreamExt;
                let mut res = reader.next().await;
                if let Some(Ok(bytes)) = res.as_mut() {
                    if let Some(key) = decrypt.as_mut() {
                        if let Err(e) = key.dec(bytes) {
                            return Some(Err(e));
                        }
                    }
                }
                res
            }
            Conn::Whole(stream) => stream.next().await,
        }
    }

    /// 结束连接：冲刷队列中的消息后再退出写任务
    async fn shutdown(self) {
        if let Conn::Split {
            tx,
            mut writer_task,
            ..
        } = self
        {
            drop(tx);
            if tokio::time::timeout(std::time::Duration::from_secs(3), &mut writer_task)
                .await
                .is_err()
            {
                log::warn!("Connection writer did not finish in 3s, aborting");
                writer_task.abort();
            }
        }
    }
}

/// 写任务：串行发送队列中的消息（只有它接触写半区）
async fn write_loop(
    mut writer: TcpWriter,
    mut encrypt: Option<hbb_common::tcp::Encrypt>,
    send_timeout: u64,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Arc<Message>>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    bytes_sent: Arc<std::sync::atomic::AtomicU64>,
    video_bytes_sent: Arc<std::sync::atomic::AtomicU64>,
) {
    use hbb_common::futures::SinkExt;
    while let Some(msg) = rx.recv().await {
        // 序列化前先判断类型，便于把视频与控制流量分开计量
        let is_video = matches!(
            msg.union,
            Some(base::message_proto::message::Union::VideoFrame(_))
        );
        let mut data = match msg.write_to_bytes() {
            Ok(d) => d,
            Err(e) => {
                pending.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                log::error!("Failed to serialize outbound message: {}", e);
                continue;
            }
        };
        if let Some(key) = encrypt.as_mut() {
            data = key.enc(&data);
        }
        let data_len = data.len() as u64;
        let fut = writer.send(Bytes::from(data));
        let res = if send_timeout > 0 {
            match hbb_common::timeout(send_timeout, fut).await {
                Ok(r) => r,
                Err(_) => {
                    log::error!("Send timeout ({}ms), dropping connection", send_timeout);
                    break;
                }
            }
        } else {
            fut.await
        };
        pending.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = res {
            log::error!("Failed to send outbound message: {}", e);
            break;
        }
        bytes_sent.fetch_add(data_len, std::sync::atomic::Ordering::Relaxed);
        if is_video {
            video_bytes_sent.fetch_add(data_len, std::sync::atomic::Ordering::Relaxed);
        }
    }
    log::debug!("Connection writer task exited");
}

/// 光标服务（进程内唯一）
///
/// `input_service::new_cursor()` **每调用一次就 spawn 一个 33ms 轮询线程**，
/// 而 `ServiceTmpl::repeat` 的循环条件是 `sp.active()`（只会被 `join()` 置 false）。
/// 所以按连接创建就等于每连一次永久泄漏一个线程，而且旧线程仍挂着 `conn #1`
/// 的订阅：控制端会收到重复的光标消息，断连后还会每秒刷一条
/// `Error of mouse_cursor service: 拒绝访问。(os error 5)`。
fn cursor_service() -> &'static librustdesk::server::service::ServiceTmpl<
    librustdesk::server::input_service::MouseCursorSub,
> {
    static SVC: std::sync::OnceLock<
        librustdesk::server::service::ServiceTmpl<
            librustdesk::server::input_service::MouseCursorSub,
        >,
    > = std::sync::OnceLock::new();
    SVC.get_or_init(librustdesk::server::input_service::new_cursor)
}

/// 鼠标位置服务（进程内唯一）。理由同 [`cursor_service`]。
fn pos_service() -> &'static librustdesk::server::service::GenericService {
    static SVC: std::sync::OnceLock<librustdesk::server::service::GenericService> =
        std::sync::OnceLock::new();
    SVC.get_or_init(librustdesk::server::input_service::new_pos)
}

/// 连接结束（任何退出路径）时自动取消光标/位置服务订阅。
///
/// 用 `Drop` 而不是在函数末尾显式调用：`run_service_loop` 里有多个
/// `return` / `break` 出口（发送失败、读失败、需要重连），漏掉任何一条
/// 都会让服务线程继续往已关闭的连接里推消息。
struct SvcGuard {
    cursor: &'static librustdesk::server::service::ServiceTmpl<
        librustdesk::server::input_service::MouseCursorSub,
    >,
    pos: &'static librustdesk::server::service::GenericService,
    id: i32,
}

impl Drop for SvcGuard {
    fn drop(&mut self) {
        use librustdesk::service::Service;
        self.cursor.on_unsubscribe(self.id);
        self.pos.on_unsubscribe(self.id);
        log::info!("Unsubscribed cursor/position services (conn #{})", self.id);
    }
}

/// 按「是否处于安全桌面」挂起/恢复光标与鼠标位置服务的订阅。
///
/// # 为什么必须挂起
///
/// 安全桌面（锁屏 / UAC）上，本进程（普通高完整性、非 SYSTEM）读不到
/// 那个桌面的光标：
///
/// ```text
/// [ERROR] Error of mouse_cursor service: 拒绝访问。 (os error 5)   ← 每秒一条
/// [ERROR] Failed to switch desktop: 拒绝访问。 (os error 5)        ← 每秒一条
/// ```
///
/// 第二条是上游 `ServiceTmpl::repeat` 在回调失败后主动调的
/// `try_change_desktop()` —— 非 SYSTEM 进程必然失败，于是再记一条。
/// 现场日志里这两条会永久刷屏，把真正有用的信息埋掉。
///
/// 而且订阅本身也没有意义：此刻画面来自 SYSTEM 子进程，光标属于**另一个**
/// 桌面，推给控制端的坐标是错的（会让远端光标乱跳）。
///
/// # 为什么可以安全地取消再订阅
///
/// `ServiceTmpl::repeat` 的循环条件是 `active()`（只在 `join()` 时置 false），
/// 与订阅数无关；没有订阅者时它走 `has_subscribes() == false` 分支只做
/// `state.reset()` 然后 sleep。所以取消订阅既不会杀线程，也不会丢状态机，
/// 重新订阅时 `state.init()` 会重新初始化。
fn sync_cursor_services(
    cursor: &'static librustdesk::server::service::ServiceTmpl<
        librustdesk::server::input_service::MouseCursorSub,
    >,
    pos: &'static librustdesk::server::service::GenericService,
    conn_id: i32,
    tx: &tokio::sync::mpsc::UnboundedSender<(
        hbb_common::tokio::time::Instant,
        std::sync::Arc<base::protos::message::Message>,
    )>,
    subscribed: &mut bool,
) {
    use librustdesk::service::Service;
    // 立刻反映真实桌面状态（`crate::capture::uac::secure_desktop()` 与主进程 watcher
    // 用的是同一判据，这里只读，不改任何全局状态）。
    //
    // 除了安全桌面，**会话断开**同样读不到光标：现场日志里 `input_desktop=<open
    // failed 5>`，`desktop_changed()` 此时还没翻成 TRUE，于是挂起判定迟到，
    // 先漏出一两条 `Error of mouse_cursor service: 拒绝访问`。断开状态下本来就
    // 没有可采集的表面，光标坐标也毫无意义，所以一并按「不订阅」处理。
    let want = !crate::capture::uac::secure_desktop()
        && crate::capture::uac::session_state().may_have_surface();
    if want == *subscribed {
        return;
    }
    *subscribed = want;
    if want {
        let inner = librustdesk::ConnInner::new(conn_id, Some(tx.clone()), None);
        cursor.on_subscribe(inner);
        let inner = librustdesk::ConnInner::new(conn_id, Some(tx.clone()), None);
        pos.on_subscribe(inner);
        log::info!("[SECURE] cursor/pos service resumed (conn #{})", conn_id);
    } else {
        cursor.on_unsubscribe(conn_id);
        pos.on_unsubscribe(conn_id);
        log::info!(
            "[SECURE] cursor/pos service suspended on secure desktop (conn #{})",
            conn_id
        );
    }
}

// min-filter 的窗口长度与「不可信样本」上限见 [`crate::net::rtt`]（那边可独立单测）。

/// 探测超时：超过这个时间没回显就丢弃重发。
/// 必须小于 QoS 的 2 秒迟延阈值，否则等待时间会被误判成「网络卡死」。
const TEST_DELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// 超过这个间隔视为采集中断而非帧间隔，不计入 gap 统计。
const GAP_OUTAGE_MS: u64 = 2000;

/// 一次统计窗口的汇总结果，用于打印 `[VIDEO:FPS]`。
struct StatsReport {
    vid_frames: u32,
    gap_min: u64,
    gap_avg: u64,
    gap_max: u64,
    network_delay: u32,
    rtt_raw: u32,
}

/// 单连接的运行期状态。
///
/// 抽成结构体是为了让 `tokio::select!` 的每个分支都能收敛成一个方法调用：
/// 否则主循环里 `loop → select! → match → match → if let → if` 会叠到 9 层，
/// 再加上各分支共享 10 个局部可变变量，几乎无法阅读和修改。
struct Session {
    conn_id: i32,
    /// 该连接正在观看的显示器索引
    display: usize,
    /// 该显示器的设备名（QoS 按名聚合）
    display_name: String,
    video_ack_required: bool,

    // ── 延迟测量（min-filter）──
    rtt: crate::net::rtt::RttFilter,
    /// 最近一次原始采样，仅用于日志对比滤波效果
    rtt_raw: u32,
    /// min-filter 之后的 RTT 下界
    network_delay: u32,
    /// 当前在途探测的发出时刻
    probe_sent_at: Option<std::time::Instant>,

    // ── 帧率诊断 ──
    vid_frames: u32,
    stats_last: std::time::Instant,
    /// 逐帧间隔：区分「编码慢」与「画面静止不发帧」
    last_frame_at: Option<std::time::Instant>,
    gap_sum: u64,
    gap_max: u64,
    gap_min: u64,

    /// 控制端勾选了「会话结束后锁定远程电脑」（`OptionMessage.lock_after_session_end`）。
    ///
    /// 上游把这一项存在 `Connection` 上，由 `on_close(reason, lock)` 决定
    /// 何时生效；SOS 没有 `on_close`，所以由 [`run_service_loop`] 在退出时判断。
    lock_after_session_end: bool,
}

impl Session {
    fn new(conn_id: i32, display: usize, video_ack_required: bool) -> Self {
        let display_name = crate::video::cached_display_name(display)
            .or_else(|| crate::video::display_info(display).map(|(_, _, n)| n))
            .unwrap_or_default();
        Self {
            conn_id,
            display,
            display_name,
            video_ack_required,
            rtt: Default::default(),
            rtt_raw: 0,
            network_delay: 0,
            probe_sent_at: None,
            vid_frames: 0,
            stats_last: std::time::Instant::now(),
            last_frame_at: None,
            gap_sum: 0,
            gap_max: 0,
            gap_min: u64::MAX,
            lock_after_session_end: false,
        }
    }

    /// 用于 QoS 查询的显示器名。
    ///
    /// **不能**只用构造时缓存的名字：连接可能建立在锁屏期间（当时
    /// `Display::all()` 返回空表，名字为空），解锁后采集线程才拿到真实
    /// 设备名。两边不一致会让 QoS 按屏聚合完全失配 —— 采集线程用真实名
    /// 查不到观看者，帧率永远停在默认值、码率上报为 0。
    ///
    /// `cached_display_name()` 只加锁读字符串，不开销枚举，可以放进每秒
    /// 的 QoS 同步路径。
    fn qos_display_name(&self) -> String {
        crate::video::cached_display_name(self.display)
            .unwrap_or_else(|| self.display_name.clone())
    }

    /// 记一帧已发出，更新逐帧间隔统计。
    ///
    /// 超过 [`GAP_OUTAGE_MS`] 的间隔是**采集中断**（锁屏、UAC、RDP 断开），
    /// 不是帧间隔。实测这类中断让 `gap_ms[max]` 变成 31508，看上去像编码器故障。
    fn note_video_frame(&mut self) {
        self.vid_frames += 1;
        let now = std::time::Instant::now();
        if let Some(prev) = self.last_frame_at {
            let gap = prev.elapsed().as_millis() as u64;
            if gap < GAP_OUTAGE_MS {
                self.gap_sum += gap;
                self.gap_max = self.gap_max.max(gap);
                self.gap_min = self.gap_min.min(gap);
            }
        }
        self.last_frame_at = Some(now);
    }

    /// 处理一次探测回显。`raw` 是本次往返的原始毫秒数。
    ///
    /// 返回 `false` 表示样本不可信（迟到的不合理值），已丢弃。
    fn on_probe_echo(&mut self, raw: u32) -> bool {
        // 滤波与「可信样本」判定都在 [`crate::net::rtt::RttFilter`] 里，那边可独立单测。
        let Some((floor, queue)) = self.rtt.push(raw) else {
            return false;
        };
        self.rtt_raw = raw;
        self.network_delay = floor;
        // 自建 QoS：只更新**这个连接**的帧率，再由显示器层聚合（同一块屏取最小）。
        crate::video::qos::with(|q| q.on_test_delay(self.conn_id, floor, queue));
        // 视频链路据此决定是否做帧率下采样：低 RTT 时不丢帧保流畅，高 RTT 时守码率。
        // 只喂一次：延迟与排队是**同一次探测**的两个量，拆成两次调用会让
        // 档位迟滞的计数被加速一倍。
        crate::video::note_link_sample(floor, queue);
        true
    }

    /// 判断这一拍是否该发出新探测（上一拍未超时才发）。
    fn should_send_probe(&mut self) -> bool {
        if let Some(tm) = self.probe_sent_at {
            if tm.elapsed() <= TEST_DELAY_TIMEOUT {
                return false;
            }
            // 超时未回显：丢弃，下一拍重发。
            // 这里**不能**把「我们等了多久」当成网络延迟写进 QoS ——
            // 迟到一秒就会注入 1000-2000ms 的假样本，把帧率砸到个位数。
            // 真实拥塞已经由有界视频通道的背压反映。
            self.probe_sent_at = None;
        }
        self.probe_sent_at = Some(std::time::Instant::now());
        true
    }

    /// 取出并重置本统计窗口的数据
    fn take_stats(&mut self) -> StatsReport {
        let gap_avg = if self.vid_frames > 1 {
            self.gap_sum / (self.vid_frames as u64 - 1)
        } else {
            self.gap_sum
        };
        let report = StatsReport {
            vid_frames: self.vid_frames,
            gap_min: if self.gap_min == u64::MAX {
                0
            } else {
                self.gap_min
            },
            gap_avg,
            gap_max: self.gap_max,
            network_delay: self.network_delay,
            rtt_raw: self.rtt_raw,
        };
        self.vid_frames = 0;
        self.gap_sum = 0;
        self.gap_max = 0;
        self.gap_min = u64::MAX;
        self.stats_last = std::time::Instant::now();
        report
    }

    /// 切换到另一块屏：更新订阅表与 QoS 归属。返回是否真的变了。
    fn switch_display(&mut self, idx: usize) -> bool {
        if idx == self.display {
            return false;
        }
        let Some((_, _, name)) = crate::video::display_info(idx) else {
            log::warn!(
                "Switch display to #{} rejected, keeping #{}",
                idx,
                self.display
            );
            return false;
        };
        self.display = idx;
        // 1. 订阅表迁移到新屏（必要时拉起该屏的采集管线）
        crate::video::set_subscriber_display(self.conn_id, idx);
        // 2. QoS 把该连接归到新屏的聚合组
        crate::video::qos::with(|q| q.set_connection_display(self.conn_id, &name));
        log::info!(
            "conn #{} switched to display #{} '{}'",
            self.conn_id,
            idx,
            name
        );
        self.display_name = name;
        true
    }
}

/// 主循环的后续动作：继续下一轮，或跳出并清理
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// 处理一条收到的远程消息。
///
/// 三类消息走完全不同的路径，且 `TestDelay` 的两个方向语义相反 ——
/// 这些判断以前全挤在 `tokio::select!` 的分支里，叠到 9 层嵌套，
/// 传 10 个跨分支共享的局部变量。抽出来之后主循环只剩一个 `match`。
///
/// `TestDelay` 的方向必须区分（与上游 `src/server/connection.rs` 一致）：
/// - `from_client = true`：控制端自己发起的探测。原样回显，让控制端算延迟。
///   **不能**用它计算本机 RTT。
/// - `from_client = false`：本机探测被回显。用它计算 RTT。
///
/// 之前不区分，把两端 1Hz 定时器的相位差当成了 RTT，于是 RTT 在 40ms 与
/// 1000ms 之间缓慢摆动，帧率被在 30 和 1 之间来回拉锯。
async fn pump_recv(
    data: hbb_common::bytes::BytesMut,
    session: &mut Session,
    file_transfer: &mut crate::net::file_transfer::FileTransferHandler,
    conn: &mut Conn,
) -> ResultType<Flow> {
    let test_delay = parse_test_delay(&data);
    match test_delay {
        Some(td) if td.from_client => {
            // 回显给控制端；它靠这个测量并显示延迟
            let mut out = Message::new();
            out.set_test_delay(td);
            if let Err(e) = conn.send(Arc::new(out)).await {
                log::error!("Failed to echo TestDelay: {}", e);
                send_close_reason_conn(conn, "Send error").await;
                return Ok(Flow::Stop);
            }
            Ok(Flow::Continue)
        }
        Some(_) => {
            // 本机探测的回显，这才是真实 RTT
            if let Some(tm) = session.probe_sent_at.take() {
                let raw = tm.elapsed().as_millis() as u32;
                if !session.on_probe_echo(raw) {
                    log::debug!("discard implausible TestDelay echo {}ms", raw);
                }
            }
            Ok(Flow::Continue)
        }
        None => {
            handle_incoming_message(data, session, file_transfer, conn).await?;
            Ok(Flow::Continue)
        }
    }
}

/// 从原始消息里取出 `TestDelay`（不是该类型则返回 `None`）
fn parse_test_delay(data: &hbb_common::bytes::BytesMut) -> Option<TestDelay> {
    let msg = base::message_proto::Message::parse_from_bytes(data).ok()?;
    match msg.union {
        Some(base::message_proto::message::Union::TestDelay(td)) => Some(td),
        _ => None,
    }
}

/// 打印一个统计窗口的 `[VIDEO:FPS]`
fn log_session_stats(session: &mut Session, conn: &mut Conn) {
    let elapsed = session.stats_last.elapsed().as_secs_f64().max(0.001);
    let total_bytes = conn.take_bytes_sent();
    let video_bytes = conn.take_video_bytes_sent();
    let kbps = total_bytes as f64 * 8.0 / 1000.0 / elapsed;
    let vkbps = video_bytes as f64 * 8.0 / 1000.0 / elapsed;
    let qos = crate::video::qos::display_snapshot(&session.qos_display_name());
    let stats = session.take_stats();
    log::info!(
        "[VIDEO:FPS] display #{} encode_out={:.1}fps sent_kbps={:.0} vkbps={:.0} sendq={} drop={} qos_fps={} qos_bitrate={}kbps codec={} gap_ms[min/avg/max]={}/{}/{} rtt={}ms raw={}ms ack_required={}",
        session.display,
        stats.vid_frames as f64 / elapsed,
        kbps,
        vkbps,
        conn.queue_len(),
        DROPPED_VIDEO_FRAMES.swap(0, std::sync::atomic::Ordering::Relaxed),
        qos.fps,
        qos.bitrate_kbps,
        crate::video::codec_name(),
        stats.gap_min,
        stats.gap_avg,
        stats.gap_max,
        stats.network_delay,
        stats.rtt_raw,
        session.video_ack_required
    );
}

/// 运行主服务循环（视频 + 输入 + 剪贴板 + 文件传输）
///
/// `video_ack_required` 来自控制端的 `LoginRequest`：为 true 时视频帧必须等待
/// 对端 `Misc::VideoReceived` 才能放行下一帧，避免无限制帧在途撑满发送缓冲。
async fn run_service_loop(
    stream: Stream,
    _config: SosConfig,
    conn_id: i32,
    video_ack_required: bool,
    // 握手期的 `Misc.Option`，画质/帧率必须在 QoS 连接注册**之后**应用（见调用点注释）
    login_option: Option<base::message_proto::OptionMessage>,
) -> ResultType<()> {
    use tokio::time::{sleep, Duration};

    // 文件传输
    let (ft_tx, mut ft_rx) =
        tokio::sync::mpsc::unbounded_channel::<base::message_proto::Message>();
    let mut file_transfer = crate::net::file_transfer::FileTransferHandler::new(ft_tx);

    // 拆分收发：写半区交由独立 task，主循环不会再被 send() 卡住
    let mut conn = Conn::split(stream);

    // ── 订阅 SOS 自建视频链路 + 上游 clipboard_service ──
    use hbb_common::tokio::time::Instant as TokioInstant;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(
        TokioInstant,
        std::sync::Arc<base::protos::message::Message>,
    )>();
    // 视频通道用有界 channel 做背压：网络写不动时捕获线程会阻塞，自动降帧
    let (tx_video, mut rx_video) = tokio::sync::mpsc::channel::<(
        tokio::time::Instant,
        std::sync::Arc<base::protos::message::Message>,
    )>(crate::video::VIDEO_CHANNEL_CAPACITY);
    // 订阅 SOS 自建视频链路（采集→I420→libvpx→发送）。
    // 替代上游 video_service：绕开 codec_thread_num 的线程钳制与每帧 flush。
    // 该连接初始观看第 0 块屏；`SwitchDisplay` 会更新 `Session` 里的值。
    let display: usize = 0;
    crate::video::subscribe(conn_id, display, tx.clone(), tx_video.clone());
    log::info!(
        "Subscribed to SOS video pipeline as conn #{} ({})",
        conn_id,
        crate::video::describe()
    );

    if let Some(clip_svc) = crate::app::config::CLIPBOARD_SVC.get() {
        let inner = librustdesk::ConnInner::new(conn_id, Some(tx.clone()), None);
        librustdesk::service::Service::on_subscribe(clip_svc, inner);
        log::info!(
            "Subscribed to upstream clipboard service as conn #{}",
            conn_id
        );
    }

    // ── 订阅光标和位置服务（使客户端显示正常鼠标样式而非触控样式）──
    let cursor_svc = cursor_service();
    let inner = librustdesk::ConnInner::new(conn_id, Some(tx.clone()), None);
    librustdesk::service::Service::on_subscribe(cursor_svc, inner);
    log::info!("Subscribed to cursor service as conn #{}", conn_id);

    let pos_svc = pos_service();
    let inner = librustdesk::ConnInner::new(conn_id, Some(tx.clone()), None);
    librustdesk::service::Service::on_subscribe(pos_svc, inner);
    log::info!("Subscribed to position service as conn #{}", conn_id);
    // 连接结束时（任何退出路径）自动取消订阅，见 [`SvcGuard`]
    let _cursor_guard = SvcGuard {
        cursor: cursor_svc,
        pos: pos_svc,
        id: conn_id,
    };
    // 当前是否处于「已订阅」状态。安全桌面期间会临时挂起，见下面
    // 1 秒定时分支里的 [`sync_cursor_services`]。
    let mut svc_subscribed = true;

    // ── 注册 cliprdr 通道（文件剪贴板引擎的回调通道）──
    let rx_cliprdr = clipboard::get_rx_cliprdr_server(conn_id);
    log::info!("Registered cliprdr channel for conn #{}", conn_id);
    let mut rx_cliprdr_guard = rx_cliprdr.lock().await;

    // 通知自建 QoS 有新连接。显示器名由 `CaptureState` 侧解析，
    // 这里先用索引对应的真实设备名注册，供 QoS 按屏聚合。
    let mut session = Session::new(conn_id, display, video_ack_required);
    crate::video::qos::with(|q| q.on_connection_open(conn_id, &session.qos_display_name()));
    // 连接已注册进 QoS，此刻才能写画质/帧率。控制端只在握手与用户改档两处发画质
    // （上游 `src/client.rs` 的 `get_option_message(true)` 与会话中改档），不会重发 ——
    // 漏掉这一处，用户默认档位是 best/low/自定义时会一直停在 Balanced(0.67)，
    // 直到手动动一次画质菜单才生效。
    if let Some(opt) = login_option.as_ref() {
        apply_option_quality(conn_id, opt);
    }

    // 本次连接是否由控制端**主动关闭**（正常结束）。
    // 只有它为 true 时「会话结束后锁定」才会生效：发送/接收错误、协议错误
    // 属于异常断开，锁屏会让用户莫名其妙丢掉桌面。
    let mut closed_by_peer = false;

    let heartbeat_interval = Duration::from_secs(10);
    // 光标/位置服务的挂起判定要比 1 秒更快：安全桌面切换与会话断开的瞬间，
    // 上游 `ServiceTmpl::repeat` 会在失败回调上各记一条 ERROR。250ms 把这个
    // 「漏日志 + 白跑一次 33ms 轮询」的窗口压到 1/4。
    let mut cursor_timer = tokio::time::interval(Duration::from_millis(250));
    let mut test_delay_timer = tokio::time::interval(Duration::from_secs(1));
    // 帧率诊断：编码器产出帧数 / 发送队列积压 / QoS 目标
    let mut stats_timer = tokio::time::interval(Duration::from_secs(5));

    // ── 定期发送 portable_service_running = true ──
    // 上游 connection.rs:portable_check() 在 is_installed()=true 时直接 return，
    // 不会发送 CmShowElevation(false) 和 Misc.portable_service_running=true。
    // Flutter 的 onPeerInfo() 会重置 _running=false，因此我们需要持续发送 true。
    let mut running_timer = tokio::time::interval(Duration::from_secs(3));
    // 发送初始 portable_service_running=true（之后的定期刷新由 running_timer 完成）
    {
        let mut misc = Misc::new();
        misc.set_portable_service_running(true);
        let mut msg = Message::new();
        msg.set_misc(misc);
        let _ = conn.send(Arc::new(msg)).await;
    }

    loop {
        tokio::select! {
            // 视频帧（来自 SOS 自建视频链路）
            Some((_ts, vf_msg)) = rx_video.recv() => {
                session.note_video_frame();
                if let Err(e) = conn.send(vf_msg).await {
                    log::error!("Failed to send video frame: {}", e);
                    send_close_reason_conn(&mut conn, "Send error").await;
                    break;
                }
            }

            // 普通消息（来自剪贴板/光标/位置服务）
            Some((_ts, msg)) = rx.recv() => {
                if let Err(e) = conn.send(msg).await {
                    log::error!("Failed to send message: {}", e);
                    send_close_reason_conn(&mut conn, "Send error").await;
                    break;
                }
            }

            // cliprdr 文件剪贴板消息（来自 C 引擎的回调，转发到远程客户端）
            clip_msg = rx_cliprdr_guard.recv() => {
                if let Some(clip) = clip_msg {
                    let msg = librustdesk::clipboard_file::clip_2_msg(clip);
                    if let Err(e) = conn.send(Arc::new(msg)).await {
                        log::error!("Failed to send cliprdr message: {}", e);
                        send_close_reason_conn(&mut conn, "Send error").await;
                        break;
                    }
                }
            }

            // 接收远程消息
            msg = conn.recv() => {
                match msg {
                    Some(Ok(data)) => {
                        match pump_recv(data, &mut session, &mut file_transfer, &mut conn).await {
                            Ok(Flow::Continue) => {}
                            Ok(Flow::Stop) => break,
                            Err(e) => log::error!("Error handling message: {}", e),
                        }
                    }
                    Some(Err(e)) => {
                        log::error!("Stream recv error: {}", e);
                        send_close_reason_conn(&mut conn, "Stream error").await;
                        break;
                    }
                    None => {
                        // 控制端主动关闭会话 —— 这是唯一「正常结束」，
                        // 「会话结束后锁定」只在这里生效（见
                        // [`should_lock_after_session_end`]）。
                        log::info!("Stream closed by remote");
                        send_close_reason_conn(&mut conn, "Remote close").await;
                        closed_by_peer = true;
                        break;
                    }
                }
            }

            // 文件传输响应（tokio mpsc 直接 await）
            Some(resp) = ft_rx.recv() => {
                if let Err(e) = conn.send(Arc::new(resp)).await {
                    log::error!("Failed to send file response: {}", e);
                    send_close_reason_conn(&mut conn, "File send error").await;
                    break;
                }
            }

            // 文件传输 send job 轮询
            _ = sleep(if file_transfer.has_send_jobs() { Duration::from_millis(10) } else { Duration::from_secs(1) }) => {
                file_transfer.poll_send_jobs().await;
            }

            // 光标/位置服务挂起状态（判据便宜：桌面句柄比较 + 一次 WTS 查询）
            _ = cursor_timer.tick() => {
                sync_cursor_services(cursor_svc, pos_svc, conn_id, &tx, &mut svc_subscribed);
            }

            // TestDelay：每 1 秒发送给客户端（供其显示延迟和码率）
            _ = test_delay_timer.tick() => {
                // 把 QoS 里该连接记录的显示器名同步成「当前真实设备名」。
                // 名字会变：RDP 会话里 `DISPLAY369`，解锁后变成 `DISPLAY1`，锁屏期间是 `UAC`。
                // 不在注册时固定下来，改名之后 `viewers_of` 就查不到这个连接，
                // 画质/帧率全部回落默认值 —— 表现为「控制端改画质完全没反应」
                // （现场日志：收到 `enum=Best`，但 `qos_bitrate` 一直停在默认档）。
                // `on_connection_open` 对同一个 id 只更新显示器，不动其它设置。
                crate::video::qos::with(|q| q.on_connection_open(conn_id, &session.qos_display_name()));
                if session.should_send_probe() {
                    let mut td = TestDelay::new();
                    td.time = hbb_common::get_time();
                    td.last_delay = session.network_delay;
                    td.target_bitrate = crate::video::qos::bitrate_kbps(&session.qos_display_name());
                    let mut msg = Message::new();
                    msg.set_test_delay(td);
                    let _ = conn.send(Arc::new(msg)).await;
                }
            }

            // 帧率诊断（每 5 秒）：编码器产出 fps / 实际发送码率 / 发送队列积压 / QoS 目标
            _ = stats_timer.tick() => {
                log_session_stats(&mut session, &mut conn);
            }

            // 心跳超时检测
            _ = sleep(heartbeat_interval) => {
                log::trace!("Heartbeat tick");
            }

            // 定期刷新 portable_service_running=true（应对 Flutter 端 onPeerInfo 重置 _running=false）
            _ = running_timer.tick() => {
                let mut misc = Misc::new();
                misc.set_portable_service_running(true);
                let mut msg = Message::new();
                msg.set_misc(misc);
                let _ = conn.send(Arc::new(msg)).await;
            }
        }
    }

    // 冲刷发送队列后关闭写任务
    conn.shutdown().await;

    // 取消订阅自建视频链路
    crate::video::unsubscribe(conn_id);
    log::info!("Unsubscribed from SOS video pipeline (conn #{})", conn_id);

    // 移除 cliprdr 通道
    clipboard::remove_channel_by_conn_id(conn_id);
    log::info!("Removed cliprdr channel for conn #{}", conn_id);

    // 通知自建 QoS 连接已关闭
    crate::video::qos::with(|q| q.on_connection_close(conn_id));

    // 恢复控制端改过的分辨率
    librustdesk::server::display_service::restore_resolutions();

    // ── 会话结束后锁定（控制端勾选的选项）──
    //
    // 放在最后：先让发送队列冲刷、订阅清理、分辨率恢复都走完，再锁屏。
    // 锁屏是本次会话的**最后**一个动作，用户回到登录界面时不该还有
    // 任何残留状态在变化。
    if should_lock_after_session_end(session.lock_after_session_end, closed_by_peer) {
        lock_workstation();
    } else if session.lock_after_session_end {
        log::info!(
            "[LOCK] lock-after-session-end is set, but the session ended abnormally (not by peer), skipping lock"
        );
    }

    Ok(())
}

/// 文件传输专用循环（不启动视频/剪贴板服务）
async fn run_file_transfer_loop(stream: Stream) -> ResultType<()> {
    use tokio::time::{sleep, Duration};

    let (ft_tx, mut ft_rx) =
        tokio::sync::mpsc::unbounded_channel::<base::message_proto::Message>();
    let mut file_transfer = crate::net::file_transfer::FileTransferHandler::new(ft_tx);
    let mut keepalive_timer = tokio::time::interval(Duration::from_secs(10));

    // 文件块发送同样可能阻塞，统一走拆分后的写任务
    let mut conn = Conn::split(stream);
    // 文件传输连接不订阅视频。但 `handle_incoming_message` 共用同一套消息处理，
    // 需要一个占位会话（显示器相关字段在此路径不会被使用）。
    let conn_id = alloc_conn_id();
    let mut session = Session::new(conn_id, 0, false);

    loop {
        tokio::select! {
            msg = conn.recv() => {
                match msg {
                    Some(Ok(data)) => {
                        if let Err(e) = handle_incoming_message(data, &mut session, &mut file_transfer, &mut conn).await {
                            log::error!("Error handling message: {}", e);
                        }
                    }
                    Some(Err(e)) => {
                        log::info!("Stream error: {}", e);
                        break;
                    }
                    None => {
                        log::info!("File transfer stream closed by remote");
                        break;
                    }
                }
            }
            Some(resp) = ft_rx.recv() => {
                if let Err(e) = conn.send(Arc::new(resp)).await {
                    log::error!("Failed to send file response: {}", e);
                    break;
                }
            }
            _ = sleep(if file_transfer.has_send_jobs() { Duration::from_millis(10) } else { Duration::from_secs(1) }) => {
                file_transfer.poll_send_jobs().await;
            }
            _ = keepalive_timer.tick() => {
                let mut misc = Misc::new();
                misc.set_portable_service_running(true);
                let mut msg = Message::new();
                msg.set_misc(misc);
                let _ = conn.send(Arc::new(msg)).await;
            }
        }
    }
    conn.shutdown().await;
    Ok(())
}

/// 构造关闭原因消息
fn close_reason_msg<'a>(reason: &'a str) -> (&'a str, Message) {
    let reason = if reason.is_empty() {
        "Closed manually by the peer"
    } else {
        reason
    };
    let mut misc = Misc::new();
    misc.set_close_reason(reason.to_string());
    let mut msg = Message::new();
    msg.set_misc(misc);
    (reason, msg)
}

/// 发送关闭原因给远程端（未拆分的 `Stream`，用于登录阶段）
async fn send_close_reason(stream: &mut Stream, reason: &str) {
    let (reason, msg) = close_reason_msg(reason);
    let _ = stream.send(&msg).await;
    log::info!("Sent close reason: {}", reason);
}

/// 发送关闭原因给远程端（拆分连接，仅入队不阻塞事件循环）
async fn send_close_reason_conn(conn: &mut Conn, reason: &str) {
    let (reason, msg) = close_reason_msg(reason);
    let _ = conn.send(Arc::new(msg)).await;
    log::info!("Sent close reason: {}", reason);
}

// ── 会话结束后锁定远程电脑 ──

/// 是否应该在本次断开时锁屏（纯函数，便于单测）。
///
/// 对应上游 `Connection::on_close(reason, lock)` 的 `if lock && self.lock_after_session_end`。
///
/// 两个条件缺一不可：
///
/// 1. **控制端勾选了这个选项**。客户端在「只读模式」下会主动把它置为 `No`
///    （`src/client.rs` 的 `update_options`），所以勾选状态本身就代表
///    「用户确实想让我锁屏」。
/// 2. **本次断开是正常结束**（`closed_by_peer`）。发送/接收错误、协议错误
///    属于异常断开，此时锁屏会让用户莫名其妙地丢掉桌面 —— 上游对
///    `on_close(err, false)` 的几处调用也是这个取舍。
///
/// 上游还要求 `self.keyboard`（服务端「允许键盘」权限）。SOS 不实现服务端
/// 输入权限（收到输入就注入），所以这里没有对应条件；控制端在只读模式下
/// 已经自己关掉了选项。
fn should_lock_after_session_end(lock_option: bool, closed_by_peer: bool) -> bool {
    lock_option && closed_by_peer
}

/// 锁定当前会话（Windows 上是 `LockWorkStation`）。
///
/// 走上游 `librustdesk::platform::lock_screen()`，**不修改**上游代码 ——
/// 它只是对 `LockWorkStation` 的一层薄封装，Windows 上就是一次系统调用，
/// 不会阻塞，因此可以直接在 async 上下文里调用。
///
/// 位置很关键：必须由**用户会话**里的进程调用。SOS 的连接处理运行在
/// 主进程（日志里的 `Type=Primary`，非 SYSTEM 子进程），所以能生效；
/// 若在 SYSTEM 子进程里调用会锁到 session 0，对用户桌面毫无作用。
fn lock_workstation() {
    #[cfg(windows)]
    {
        log::info!("[LOCK] session ended, locking the workstation (peer requested)");
        librustdesk::platform::lock_screen();
    }
    #[cfg(not(windows))]
    log::warn!("[LOCK] lock-after-session-end is not implemented on this platform");
}

/// 把协议里的 `PreferCodec` 映射为本端能理解的编码偏好。
///
/// 本端只有 VP8/VP9 软件编码器，H264/H265/AV1 一律降级为 `Auto`，
/// 由 [`crate::video::vpx::choose_codec`] 的后备启发式决定。
fn codec_prefer_of(
    sd: &base::message_proto::SupportedDecoding,
) -> crate::video::vpx::CodecPrefer {
    use crate::video::vpx::CodecPrefer;
    use base::message_proto::supported_decoding::PreferCodec;
    match sd.prefer.enum_value() {
        Ok(PreferCodec::VP8) => CodecPrefer::Vp8,
        Ok(PreferCodec::VP9) => CodecPrefer::Vp9,
        Ok(PreferCodec::H264) => CodecPrefer::H264,
        Ok(PreferCodec::H265) => CodecPrefer::H265,
        Ok(PreferCodec::Auto) => CodecPrefer::Auto,
        Ok(other) => {
            log::info!(
                "[VIDEO] peer prefers {:?}, no such encoder here, using Auto",
                other
            );
            CodecPrefer::Auto
        }
        Err(_) => CodecPrefer::Auto,
    }
}

/// 应用握手期控制端选项：解码能力（选编码格式）+ 画质/帧率（定码率预算）。
///
/// 自建视频链路不使用 `scrap` 的编码器，因此无需维护其 `PEER_DECODINGS`。
///
/// 画质部分与运行期 `Misc::Option` 的处理（`handle_incoming_message`）语义一致：
/// 枚举优先，`NotSet` 才看自定义值 —— 两者解析的是同一个 `OptionMessage`，
/// 只是到达时机不同（登录握手 vs 会话中）。上游同样把整条 login option 交给
/// `update_options`（`src/server/connection.rs`），不会漏掉画质。
/// 应用一条 `OptionMessage` 里的画质与帧率。
///
/// 登录期（`LoginRequest.option`）与会话中（`Misc.Option`）走同一套语义：
/// **枚举优先，`NotSet` 才看自定义值** —— 两者本来就是同一个消息，只是到达时机不同。
/// 调用方必须保证该 `conn_id` 已在 `video::qos` 注册（否则值会被丢弃并记 warn）。
fn apply_option_quality(conn_id: i32, opt: &base::message_proto::OptionMessage) {
    if opt.custom_fps > 0 {
        crate::video::qos::with(|q| q.set_custom_fps(conn_id, opt.custom_fps as u32));
    }
    if let Ok(q) = opt.image_quality.enum_value() {
        use base::message_proto::ImageQuality;
        let v = if q == ImageQuality::NotSet {
            opt.custom_image_quality
        } else {
            q as i32
        };
        if v > 0 {
            log::info!(
                "[VIDEO] image quality from conn #{}: enum={:?} custom={} -> {}",
                conn_id,
                q,
                opt.custom_image_quality,
                v
            );
            crate::video::qos::with(|q| q.set_image_quality(conn_id, v));
        }
    }
}

fn register_peer_decoding(lr: &base::message_proto::LoginRequest) {
    let sd = lr
        .option
        .as_ref()
        .and_then(|o| o.supported_decoding.as_ref());
    let (vp8, vp9, h264, h265, prefer) = match sd {
        Some(sd) => (
            sd.ability_vp8 > 0,
            sd.ability_vp9 > 0,
            sd.ability_h264 > 0,
            sd.ability_h265 > 0,
            codec_prefer_of(sd),
        ),
        None => (
            true,
            true,
            false,
            false,
            crate::video::vpx::CodecPrefer::Auto,
        ),
    };
    crate::video::set_peer_codec_ability(vp8, vp9, h264, h265, prefer);
}

/// 处理接收到的远程消息
async fn handle_incoming_message(
    data: hbb_common::bytes::BytesMut,
    session: &mut Session,
    file_transfer: &mut crate::net::file_transfer::FileTransferHandler,
    _conn: &mut Conn,
) -> ResultType<()> {
    let conn_id = session.conn_id;
    if data.is_empty() {
        return Ok(());
    }

    let msg = match Message::parse_from_bytes(&data) {
        Ok(m) => m,
        Err(_) => return Ok(()), // 忽略无法解析的消息
    };

    // 调试日志：打印键盘事件类型（用于排查 IME 问题）
    if msg.has_key_event() {
        let ke = msg.key_event();
        log::trace!(
            "[INPUT] KeyEvent down={} CtrlKey={} Chr={} Uni={} Seq(len={})",
            ke.down,
            ke.has_control_key(),
            ke.has_chr(),
            ke.has_unicode(),
            if ke.has_seq() { ke.seq().len() } else { 0 }
        );
    }

    if log::log_enabled!(log::Level::Trace) {
        log_msg_types(&msg);
    }

    if msg.has_mouse_event() {
        // 序列化 MouseEvent 原始字节 → 管道 → SYSTEM 子进程调上游 handle_mouse_
        let me = msg.mouse_event();
        let proto_bytes = me.write_to_bytes().unwrap_or_default();
        let mut data = Vec::with_capacity(4 + proto_bytes.len());
        data.extend_from_slice(&crate::app::constants::MSG_MOUSE.to_ne_bytes());
        data.extend_from_slice(&proto_bytes);
        crate::ipc::pipe::try_send_input(data);
    } else if msg.has_key_event() {
        // 序列化 KeyEvent 原始字节 → 管道 → SYSTEM 子进程调上游 handle_key
        let ke = msg.key_event();
        let proto_bytes = ke.write_to_bytes().unwrap_or_default();
        let mut data = Vec::with_capacity(4 + proto_bytes.len());
        data.extend_from_slice(&crate::app::constants::MSG_KEY.to_ne_bytes());
        data.extend_from_slice(&proto_bytes);
        crate::ipc::pipe::try_send_input(data);
    } else if msg.has_clipboard() {
        // 单个剪贴板（文本/图片）
        librustdesk::clipboard::update_clipboard(
            vec![msg.clipboard().clone()],
            librustdesk::clipboard::ClipboardSide::Host,
        );
    } else if msg.has_multi_clipboards() {
        // 多格式剪贴板（文本/图片/特殊格式）
        let mcb = msg.multi_clipboards();
        librustdesk::clipboard::update_clipboard(
            mcb.clipboards.clone(),
            librustdesk::clipboard::ClipboardSide::Host,
        );
    } else if msg.has_cliprdr() {
        // 文件剪贴板协议（Cliprdr）- 使用上游 cliprdr C 引擎处理
        #[cfg(windows)]
        {
            if let Some(clip) = librustdesk::clipboard_file::msg_2_clip(msg.cliprdr().clone()) {
                let _ = clipboard::ContextSend::proc(|context| {
                    context
                        .server_clip_file(conn_id, clip)
                        .map_err(|e| e.into())
                });
            }
        }
        #[cfg(not(windows))]
        log::warn!("Cliprdr messages not supported on this platform");
    } else if msg.has_misc() {
        let misc = msg.misc();
        // 调试日志
        if misc.has_option() {
            let o = misc.option();
            // image_quality 也要打：画质选项是否生效全靠这里看，
            // 只打 custom_fps 会让「调了画质但没反应」无从定位。
            log::info!(
                "[INPUT] Misc::Option fps={} image_quality={:?} custom_image_quality={} decoding={}",
                o.custom_fps,
                o.image_quality.enum_value(),
                o.custom_image_quality,
                o.supported_decoding.is_some()
            );
        }
        if misc.has_switch_display() {
            log::info!(
                "[INPUT] Misc::SwitchDisplay display={}",
                misc.switch_display().display
            );
        }
        if misc.has_video_received() {
            // 视频帧确认：背压由通道容量保证，这里只留一条 trace 便于排查
            log::trace!("[INPUT] Misc::VideoReceived");
        }
        // 控制端请求刷新：它解码出错时靠这个消息等一个关键帧重同步。
        // 上游由 `refresh_video_display()` 处理；SOS 之前**完全忽略**，
        // 导致对端出现一次解码失败就只能靠断开重连接恢复。
        if misc.has_refresh_video() {
            log::info!("[VIDEO] peer requested video refresh");
            crate::video::request_keyframe();
        }
        if misc.has_refresh_video_display() {
            log::info!(
                "[VIDEO] peer requested refresh of display {}",
                misc.refresh_video_display()
            );
            crate::video::request_refresh();
        }
        // 客户端选项更新（含 image_quality, custom_fps, supported_decoding 等）
        if misc.has_option() {
            let opt = misc.option();
            if opt.custom_fps > 0 {
                crate::video::qos::with(|q| q.set_custom_fps(conn_id, opt.custom_fps as u32));
            }
            // 「会话结束后锁定远程电脑」。语义与上游
            // `server::connection::update_options` 完全一致：
            // `NotSet` 同样被当作 `No`（上游 `q == BoolOption::Yes`），
            // 因为客户端每次下发 `OptionMessage` 都会带上这个字段
            // （`src/client.rs::update_options`），只读模式下主动填 No。
            if let Ok(q) = opt.lock_after_session_end.enum_value() {
                use base::message_proto::option_message::BoolOption;
                let want = q == BoolOption::Yes;
                if want != session.lock_after_session_end {
                    session.lock_after_session_end = want;
                    log::info!(
                        "[LOCK] conn #{} lock-after-session-end -> {}",
                        conn_id,
                        want
                    );
                }
            }
            if let Ok(q) = opt.image_quality.enum_value() {
                use base::message_proto::ImageQuality;
                // 控制端的画质选择**原样采纳**，与上游
                // `server::connection::update_options` 的语义一致：
                // 枚举为 `NotSet` 时才看自定义值，其余情况用枚举值。
                //
                // 这里原本有一层「VP9 就强制抬到 Best」的钳制，理由是
                // 「VP9 在高分辨率下码率不足会丢帧」。实际后果是画质选项
                // **完全失效**：默认链路就是 VP9，于是 Low / Balanced /
                // 自定义值全被改写成 Best，控制端怎么调都停在 7500kbps。
                //
                // 码率不足导致的丢帧本来就该由 QoS 的帧率自适应和
                // dropframe 水位处理，不该靠「永远最高画质」来兜。
                let v = if q == ImageQuality::NotSet {
                    opt.custom_image_quality
                } else {
                    q as i32
                };
                if v > 0 {
                    log::info!(
                        "[VIDEO] image quality from conn #{}: enum={:?} custom={} -> {}",
                        conn_id,
                        q,
                        opt.custom_image_quality,
                        v
                    );
                    crate::video::qos::with(|q| q.set_image_quality(conn_id, v));
                }
            }
            if let Some(sd) = opt.supported_decoding.clone().take() {
                // 运行中控制端更新了解码能力 / 编码偏好
                // （用户在客户端切换 VP8↔VP9 走的就是这条路径）。
                crate::video::set_peer_codec_ability(
                    sd.ability_vp8 > 0,
                    sd.ability_vp9 > 0,
                    sd.ability_h264 > 0,
                    sd.ability_h265 > 0,
                    codec_prefer_of(&sd),
                );
            }
        }
        // 显示器切换请求：控制端点了「切换显示器」或连上后指定了要看的屏。
        //
        // 切换是**按连接**的：订阅表与 QoS 都记录该连接看哪块屏，
        // 其他控制端继续看自己那块，互不影响。
        if misc.has_switch_display() {
            let sd = misc.switch_display();
            let idx = sd.display.max(0) as usize;
            log::info!("Switch display requested: #{}", idx);
            session.switch_display(idx);
        }
        // 分辨率变更请求（ChangeResolution 已废弃但仍需兼容；ChangeDisplayResolution 1.2.4+）
        if misc.has_change_resolution() {
            let r = misc.change_resolution();
            handle_change_resolution(None, r.width as usize, r.height as usize);
        }
        if misc.has_change_display_resolution() {
            let dr = misc.change_display_resolution();
            handle_change_resolution(
                Some(dr.display as usize),
                dr.resolution.width as usize,
                dr.resolution.height as usize,
            );
        }
    } else if msg.has_file_action() {
        file_transfer.handle_action(msg.file_action());
        // 响应由 tokio mpsc channel 经 select! 中 ft_rx.recv() 异步发送
    } else if msg.has_file_response() {
        use base::message_proto::file_response;
        let resp = msg.file_response();
        match resp.union {
            Some(file_response::Union::Block(ref block)) => {
                file_transfer.handle_data(block).await;
            }
            Some(file_response::Union::Done(ref d)) => {
                log::info!(
                    "[FT] File transfer done: id={} file_num={}",
                    d.id,
                    d.file_num
                );
                file_transfer.handle_done(d.id, d.file_num);
            }
            Some(file_response::Union::Error(ref e)) => {
                log::info!(
                    "[FT] File transfer error: id={} file_num={} error={}",
                    e.id,
                    e.file_num,
                    e.error
                );
                file_transfer.handle_error(e.id);
            }
            Some(file_response::Union::Digest(ref d)) => {
                log::info!("[FT] File digest: id={} file_num={}", d.id, d.file_num);
                // Digest handling for overwrite detection - not implemented yet
                file_transfer.handle_digest(d);
            }
            _ => {
                log::debug!("[FT] Received FileResponse (unhandled type)");
            }
        }
    } else {
        log::debug!("[INPUT] Unknown message type (no handler)");
    }

    Ok(())
}

/// 处理分辨率变更请求（来自控制端的 Misc::ChangeResolution / ChangeDisplayResolution）
///
/// 索引语义：控制端发来的 `display` 来自我们上报的 `PeerInfo.displays` /
/// `SwitchDisplay.display`，两者都取自 `scrap::Display::all()` 的顺序。
/// 所以这里必须用**同一个枚举源**（`scrap::Display::all()`），
/// 而不是顺序可能不同的 `display_service::try_get_displays()`，
/// 否则多显示器下会改错屏。
fn handle_change_resolution(display_idx: Option<usize>, width: usize, height: usize) {
    // 未指定时回退到 #0：已废弃的 `ChangeResolution` 不带显示器索引，只关心主屏。
    let wanted = display_idx.unwrap_or(0);
    let Ok(displays) = scrap::Display::all() else {
        log::warn!("Failed to enumerate displays for change_resolution");
        return;
    };
    let Some(display) = displays.get(wanted) else {
        log::warn!(
            "Display #{} not found ({} available), change resolution ignored",
            wanted,
            displays.len()
        );
        return;
    };
    let cur_w = display.width() as usize;
    let cur_h = display.height() as usize;
    // 请求的分辨率与当前一致 → 跳过（控制端可能在连接握手时发送了兼容字段）
    if width == cur_w && height == cur_h {
        log::debug!("Change resolution skipped: already {}x{}", width, height);
        return;
    }
    log::info!(
        "Change resolution requested: display=#{} '{}', {}x{} (current: {}x{})",
        wanted,
        display.name(),
        width,
        height,
        cur_w,
        cur_h,
    );
    let name = display.name();
    // 保存原始分辨率，以便控制端断开后恢复
    librustdesk::server::display_service::set_last_changed_resolution(
        &name,
        (cur_w as i32, cur_h as i32),
        (width as i32, height as i32),
    );
    // 分辨率切换放在独立线程，避免阻塞当前 Tokio 任务。
    //
    // **顺序很关键**：必须先改分辨率、**再**请求重建。反过来的话，
    // `request_refresh` 会在 33ms 内触发重建，而 `change_resolution` 要
    // 100-500ms —— 重建时拿到的还是旧分辨率，白闪一次，然后靠尺寸检测
    // 再重建第二次。
    std::thread::spawn(move || {
        match librustdesk::platform::change_resolution(&name, width, height) {
            Ok(()) => {
                log::info!("Resolution changed to {}x{} on '{}'", width, height, name);
                // 改完才重建，编码器一次到位
                crate::video::request_refresh();
            }
            Err(e) => log::error!(
                "Failed to change resolution '{}' to {}x{}: {:?}",
                name,
                width,
                height,
                e
            ),
        }
    });
}

/// 打印消息的所有 union 类型（用于调试客户端发送了什么）
fn log_msg_types(msg: &Message) {
    use base::message_proto::message::Union;
    if let Some(ref u) = msg.union {
        let name = match u {
            Union::LoginRequest(_) => "LoginRequest",
            Union::LoginResponse(_) => "LoginResponse",
            Union::Hash(_) => "Hash",
            Union::PeerInfo(_) => "PeerInfo",
            Union::MouseEvent(_) => "MouseEvent",
            Union::KeyEvent(_) => "KeyEvent",
            Union::Clipboard(_) => "Clipboard",
            Union::VideoFrame(_) => "VideoFrame",
            Union::Misc(_) => "Misc",
            Union::FileAction(_) => {
                // 展开 FileAction 子类型
                if let Some(fa) = msg.file_action().union.as_ref() {
                    let fa_name = match fa {
                        file_action::Union::ReadDir(_) => "FileAction::ReadDir",
                        file_action::Union::ReadEmptyDirs(_) => "FileAction::ReadEmptyDirs",
                        file_action::Union::Send(_) => "FileAction::Send",
                        file_action::Union::Receive(_) => "FileAction::Receive",
                        file_action::Union::Cancel(_) => "FileAction::Cancel",
                        file_action::Union::RemoveDir(_) => "FileAction::RemoveDir",
                        file_action::Union::RemoveFile(_) => "FileAction::RemoveFile",
                        file_action::Union::Create(_) => "FileAction::Create",
                        file_action::Union::Rename(_) => "FileAction::Rename",
                        file_action::Union::AllFiles(_) => "FileAction::AllFiles",
                        file_action::Union::SendConfirm(_) => "FileAction::SendConfirm",
                        _ => "FileAction::Other",
                    };
                    return log::trace!("[MSG] received: {}", fa_name);
                }
                "FileAction"
            }
            Union::FileResponse(_) => "FileResponse",
            Union::Cliprdr(_) => "Cliprdr",
            Union::MultiClipboards(_) => "MultiClipboards",
            Union::PublicKey(_) => "PublicKey",
            Union::SignedId(_) => "SignedId",
            Union::TestDelay(_) => "TestDelay",
            Union::MessageBox(_) => "MessageBox",
            Union::Auth2fa(_) => "Auth2fa",
            Union::CursorData(_) => "CursorData",
            Union::CursorPosition(_) => "CursorPosition",
            Union::CursorId(_) => "CursorId",
            Union::VoiceCallRequest(_) => "VoiceCallRequest",
            Union::VoiceCallResponse(_) => "VoiceCallResponse",
            Union::SwitchSidesResponse(_) => "SwitchSidesResponse",
            Union::PointerDeviceEvent(_) => "PointerDeviceEvent",
            Union::ScreenshotRequest(_) => "ScreenshotRequest",
            Union::ScreenshotResponse(_) => "ScreenshotResponse",
            Union::TerminalAction(_) => "TerminalAction",
            Union::TerminalResponse(_) => "TerminalResponse",
            _ => "Unknown/Other",
        };
        log::trace!("[MSG] received: {}", name);
    }
}

#[cfg(test)]
mod tests {
    use super::{password_matches, should_lock_after_session_end};

    /// 构造一个只填了密码的配置（其余字段与本测试无关）
    fn config_with_password(pwd: &str) -> crate::app::config::SosConfig {
        crate::app::config::SosConfig {
            id: "123456789".to_string(),
            rendezvous_server: String::new(),
            relay_server: String::new(),
            password: pwd.to_string(),
            server_pub_key: String::new(),
        }
    }

    /// 按协议算出客户端该发的 h2：sha256(sha256(pwd || salt) || challenge)
    fn h2(pwd: &str, salt: &str, challenge: &str) -> Vec<u8> {
        use hbb_common::sha2::{Digest, Sha256};
        let mut h1 = Sha256::new();
        h1.update(pwd.as_bytes());
        h1.update(salt.as_bytes());
        let h1 = h1.finalize();
        let mut h2 = Sha256::new();
        h2.update(h1);
        h2.update(challenge.as_bytes());
        h2.finalize().to_vec()
    }

    /// 授权闸门：只有算出正确 h2 才放行
    #[test]
    fn password_matches_only_the_right_hash() {
        let cfg = config_with_password("s3cret");
        let salt = "saltsaltsalt";
        let challenge = "abc123";
        assert!(password_matches(
            &cfg,
            salt,
            challenge,
            &h2("s3cret", salt, challenge)
        ));
        // 大小写/长度只差一点都必须拒绝
        assert!(!password_matches(
            &cfg,
            salt,
            challenge,
            &h2("S3cret", salt, challenge)
        ));
        assert!(!password_matches(&cfg, salt, challenge, &[]));
        // challenge 不同 ⇒ 该 h2 失效（防重放）
        assert!(!password_matches(
            &cfg,
            salt,
            challenge,
            &h2("s3cret", salt, "other1")
        ));
    }

    /// 本机没配置任何密码时必须拒绝，绝不能把「空密码」当成放行条件
    #[test]
    fn no_configured_password_never_authorizes() {
        let cfg = config_with_password("");
        assert!(!password_matches(&cfg, "salt", "chal", &[]));
        assert!(!password_matches(&cfg, "salt", "chal", &h2("", "salt", "chal")));
    }

    /// 控制端勾选了选项 + 正常断开（它自己关掉会话）→ 锁屏
    #[test]
    fn locks_on_normal_peer_close() {
        assert!(should_lock_after_session_end(true, true));
    }

    /// 没勾选就绝不锁屏，无论怎么断开
    #[test]
    fn never_locks_without_the_option() {
        assert!(!should_lock_after_session_end(false, true));
        assert!(!should_lock_after_session_end(false, false));
    }

    /// 勾选了但**异常断开**（发送/接收错误、协议错误）→ 不锁屏。
    ///
    /// 这是刻意的取舍：网络抖动把链路打断时，用户并没有「结束会话」的意图，
    /// 直接锁掉桌面会让人莫名其妙。上游对 `on_close(err, false)` 的几处
    /// 调用也是同一个取舍。
    #[test]
    fn does_not_lock_on_abnormal_close() {
        assert!(!should_lock_after_session_end(true, false));
    }
}
