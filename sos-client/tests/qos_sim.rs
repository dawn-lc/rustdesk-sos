//! 闭环网络仿真：把 video::qos 与 video::link 放进一条会拥塞的链路里跑。
//!
//! 结构照上游 src/server/video_qos/tests/sim.rs（那边 911 行）：视频帧与
//! TestDelay 探针共享一条 FIFO，探针测到的就是排在它前面的字节造成的排队；
//! 控制器每秒被喂一次样本，而它的决策又决定下一秒的帧大小 —— 闭环。
//!
//! 与上游那套的差别，都是 SOS 的结构决定的：
//!
//! - 上游 QoS 是全局标量，SOS 按显示器聚合，所以这里自建一个 SosQos 实例；
//! - rtt/queue 的派生直接用生产实现 net::rtt::RttFilter（queue = raw - 窗口最小值），
//!   不在这里复刻公式 —— 复刻会让仿真和实现悄悄分叉，这正是 AGENTS.md 明令禁止的；
//! - 链路档位用生产全局 with_link_tier / note_link_sample / link_budget。
//!   它不是线程安全的仿真输入，所以本文件的测试用一把静态锁串行执行，且每轮开始复位。
//!
//! 这个仿真要回答的问题只有一个：自定义画质下 QoS 到底动不动。
//! 生产语义是「自定义 = 用户自己权衡，QoS 完全退出」（码率与帧率都不动），
//! 代价是拥塞时只能靠发送队列丢帧 —— 下面几个 #[test] 分别锁住这两面。
//!
//! 它仍然是模型，不是网络：不还原 transport 的拥塞控制，也不还原真实编码器。

#![cfg(all(windows, sos_vpx))]

use rustdesk_sos::net::rtt::RttFilter;
use rustdesk_sos::video as sos_video;
use rustdesk_sos::video::qos::SosQos;
use rustdesk_sos::video::vpx::SosVpxConfig;

use std::collections::VecDeque;
use std::sync::Mutex;

/// 同一时刻只让一个仿真驱动全局链路档位（LINK_TIER）。
static SIM_GATE: Mutex<()> = Mutex::new(());

/// 仿真步长。上游也用 10ms：比控制器（1Hz）细 100 倍，排队延迟才不会粗到看不见。
const TICK_MS: u32 = 10;

/// 采集分辨率（现场常见档，default_bitrate_kbps 由此算出 1736kbps 基数）。
const W: usize = 1600;
const H: usize = 900;

/// 显示器名。只需 QoS 内部自洽，用固定串以免与设备枚举耦合。
const DISPLAY: &str = "SIM-DISPLAY1";

/// 已在发送队列里的视频帧上限。
///
/// 生产是 net::connection 的 MAX_VIDEO_BACKLOG = 6（30fps 下约 200ms）。
/// 超限的帧在入队处被丢弃 —— 所以「不自适应」并不等于「延迟无限增长」，
/// 而是变成可见的丢帧。仿真必须把这个机制建进去，否则会得出错误的结论。
const MAX_IN_FLIGHT_FRAMES: u32 = 6;

/// 确定性随机源（xorshift64*），避免为一个测试引入 rand 依赖。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// [0, 1)
    fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.uniform()
    }
}

/// 下行链路：容量阶梯 + 基础 RTT + 每次探测的抖动。
#[derive(Clone)]
struct Link {
    /// (from_ms, kbps)，按时间升序；取最后一条已生效的。
    steps: Vec<(u32, f64)>,
    base_rtt_ms: f64,
    jitter_ms: f64,
}

impl Link {
    fn capacity_kbps(&self, now: u32) -> f64 {
        self.steps
            .iter()
            .rev()
            .find(|(from, _)| *from <= now)
            .map(|(_, kbps)| *kbps)
            .unwrap_or(self.steps[0].1)
    }
}

/// 内容类型，决定每秒有多少帧真的在变。
#[derive(Clone, Copy, PartialEq)]
enum Content {
    /// 每帧都变（播视频 / 拖窗口）。
    Video,
}

struct Scenario {
    image_quality: i32,
    custom_fps: u32,
    content: Content,
    link: Link,
    seconds: u32,
    seed: u64,
}

/// 一次仿真的观测量。
struct Report {
    /// 每秒采样一次：控制器给这块屏的目标帧率
    fps: Vec<u32>,
    /// 每秒采样一次：下发给编码器的目标码率
    kbps: Vec<u32>,
    /// 每秒采样一次：画质比
    ratio: Vec<f32>,
    /// 每秒采样一次：共享路径上的排队延迟
    queue_ms: Vec<u32>,
    /// 整个运行期间该屏是否一直被判为自定义画质
    always_custom: bool,
    dropped: u64,
}

