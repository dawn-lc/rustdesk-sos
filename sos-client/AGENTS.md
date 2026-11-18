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
| **默认不改上游** | 新改动一律落在 `sos-client/`。需要上游行为改变时优先在 SOS 侧包一层或绕开（`sos_dxgi`/`sos_vpx`/`sos_cpu` 都是这么来的）。 |
| **已存在的上游补丁，只减不增** | 分叉早期留下 4 处，见下表。不要新增第 5 处；能让某处退场时优先退场。 |
| **不钉第三方工具版本** | UPX 等外部工具禁止在源码里写死版本号或 sha256（见 §4）。 |
| **生产代码禁 `unwrap()`/`expect()`** | 与根 `AGENTS.md` 一致；锁定中毒等例外同样适用。 |
| **不跑 codegen** | 不要运行 `flutter_rust_bridge_codegen`；SOS 也不用 protobuf codegen（协议类型全部来自 `base` crate）。 |

### 现存的上游补丁（提交前用 `git diff --name-status origin/master...HEAD -- . ':!sos-client'` 核对，应只有这几项）

| 文件 | 改动 | 为什么还在 |
| --- | --- | --- |
| `.gitignore` | 加 `/sos-client/target` | 独立项目有自己的 target |
| `Cargo.toml` / `Cargo.lock` | `exclude` 加 `sos-client`；`tray-icon` 补 `rev` | 可解析性必需（见 §3） |
| `src/lib.rs`、`src/server.rs` | 把 `server` / `clipboard` / `clipboard_service` / `service` 改成 `pub mod` | `sos-client` 直接调 `librustdesk::server::clipboard_service`、`librustdesk::clipboard`、`librustdesk::service` |
| `libs/scrap/src/common/vpxcodec.rs` | `rc_dropframe_thresh = 0`、`rc_buf_sz = target*5` | **只被 `tests/enc_bench.rs` 用到**：它拿上游编码器做基线对比，必须关掉丢帧、放大缓冲，否则基线被丢帧水位污染（见 §7「编码器 A/B 必须控制丢帧水位」）。生产链路走自建 `sos_vpx`，不经过这里。 |
| `src/server/video_service.rs` | 新增 `CUSTOM_CAPTURER_FACTORY` 注入点 | **目前无调用方**（SOS 已全面改用 `sos_video` 自建链路）。属于可退场的候选，退场时连同 §2 的说明一起删。 |

---

## 2. 目录结构

```
sos-client/
├── Cargo.toml            独立项目（被根 workspace exclude），自带 Cargo.lock
├── Cargo.lock
├── build.rs              winres 图标/版本 · cc 编译 windows.cc · bindgen 生成 libvpx 绑定
├── .cargo/config.toml    `cargo xtask` 别名 + 静态 CRT
├── .gitignore            dist/ · xtask/target/ · tools/
├── README.md             面向人的构建/运行说明
├── AGENTS.md             本文件
├── platform/
│   └── windows.cc        SetWinEventHook 桌面切换等待、run-as-system、切换桌面 FFI
├── src/                  全部平铺 + `sos_` 前缀（见下）
│   └── bindings/
│       └── vpx_ffi.h     bindgen 的输入，只声明用到的 libvpx 符号
├── tests/                35 个集成测试二进制；support/ 是共用内容生成器（非测试目标）
├── xtask/                发布构建工具，自带 `[workspace]`，零依赖
├── tools/                【不入库】UPX 缓存 + 一次性现场诊断脚本
├── dist/                 【不入库】交付物（UPX 压缩后）
└── target/               【不入库】cargo 原生产物
```

### `src/` 为什么是平铺的

`tests/*.rs` 用 `#[path = "../src/sos_xxx.rs"] mod sos_xxx;` 单独复用某个源文件做
单测（测试里的 `crate::sos_xxx` 因此也能解析）。**移动文件会连带改 30+ 个测试的
`#[path]`**，所以只在 `main.rs` 顶部维护「模块地图」做逻辑分组，不动文件位置。

### 模块地图（与 `main.rs` 顶部一致）

