# RustDesk SOS 精简受控端 — 开发指引

本文件只覆盖 `sos-client/`。仓库根的 `AGENTS.md` 属于上游 RustDesk，其全部规则
（Rust 风格、日志、Tokio、最小改动、回归面检查、本地化等）在这里同样适用；
本文件补充 SOS 特有的事实与约束。

> 面向人的构建/运行说明在 [`README.md`](README.md)，本文不重复，只写**改代码前
> 必须知道的事**。

---

## 文档索引

| 文档 | 内容 |
| --- | --- |
| 本文件 | 改代码前必须知道的**规则**：硬约束、目录/模块地图、构建发布、上游依赖、架构要点、日志字段、测试与提交规范 |
| [`docs/PITFALLS.md`](docs/PITFALLS.md) | 实机踩出来的**实测结论**：采集 / 硬件编码 / 码率控制 / 码率帧率耦合 / 链路档位 / 关键帧 / 发送队列 / 锁屏 UAC / 输入 / 连接确认弹窗 / CPU / 环境事实。**动手前先读对应主题** |
| [`docs/KNOWN-LIMITS.md`](docs/KNOWN-LIMITS.md) | 刻意留下的行为与系统级限制，**不要当 bug 修** |
| [`README.md`](README.md) | 面向人的构建 / 运行 / 部署说明 |

> 这份文件会被自动加载进上下文，**有字节预算**（超了会被静默截断）。新增内容前先想
> 清楚它是否属于「规则」；属于「为什么这么写」「踩过什么坑」的，放 `docs/PITFALLS.md`。



## 1. 硬约束

| 约束 | 说明 |
| --- | --- |
| **默认不改上游** | 新改动一律落在 `sos-client/`。需要上游行为改变时优先在 SOS 侧包一层或绕开（`capture::dxgi`/`video::vpx`/`app::cpu` 都是这么来的）。 |
| **已存在的上游补丁，只减不增** | 现存 4 处，见下表。不要新增第 5 处；能让某处退场时优先退场。 |
| **不钉第三方工具版本** | UPX 等外部工具禁止在源码里写死版本号或 sha256（见 §4）。 |
| **生产代码禁 `unwrap()`/`expect()`** | 与根 `AGENTS.md` 一致；锁定中毒等例外同样适用。 |
| **不跑 codegen** | 不要运行 `flutter_rust_bridge_codegen`；SOS 也不用 protobuf codegen（协议类型全部来自 `base` crate）。 |

### 现存的上游补丁（提交前用 `git diff --name-status origin/master...HEAD -- . ':!sos-client'` 核对，应只有这几项）

| 文件 | 改动 | 为什么还在 |
| --- | --- | --- |
| `.gitignore` | 加 `/sos-client/target` | 独立项目有自己的 target |
| `Cargo.toml` / `Cargo.lock` | `exclude` 加 `sos-client`；`tray-icon` 补 `rev` | 可解析性必需（见 §3） |
| `src/lib.rs`、`src/server.rs` | 把 `server` / `clipboard` / `clipboard_service` / `service` 改成 `pub mod` | `sos-client` 直接调 `librustdesk::server::clipboard_service`、`librustdesk::clipboard`、`librustdesk::service` |
| `src/lib.rs` 的 `kcp_stream` | 由 `pub mod kcp_stream;` 公开 | UDP 打洞要调 `librustdesk::kcp_stream::KcpStream::accept`；上游没有别的公开入口（`common::punch_udp` 本来就是 pub）。退场条件：上游把 KCP 收进 `hbb_common` 或提供公开封装 |

已退场的补丁（留一行记录，防止有人照着旧资料又加回来）：

| 文件 | 曾经的改动 | 退场原因 |
| --- | --- | --- |
| `src/server/video_service.rs` | `CUSTOM_CAPTURER_FACTORY` 静态注入点 + 3 个 `pub fn`（37 行纯新增） | SOS 全面改用 `video` 自建链路后已无调用方，2026-11-18 整文件还原到上游版本。**不要重新引入**：要影响采集器请在 `capture` 侧自己建，不要再往 `libs/scrap` / `video_service` 里塞钩子。 |

---

## 2. 目录结构