impl Report {
    fn min_fps(&self) -> u32 {
        self.fps.iter().copied().min().unwrap_or(0)
    }

    /// 第 from_s 秒之后的最小帧率（用来看恢复）。
    fn min_fps_after(&self, from_s: usize) -> u32 {
        self.fps.iter().skip(from_s).copied().min().unwrap_or(0)
    }

    fn max_kbps_after(&self, from_s: usize) -> u32 {
        self.kbps.iter().skip(from_s).copied().max().unwrap_or(0)
    }

    fn min_kbps_after(&self, from_s: usize) -> u32 {
        self.kbps
            .iter()
            .skip(from_s)
            .copied()
            .min()
            .unwrap_or(u32::MAX)
    }

    fn max_queue_ms_after(&self, from_s: usize) -> u32 {
        self.queue_ms.iter().skip(from_s).copied().max().unwrap_or(0)
    }
}

/// 共享 FIFO 里的一个包（视频帧或探针）。
struct Packet {
    /// 剩余待发字节（bits）
    bits: f64,
    is_probe: bool,
    /// 入队时刻：探针测的是「排在它前面的字节」造成的排队，必须从入队算起。
    enq_ms: u32,
}

fn run(sc: &Scenario) -> Report {
    // 全局链路档位是共享状态：串行执行并在每轮开始时复位，否则上一个场景的
    // generous 会漏给下一个场景（生产里则在第一个控制端接入时 reset）。
    let _gate = SIM_GATE.lock().unwrap_or_else(|e| e.into_inner());
    sos_video::with_link_tier(|tier| tier.reset());

    let mut qos = SosQos::new();
    qos.on_connection_open(1, DISPLAY);
    qos.set_custom_fps(1, sc.custom_fps);
    qos.set_image_quality(1, sc.image_quality);

    let mut rng = Rng::new(sc.seed);
    let mut rtt_filter = RttFilter::default();
    let ticks = sc.seconds * 1000 / TICK_MS;

    let mut queue: VecDeque<Packet> = VecDeque::new();
    let mut queued_bits = 0.0_f64;
    let mut frames_in_flight = 0_u32;

    let mut encode_phase = 0.0_f64;
    let mut probe_outstanding = false;
    let mut reply_at: Option<(u32, u32)> = None; // (到达时刻, 原始往返 ms)

    let mut report = Report {
        fps: Vec::new(),
        kbps: Vec::new(),
        ratio: Vec::new(),
        queue_ms: Vec::new(),
        always_custom: true,
        dropped: 0,
    };

    for tick in 0..ticks {
        let now = tick * TICK_MS;
        let capacity_kbps = sc.link.capacity_kbps(now);

        // ── 1. 采集循环的决策（这里每 tick 读一次，等价于每秒重算） ──
        let fps = qos.fps(DISPLAY);
        let ratio = qos.ratio(DISPLAY);
        let custom = qos.is_custom(DISPLAY);
        if !custom {
            report.always_custom = false;
        }
        // 与 capture_loop 完全一致：自定义画质走 exact 路径，否则走链路预算折算。
        let target_kbps = if custom {
            SosVpxConfig::bitrate_kbps_exact(W, H, ratio)
        } else {
            SosVpxConfig::bitrate_kbps_for_link(W, H, ratio, fps, sos_video::link_budget())
        };
        qos.store_bitrate(DISPLAY, target_kbps);

        // ── 2. 编码器产帧 ──
        // CBR：每秒比特由码率决定，与帧率无关 —— 帧少则每帧更大。
        // 这正是「帧率降而码率不降 ⇒ 每帧变大 ⇒ 排队更久」那条实测的来源。
        let produce_rate = match sc.content {
            Content::Video => fps as f64,
        };
        let per_frame_bits = if produce_rate > 0.0 {
            target_kbps as f64 * 1000.0 / produce_rate
        } else {
            0.0
        };
        encode_phase += produce_rate * TICK_MS as f64 / 1000.0;
        while encode_phase >= 1.0 {
            encode_phase -= 1.0;
            if frames_in_flight >= MAX_IN_FLIGHT_FRAMES {
                // 生产在入队处丢弃超限的帧（MAX_VIDEO_BACKLOG）：
                // 不自适应 ⇒ 延迟有界，代价变成可见的丢帧。
                report.dropped += 1;
                continue;
            }
            let bits = per_frame_bits * rng.range(0.65, 1.35);
            queued_bits += bits;
            frames_in_flight += 1;
            queue.push_back(Packet {
                bits,
                is_probe: false,
                enq_ms: now,
            });
        }

        // ── 3. 共享路径排空（探针与帧同一条 FIFO） ──
        let mut budget = capacity_kbps * TICK_MS as f64; // 一个 tick 能发多少 bit
        while budget > 0.0 {
            let Some(head) = queue.front_mut() else { break };
            if head.is_probe {
                // 探针极小，到达队首才真正发出。它测到的往返 = **在队里等的时间**
                // + 路径 RTT + 抖动 —— 前半段就是「排在它前面的帧」造成的排队，
                // 少了它这个模型就永远看不到拥塞（第一版就是这么错的）。
                let enq = head.enq_ms;
                let path_ms = sc.link.base_rtt_ms + rng.range(0.0, sc.link.jitter_ms);
                let raw = path_ms + (now - enq) as f64;
                reply_at = Some((now + path_ms as u32, raw as u32));
                queue.pop_front();
                continue;
            }
            let take = budget.min(head.bits);
            head.bits -= take;
            queued_bits -= take;
            budget -= take;
            if head.bits <= 1e-9 {
                queue.pop_front();
                frames_in_flight -= 1;
            }
        }

        // ── 4. 探针回显 → 生产同一条派生路径 ──
        if let Some((arrive_ms, raw)) = reply_at {
            if now >= arrive_ms {
                reply_at = None;
                probe_outstanding = false;
                if let Some((floor, queue_ms)) = rtt_filter.push(raw) {
                    qos.on_test_delay(1, floor, queue_ms);
                    // 与 Session::on_probe_echo 一致：一次探测只喂一个样本
                    sos_video::note_link_sample(floor, queue_ms);
                }
            }
        }

        // ── 5. 每秒一次的探针发送（上一只未回来时才发新的） ──
        if now % 1000 == 0 && !probe_outstanding && reply_at.is_none() {
            probe_outstanding = true;
            queue.push_back(Packet {
                bits: 0.0,
                is_probe: true,
                enq_ms: now,
            });
        }

        // ── 6. 每秒采样，供断言使用 ──
        if now % 1000 == 0 {
            report.fps.push(qos.fps(DISPLAY));
            report.kbps.push(target_kbps);
            report.ratio.push(qos.ratio(DISPLAY));
            report.queue_ms.push((queued_bits / capacity_kbps.max(1.0)) as u32);
        }
    }

    report
}