| 分组 | 模块 | 职责 |
| --- | --- | --- |
| 启动/进程 | `sos_bootstrap` | 日志初始化（主进程 / SYSTEM 子进程各一套文件）、单实例互斥体 |
| | `sos_config` | 注册表配置（`HKLM\SOFTWARE\RustDeskSOS\`）+ CLI 合并 |
| | `sos_constants` | 全局常量、`Global\` 内核对象名、`VERSION` |
| | `sos_system` | SYSTEM 子进程启动与服务循环 |
| | `sos_tray` | 托盘图标与菜单 |
| | `sos_cpu` | `GetSystemTimes` CPU 采样（替代上游 PDH） |
| 捕获 | `sos_capture` | SYSTEM 子进程采集线程（写 SHMEM），安全桌面图像降级 |
| | `sos_dxgi` | 自建 DXGI Desktop Duplication 采集器 |
| | `sos_gdi` | 自建 GDI 采集器（自检 / 逐档降级 / 变化检测 / 状态图） |
| | `sos_display` | 显示器枚举与按名查尺寸 |
| | `sos_uac` | 安全桌面判定、会话连接状态 |
| 视频 | `sos_video` | 每显示器的采集→转换→编码→发送循环 |
| | `sos_vpx` | 自建 libvpx 编码器 + 码率/链路档位/降级策略（纯函数集中在这里，便于单测） |
| | `sos_qos` | RTT / 帧率 / 码率 / 画质比，按显示器聚合 |
| 网络 | `sos_connection` | 会话主循环、能力协商、消息分派、`Conn` 拆半区 |
| | `sos_rendezvous` | 信令服务器交互（注册、PunchHole、RequestRelay） |
| | `sos_relay` | 中继地址解析与多候选挑选 |
| | `sos_file_transfer` | 文件传输作业管理 |
| | `sos_pipe` | 命名管道（主进程 ↔ SYSTEM 子进程） |
| | `sos_shmem` | 共享内存帧通道 |
| 输入 | `sos_input` | 远程鼠标/键盘注入 |

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
3. **测试必须带 `--manifest-path`。**
   `cargo test --manifest-path d:\code\rustdesk\sos-client\Cargo.toml`。
   单纯 `cd` 会被工具/脚本重写回 workspace 根，测到上游去。

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
`sos-client/target` 曾达 44GB，已放进 `files.watcherExclude`。

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
- **WebRTC 被链进二进制**：`Stream` 的一个变体，`drop` 会分发到 `close_webrtc()`，
  链接器无法裁剪整条栈（产物里 200+ 处 WebRTC 串）。SOS 不走 WebRTC，但这段不是
  死代码，改 SOS 去不掉。产物因此从 8.75 MB 涨到 13.57 MB（压缩前）。
- **版本号故意保持 `1.4.9`**（上游已 1.5.3）：`sos_constants::VERSION` 会作为
  `PeerInfo.version` 上报并驱动控制端特性开关，报 1.5.x 会让控制端尝试 sos 未
  实现的 WebRTC 传输。改之前先确认协议面已对齐。
- `libs/hbb_common` 是 git submodule，不要在父仓库里直接改它的内容。

---

## 6. 架构要点

### 双进程

主进程（用户权限，托盘 + 网络 + 全部服务）用 winlogon 令牌把自身以 SYSTEM 身份再
拉起一份 `--service` 实例，后者只负责在安全桌面出现时往共享内存写帧。

- 帧数据走 SHMEM（`sos_shmem`），控制信令走命名管道（`sos_pipe`）。
- **跨会话必须用 `Global\` 内核对象**：`sos_constants` 的事件名带 `Global\` 前缀，
  `sos_shmem::os_id()` 生成 `Global\RustDeskSOS_{name}`，`scoped_name()` 统一取值。
  创建方需**显式启用** `SeCreateGlobalPrivilege`（管理员令牌持有但默认禁用，且
  `AdjustTokenPrivileges` 部分授予也返回成功，必须再查 `GetLastError()==1300`）。
- `create()` 失败会回退会话内命名，此时主进程按 `uses_global_namespace()` 给子进程
  传 `--no-global`，**不能**把它送进别的会话（连不上 SHMEM/事件）。

### 自建视频链路

不使用上游 `video_service`，链路是：`sos_video` 的采集线程 →
`Frame::to(yuvfmt)` → `sos_vpx::SosVpxEncoder` → `Conn::send`。

- `#[cfg(not(sos_vpx))]` 的「回退上游 video_service」分支**编译不过**（`sos_connection`
  无条件引用 `sos_video`/`sos_vpx`），已换成 `compile_error!`。缺 libvpx 时构建
  直接给出明确原因，不要复活那条分支。
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

