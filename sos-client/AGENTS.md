# RustDesk SOS 精简受控端 — 开发指引

本文件只覆盖 `sos-client/`。仓库根的 `AGENTS.md` 属于上游 RustDesk，其全部规则
（Rust 风格、日志、Tokio、最小改动、回归面检查、本地化等）在这里同样适用；
本文件补充 SOS 特有的事实与约束。

> 面向人的构建/运行说明在 [`README.md`](README.md)，本文不重复，只写**改代码前
> 必须知道的事**。

---

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
├── AGENTS.md             本文件
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
│   │   ├── hwenc.rs      硬件 H.264 编码器（`hwcodec` 的 FFmpeg 后端，吃 CPU 的 NV12）
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
│   │   └── consent.rs    连接确认弹窗
│   └── bindings/
│       └── vpx_ffi.h     bindgen 的输入，只声明用到的 libvpx 符号
├── tests/                36 个集成测试二进制；support/ 是共用内容生成器（非测试目标）
├── vcpkg/ffmpeg/         FFmpeg overlay port（官方 7.1.2 + 上游两个 AMF 补丁）
├── vendor/hwcodec/       hwcodec 本地副本（只改「调优项失败」不再致命）
├── xtask/                发布构建工具，自带 `[workspace]`，零依赖
├── tools/                【不入库】UPX 缓存 + 一次性现场诊断脚本
├── dist/                 【不入库】交付物（UPX 压缩后）
└── target/               【不入库】cargo 原生产物
```

### `src/` 为什么现在分层

旧结构是「全部平铺 + `sos_` 前缀」，唯一的理由是**测试要复用源文件**：那时没有库
目标，`tests/*.rs` 只能用 `#[path = "../src/sos_xxx.rs"] mod sos_xxx;` 把某个源文件
重新编译进自己的二进制（测试里的 `crate::sos_xxx` 因此也能解析）。文件一移动就要
连带改 30+ 个 `#[path]`，所以只在 `main.rs` 顶部维护一张「模块地图」做逻辑分组，
不动文件位置。

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
| | `video::hwenc` | `src/video/hwenc.rs` | 硬件 H.264 编码器（`hwcodec` 的 FFmpeg 后端：nvenc/qsv/amf，吃 CPU 的 NV12） |
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
   解析成仓库根的清单 —— 跑的是上游 `rustdesk` 的 225 个单测，而不是 SOS 的 36 个
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

`rust-analyzer analysis-stats` 实测：`Database loaded 9.7s / 2.6GB`、
`Item Tree Collection 57s / 11.8GB`、依赖 2856 万行 —— SOS 经
`rustdesk = { path = ".." }` 依赖整个根 crate，**很重是正常的**，不是卡住。
`sos-client/target` 已放进 `files.watcherExclude`。

### `target/debug` 会疯涨（实测峰值 148.8 GB）

一次完整构建（含 36 个测试二进制）就 **29.1 GB**，cargo 还不回收历史 hash 产物，
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
  `restore_resolutions` 等），**不改**它的实现。早期曾在
  `src/server/video_service.rs` 里塞过 `CUSTOM_CAPTURER_FACTORY` 注入点，已随
  自建链路成熟而退场（见 §1 的退场记录）——**该文件现在与上游完全一致**。
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

---

## 7. 已踩实的坑

写代码前扫一遍；这些都是实测结论，不要凭直觉改回去。

### 采集

- **`scrap` 的 DXGI 实现有两个致命 bug**（`unmap()` 顺序颠倒、提前 return 不释放
  frame），fastlane 下会让 `MapDesktopSurface` 永久返回 `DXGI_ERROR_INVALID_CALL`
  且重建也救不回。所以有自建 `capture::dxgi`（实测连续 300 次采集 0 个 InvalidData）。
- **`scrap` 把 `DXGI_ERROR_WAIT_TIMEOUT` 映射为 `ErrorKind::TimedOut`**，不是
  `WouldBlock`。只白名单 `WouldBlock` 会把「手静止桌面」误判成「DXGI 不可用」。
- `scrap::Display` 既不 `Clone` 也不能共享（`Capturer::new` 取所有权），需要两份时
  重新 `Display::all()` 并按 `name()` 匹配。它内含 DXGI/COM 裸指针、**非 `Send`**，
  必须包在独立作用域里，否则活过 `.await` 会让整个 future 不再 `Send`。
- **尺寸检测必须用 `GetMonitorInfo`，不能用 `EnumDisplaySettingsW`**：前者
  (`rcMonitor`) 与进程 DPI 感知一致，后者 (`DEVMODE`) 永远是物理像素。125% 缩放
  下两者差 1.25 倍，用错会恒定判「分辨率变了」→ 每秒重建一次编码器。
  `tests/display_select.rs` 锁死这条。
- **安全桌面上 `Display::all()` 必然为空**（`EnumDisplayDevicesW` 在 Winlogon 桌面
  拿不到设备），必须有**不依赖枚举**的回退：`capture::gdi::primary()` =
  `CreateDCW(L"DISPLAY")` 与 `GetDC(NULL)`，尺寸用 `GetDeviceCaps(HORZRES/VERTRES)`。
- **「建得出 DC」≠「抄得动像素」**：安全桌面上 `CreateDCW` 成功但 `BitBlt` 连续被拒
  (`E_ACCESSDENIED`)。所以自检要实测且多轮，采集方式要能逐档降级
  （`BitBlt` → `BitBlt|CAPTUREBLT` → `PrintWindow(桌面窗口)`），帧级连续 2 次失败才换档。
- `CapturerGDI` 是 `CreateDCW`，**DC 绑定创建时线程所在的桌面**。SYSTEM 采集线程
  必须先 `SosSwitchToInputDesktop()` 再建采集器，顺序不能反。
- **不能关闭 `GetThreadDesktop()` 返回的句柄**（MSDN 明确禁止，句柄由系统拥有）。
  只关自己 `OpenDesktop`/`OpenInputDesktop` 打开的。
- **`SetWinEventHook` 从 exe 里注册时永远不能用 `WINEVENT_INCONTEXT`**（要求回调在
  DLL 里），实测直接返回 NULL。用 `WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS`
  并每 250ms 片轮询泵消息。**任何「阻塞等待」型 API 都必须检查返回值**，否则失败即
  变成忙等（现场：无会话时稳定烧 25% CPU）。
- `SosWaitForDesktopSwitch` 的等待**必须是有限超时**（1s）。钩子只是加速器：会话被锁
  / RDP 断开后**不再收到任何 WinEvent**，`INFINITE` 会让 watcher 永远静默。
- GDI 路径 `BitBlt` 固定开销 7-8ms（win32k 往返），1080p 以上受 ~780MB/s 带宽瓶颈；
  `scrap` 原版还额外做 GetDIBits + 全帧镜像 + 全帧旋转 + 每帧 8MB 分配 + 8MB memcmp，
  自建 DIB 直写全部省掉。

### 硬件编码（H.264）

- **`hwcodec` 是默认 feature**（`sos-client/Cargo.toml` 的 `default = ["hwcodec"]`）：
  普通 `cargo xtask` 出的就是带硬件编码的版本，产物约 29.4 MB（压缩前）/ 7~8 MB（UPX）。
  要回到纯软件、不依赖 FFmpeg 静态库的小体积版本用 `--no-default-features`（13.6 MB）。
  关闭时 `video::hwenc` 走一个**同 API 的桩**，所以上层（`video`/`net::connection`）
  两种配置下是同一份代码，不需要 `#[cfg]`。
- **上游的硬件编码只出 H.264/H.265，没有 VP8/VP9**（`hwcodec` 的 `available_encoders` 只枚举
  h264/hevc）。所以这不是「换个后端函数」，而是**换编码格式**：`SosCodec`/`CodecPrefer`/
  `choose_codec`/`make_video_msg`/能力声明全都要一起动。
- 走的是 **RAM 路径**（`hwcodec::ffmpeg_ram::encode::Encoder`，吃 CPU 的 NV12），不是 D3D11 纹理直通。想零拷贝必须
  让采集器产出 `Frame::Texture` + `set_output_texture` + `input_texture()==true`，而
  `capture::dxgi`/`capture::gdi`/`ipc::shmem` 三个采集器**只产出 CPU 的 `PixelBuffer(BGRA)`**，
  且上游明令「GDI 采集 + VRAM 编码」互斥（`video_service.rs:789-794` 直接 `bail!`）——
  而 GDI 正是 SOS 锁屏/RDP 场景的唯一路径。所以纹理直通在 SOS 里没有价值。
- **FFmpeg 是静态链接进产物的，目标机不需要装任何东西**：`hwcodec/build.rs` 从
  `$VCPKG_ROOT/installed/<triplet>/lib` 取 `avcodec/avutil/avformat` 并以静态库链接，
  再 `cc` 编它的 C++ 外壳。所以「需要 vcpkg」只是**构建期**约束，不是部署期依赖。
  那份静态库由 **`sos-client/vcpkg/ffmpeg/`**（本项目自己的 overlay port）装出来，
  它 = vcpkg 官方 7.1.2 端口 + 上游那两个 AMF 补丁。**不能改用仓库的 `res/vcpkg`
  overlay**：那个端口 `--disable-swresample`，而 hwcodec 的 `dec.o`（opus 解码）
  需要 `swr_*`，会 LNK2019。装法见 README（构建前置步骤）。
  注意 hwcodec 写死的是 `VCPKG_ROOT/installed`（classic 模式的全局安装目录），
  **不是**仓库里的 `vcpkg_installed`（manifest 模式）—— 上游 CI 用
  `lukka/run-vcpkg` 装的正是前者。
- **`yuvfmt()` 必须用编码器自己给的那份**：硬件要 NV12 且 stride 由 ffmpeg 决定，
  不能沿用 `video::vpx` 里 `calc_yuvfmt` 那套「用假指针让 libvpx 反解布局」的做法。
- **探测结论必须自己缓存**（`video::hwenc` 的 `STATE`）。`Encoder::available_encoders` 会
  **真的逐個建编码器并试编一帧**（实测 2.3s），不缓存就等于每隔几秒重探一次。
  `available()` 会被 `[VID_FPS]` 这类周期性日志间接调用，所以必须走原子量。
- 探测放**后台线程**（`probe_async`），不用上游的子进程模式：上游要 `--check-hwcodec-config`
  子命令，而 SOS 的 clap 会拒绝未知参数，启动路径上还有管理员检查 + 单实例互斥体。
  探测完成前 `available()` 恒 false ⇒ 一律走 VP9，功能不受影响。
- 选码时**不看 `ENC_LEVEL`**：那套阶梯是按「软件编码吃 CPU」标定的。
  反过来 `encoder_behind_budget` 的第一个条件 `enc > cap` 在硬件上**永远不成立**
  （提交耗时 <1ms，`cap` 约 5ms），所以降级阶梯对硬件后端天然空转 —— 这是设计，不是 bug。
- 能力声明的门控：`encoding.h264` / `encoding.h265` **各自独立探测**（`hw_available*`）；
  声明了没实现 ⇒ 控制端出现选了没用的选项，`prefer=H264/H265` 在软件规则里当 `Auto`。
- **H.265 与 H.264 并列**：选码纯函数 `video::pick_codec`（`Auto: H265 > H264 > VP9`，同上游）；
  码流必须用 `h265s`（写错会花屏）；初始化失败 H.265 → H.264 → VP9 逐级回退。
### 码率控制（libvpx）

- **`rc_buf_sz` / `rc_buf_initial_sz` / `rc_buf_optimal_sz` 单位是毫秒，不是比特。**
  原写法 `target_bitrate * 5` 在 1675kbps 下 = 8375ms 缓冲，允许编码器长期超发。
  现固定 `RC_BUFFER_MS = 1000`。
- **`rc_max_quantizer` 要给满 63**（VP8/VP9 都是 0-63）。设 56 会让码控撞上限而放弃
  调节。`rc_min_quantizer = 4`，否则不可压缩内容下「无论怎么超发都合规」。
  （VP8 的 Q 范围不是 0-127，设 127 直接 `VPX_CODEC_INVALID_PARAM`。）
- **pts 必须用真实流逝时间**，不能 `frame_index * 33`。CBR 按 pts 记账，固定 33ms
  而实际帧率偏离时码率会等比放大（83fps 灌帧 → 产出 340% 目标）。
- **`rc_dropframe_thresh` 是「带宽换流畅」开关，不是调优参数。** 0 = 永不丢帧但码率
  可到 1.97x；5/10/20 全落在 80-83% 丢帧的同一档。现按链路档位自适应：
  好链路 → 0，否则 → `RC_DROPFRAME_THRESH = 20`。
- **`rc_overshoot_pct` 对丢帧和关键帧都无效**（10/30/60 结果一字不差）：它约束的是
  总码率，而丢帧本身已经让总码率达标了。
- **关键帧约 P 帧的 9 倍**。`RC_BUFFER_MS=200` 装不下一个关键帧 → 关键帧被压到
  Q62.5 且后续 P 帧被抬价补偿，总码率反而超到 1.13x。1000ms 时 Q54、1.02x。
- **`VP8E_SET_MAX_INTRA_BITRATE_PCT` 默认 0=不限，设 450/900 反而更差**（内部还有隐含
  上限），它不是提高预算的旋钮。
- **不要用 VBR**：`g_lag_in_frames=0` 下没有前瞻，VBR 的理论优势不存在（静态内容
  与 CBR 差 0.4%），但实测 5x 超发、峰值 16.7x，且**完全无视 `rc_dropframe_thresh`**
  ⇒ 唯一的保护机制失效，还会破坏 ABR 契约（QoS 假设目标码率是上限）。
- **libvpx 的「少而精」**：丢 83% 帧时出图帧 43KB 看着失控，但总码率只超 7%。判断
  码控是否正常必须看**总码率**，不能看单帧大小。
- 量化器必须用 `VP8E_GET_LAST_QUANTIZER_64`（0-63，与 `rc_max_quantizer` 同单位）；
  `VP8E_GET_LAST_QUANTIZER` 是内部刻度（VP8 0-127 / VP9 0-255），直接比会得反结论。
- `VP9E_CONTENT_SCREEN` 在桌面内容下量化器降 22-25%（同码率画质更好）或省 23% 码率，
  但编码耗时 +10%~33%。**不能无条件开**（4 核机 `enc=24-27ms/spf=33` 已无余量），
  仅建议给实测 `enc_avg < 0.6*spf` 的机器。
- VP9 realtime 有 row-mt + tile columns，VP8 帧内并行度弱。**2 线程及以上 VP8 在速度
  和画质两个维度同时更差**（慢 27%~127%，量化器高 14%~38%），所以 `Auto` 直接选 VP9，
  「编不动」交给实测降级判定。**编码器 A/B 必须同时控制「实际码率」和「丢帧水位」**，
  否则数据毫无意义（旧结论「VP8 快 4.8 倍」就是这么错的）。

### 码率 / 帧率耦合

- **码率必须随帧率等比缩放**（`bitrate_kbps_for`，`clamp(fps/30, 0.5, 1.0)`）。
  视频帧和 `TestDelay` 探针走同一条 TCP 流 ⇒ 测到的 RTT 就等于单帧排队延迟。
  不缩放则形成正反馈：延迟高 → 压帧率 → 码率不变 → 每帧变大 → 延迟更高，不收敛
  （实测 rtt 从 35ms 冲到 451ms，然后卡死数秒）。
- 缩放下限是 `MIN_FPS_FACTOR = 0.5`。更小会塌缩到 318kbps，1080p 编不出来只能疯狂
  丢帧，而且掩盖了「采集侧永不低于 `MIN_CAPTURE_FPS = 8`」的事实 ⇒ 1Hz 循环里传的是
  `max(qos_fps, MIN_CAPTURE_FPS)`。
- `video::qos` 的 `DEFAULT_FPS` 是 30（不是上游 `VIDEO_QOS` 的初始 15），所以启动即满速，
  不存在「预热期按 15fps 算码率 → 837kbps → 82% 丢帧」那个坑。**改初始值前先想清楚这一点。**
- 变更判定用「码率相对变化 > 8%」而不是「画质比变化 > 0.05」，因为帧率变化也会改码率。

### 链路档位 / QoS 采样

- **延迟样本必须做窗口最小值滤波（min-filter，N=5），不是阈值也不是中值。**
  阈值 600ms 会让 490ms 尖峰照样通过；中值滤波会被「连续 3 个以上尖峰」带跑。
  排队延迟恒为正、真实 RTT 是下界 ⇒ 只有窗口内所有样本都变高才是真拥塞。
  （实现：`net::rtt::RttFilter`，窗口 `RTT_WINDOW = 5`、可信上限 `MAX_TRUSTED_RTT_MS`，
  `Session` 持有一个实例；档位状态机是 `video::link` 的 `LINK_TIER`。）
- **`TestDelay` 必须区分 `from_client`**：`true` 是控制端自己的探测（原样回显，
  绝不能算本机 RTT），`false` 才是本机探测的回显。不区分会把「双方 1Hz 定时器的
  相位差」当 RTT（0~1000ms 缓慢漂移）⇒ `qos_fps` 在 1↔30 之间正弦振荡。
- **不要调用 `user_delay_response_elapsed()`**：它把「我们等了多久」也塞进历史，
  既把 fps 压到 2，又注入假样本（2000ms）。真实拥塞由有界视频通道（容量 2）的背压
  和排队延迟反映。探测超时设 1500ms，超时丢弃重发，否则 `last_test_delay` 老化会
  造成活锁。
- **链路档位要有迟滞**：`LinkTier` 进 Generous 要连续 5 个好样本，出档要连续 2 个坏
  样本。单样本判据会在编码器重建瞬间被一个迟到回显翻档（实测码率 5208→2604→5208）。
  一次探测只喂**一个**样本，拆成两个 setter 会把窗口压缩一半。
- 好链路判据是 `rtt < GOOD_LINK_RTT_MS(50)` **且** `queue < GOOD_LINK_QUEUE_MS(40)`。
  只看 RTT 会把低延迟 Wi-Fi 误判成有线内网。
- 新会话（订阅表原本为空）要 `LinkTier::reset()`，否则继承上一个控制端的
  「generous」档（码率 ×2 + 关丢帧保护）。多控制端场景不重置。

### 关键帧 / 静止画面

- **`request_keyframe()`（`video::hub`）只置位，等下一帧 `encode()` 才生效**；画面静止时采集器一直
  返回 WouldBlock/TimedOut → 永不 encode → 对端永久花屏到重连。
  修法是保留上一帧 `yuv`，在 WouldBlock **或** TimedOut 分支里按
  `should_reemit_retained()` 重编一次。
- `kf_max_dist` 数的是**送进编码器的帧数**，不是墙钟时间 ⇒ 静止画面上周期性关键帧
  会被无限推迟，别指望它兜底。
- 强制关键帧要节流到 ≥ 1.5s（`MIN_KEYFRAME_INTERVAL`）。发送队列丢帧时每丢一帧请求
  一个关键帧，会向已拥塞的链路灌成倍突发（关键帧 9 倍大）。
- 重建编码器后 `last_keyframe_at = Instant::now()`：新编码器首帧必是关键帧，不重新
  起算会立刻为待处理请求再发一个大关键帧。

### 发送队列

`Conn` 拆分后 `tx` 是 `unbounded_channel()` ⇒ 有界视频通道的背压被完全旁路，
网络慢时帧无限堆在写队列（实测约 2 秒画面），表现为延迟持续增大而不是丢帧。
`MAX_VIDEO_BACKLOG = 6`（30fps 下 ~200ms，与 `RC_BUFFER_MS` 对齐），**超限只丢
`VideoFrame`**；控制消息（鼠标/键盘/剪贴板）必须入队，否则会丢输入。

### 锁屏 / UAC / 会话

- 这段的监听线程在 `app::secure_watch`（`spawn(capture_start_event, capture_stop_event)`）：
  `UAC_FALLBACK_POLL_INTERVAL` / `SECURE_READY_WAIT_MS` / `SECURE_POLL_INTERVAL_MS`
  三个常量随代码从 `app::run` 搬到那里。下面的结论都是针对这段代码的。
- **`WTSDisconnected` 会话没有合成表面**：退出 RDP 后该会话停止被合成，
  `BitBlt` / `PrintWindow` / `DXGI` 全部拿不到像素。DC 能建出正确尺寸，缺的是**表面**。
  虚拟显示器只加设备、不加表面，解决不了。
  现状：`session_state()` 判 `may_have_surface()`，为假时写一张 GDI 绘制的**状态图**
  进 SHMEM，让控制端看到「为什么没画面」；SYSTEM 子进程改放在**控制台会话**。
  诚实结论：RDP 连接会顶掉控制台会话，只有控制台停在登录界面（无人登录）时才稳定可采。
- **`set_uac_active` 的唯一权威是 main 进程**，`video` 绝不能自己翻 —— 否则会在
  SHMEM 未注册时硬失败并每 3s 刷 `SOS SHMEM not registered`。
- `acquire_uac_capturer()` 返回的宽高**以 SHMEM 帧头为准**，不能用注册时记下的值
  （启动时枚举可能为空，会退化成 1280x720，与真实帧不符 → I420 错位）。
- **安全桌面上必须挂起光标/位置服务**（250ms 定时查），否则每次两条
  `拒绝访问 (os error 5)`。取消订阅**不会**杀服务线程（`repeat` 的循环条件是
  `active()`），所以 `new_cursor()` / `new_pos()` 要用 `OnceLock` 全局唯一 + `SvcGuard`。
  - 判据必须同时包含**会话断开**：`!secure_desktop() && session_state().may_have_surface()`。
    只看 `secure_desktop()` 会漏掉「RDP 断开」那一刻 —— 现场日志里 `input_desktop=
    <open failed 5>` 时 `desktop_changed()` 还没翻成 TRUE，于是每条连接都会先漏 1~2 条
    `Error of mouse_cursor service: 拒绝访问` 才轮到挂起。
  - 轮询周期是 **250ms**（不是 1s）：这个判据只有一次桌面句柄比较 + 一次 WTS 查询，
    而漏判的代价是上游 `repeat` 在失败回调里各记一条 ERROR 并白跑一个 33ms 轮询。
- **断开的会话里采集线程必须停止撞 `BitBlt`**：`Disconnected` 状态下 Windows 不合成
  桌面，重试 25Hz 只会空转 —— 现场日志 10 秒刷了 495 次 `BitBlt: 句柄无效`，还每 5 秒
  重建一次采集器。现在按 1 秒分片小睡（每片 250ms 探测会话是否回来），既不做无效采集，
  也不会把恢复延迟叠加到 2 秒的状态轮询上（`SESSION_CHECK_INTERVAL`）。
  同理「仍无帧 → 重建采集器」这个自愈动作只在 `may_have_surface()` 为真时做。
- 锁屏 / UAC / RDP 断开时 `Display::all()` 为空。`send_login_response` 必须上报
  **占位显示器**（`capture::display::primary_size()`，失败回退 1024x768），否则上游
  `handle_peer_info` 直接报 "No displays" ⇒ 用户连不上「重启后无人登录」的机器，
  而这正是远程协助最常见的场景。`create_shmem` 同理，不能 `Display::all()?` + `bail!`。
- 解锁后采集线程按真实设备名查 QoS，而 `Session` 可能缓存了空串 ⇒ 帧率恒默认值、
  码率上报 0。`Session::qos_display_name()` 优先读 `video::cached_display_name()`。
  缓存读只加锁读字符串，可放进每秒路径；**不能**回退 `display_info()`（数十毫秒）。

### 输入

- 注入直接调 `handle_mouse_simulation_(&me, 1)` / `handle_key_`，**不要**改用
  `handle_mouse_` wrapper：后者多一个 `handle_mouse_show_cursor_`（白板/远程光标渲染），
  在 SYSTEM 子进程里既无订阅者也在画面合成中没用途。`handle_mouse_simulation_` 内部会
  做 `try_change_desktop()`，绕过它 ⇒ 锁屏下打不了字。`conn = 1` 是单控制端模型的常量，
  只影响 `LATEST_PEER_INPUT_CURSOR` 的归属（传 -1 会记录无效 ID）。
- 双进程下 `LATEST_PEER_INPUT_CURSOR` 不共享（SYSTEM 子进程更新的是自己的副本），
  `run_pos` 的 exclude 逻辑永远不生效，控制端会收到自己的光标回显 —— 已知且可接受。

### 连接确认弹窗（默认开启，无开关）

- **语义与上游 `approve-mode=both` 一致：点击是密码的「替代」，不是附加条件。**
  两条路径谁先完成谁生效（`src/server/connection.rs`）：
  * 密码正确（`:3091`）→ 直接放行，不弹窗；
  * 空密码（`:3066` `lr.password.is_empty()`）→ 弹窗，**一个包都不回**；
  * 密码错误（`:3083`）→ 回 `Wrong Password` + **也弹窗**（本机用户仍可同意）。
  所以弹窗出现时，控制端**正停在它自己的密码输入框上** —— 两个 UI 并存，这也是上游
  的行为。**没有开关**：SOS 恒走 both 语义，唯一的例外是下面那条「用户看不见就不弹」。
- 因此 `wait_for_local_approval` 在等弹窗的同时**必须继续读这条连接**：控制端可能
  正在输密码，输对了就要走密码路径并 `cancel_pending()` 把弹窗收掉。
- 上游只在 `approve_mode=Click`（本机没有有效密码）时才回
  `LOGIN_MSG_NO_PASSWORD_ACCESS`（`:3057`）。SOS 恒有密码、走的是 both 语义，
  所以等待期间**不发这条响应** —— 发了会让控制端 `dismissAll` 把密码框换成
  「等待接受」提示，反而破坏了「两个 UI 并存」。
- **拒绝必须沿用上游文案**：`send_close_reason(stream, "")` →
  `Misc.close_reason = "Closed manually by the peer"`。控制端 `check_if_retry()`
  用 `text.contains("manually")` 决定要不要在错误框里给「重试」按钮
  （`src/client.rs:5245`）。换成 "Rejected by the local user" 之类文案，对端会多出
  一个 Retry 按钮，点一下又重新弹窗 ⇒ 把「拒绝」变成可以反复触发的骚扰。具体原因
  （拒绝 / 超时 / 并发忙 / 弹窗建不出来）只记本机日志，不上线。
- 弹窗是 Win32 顶层窗口 + 自己的消息循环（`ui::consent` 的私有 `show`，对外只有
  `confirm`），**必须在 `spawn_blocking` 里跑**，不能占住 tokio worker（`confirm` 已这么做，
  调用方不要直接调 `show`）。
- **界面观感**（实测截图核对过）：清单文件 `res/sos.manifest` 只声明 Common-Controls 6，
  打开视觉样式，按钮/边框走系统主题；字体取 `SPI_GETNONCLIENTMETRICS` 的
  `lfMessageFont`（现代系统上是 Segoe UI 9pt），标题在它基础上加粗放大 4/3 ——
  不要用 `DEFAULT_GUI_FONT`，那是 8pt MS Shell Dlg，弹窗会显旧。
  版式是「标题 + 分隔线 + 标签/取值两列 + 灰色说明 + 右下按钮」，尺寸按
  `GetDC(NULL)` 的 `LOGPIXELSX` 缩放（进程是系统 DPI 感知，窗口不会被逐屏缩放）。
  - 清单里**刻意不写** `dpiAware` / `supportedOS`：前者会改进程 DPI 感知级别，
    影响采集与输入注入依赖的坐标语义；后者会改 `GetVersionEx` 的返回值。
  - 清单必须用**绝对路径**传给 `winres`：rc.exe 在 OUT_DIR 里跑，相对路径会解析成
    OUT_DIR/../res/...，实测报 RC2135。
  - 静态文本要接 `WM_CTLCOLORSTATIC`（0x0138）才能上色：控件按 ID 区分，标签与
    说明用 `COLOR_GRAYTEXT`，取值用 `COLOR_WINDOWTEXT`，否则一律是黑色。
- **同一时刻只允许一个弹窗**：并发请求直接按 `Busy` 拒绝，不排队 —— 排队等于让
  攻击者用重连堆出弹窗风暴，或者把阻塞线程池占满。
- 弹窗建不出来时**不算拒绝**（`Decision::Unavailable` ⇒ `Approval::NoDialog`）：
  退回「只认密码」继续等。弹窗是密码的**替代**路径，不是前置条件 —— 因为窗口创建
  失败就拒绝，等于「系统出个小问题谁也连不上」，而它本来就没打算比密码更严格。
  注意这与「拒绝」是两回事：真拒绝（点拒绝 / 超时 / 并发 Busy）仍然立刻关连接并
  记一次失败。
- **等待期间必须发保活**：控制端发出 `LoginRequest` 后有 30s「无数据」看门狗
  （`src/client/io_loop.rs:337`，与它是否正在显示等待提示无关），所以
  `wait_for_local_approval` 每 `WAIT_REFRESH`（10s）发一个空 `Message` 保活
  （判断依据就是 `next_timeout` 返回 None）。这样窗口才能开满
  `CONSENT_TIMEOUT`（180s，与上游 `LOGIN_GRACE` 对齐），而且控制端的密码框
  一直留在屏幕上。
- **保活包必须是空 `Message`，不能重发那条 LoginResponse**：控制端的
  `wait-remote-accept-nook` 走 `showWaitAcceptDialog`，而它开头就
  `dialogManager.dismissAll()`（`flutter/lib/common/widgets/dialog.dart:1398`）
  —— 每 10s 重发等于让控制端对话框每 10s 被关掉再弹一次。空消息不命中任何分支
  （`src/client/io_loop.rs:2588` 的 `_ => {}`），只在收包处刷新
  `last_recv_time`（`:303`）。上游自己也用空 `Message` 当空包敲对端
  （`src/client.rs:1698`）。
- 保活发送失败 ⇒ 控制端已断开 ⇒ `cancel_pending()` 立刻收窗（`PostMessageW`
  投 `IDCANCEL`），别让用户对着没人的弹窗点「同意」，也别让 `GATE` 被占住。
- **用户看不见就不弹**（`net::connection` 的 `local_user_can_approve`）：判据是
  `!secure_desktop() && session_state().may_have_surface()`，为假时**跳过弹窗**、
  退回「密码即授权」（等价于旧行为）。
  - 为什么不是「弹了等超时」：锁屏 / UAC 期间活动桌面是 Winlogon，窗口只能建在用户
    Default 桌面上 ⇒ 用户看不见也点不到，弹出等于白等 180s 再拒绝，把「有人在场」
    误判成「无人值守」。会话断开（退出 RDP）同理，那一侧没人在看屏幕。
  - 这条判断只看**本机**状态，不看请求方。用户解锁后下一次连接就会正常弹窗。
  - 放在调用方而不是 `ui::consent` 里：弹窗模块只 `use` win32 / std / `hbb_common`，
    不引用本 crate 的其他模块（`ui/consent.rs` 至今如此）。它只负责「弹出窗口并回一个
    决定」，而「本机现在该不该弹」依赖会话/安全桌面状态，那是调用方的事 —— 塞进弹窗
    就得让它依赖 `capture::uac`，从此拖着一整套采集/会话代码。

### CPU / 进程

- **采集线程禁止调用 `librustdesk::platform::windows::is_process_consent_running()`**：
  它实现为 `cmd /C tasklist | findstr consent.exe`，每次建 3 个进程，**实测 205ms**
  ⇒ 每秒给视频链路插一次 200ms 停顿。用 `capture::uac` 的 `CreateToolhelp32Snapshot`
  快照（**4.24ms**，48× 快）。
- 同理 `current_display_size()` 不要用 `Display::all()`（要建 DXGI 工厂，1.47ms 起），
  用 `GetSystemMetrics`（0.00ms）。
- `app::cpu` 用 `GetSystemTimes` 替代上游 PDH（单次内核调用约 1μs，不经过
  PDH / WMI / 任何 provider）。注意 `kernel` 时间**包含 idle**，
  `busy = (kern_d - idle_d) + user_d`，漏减会恒接近 100%。
- `platform/windows.cc` 里 `ImpersonateLoggedOnUser` 之后**必须 `RevertToSelf()`**。
  模拟令牌是**线程级**的，而调用者主线程同时是 tokio worker ⇒ 主线程会永久以
  winlogon 的 SYSTEM 身份运行，这是安全软件 HIPS 里最高等级的告警特征。
- `SetWinEventHook` / 管道 / 单实例都影响这个：早期「两个实例抢同一管道 → 抢输的
  毫秒级退出 → 看门狗立刻重启」造成高频 `CreateProcessWithTokenW` 循环，最终表现为
  `Winmgmt` 高 CPU。单实例互斥体 + 看门狗指数退避（退避实现现在在 `app::watchdog`）就是为了掐断这条链。

### 环境事实

- `scrap::codec_thread_num()` 在 4GB 机器上恒返回 1（`available_memory()/2` 是整数
  GB 除法，1G/2=0）⇒ **不要用它的值做诊断**。
- 线程数由 `recommended_threads()` 决定：`(cpus*3/4).clamp(2, 8)`，只有可用内存低于
  `ENCODER_MIN_FREE_BYTES`（512MB）时才退回 2 线程。内存每个线程只增约 4MB，开销由
  分辨率决定 ⇒ 线程数只看 CPU，不用内存卡。
- Windows 默认定时器粒度 ~15.6ms，`thread::sleep(5ms)` 会睡满一个 tick。帧节奏必须
  `timeBeginPeriod(1)` + **绝对时间轴**（`next_frame_at += spf`），落后超过一帧就重置，
  避免补帧式背靠背发送。

---

## 8. 诊断日志

生产不要开 `--debug`，但要知道怎么读常规字段。

| 字段 | 含义 |
| --- | --- |
| `[VID_FPS] encode_out= sent_kbps= sendq= qos_fps= qos_bitrate= codec= rtt= raw=` | 5s 一次。`encode_out` 低**不等于**有问题（画面静止时 `skip` 很高，属正常）；判据看 `edrop` 和 `rtt`。`rtt` 是滤波后喂 QoS 的值，`raw` 是最近原始样本 |
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
- 不少测试是**实机基准**（编码吞吐、内存、采集延迟），数值随机器变化，
  它们的作用是「同机 A/B 对比」，不要按注释里的绝对数字做断言。
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

## 11. 已知限制（不要当 bug 修）

| 限制 | 原因 |
| --- | --- |
| `WTSDisconnected` 会话黑屏 | 系统不为该会话合成表面，应用层无解；已改为显示状态图 |
| 产物含 WebRTC 栈（+5MB） | 上游把 WebRTC 做成 `Stream` 变体，`drop` 分发到 `close_webrtc()`，链接器无法裁剪。**传输本身已支持**（见 README「接入协议」） |
| 音频 / 端口转发 / 摄像头 / 隐私模式 / 远程打印 / 终端服务 | 有意移除 |
| 4 核机上 VP9 1080p 编不到 30fps | 软件编码物理上限；由 `ENC_LEVEL` 逐级降 `cpu-used`，实测不划算才试 VP8 |
| ~~AMD（AMF）机器上没有硬件 H.264~~ **已修复** | 三层原因叠在一起，缺一层都不行：(1) `hwcodec` 给 AMF 设私有选项 `query_timeout`，**FFmpeg 7.1 已移除该选项** ⇒ `Option not found`；(2) 更关键的是 FFmpeg 侧还要有上游的 `0008-remove-amf-loop-query` 补丁，否则 AMF 的 `QueryOutput` 阻塞轮询会让**试编码超时**（实测 `Encoder h264_amf test attempt 1 returned error: -1`），编码器建得出来但探测判它不可用；(3) `hwcodec` 把「设调优项失败」当致命错误，直接放弃编码器。**上游能用的原因是它自己的 vcpkg overlay 给 FFmpeg 打了这两个补丁**（`res/vcpkg/ffmpeg/patch/0001`、`0008`）。SOS 的做法（全部在 `sos-client/` 内，**上游一行没改**）：`sos-client/vcpkg/ffmpeg/` = 完整 FFmpeg 端口 + 这两个上游补丁（上游 overlay 会 `--disable-swresample`，而 hwcodec 的 `dec.o` 需要 `swr_*`，所以不能直接复用上游 overlay）；`sos-client/vendor/hwcodec/` = 本地副本，把调优项失败降级为警告。实测（Radeon 780M，`tests/hw_vs_sw_bench`，软件侧现已跑**生产** `video::vpx`）：1600x900 VP9 3.43ms → AMF **2.85ms**；1080p 3.93ms → **3.49ms**，`max` 24.3-24.9ms → **9.4-10.8ms**。**旧记录的 9.5/12.0ms 是上游编码器数（它每帧 `flush()`，比生产链路慢一倍多），不能当基准**：8 核机上硬件只快 11%-17%，真正的收益在 4 核机（1080p 软编 24-27ms）与最差帧延迟上。 |
| 硬件编码没有「编码器内部丢帧」 | 没有 `rc_dropframe_thresh` 的等价物 ⇒ `take_dropped_frames()` 恒 0，拥塞保护只剩发送侧 `MAX_VIDEO_BACKLOG`。`dropframe_thresh_for_link()` 对硬件后端是空转。 |
| 硬件编码没有「强制 IDR」 | hwcodec 只暴露 `new/encode/set_bitrate` ⇒ `force_keyframe()` 实现为**重建编码器**（新序列首帧必是 IDR），代价是一次编码器初始化。 |