```
sos-client/
├── Cargo.toml            独立项目（被根 workspace exclude），自带 Cargo.lock
├── Cargo.lock
├── build.rs              winres 图标/版本 · cc 编译 windows.cc · bindgen 生成 libvpx 绑定
│                        （只有绑定生成成功才置 `cfg(sos_vpx)`；缺它时 `lib.rs` 的 `compile_error!` 报原因）
├── .cargo/config.toml    `cargo xtask` 别名 + 静态 CRT
├── .gitignore            dist/ · xtask/target/ · tools/
├── README.md             面向人的构建/运行说明
├── AGENTS.md             本文件（自动加载的开发指引，有字节预算）
├── docs/
│   ├── PITFALLS.md       实测结论（从 AGENTS §7 拆出）
│   └── KNOWN-LIMITS.md   已知限制（从 AGENTS §11 拆出）
├── res/sos.manifest      应用清单：只声明 Common-Controls 6（打开视觉样式）
├── platform/
│   └── windows.cc        SetWinEventHook 桌面切换等待、run-as-system、切换桌面 FFI
├── src/
│   ├── lib.rs            库根：`pub mod` 七个域；缺 libvpx 时这里的 `compile_error!` 说明原因
│   ├── main.rs           二进制入口（14 行）：GUI 子系统属性 + `#[tokio::main]` → `app::run()`
│   ├── app.rs            进程生命周期域根（域根与目录同名同级，不用 `xxx/mod.rs`）
│   ├── app/
│   │   ├── run.rs        原 `main.rs` 主体：CLI、模式分发、主进程装配（控制台 / 看门狗 / 安全桌面监听已拆出）
│   │   ├── console.rs    调试控制台挂接：AttachConsole + CreateFile("CONOUT$") + SetStdHandle，`setup(debug, service)`
│   │   ├── bootstrap.rs  日志初始化（主进程 / SYSTEM 各一套文件）、单实例互斥体
│   │   ├── config.rs     注册表配置 + CLI 合并
│   │   ├── constants.rs  全局常量、`Global\` 事件名、`VERSION`
│   │   ├── system.rs     SYSTEM 子进程启动与服务循环
│   │   ├── tray.rs       托盘图标与菜单
│   │   ├── cpu.rs        `GetSystemTimes` CPU 采样
│   │   ├── watchdog.rs   SYSTEM 子进程存活看门狗：WaitForSingleObject 事件驱动 + 启动期失败指数退避（`SUBPROC_*` 常量在此）
│   │   └── secure_watch.rs 安全桌面（锁屏 / UAC）监听线程，`UAC_FALLBACK_POLL_INTERVAL` / `SECURE_READY_WAIT_MS` / `SECURE_POLL_INTERVAL_MS` 随迁至此
│   ├── capture.rs        采集域根 + SYSTEM 子进程采集线程（安全桌面经 GDI 抓屏写 SHMEM）
│   ├── capture/
│   │   ├── dxgi.rs       自建 DXGI Desktop Duplication 采集器
│   │   ├── gdi.rs        自建 GDI 采集器（自检 / 逐档降级 / 变化检测 / 状态图）
│   │   ├── display.rs    显示器枚举与按名查尺寸
│   │   └── uac.rs        安全桌面判定、会话连接状态
│   ├── video.rs          视频域根：`Sender` / `VideoSender` 类型别名、全部调优常量、`start` / `ensure_pipeline`、
│   │                     `capture_loop`（732 行，**故意不拆**）、`create_capturer` / `classify_frame_error` /
│   │                     `demote_capturer`、`ActiveEncoder`、`CaptureState`、`mod tests`；
│   │                     另有 `pub use ability::*; displays::*; hub::*; ladder::*; link::*;`（刻意的再导出，见下）
│   ├── video/
│   │   ├── link.rs       链路档位与码率预算（`LINK_TIER` 迟滞状态机、`note_link_sample`、`link_budget`、
│   │   │                 `dropframe_thresh_for_link`）
│   │   ├── hub.rs        订阅表、广播与每显示器控制标志（`Subscriber` / `SUBSCRIBERS`、`subscribe` / `unsubscribe`、
│   │   │                 `send_video` / `broadcast_video`、`REFRESH_MASK` / `KEYFRAME_MASK`、`request_refresh` / `request_keyframe`）
│   │   ├── ladder.rs     编码成本自适应与采集档位降级（`MIN_TIER`、`ENC_LEVEL` / `ENC_BASELINE_MS` / `VP8_REJECTED`、
│   │   │                 `capture_spf`、`step_down_encoder`、`revert_vp8_if_useless`、`is_no_display_error`）
│   │   ├── ability.rs    控制端编码能力与当前编码格式（`PEER_ABILITY`、`set_hw_unavailable` / `hw_available`、
│   │   │                 `set_peer_codec_ability`、`current_codec` / `describe` / `codec_name`）
│   │   ├── displays.rs   显示器枚举、命名与切换消息（`display_info` / `list_displays` / `set_display_name` /
│   │   │                 `cached_display_name` / `make_switch_display_msg` / `is_remote_session`）
│   │   ├── vpx.rs        自建 libvpx 编码器 + 码率/链路档位/降级策略（纯函数集中在这里）
│   │   ├── hwenc.rs      硬件 H.264/H.265 编码器（`hwcodec` 后端，吃 CPU 的 NV12）
│   │   └── qos.rs        RTT / 帧率 / 码率 / 画质比，按显示器聚合
│   ├── net.rs            网络域根
│   ├── net/
│   │   ├── connection.rs 会话主循环、能力协商、消息分派、`Conn` 拆半区
│   │   ├── rendezvous.rs 信令服务器交互（注册、PunchHole、RequestRelay）
│   │   ├── relay.rs      中继地址解析、多候选挑选；另有两个纯函数 `classify_nat` / `must_relay`
│   │   ├── rtt.rs        RTT 采样 min-filter：`RttFilter` + `RTT_WINDOW` + `MAX_TRUSTED_RTT_MS`
│   │   └── file_transfer.rs 文件传输作业管理
│   ├── ipc.rs            IPC 域根
│   ├── ipc/
│   │   ├── pipe.rs       命名管道（主进程 ↔ SYSTEM 子进程）
│   │   └── shmem.rs      共享内存帧通道
│   ├── input.rs          远程鼠标/键盘注入（单文件，无子模块）
│   ├── ui.rs             本机用户界面域根
│   ├── ui/
│   │   ├── consent.rs    连接确认弹窗
│   │   └── device_id.rs  设备编号窗（注册后置顶居中显示编号，控制请求到达即关）
│   └── bindings/
│       └── vpx_ffi.h     bindgen 的输入，只声明用到的 libvpx 符号
├── tests/                38 个集成测试二进制；support/ 是共用内容生成器（非测试目标）
├── vcpkg/ffmpeg/         FFmpeg overlay port（官方 7.1.2 + 上游两个 AMF 补丁）
├── vendor/hwcodec/       hwcodec 本地副本（只改「调优项失败」不再致命）
├── xtask/                发布构建工具，自带 `[workspace]`，零依赖
├── tools/                【不入库】UPX 缓存 + 一次性现场诊断脚本
├── dist/                 【不入库】交付物（UPX 压缩后）
└── target/               【不入库】cargo 原生产物
```