### QoS

`sos_qos` 替代上游全局标量 `VIDEO_QOS`，按显示器聚合多连接：

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
  且重建也救不回。所以有自建 `sos_dxgi`（实测连续 300 次采集 0 个 InvalidData）。
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
  拿不到设备），必须有**不依赖枚举**的回退：`sos_gdi::primary()` =
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
- `sos_qos` 的 `DEFAULT_FPS` 是 30（不是上游 `VIDEO_QOS` 的初始 15），所以启动即满速，
  不存在「预热期按 15fps 算码率 → 837kbps → 82% 丢帧」那个坑。**改初始值前先想清楚这一点。**
- 变更判定用「码率相对变化 > 8%」而不是「画质比变化 > 0.05」，因为帧率变化也会改码率。

### 链路档位 / QoS 采样

- **延迟样本必须做窗口最小值滤波（min-filter，N=5），不是阈值也不是中值。**
  阈值 600ms 会让 490ms 尖峰照样通过；中值滤波会被「连续 3 个以上尖峰」带跑。
  排队延迟恒为正、真实 RTT 是下界 ⇒ 只有窗口内所有样本都变高才是真拥塞。
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

- **`request_keyframe()` 只置位，等下一帧 `encode()` 才生效**；画面静止时采集器一直
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

- **`WTSDisconnected` 会话没有合成表面**：退出 RDP 后该会话停止被合成，
  `BitBlt` / `PrintWindow` / `DXGI` 全部拿不到像素。DC 能建出正确尺寸，缺的是**表面**。
  虚拟显示器只加设备、不加表面，解决不了。
  现状：`session_state()` 判 `may_have_surface()`，为假时写一张 GDI 绘制的**状态图**
  进 SHMEM，让控制端看到「为什么没画面」；SYSTEM 子进程改放在**控制台会话**。
  诚实结论：RDP 连接会顶掉控制台会话，只有控制台停在登录界面（无人登录）时才稳定可采。
- **`set_uac_active` 的唯一权威是 main 进程**，`sos_video` 绝不能自己翻 —— 否则会在
  SHMEM 未注册时硬失败并每 3s 刷 `SOS SHMEM not registered`。
- `acquire_uac_capturer()` 返回的宽高**以 SHMEM 帧头为准**，不能用注册时记下的值
  （启动时枚举可能为空，会退化成 1280x720，与真实帧不符 → I420 错位）。
- **安全桌面上必须挂起光标/位置服务**（1s 定时查 `secure_desktop()`），否则每秒两条
  `拒绝访问 (os error 5)`。取消订阅**不会**杀服务线程（`repeat` 的循环条件是
  `active()`），所以 `new_cursor()` / `new_pos()` 要用 `OnceLock` 全局唯一 + `SvcGuard`。
- 锁屏 / UAC / RDP 断开时 `Display::all()` 为空。`send_login_response` 必须上报
  **占位显示器**（`sos_display::primary_size()`，失败回退 1024x768），否则上游
  `handle_peer_info` 直接报 "No displays" ⇒ 用户连不上「重启后无人登录」的机器，
  而这正是远程协助最常见的场景。`create_shmem` 同理，不能 `Display::all()?` + `bail!`。
- 解锁后采集线程按真实设备名查 QoS，而 `Session` 可能缓存了空串 ⇒ 帧率恒默认值、
  码率上报 0。`Session::qos_display_name()` 优先读 `sos_video::cached_display_name()`。
  缓存读只加锁读字符串，可放进每秒路径；**不能**回退 `display_info()`（数十毫秒）。

### 输入