/// 链路：容量在 high 与 low 之间每 toggle_ms 抖一次；到 stable_from_ms 之后固定 high。
///
/// 这正是家里 Wi-Fi / 中继的真实形态：**排队是相对的**——瓶颈期间延迟抬起来，
/// 瓶颈一过就落回去。控制器的 min-filter 只在「相对最近的底噪」上判排队
/// （见 net::rtt），所以只有这种「抬起来又落回去」才会被它看见。
fn wobbling_link(toggle_ms: u32, low_kbps: f64, stable_from_ms: Option<u32>) -> Link {
    const HIGH_KBPS: f64 = 40_000.0;
    let mut steps = Vec::new();
    let mut t = 0_u32;
    let mut low = false;
    loop {
        if let Some(stable) = stable_from_ms {
            if t >= stable {
                steps.push((stable, HIGH_KBPS));
                break;
            }
        }
        steps.push((t, if low { low_kbps } else { HIGH_KBPS }));
        low = !low;
        t += toggle_ms;
    }
    Link {
        steps,
        base_rtt_ms: 20.0,
        jitter_ms: 6.0,
    }
}

/// 链路：40Mbps 稳定 → 第 collapse_ms 起塌到 bad_kbps →（可选）restore_ms 恢复。
fn collapsing_link(collapse_ms: u32, bad_kbps: f64, restore_ms: Option<u32>) -> Link {
    let mut steps = vec![(0, 40_000.0), (collapse_ms, bad_kbps)];
    if let Some(restore) = restore_ms {
        steps.push((restore, 40_000.0));
    }
    Link {
        steps,
        base_rtt_ms: 20.0,
        jitter_ms: 6.0,
    }
}