### `src/` 为什么现在分层

旧结构是「全部平铺 + `sos_` 前缀」，唯一理由只是**测试要复用源文件**：没有库目标时
`tests/*.rs` 只能用 `#[path]` 把源文件编进自己的二进制。

**现在有了 `src/lib.rs`，这个理由消失了**：测试改成 `use rustdesk_sos::video::vpx as
sos_vpx;`，直接链接库，于是按域分层：

| 变化 | 为什么 |
| --- | --- |
| 每个域一个 `xxx.rs` 域根 + 同名 `xxx/` 目录 | 模块路径本身就是分组（`video::vpx`），不再靠 `main.rs` 顶部的注释维护地图 |
| 测试走库（`use rustdesk_sos::…`） | 源文件只编一遍；测试拿不到 `pub(crate)`，等于顺手锁死「测试只依赖公开面」 |
| 模块可见性统一 `pub` | 集成测试在 crate 外，`pub(crate)` 对它不可见 |

两条约束是这次分层的直接后果，改目录前先看：

- **域根与目录必须同名同级**（`app.rs` + `app/`），不要改回 `app/mod.rs`。
- **域根本身也放实现，但只放「循环与状态」**：`capture.rs` 是 SYSTEM 采集线程；
  `video.rs` 现在只留 `capture_loop`（732 行，**故意不拆**）、`ActiveEncoder` /
  `CaptureState`、`create_capturer` / `classify_frame_error` / `demote_capturer`、
  两个通道类型别名与全部调优常量 —— 策略已经搬进 `video/` 里新增的 5 个子模块；
  `input.rs` 是全部输入逻辑。只有 `net.rs` / `ipc.rs` / `ui.rs` / `app.rs` 基本是
  `pub mod` 清单。不要把域根掏空成纯清单再给实现单开一个 `mod.rs`。
- **`video.rs` 顶部那组 `pub use` 是刻意的调用点契约**：
  `pub use ability::*;` / `pub use displays::*;` / `pub use hub::*;` /
  `pub use ladder::*;` / `pub use link::*;` —— 让 `crate::video::xxx` 与本 crate 外
  测试里的 `rustdesk_sos::video::xxx` 全部不用改。**不要删这组再导出**，也不必把
  调用点改成 `video::link::xxx`：拆文件的目的是让模块边界清晰，不是重新命名 API。

### 模块地图（与 `src/lib.rs` 顶部一致）

模块名就是路径：原来的 `sos_vpx` 现在是 `video::vpx`，`sos_pipe` 是 `ipc::pipe`。