- 注入直接调 `handle_mouse_simulation_(&me, 1)` / `handle_key_`，**不要**改用
  `handle_mouse_` wrapper：后者多一个 `handle_mouse_show_cursor_`（白板/远程光标渲染），
  在 SYSTEM 子进程里既无订阅者也在画面合成中没用途。`handle_mouse_simulation_` 内部会
  做 `try_change_desktop()`，绕过它 ⇒ 锁屏下打不了字。`conn = 1` 是单控制端模型的常量，
  只影响 `LATEST_PEER_INPUT_CURSOR` 的归属（传 -1 会记录无效 ID）。
- 双进程下 `LATEST_PEER_INPUT_CURSOR` 不共享（SYSTEM 子进程更新的是自己的副本），
  `run_pos` 的 exclude 逻辑永远不生效，控制端会收到自己的光标回显 —— 已知且可接受。

### CPU / 进程

- **采集线程禁止调用 `librustdesk::platform::windows::is_process_consent_running()`**：
  它实现为 `cmd /C tasklist | findstr consent.exe`，每次建 3 个进程，**实测 205ms**
  ⇒ 每秒给视频链路插一次 200ms 停顿。用 `sos_uac` 的 `CreateToolhelp32Snapshot`
  快照（**4.24ms**，48× 快）。
- 同理 `current_display_size()` 不要用 `Display::all()`（要建 DXGI 工厂，1.47ms 起），
  用 `GetSystemMetrics`（0.00ms）。
- `sos_cpu` 用 `GetSystemTimes` 替代上游 PDH（单次内核调用约 1μs，不经过
  PDH / WMI / 任何 provider）。注意 `kernel` 时间**包含 idle**，
  `busy = (kern_d - idle_d) + user_d`，漏减会恒接近 100%。
- `platform/windows.cc` 里 `ImpersonateLoggedOnUser` 之后**必须 `RevertToSelf()`**。
  模拟令牌是**线程级**的，而调用者主线程同时是 tokio worker ⇒ 主线程会永久以
  winlogon 的 SYSTEM 身份运行，这是安全软件 HIPS 里最高等级的告警特征。
- `SetWinEventHook` / 管道 / 单实例都影响这个：早期「两个实例抢同一管道 → 抢输的
  毫秒级退出 → 看门狗立刻重启」造成高频 `CreateProcessWithTokenW` 循环，最终表现为
  `Winmgmt` 高 CPU。单实例互斥体 + 看门狗指数退避就是为了掐断这条链。

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
- `tests/support/` 放共用内容生成器（在子目录里，不会被当成测试目标）。
- 不少测试是**实机基准**（编码吞吐、内存、采集延迟），数值随机器变化，
  它们的作用是「同机 A/B 对比」，不要按注释里的绝对数字做断言。
- 涉及真实桌面的测试在锁屏 / RDP / 无 DXGI 环境下会走降级路径，属预期。
- 策略集中写成纯函数放在 `sos_vpx`（`choose_codec`、`bitrate_kbps_for`、
  `encoder_behind_budget`、`should_reemit_retained`、`LinkTier` 等），
  单测直接打这些函数，不要为测行为去搭采集器。

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
| 产物含 WebRTC 栈（+5MB） | 上游把 WebRTC 做成 `Stream` 变体，`drop` 分发到 `close_webrtc()`，链接器无法裁剪 |
| UDP 打洞未实现 | 依赖上游私有 `mod kcp_stream`；TCP 打洞 + 中继已够用（控制端 `select_ok` 会选中 TCP 分支） |
| 音频 / 端口转发 / 摄像头 / 隐私模式 / 远程打印 / 终端服务 | 有意移除 |
| 4 核机上 VP9 1080p 编不到 30fps | 软件编码物理上限；由 `ENC_LEVEL` 逐级降 `cpu-used`，实测不划算才试 VP8 |
| 硬编码器（NVENC/QSV/AMF/MFT） | 已评估并否决：MFT 软件 H.264 总 CPU 与 VP9 相同、关键帧大一倍、无丢帧机制；硬件 MFT 需 D3D11 纹理池，生产化成本高 |