/// 自定义画质 = 用户自己权衡，QoS 完全不动。
///
/// 链路在 8 秒后从 40Mbps 塌到 700kbps，而用户要的 2.0x（约 3.5Mbps）远超容量。
/// 此时控制器必须一步都不退：帧率恒为用户给的 30、码率恒定、画质比恒定。
/// 代价由发送队列丢帧承担 —— 这就是「不插手」的真实含义。
#[test]
fn custom_quality_does_not_react_to_congestion() {
    let sc = Scenario {
        // 2.0x 自定义画质：协议编码 (100 << 8)，解出 100 * 2 / 100 = 2.0
        image_quality: 100 << 8,
        custom_fps: 30,
        content: Content::Video,
        link: collapsing_link(8_000, 700.0, None),
        seconds: 22,
        seed: 7,
    };
    let r = run(&sc);

    assert!(r.always_custom, "2.0x 应全程被判为自定义画质");
    assert_eq!(r.min_fps(), 30, "自定义画质下拥塞也不得降帧（帧率交给用户）");

    // 码率与画质比在拥塞前后完全一致（跳过第 0/1 秒的冷启动采样）
    let kbps_min = r.min_kbps_after(2);
    let kbps_max = r.max_kbps_after(2);
    assert_eq!(
        kbps_min, kbps_max,
        "自定义画质码率必须恒定：2..22s 内出现 {kbps_min}..{kbps_max}"
    );
    // 不变量：QoS 永远不许把自定义码率抬到用户设定之上（增益 / 帧率缩放都不行）。
    let user_kbps = (SosVpxConfig::default_bitrate_kbps(W, H) as f64 * 2.0) as u32;
    assert!(
        kbps_max <= user_kbps,
        "自定义码率不得超过用户设定 {user_kbps}kbps，实测最高 {kbps_max}"
    );
    let ratio_min = r.ratio.iter().skip(2).copied().fold(f32::MAX, f32::min);
    let ratio_max = r.ratio.iter().skip(2).copied().fold(0.0_f32, f32::max);
    assert!(
        (ratio_max - ratio_min).abs() < 1e-6,
        "自定义画质比必须恒定：{ratio_min}..{ratio_max}"
    );

    // 代价是真实的：拥塞后排队顶到背压上限并开始丢帧。
    assert!(
        r.dropped > 0,
        "不自适应时拥塞只能靠丢帧承担，实测 dropped={}",
        r.dropped
    );
    assert!(
        r.max_queue_ms_after(12) <= 2_000,
        "丢帧机制必须让排队有界，实测 {}ms",
        r.max_queue_ms_after(12)
    );
}

/// 链路**持续**饱和时，不压画质比也不降帧——靠发送队列丢帧把延迟钉住。
///
/// 这是 SOS 有意选择的语义（见 README 的「自定义画质 = 用户自己权衡」与
/// 「拥塞时不压画质比」）：CBR 编码器少发帧不会少发字节，降帧救不了带宽，
/// 而压画质比会让用户看到的画质低于自己选的档位。所以稳态是「丢帧 + 有界延迟」。
#[test]
fn saturated_link_drops_frames_instead_of_raising_latency() {
    let sc = Scenario {
        image_quality: 3, // Balanced
        custom_fps: 30,
        content: Content::Video,
        link: collapsing_link(8_000, 700.0, None),
        seconds: 22,
        seed: 7,
    };
    let r = run(&sc);

    // 控制器会收敛在「低于目标、但不掉到下限」的帧率上：帧率降一点、每帧变小、
    // 排队就退回去；再往下降没有意义（CBR 少发帧并不会少发字节）。
    assert!(
        r.min_fps() < 30,
        "持续饱和必须触发降帧，实测最低 {}fps",
        r.min_fps()
    );
    let steady = r.min_fps_after(14);
    assert!(
        (rustdesk_sos::video::qos::MIN_FPS..30).contains(&steady),
        "稳态应落在 [MIN_FPS, 目标) 之间，实测 {steady}fps"
    );
    assert!(
        r.dropped > 0,
        "撑不住的码率只能靠丢帧承担，实测 dropped={}",
        r.dropped
    );
    assert!(
        r.max_queue_ms_after(12) <= 2_000,
        "丢帧机制必须让排队有界，实测 {}ms",
        r.max_queue_ms_after(12)
    );
}

/// 排队**相对底噪**抬起来时（Wi-Fi 抖动 / 中继拥塞）必须降帧，恢复后爬回目标。
///
/// 这才是 QoS 帧率自适应真正该起作用的情形：链路不是一直慢，而是反复卡一下。
#[test]
fn queue_spikes_reduce_fps_and_it_recovers() {
    let sc = Scenario {
        image_quality: 3,
        custom_fps: 30,
        content: Content::Video,
        // 每 2 秒在 40Mbps 与 700kbps 之间抖一次，18 秒后稳定为 40Mbps
        link: wobbling_link(2_000, 700.0, Some(18_000)),
        seconds: 26,
        seed: 11,
    };
    let r = run(&sc);

    assert!(
        r.min_fps() < 30,
        "排队尖峰必须触发降帧，实测最低 {}fps",
        r.min_fps()
    );
    // 尖峰期间必须真的把目标码率一起降下来（否则每帧更大、排队更久）
    let kbps_min = r.min_kbps_after(2);
    let kbps_max = r.max_kbps_after(2);
    assert!(
        kbps_min < kbps_max,
        "降帧必须带动码率下降：{kbps_min} vs {kbps_max}"
    );
    assert_eq!(
        r.min_fps_after(23),
        30,
        "链路稳定后 5 秒内应爬回目标帧率，实测 {}fps",
        r.min_fps_after(23)
    );
}