| 分组 | 模块 | 位置 | 职责 |
| --- | --- | --- | --- |
| 启动/进程 | `app` | `src/app.rs` | 域根 + `pub use run::run`；入口 `app::run()`（`mod run` 私有） |
| | `app::run` | `src/app/run.rs` | 原 `main.rs` 主体：CLI 解析、模式分发、主进程装配（控制台 / 看门狗 / 安全桌面监听已拆出） |
| | `app::console` | `src/app/console.rs` | 调试控制台挂接：`AttachConsole` + `CreateFile("CONOUT$")` + `SetStdHandle`，签名 `setup(debug, service)` |
| | `app::watchdog` | `src/app/watchdog.rs` | SYSTEM 子进程存活看门狗：`WaitForSingleObject` 事件驱动 + 启动期失败指数退避（`SUBPROC_*` 常量随迁） |
| | `app::secure_watch` | `src/app/secure_watch.rs` | 安全桌面（锁屏 / UAC）监听线程，`spawn(capture_start_event, capture_stop_event)`（三个轮询/等待常量随迁） |
| | `app::bootstrap` | `src/app/bootstrap.rs` | 日志初始化（主进程 / SYSTEM 子进程各一套文件）、单实例互斥体 |
| | `app::config` | `src/app/config.rs` | 注册表配置（`HKLM\SOFTWARE\RustDeskSOS\`）+ CLI 合并 |
| | `app::constants` | `src/app/constants.rs` | 全局常量、`Global\` 内核对象名、`VERSION` |
| | `app::system` | `src/app/system.rs` | SYSTEM 子进程启动与服务循环 |
| | `app::tray` | `src/app/tray.rs` | 托盘图标与菜单 |
| | `app::cpu` | `src/app/cpu.rs` | `GetSystemTimes` CPU 采样（替代上游 PDH） |
| 捕获 | `capture` | `src/capture.rs` | SYSTEM 子进程采集线程（写 SHMEM），安全桌面图像降级 |
| | `capture::dxgi` | `src/capture/dxgi.rs` | 自建 DXGI Desktop Duplication 采集器 |
| | `capture::gdi` | `src/capture/gdi.rs` | 自建 GDI 采集器（自检 / 逐档降级 / 变化检测 / 状态图） |
| | `capture::display` | `src/capture/display.rs` | 显示器枚举与按名查尺寸 |
| | `capture::uac` | `src/capture/uac.rs` | 安全桌面判定、会话连接状态 |
| 视频 | `video` | `src/video.rs` | 域根：`capture_loop`（732 行，故意不拆）、`ActiveEncoder` / `CaptureState`、全部调优常量；`pub use` 五个子模块 |
| | `video::link` | `src/video/link.rs` | 链路档位与码率预算：`LINK_TIER` 迟滞状态机、`note_link_sample`、`link_budget`、`dropframe_thresh_for_link` |
| | `video::hub` | `src/video/hub.rs` | 订阅表、广播与每显示器控制标志：`Subscriber` / `SUBSCRIBERS`、`subscribe` / `unsubscribe`、`send_video` / `broadcast_video`、`REFRESH_MASK` / `KEYFRAME_MASK` |
| | `video::ladder` | `src/video/ladder.rs` | 编码成本自适应与采集档位降级：`MIN_TIER`、`ENC_LEVEL` / `ENC_BASELINE_MS` / `VP8_REJECTED`、`capture_spf`、`step_down_encoder`、`revert_vp8_if_useless`、`is_no_display_error` |
| | `video::ability` | `src/video/ability.rs` | 控制端编码能力与当前编码格式：`PEER_ABILITY`、`set_hw_unavailable` / `hw_available`、`set_peer_codec_ability`、`current_codec` / `describe` / `codec_name` |
| | `video::displays` | `src/video/displays.rs` | 显示器枚举、命名与切换消息：`display_info` / `list_displays` / `set_display_name` / `cached_display_name` / `make_switch_display_msg` / `is_remote_session` |
| | `video::vpx` | `src/video/vpx.rs` | 自建 libvpx 编码器 + 码率/链路档位/降级策略（纯函数集中在这里，便于单测） |
| | `video::hwenc` | `src/video/hwenc.rs` | 硬件 H.264/H.265 编码器（`hwcodec` 后端：nvenc/qsv/amf，吃 CPU 的 NV12） |
| | `video::qos` | `src/video/qos.rs` | RTT / 帧率 / 码率 / 画质比，按显示器聚合 |
| 网络 | `net::connection` | `src/net/connection.rs` | 会话主循环、能力协商、消息分派、`Conn` 拆半区 |
| | `net::rendezvous` | `src/net/rendezvous.rs` | 信令服务器交互（注册、PunchHole、RequestRelay） |
| | `net::relay` | `src/net/relay.rs` | 中继地址解析与多候选挑选；NAT 判定/中继决策提成纯函数 `classify_nat` / `must_relay`（`net::rendezvous` 调用，测试直接打） |
| | `net::rtt` | `src/net/rtt.rs` | RTT 采样 min-filter：`RttFilter` + `RTT_WINDOW`(5) + `MAX_TRUSTED_RTT_MS`；`Session` 持有一个实例而不是 `VecDeque` |
| | `net::file_transfer` | `src/net/file_transfer.rs` | 文件传输作业管理 |
| IPC | `ipc::pipe` | `src/ipc/pipe.rs` | 命名管道（主进程 ↔ SYSTEM 子进程） |
| | `ipc::shmem` | `src/ipc/shmem.rs` | 共享内存帧通道 |
| 输入 | `input` | `src/input.rs` | 远程鼠标/键盘注入（单文件，无子模块） |
| 界面 | `ui::consent` | `src/ui/consent.rs` | 连接确认弹窗（Win32 置顶窗口，默认启用；锁屏/UAC 时由调用方跳过） |
| | `ui::device_id` | `src/ui/device_id.rs` | 设备编号窗：注册上管理服务器后弹一次、置顶居中、**不自动关**；用户关闭或入站控制请求到达才收，重连不重复弹 |

依赖方向**不是**严格单向，按实际调用统计是这样（与 `src/lib.rs` 顶部注释一致）：

- `app` 是装配层，依赖其余全部六个域；
- `net` → `video` 约 30 处（`subscribe`/`unsubscribe`、`qos::with`、`request_keyframe`、
  `set_peer_codec_ability`、`display_info`、`cached_display_name` 等），
  `video` → `capture` 9 处，`net` → `capture` 5 处（`uac::*`、`display::primary_size`）；
- `capture` → `ipc`（写帧）；`ipc` 与 `ui` **没有出边**。

这轮拆出的子模块没有改变上面这张图：`video/*` 仍在 `video` 域内部调用，`net::rtt`
只被 `net::connection` 使用，`net::relay` 的两个纯函数只被 `net::rendezvous` 调用。

**`ipc` 必须保持零出边。** 它只提供「命名共享内存原语」，至于帧缓冲区要多大由调用方
算好传进来：`ipc::shmem::create_shmem(fallback)` 收一个回退尺寸参数，由
`app::run` 传 `capture::display::primary_size()`。曾经这里是 `create_shmem()` 自己就地
调 `capture::display::primary_size()`，于是 `ipc ↔ capture` 成了整个 crate 唯一的双向环；
改成参数传入后就断开了。**以后别再让 `ipc` 回头查采集/会话状态** —— 真需要就加参数。

改跨域调用时按这个真实图判断，不要按「底座只经 ipc 解耦」的设想去找依赖。

---

## 3. 三个互相独立的 Cargo 项目

| 项目 | 说明 |
| --- | --- |
| `Cargo.toml`（仓库根） | workspace：`rustdesk` + `libs/*` |
| `sos-client/Cargo.toml` | 独立项目，`exclude` 排除，自带 `Cargo.lock` |
| `sos-client/xtask/Cargo.toml` | 又一层独立 workspace，`cargo xtask` 用 |

由此产生的三条硬事实：

1. **`[patch]` / `[patch.crates-io]` 只在做解析的那个 workspace 根生效。**
   `sos-client` 是自己的根，所以上游 `[patch.crates-io]` 的每一项都必须在这里
   重复一遍（`libxdo-sys` / `tungstenite` / `webrtc*`），否则拿不到上游修复过的
   签名，编译直接失败。
2. **上游把某些 git 依赖的 `rev` 只写在根 `Cargo.lock` 里，不写在清单里。**
   `sos-client` 有独立锁文件，遇到这种依赖只能显式钉 `rev` 或用
   `cargo update -p <crate> --precise <rev>` 对齐到根锁记录的那个提交。
3. **测试必须带绝对 `--manifest-path`，且不要先 `cd`。**
   `cargo test --manifest-path d:\code\rustdesk\sos-client\Cargo.toml`。
   `Set-Location`/`cd` 会被某些工具链**静默剥掉**，于是 `--manifest-path Cargo.toml`
   解析成仓库根的清单 —— 跑的是上游 `rustdesk` 的 225 个单测，而不是 SOS 的 38 个
   测试二进制。判据：输出里的 `Running` 行应是 `target\debug\deps\` 下带
   `rustdesk_sos` / `vpx_*` / `qos_*` 等名字，且 target 目录是 `sos-client\target`。

   > 顺带记一笔：直接跑仓库根的 `cargo test` 本来就有 2 个**上游既有失败**，与 SOS
   > 无关 —— `platform::windows::installer_handoff::directory::tests::
   > protected_directory_enforces_creator_permissions`（删不掉受保护目录，os error 5）
   > 和 `src/server/terminal_helper.rs` 的 doctest（缺 `use` 前缀编译不过）。
   > 看到它们不要当成自己的回归。

### 依赖名

根包 `rustdesk` 在 `sos-client/Cargo.toml` 里重命名为 **`librustdesk`**（30+ 处真实
调用：`input_service`、`clipboard`、`ConnInner`、`platform::*` 等）。看到
`librustdesk::` 就是上游服务框架，不是本项目的模块。

### rust-analyzer

三个项目必须用 `rust-analyzer.linkedProjects` 显式列出（该 setting 会**关闭自动
发现**，所以三个都要写）。配置在仓库根 `.vscode/settings.json`，而 `.vscode` 在根
`.gitignore` 里 ⇒ **本机配置，换机器要重建**。

```json
{
  "rust-analyzer.linkedProjects": [
    "Cargo.toml",
    "sos-client/Cargo.toml",
    "sos-client/xtask/Cargo.toml"
  ]
}
```

SOS 经 `rustdesk = { path = ".." }` 依赖整个根 crate（连 WebRTC + 静态 FFmpeg），
rust-analyzer 又慢又吃内存（实测 11.8 GB）**是正常的**，不是卡住。
`sos-client/target` 已放进 `files.watcherExclude`。

### `target/debug` 会疯涨（实测峰值 148.8 GB）

一次完整构建（含 38 个测试二进制）就 **29.1 GB**，cargo 还不回收历史 hash 产物，
大重构做几十轮就过百 GB。清理命令、成因分析与防复发选项见
[`README.md`](README.md)「清理 debug 产物」一节。

---

## 4. 构建与发布

入口 `cargo xtask`（**只在本目录有效**，别名从当前工作目录向上找 `.cargo/config.toml`）。

流程：`cargo build --release` → 校验 PE 头 → 复制到 `dist\` → `upx --best --lzma`
压副本 → PE 头再校验 → `upx -t` 回读 → 打印体积对比。

- **压的是副本，不是 `target/release` 原件。** 原地改写会破坏 cargo 的 mtime
  判据，导致每次 build 都重新链接。
- **不要用 build.rs 做压缩**：它在链接**之前**跑，产物还不存在；且 cargo 会缓存
  build script，源码没变时根本不重跑 → 产物会悄悄变回未压缩。
- **签名必须在压缩之后**（UPX 改写 PE 布局）。
- `build.rs` 每次编译先 `taskkill /f /im rustdesk-sos.exe`，否则链接失败。

### UPX 策略（用户明确要求，不要改回钉死）

- **禁止**在源码中写死版本号或 sha256：UPX 发布安全修复后，钉死会让构建长期停在
  有已知问题的旧二进制上。
- 缺 UPX 时向 `https://api.github.com/repos/upx/upx/releases/latest` 取
  `tag_name` + win64 资产 + 资产 `digest`（sha256），**强制校验**后才解压。
  没有「跳过校验」分支。
- `--upx-url` 只换下载位置，sha256 仍取自 GitHub ⇒ 镜像文件必须与官方字节一致。
- 关闭：`--no-upx-fetch` / `SOS_NO_UPX_FETCH=1`；离线：`--upx-path`（该路径不存在
  就报错，**不会**回退去下载）。
- `tools/upx/` 只是缓存目录，不入库。

### `rust-version = "1.80"` 是被 build.rs 逼出来的

`build.rs` 用 `cargo::rustc-check-cfg=...`（Cargo 1.77+ 才认），而这条指令本身
1.80+ 才支持。改 `rust-version` 前先确认 `build.rs` 里还有没有 `cargo::`。

### winres 的 `rerun-if-changed`

`winres` 0.1.12 全程不 emit `cargo:rerun-if-changed`。`build.rs` 自己 emit 了
（`WtsApi32` 那几行），于是 cargo 的「任何文件变了就重跑」兜底被关掉 → 只改
`[package.metadata.winres]` 时 build script 不重跑，exe 里留旧版本字符串。
`build.rs` 里那行 `cargo:rerun-if-changed=Cargo.toml` 就是为此，不要删。

---

## 5. 上游依赖现状（2026-11 同步 master 后）

- 上游把客户端侧代码从 `hbb_common` 拆到新 crate **`libs/base`**：
  `base::message_proto` / `base::protos` / `base::fs` / `base::platform` / `base::config`。
  `hbb_common` 现在只留服务端共用的信令协议、socket、`Config` 核心。
  **`IdPk` 在 `hbb_common::rendezvous_proto`**，不在 `message.proto` 里。
- `hbb_common::Stream` 现在实现 `Drop`（调用 `close_webrtc()`），不能再按值解构。
  `Conn::split` 用 `ManuallyDrop` + `ptr::read` 取出 `tcp::FramedStream`
  （该结构无 `Drop`、字段全 `pub`，目标变体已判定的安全前提写在函数文档里）。
- **WebRTC**：`Stream` 的一个变体，`drop` 会分发到 `close_webrtc()`，链接器无法裁剪，
  产物因此多出 ~5 MB。SOS **接受** WebRTC 入站（见 [`README.md`](README.md)「接入协议」），回答 SDP offer、
  trickle ICE、并在签名身份里绑定 DTLS 指纹；实现全在 `net::webrtc_in` + `net::rendezvous`。
- **版本号故意保持 `1.4.9`**（上游已 1.5.3）：`app::constants::VERSION` 会作为
  `PeerInfo.version` 上报并驱动控制端特性开关。WebRTC 传输虽然已经实现，但 1.5.x
  还有其它 sos 没有的能力（音频、摄像头、终端、隐私模式等），报高版本会让控制端
  放开这些开关。改之前先逐项确认协议面已对齐。
- `libs/hbb_common` 是 git submodule，不要在父仓库里直接改它的内容。

---

## 6. 架构要点

### 双进程

主进程（用户权限，托盘 + 网络 + 全部服务）用 winlogon 令牌把自身以 SYSTEM 身份再
拉起一份 `--service` 实例，后者只负责在安全桌面出现时往共享内存写帧。

- 帧数据走 SHMEM（`ipc::shmem`），控制信令走命名管道（`ipc::pipe`）。
- **跨会话必须用 `Global\` 内核对象**：`app::constants` 的事件名带 `Global\` 前缀，
  `ipc::shmem` 的私有 `os_id()` 生成 `Global\RustDeskSOS_{name}`，`scoped_name()` 统一取值。
  创建方需**显式启用** `SeCreateGlobalPrivilege`（管理员令牌持有但默认禁用，且
  `AdjustTokenPrivileges` 部分授予也返回成功，必须再查 `GetLastError()==1300`）。
- `create()` 失败会回退会话内命名，此时主进程按 `uses_global_namespace()` 给子进程
  传 `--no-global`，**不能**把它送进别的会话（连不上 SHMEM/事件）。

### 自建视频链路

不使用上游 `video_service`，链路是：`video` 的采集线程 →
`Frame::to(yuvfmt)` → `video::vpx::SosVpxEncoder` → `Conn::send`。

- 只**读取**上游的少量公开 API（`make_display_changed_msg`、`VideoSource`、
  `restore_resolutions` 等），**不改**它的实现（早期注入点已退场，见 §1 的
  退场记录；`video_service.rs` 现在与上游完全一致）。
- `#[cfg(not(sos_vpx))]` 的「回退上游 video_service」分支**编译不过**（`net::connection`
  无条件引用 `video` 的 `vpx`/`hwenc`/`qos` 子模块），已换成 `src/lib.rs` 里的
  `compile_error!`。缺 libvpx 时构建直接给出明确原因，不要复活那条分支。
- `g_lag_in_frames = 0` 是硬要求：VP9 默认 25 帧前瞻，不设 0 则 `encode` 成功但
  `get_cx_data` 取不到包（实测 0 输出）。上游正是靠每帧 `flush()` 强逼，代价是
  丢掉帧间优化（单帧耗时翻 2.15 倍、码率暴增）。
- YUV 布局用假指针 `vpx_img_wrap` 求解 libvpx 的 stride/平面偏移，再作为
  `scrap::EncodeYuvFormat` 交给 `Frame::to()`，保证 libyuv 写入与 libvpx 读取一致。
- `frame_len` 取三平面末尾最大值 `max(u + stride[1]*h/2, v + stride[2]*h/2)`，
  不能重复累加 `stride[0]*h`。

### 采集降级

`MIN_TIER` 记档：DXGI(0) → GDI(1)，构造失败才降档；降档后每 30s 在**独立线程**
重探，只有成功才把 `MIN_TIER` 复位 + 请求刷新。旧实现把重探做成重建路径，探测失败
也会换掉正常工作的采集器（~250ms 停顿、丢参考帧），DXGI 不可用的机器上反复发生。

`classify_frame_error()`：WouldBlock / TimedOut / Interrupted = 正常无新帧；
ConnectionRefused = 降档；其余 = 重建（连续 5 次才降档）。

（实现位置：`MIN_TIER` / `capture_spf` 在 `video::ladder`；`create_capturer` /
`classify_frame_error` / `demote_capturer` 与 `MAX_REBUILD_BACKOFF` 仍在 `video.rs` 域根。）

### QoS

`video::qos` 替代上游全局标量 `VIDEO_QOS`，按显示器聚合多连接：

| 量 | 规则 | 理由 |
| --- | --- | --- |
| `ratio` | 同屏取**最大** | 有人要「最佳」不该被拉低；未表态按 `BR_BALANCED` 参与 |
| `target_fps` | 同屏 `custom_fps` 取**最小** | 编码器只有一个 |
| `fps` | 同屏自适应值取**最小** | 任一链路拥塞就该降 |

连接帧率独立自适应：降 ×3/4（每 `BAD_SAMPLES_TO_REDUCE=2` 个拥塞样本），
升 ×5/4+1（每样本）。**只由排队延迟驱动**（`queue ≥ 40ms`），RTT 高但无排队
（卫星链路）不降帧。

**自定义画质（协议三档之外）QoS 完全退出**：码率只按 `ratio` 折算
（`bitrate_kbps_exact`），帧率恒为 `custom_fps`。两者**必须同时**退出 ——
只锁码率而继续降帧会让每帧变大→排队更久（实测 rtt 35→451ms）。

---

## 7. 已踩实的坑

完整清单在 [`docs/PITFALLS.md`](docs/PITFALLS.md)（按主题分节，动手前按主题查）。
下面五条是**最容易凭直觉改错**的，先记住：

- **码率必须随帧率等比缩放**（`bitrate_kbps_for`，`clamp(fps/30, 0.5, 1.0)`）。帧与
  `TestDelay` 探针同一条流 ⇒ 不缩放就是正反馈：压帧率 → 码率不变 → 每帧更大 → 延迟
  更高（实测 rtt 35→451ms 卡死数秒）。
- **延迟样本要做窗口最小值滤波**（`net::rtt::RttFilter`，N=5），不是阈值也不是中值。
- **密码错误必须在两个地方都回包**（登录主循环 + 等待本机确认期间，共用
  `report_wrong_password`），详见 PITFALLS 的「连接确认弹窗」一节。
- **尺寸检测用 `GetMonitorInfo`，不能用 `EnumDisplaySettingsW`**（125% 缩放下差 1.25 倍，
  用错会每秒重建一次编码器）。
- **编码器 A/B 必须同时控制「实际码率」和「丢帧水位」**，否则数据毫无意义（旧结论
  「VP8 快 4.8 倍」就是这么错的）。

---

## 8. 诊断日志

生产不要开 `--debug`，但要知道怎么读常规字段。

| 字段 | 含义 |
| --- | --- |
| `[VIDEO:FPS] encode_out= sent_kbps= sendq= qos_fps= qos_bitrate= codec= rtt= raw=` | 5s 一次。`encode_out` 低**不等于**有问题（画面静止时 `skip` 很高，属正常）；判据看 `edrop` 和 `rtt`。`rtt` 是滤波后喂 QoS 的值，`raw` 是最近原始样本。**`qos_bitrate` 是 QoS 目标值，不是编码器实际生效值**（被网卡/编码器上限钳制，实际值看 `[HWENC] ... clamping` 或 `capturing ... bitrate=`） |
| `[SOS_PERF] loop= cap= conv= enc= spf= it=` | 分阶段耗时。`loop` 与 `spf` 接近才说明节奏正常；`cap` 占大头 ⇒ GDI 采集是瓶颈，改编码器无效 |
| `[SOS_DROP] n/m frames dropped (P%) target=Kkbps qos_fps=` | 仅 `edrop>0` 时 5s 一次 |
| `[VIDEO:PERF] stale=` | 本窗口「重编保留帧」的次数 |
| `[LOOP_RATE]` | 循环迭代率。跌到 0 说明主循环被某处 await 阻塞（历史病因见 `Conn` 拆半区） |
| `[CAP] first frame written` | SYSTEM 子进程首帧落盘 SHMEM |

`skip` / `wait` 计的是「抓到帧但判定未变化而丢弃」，静止桌面下很高是正常的。

---

## 9. 测试

- 运行：`cargo test --manifest-path d:\code\rustdesk\sos-client\Cargo.toml`
  （见 §3 第 3 条，别省 `--manifest-path`）。
  **解锁桌面约 25 秒，锁屏时约 80 秒**（差在采集类测试的空转预算，见下）。
- `tests/support/` 放共用内容生成器（在子目录里，不会被当成测试目标）。
- **集成测试走库，不要再 `#[path]` 直接包含源文件**：写成
  `use rustdesk_sos::video::vpx as sos_vpx;`（`as sos_<旧名>` 只是保住原有调用点，
  模块真名是 `video::vpx`）。现在 `tests/` 里只剩 `#[path = "support/desktop.rs"]`
  这种对共用辅助模块的引用；**不要再写出指向 `../src/` 的 `#[path]`** —— 那等于把
  源文件重新编进每个测试二进制，还会绕开可见性：集成测试在 crate 外，`pub(crate)`
  根本看不见，这正是「测试只依赖公开面」的保证来源。
- **库有 doctest**：`cargo test` 会编译文档注释里的围栏代码块。引用外部源码（例如
  hbbs 的 Rust 片段）的块必须标 `text`，否则被当成示例编译并失败 —— `net/relay.rs`
  就是这么改过来的。新增文档块时同样处理。
- **纯测量型基准默认不跑**（`#[ignore]`）：13 个文件都没有断言，只打印数字供人看；
  曾占掉整套 `cargo test` 187 秒里的 167 秒。要跑某个基准：

  ```powershell
  cargo test --manifest-path d:\code\rustdesk\sos-client\Cargo.toml --test vpx_drop_diag -- --ignored --nocapture
  cargo test --manifest-path d:\code\rustdesk\sos-client\Cargo.toml -- --ignored   # 全部基准
  ```

  已忽略的文件：`vpx_drop_diag` `vpx_rc_mode` `vpx_mem` `vpx_enc_throughput` `vpx_cpuused`
  `vpx_budget` `vpx_screen_content` `periodic_cost` `hw_vs_sw_bench` `vpx_threads`
  `enc_bench` `codec_cost` `gdi_bench` `hw_bitrate_limit`。**新增基准请照此办理**（`#[test]` 下面加
  `#[ignore = "实机基准…"]`），别塞进默认流程。
- 实机基准的数值随机器变化，作用只是「同机 A/B 对比」，不要按注释里的数字断言。
- **采集类测试都要能在「抓不到帧」的环境里优雅跳过**（锁屏 / 会话断开 / 同一个二进制里
  两个测试抢 DXGI duplication —— 每个输出只允许一个）：
  * 先做**前置探测**（跑几百毫秒动画窗口再试着抓几帧），一帧都抓不到就打印 `[skip]` 后
    `return`，不要 `assert`。真正该失败的只有「抓到了帧却编不出包」那种。
  * 循环必须有**墙钟预算**：`dxgi_long` 曾按 300 帧 × 200ms 超时 = 空转 60 秒，现在 6 秒。
  * 同一文件里的多个采集测试要用 `static Mutex` 串起来（`dxgi_long` 的 `CAPTURE_GATE`）：
    原注释要求调用方加 `--test-threads=1`，但要求人记住参数不可靠，实测并行时后建的那个
    采集器 `ok=0/20`。
- 涉及真实桌面的测试在锁屏 / RDP / 无 DXGI 环境下会走降级路径，属预期。
- **策略 / 判定逻辑必须提成 crate 内可见的纯函数，测试直接打真实实现。**
  `tests/` 里**不要**再复刻一份判定规则。历史教训：复刻版测试全绿，而生产改动
  根本没有生效；复刻版还漏了 `MAX_TRUSTED_RTT_MS` 校验，等于把 bug 一起抄走了。
  现在直接打真实实现的有三处：
  * `tests/p2p_logic.rs` → `use rustdesk_sos::net::relay::{classify_nat, must_relay};`
    （不再复刻 NAT 判定与中继决策规则）
  * `tests/rtt_filter.rs` → `use rustdesk_sos::net::rtt::{RttFilter, RTT_WINDOW};`
    （不再复刻窗口逻辑）
  * `tests/desktop_unavailable.rs` 的 `only_no_display_is_treated_as_transient`
    → `rustdesk_sos::video::is_no_display_error`。
  所以新写策略时先在库里放一个 `pub fn`（`video::vpx` / `video::link` / `video::ladder` /
  `video::hub` / `net::rtt` / `net::relay` 就是这么分布的），再让测试 `use` 它；
  **不要**把规则内联在生产路径里、再抄一份到测试。
- **仍然保留复刻 / 自建的三处，都是刻意的**：
  * `enc_bench`：测**生产编码器** `video::vpx` 的本机吞吐（原「上游基线」已退场，
    见下方「基准怎么在不改上游的前提下做公平对比」）；
  * `dxgi_live`：测**上游** `scrap` 的 DXGI 行为（静止桌面 `WouldBlock`、真实变化时
    出帧），不是测自建 `capture::dxgi`；
  * `gdi_bench`：纯 Win32 开销拆解（BitBlt / CAPTUREBLT / GetDIBits / 全帧翻转），
    只量系统调用，不经过本项目代码。
  > **基准怎么在不改上游的前提下做公平对比**：两个编码基准都跑生产编码器
  > （`video::vpx`），并在创建前调 `sos_vpx::set_rc_override(Some((1000, 0)))` ——
  > buffer 1000ms + `rc_dropframe_thresh = 0`。不这么做的话，硬内容会触发丢帧
  > （实测 40 帧只出 1 个包），被丢的帧不参与编码却照样计入耗时，fps 被抬高。
  > 这个钩子是本 crate 的 `#[doc(hidden)]` 诊断 API（线程局部，生产不调用），
  > **不需要动 `libs/scrap`**。两个基准还会打印 `encoded=N/frames`，丢帧不会再被
  > 静默忽略。
  另外 `desktop_unavailable` 里还有三条仍是规则复刻（重建退避曲线、日志闸门、
  `gap_ms` 统计），因为 `MAX_REBUILD_BACKOFF`、`GAP_OUTAGE_MS` 还是 `video` /
  `net::connection` 的私有常量；要消掉它们得先把常量公开，算单独一步。
- 其余策略仍集中在 `video::vpx`（`choose_codec`、`bitrate_kbps_for`、
  `encoder_behind_budget`、`should_reemit_retained`、`LinkTier` 等），
  单测直接打这些函数，不要为测行为去搭采集器。
- 现状：`cargo check --all-targets`（在 `sos-client` 内）**0 error / 0 warning**，
  7 条历史 test 警告已清。新增测试与测试侧改动不要再引入 SOS 侧警告。

---

## 10. 提交

- 改动应落在 `sos-client/`；上游文件的偏离面**只能减少不能增加**（核对方法见 §1 的表）。
- 消息用**中文**、描述性长消息；说清「为什么」和**回归面**（哪些既有路径行为变了、
  为什么不可避）。
- 提交前自检：
  - `git status --short` 干净；
  - `cargo check --all-targets`（在 `sos-client` 内）零 SOS 侧警告；
  - `git diff --name-status origin/master...HEAD -- . ':!sos-client'` 与 §1 的表吻合。
- `.vscode/`、`sos-client/tools/`、`sos-client/dist/`、`*/target/` 都不入库。
- 换行：`.gitattributes` = `* text=auto` + `core.autocrlf=true`，
  `warning: LF will be replaced by CRLF` 是预期噪音。
- 编辑器可能对根 `Cargo.toml`、`README.md` 做纯格式重排 —— 提交前 `git diff` 检查，
  对根清单的格式改动必须 `git checkout --` 还原。`cargo fmt --check` 在 `sos-client`
  上目前并不干净，**不要**整体 `cargo fmt` 制造无关大 diff。

---

## 11. 已知限制

见 [`docs/KNOWN-LIMITS.md`](docs/KNOWN-LIMITS.md)：那是一批**刻意留下的**行为与系统级
限制（会话断开黑屏、WebRTC 栈体积、硬件编码没有丢帧旋钮/强制 IDR 等），
每一条后面都写了原因。**看到它们不要当 bug 修。**
