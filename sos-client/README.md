# RustDesk SOS 精简受控端

RustDesk 的裁剪版**受控端**（被控方）。仅支持 Windows，单一可执行文件，
托盘交互，可在 UAC 安全桌面上继续操作。

与上游 RustDesk 的差异：

|          | 上游 RustDesk                      | 本项目                                              |
| -------- | ---------------------------------- | --------------------------------------------------- |
| 视频编码 | 平台硬件编码 + VP8/VP9 软件编码    | 自建 VP8/VP9（`libvpx`）+ 硬件 H.264 / H.265（`hwcodec`） |
| 视频链路 | `scrap::Encoder` + `video_service` | 自建 `video` 采集→编码→发送链路                     |
| QoS      | 全局标量                           | **按显示器独立**（每屏独立采集器/编码器/帧率/画质） |
| 界面     | Flutter / Sciter                   | 仅托盘                                              |
| 音频     | 支持                               | **不支持**                                          |
| 端口转发 | 支持                               | **不支持**                                          |
| 部署形态 | 安装为服务                         | 便携单文件，需管理员启动                            |

## 功能

- ✅ 屏幕捕获：DXGI（首选，约 5ms）→ GDI（回退，约 50ms）
- ✅ 编码：VP9 / VP8（`libvpx`）；带硬件编码的构建另可用 **H.264 / H.265**（见下）。
  控制端能解 H.265 时 `Auto` 优先选它（同硬件下比 H.264 省 25-40% 带宽），均可被控制端的编码偏好覆盖
- ✅ 鼠标键盘输入注入
- ✅ 剪贴板同步（文本 + 文件列表）
- ✅ 文件传输
- ✅ 同意受控：每次连接都等本机用户点「同意」（Win32 置顶弹窗，与密码二选一，无开关）
- ✅ UAC 安全桌面：SYSTEM 子进程通过共享内存送帧，可在提权对话框上操作
- ✅ 多显示器：每块屏独立的采集管线与 QoS
- ✅ 编码成本自适应：编不进帧预算时逐级降 `cpu-used`，再不行才试 VP8（实测不划算会自行回退）

## 构建

需要 vcpkg 提供静态 `libvpx`（`VCPKG_ROOT` 指向 vcpkg 安装目录）。

```powershell
cd sos-client

# 发布构建：cargo build --release + 复制到 dist + UPX 压缩 + 校验（推荐）
cargo xtask
```

产物有两个，用途不同：

| 路径                              | 说明                                                          |
| --------------------------------- | ------------------------------------------------------------- |
| `target\release\rustdesk-sos.exe` | cargo 原样输出，**不压缩**，供增量编译使用                    |
| `dist\rustdesk-sos.exe`           | 交付物，UPX 压缩后（带硬件编码实测 **28.05 MB → 8.97 MB，32.0%**）|

默认构建带硬件编码（静态 FFmpeg + 厂商 SDK），所以体积比纯软件版本大一倍多：
纯软件版本（`--no-default-features`）是 **13.60 MB → 4.24 MB（31.2%）**。

体积几乎全部来自上游 `librustdesk`：sos 自己只负责采集/编码/连接，但 `Stream`、
`input_service`、`clipboard` 等都复用上游实现。2026-11 同步上游 master 后产物
涨了约 1 MB（压缩前 9.1 → 13.6 MB），原因是上游把 WebRTC 做成了
`hbb_common::Stream` 的一个变体 —— `Stream::drop` 会分发到 `close_webrtc()`，
链接器因此无法裁剪整条 WebRTC 栈。sos 自身不走 WebRTC，但这段代码不是死代码。

`build.rs` 用 `bindgen` 生成 libvpx 绑定。视频链路**强依赖**它：没有 libvpx 时
`build.rs` 不会置 `sos_vpx`，`src/lib.rs` 里对应的 `compile_error!` 会直接说明原因
（早先那条「回退到上游 `video_service`」的分支其实编译不过，已删除）。

### 硬件编码（H.264 / H.265，默认开启）

软件链路只有 VP8/VP9。想在弱 CPU 机器上跑满 1080p30，可以编进硬件 H.264 或 H.265。

两种硬件编码器在启动时**各自独立探测**（`h264_*` 与 `hevc_*`），选码规则是纯函数
`video::pick_codec`，与上游一致：

| 控制端情况 | 编码格式 |
| --- | --- |
| 能解 H.265，本机有 `hevc_*` | **H.265**（`Auto` 首选） |
| 只能解 H.264（或控制端指定 H.264），本机有 `h264_*` | H.264 |
| 都没有 | VP9（软件） |

H.265 初始化失败会逐级回退 H.265 → H.264 → VP9，不影响出画。「控制端工具栏里能不能选
H.265」取决于 `PeerInfo.encoding.h265`，我们只在本机真的探测到编码器时才声明。

> 硬件 H.264 的部署细节（FFmpeg overlay、AMF 补丁等）见下，H.265 走同一套。

**硬件编码默认开启**（`default = ["hwcodec"]`），所以普通的 `cargo xtask` 出的就是
支持硬件编码的版本：

```powershell
cargo xtask
```

只想出纯软件编码的版本（不需要 FFmpeg 静态库、体积回到 13.6 MB）：

```powershell
cargo build --release --no-default-features
```

**FFmpeg 是静态链进产物的**（`hwcodec/build.rs` 把 `$VCPKG_ROOT/installed/<triplet>/lib/`
下的 `avcodec`/`avutil`/`avformat` 等以静态库形式链接），所以：

- **目标机器不需要安装 FFmpeg**，产物里也没有 ffmpeg 的 DLL 依赖；
- 但**构建机器**上要先装好那份静态库，命令如下（一次即可，装进
  `$VCPKG_ROOT/installed`，正是 `hwcodec/build.rs` 写死的路径）：

  ```powershell
  # 从没有 vcpkg.json 的目录执行（否则会进 manifest 模式，忽略 --classic）
  cd $env:VCPKG_ROOT
  .\vcpkg.exe install "ffmpeg[amf,nvcodec,qsv]:x64-windows-static" `
      --overlay-ports="<repo>\sos-client\vcpkg" --classic
  ```

  overlay 目录里有两个端口：

  - `vcpkg/ffmpeg/` = vcpkg 官方 7.1.2 端口 + 上游那两个 AMF 补丁。
    **不能直接用仓库根的 `res/vcpkg` overlay**：那个端口 `--disable-swresample`，
    而 hwcodec 的 `dec.o`（opus 解码）需要 `swr_*`，链接会报 `LNK2019`。
  - `vcpkg/mfx-dispatch/` = 上游 vendor 的端口副本（含 `fix-missing-cstdint-import.patch`）。
    FFmpeg 的 `amf` 特性依赖 `mfx-dispatch`，而 vcpkg 官方端口会去 GitHub 下
    commit 的 `.patch`，匿名请求返回 HTTP 429；新 MSVC/gcc13 又要求显式 `#include
    <cstdint>`。带这个 overlay 才不会被卡住。两个端口的文件与上游 `res/vcpkg/` 逐字节一致。

运行期行为（不需要任何新参数）：

- 启动时后台线程探测一次可用编码器，**探测完成前一律按「没有硬件」处理**；
- 有可用编码器、且控制端声明 `ability_h264` 时，视频走 H.264 硬件编码；
- 没有硬件、探测失败、或初始化失败 → 自动回落 VP9 软件编码（`[HWENC]` 日志会说明原因）；
- 控制端工具栏里能不能看到 H.264 选项，取决于 `PeerInfo.encoding.h264`，我们只在本机
  真的能用时才声明它。

> **AMD（AMF）也能用**（已在 Radeon 780M 上验证）。这条链路曾经完全不工作，原因有三层，
> 缺一层都不行：
>
> 1. hwcodec 给 AMF 设私有选项 `query_timeout`，而 FFmpeg 7.1 已把它移除；
> 2. 更要紧的是 FFmpeg 侧还缺上游的 `0008-remove-amf-loop-query` 补丁 —— 缺了它 AMF 的
>    `QueryOutput` 阻塞轮询会让**试编码超时**（`Encoder h264_amf test attempt 1 returned
>    error: -1`），编码器建得出来但探测判它不可用；
> 3. hwcodec 把「设调优项失败」当致命错误，直接放弃整个编码器。
>
> 所以上面那条 vcpkg 命令带的 overlay 就是 (1)(2)，`vendor/hwcodec` 的本地副本负责 (3)。
> **上游仓库一行没改**，全部落在 `sos-client/` 内。
>
> NVENC 用 `delay`、QSV 用 `async_depth`，这两个选项在 FFmpeg 7 里还在，不受影响。

同机实测（Radeon 780M，`cargo test --release --test hw_vs_sw_bench`）：

| 分辨率 | VP9 软编 | H.264 AMF | 最大单帧耗时 |
| --- | --- | --- | --- |
| 1600x900 | 9.5 ms | **3.1 ms** | 44 ms → 11.4 ms |
| 1920x1080 | 12.0 ms | **3.8 ms** | 57 ms → 11.6 ms |



### IDE 接入（rust-analyzer）

本仓库里有 **三个互相独立** 的 Cargo 项目：

| 项目                          | 说明                                                          |
| ----------------------------- | ------------------------------------------------------------- |
| `Cargo.toml`                  | 根 workspace（`rustdesk` + `libs/*`）                         |
| `sos-client/Cargo.toml`       | 独立项目，被根 workspace 的 `exclude` 排除，自带 `Cargo.lock` |
| `sos-client/xtask/Cargo.toml` | 又一层独立 workspace（自带 `[workspace]`），`cargo xtask` 用  |

`cargo metadata` 对 `sos-client/Cargo.toml` 报告的 `workspace_root` 就是
`sos-client` 自己，所以三者在 Cargo 眼里完全平等。但 rust-analyzer 默认只做
「自动发现」，被上层 workspace `exclude` 掉的项目容易落在发现结果之外 ——
表现是 `sos-client` 下的文件不属于任何已加载项目（没有补全/跳转，并弹出
「以下文件未包含在任何项目中」）。

仓库根的 `.vscode/settings.json` 用 `rust-analyzer.linkedProjects` 把三个项目
显式列了出来（注意 `.vscode` 在根 `.gitignore` 里，该文件是**本机配置**，
换机器需要重新建）。若不想改配置，把 `sos-client` 作为第二个工作区文件夹
加进 VS Code 也能达到同样效果。

### 为什么不直接 `cargo build --release`

直接构建也能用，但产物是未压缩的 13.57 MB。`cargo xtask` 额外做三件事：
编译 → 复制到 `dist\` → 压缩并校验。

**压缩的是副本，不是 `target\release` 里的原件。** 这一点很关键：cargo 用产物
mtime 判断是否需要重新链接，如果原地改写产物，每次 `cargo build` 都会重新
链接（多花几十秒）。因此 `target\release` 永远保持 cargo 出厂的原始状态，
`dist\` 每次覆盖重来，不存在「压过没有」的判断分支。

`xtask` 是一个零依赖的小 crate（`xtask/`），通过 `.cargo\config.toml` 里的
cargo 别名调用。

### 参数

```powershell
cargo xtask                              # 默认：best + lzma，输出到 dist\
cargo xtask --no-upx                     # 只复制不压缩
cargo xtask --out-dir out                # 换个输出目录
cargo xtask --level fast                 # 压缩等级 best|normal|fast
cargo xtask --no-lzma                    # 不用 LZMA（体积略大、解压稍快）
cargo xtask --upx-path C:\upx\upx.exe    # 指定 upx.exe
cargo xtask -- --offline                 # `--` 之后的参数原样透传给 cargo build
cargo xtask --help                       # 完整帮助
```

### UPX 查找顺序

`--upx-path` → 环境变量 `UPX_PATH` → 本机 `tools\upx\**\upx.exe`
→ `PATH` → 常见安装位置（Program Files / scoop / chocolatey）→ **自动下载**。

**`tools\` 目录不入库**（见 `.gitignore`）。里面的东西都是本机工具、随用随放：
`tools\upx\` 放 UPX 副本（xtask 会自动发现，免去设环境变量），
`tools\diag-wmi.*` 是排查 WMI CPU 占用的现场诊断脚本。两者都不是交付源码，
所以不跟着仓库走。

### 自动补全 UPX

前面都找不到时，xtask 会向 GitHub API 查询 UPX **最新 release**，下载 win64 zip
到 `tools\upx\`，用该 release 的官方 sha256 强制校验后解压使用。之后就一直复用
这份本地副本，不会重复下载。

**版本与哈希都不在本项目里钉死**——这样 UPX 发布安全修复后，下一次构建就能用上，
不需要改代码、也不需要等宿主项目发版。

| 场景            | 做法                                                                        |
| --------------- | --------------------------------------------------------------------------- |
| 正常联网        | 什么都不用做                                                                |
| 内网镜像 / 代理 | `--upx-url <URL>`（仍用 GitHub 公布的 sha256 校验，确保与官方资产字节一致） |
| 完全离线        | `--upx-path` 指定本地 `upx.exe`，或先 `winget install UPX.UPX`              |
| 禁止联网下载    | `--no-upx-fetch` 或环境变量 `SOS_NO_UPX_FETCH=1`                            |
| 不压缩          | `--no-upx`                                                                  |

下载失败或校验不符时，`dist\` 里仍留着一份**可用的未压缩 exe**，只是退出码为 1。

### 三条注意事项

- **签名必须在压缩之后**：UPX 会改写 PE 布局，先签名会被破坏。
- **杀软误报**：UPX 壳 + 远程控制类程序是启发式规则的经典组合，部分 EDR
  可能拦截。部署环境严格时用 `--no-upx` 出未压缩版本。
- 已有运行中的 `rustdesk-sos.exe` 会被 `build.rs` 先 `taskkill` 掉（否则链接失败）。

### 不带 UPX 的普通构建

```powershell
cargo build --release --manifest-path sos-client\Cargo.toml
# 或
cargo xtask --no-upx
```

## 运行

```powershell
# 默认管理服务器
rustdesk-sos.exe

# 指定管理服务器与公钥
rustdesk-sos.exe --manager hbbs.example.com --key <base64-pubkey>

# 指定中继服务器
rustdesk-sos.exe --relay hbbr.example.com

# 固定密码（不传则每次连接断开后自动换新密码）
rustdesk-sos.exe --password mypassword

# 输出详细日志到控制台与 rustdesk-sos.log
rustdesk-sos.exe --debug
```

每个参数都有短选项，等价写法：

```powershell
rustdesk-sos.exe -m hbbs.example.com -r hbbr.example.com -k <base64-pubkey> -p mypassword -d
```

### 参数

| 长选项             | 短选项 | 说明                                                                 |
| ------------------ | ------ | -------------------------------------------------------------------- |
| `--manager <HOST>` | `-m`   | 管理（信令/ID）服务器地址，如 `hbbs.example.com`                     |
| `--relay <HOST>`   | `-r`   | 中继服务器地址，不指定则用管理服务器下发的；本机可逗号分隔配多台后备 |
| `--key <BASE64>`   | `-k`   | 管理服务器公钥，自定义服务器时必填                                   |
| `--password <PWD>` | `-p`   | 固定密码；不传则每次断开后自动换新密码                               |
| `--debug`          | `-d`   | 分配控制台并输出详细日志                                             |
| `--service`        | `-s`   | 内部使用：主进程以 SYSTEM 身份拉起自身时传入                         |

`--debug` 会打开大量诊断日志（信令收包、QoS 采样、分阶段耗时等），
生产环境不要开。

### 中继选择

候选来源优先级：**`--relay` > 管理服务器下发 > 管理服务器端口 +1**。
本地配置优先，与上游 `get_relay_server()` 一致（内网部署或临时切换时，
本机比服务器更清楚该走哪台）。

**多中继是服务器侧的配置概念，不是协议字段的做法。** 要区分清楚：

| 位置                                                    | 形态               |
| ------------------------------------------------------- | ------------------ |
| hbbs 配置（`relay-servers=` / `RELAY=` 环境变量）       | 逗号分隔的**多台** |
| hbbs 内部 `relay_servers: Vec<String>`                  | 列表               |
| hbbs `get_relay_server()` 选一台后下发                  | **单个**主机名     |
| 协议 `relay_server` 字段（PunchHole / RequestRelay 等） | **单个**主机名     |

hbbs 每连接自行选一台：开源版轮询（`ROTATION_RELAY_SERVER`），
Pro 版按 GeoLite2 选地理最近的（见官方文档 _Configure Relay Servers_）。
客户端拿到的永远是单值，**没有在多台之间挑选的余地**。

`--relay` 支持逗号分隔，是为了让**本机**也能配后备（服务器只给一台，
而那一台可能正好挂了，或 hbbs 配置有误）：

```powershell
rustdesk-sos.exe -m hbbs.example.com -r relay1.example.com,relay2.example.com
```

按顺序逐个尝试取首个连通的那台，总耗时以一次 `CONNECT_TIMEOUT` 为预算封顶
（不劣于上游单次尝试）。`RelayResponse.relay_server` 上报的是**实际连上的那台**
—— 该字段会被 hbbs 转发给对端，对端据此决定连哪台，报错就会配不上。
全部失败时错误信息会列出每台的失败原因。

### 同意受控（默认开启，无开关）

**每次连接都要本机用户点一下「同意」**，不需要任何参数。它与输入密码**二选一**、
谁先完成谁生效（与上游 `approve-mode=both` 一致）：

```powershell
rustdesk-sos.exe -m hbbs.example.com
```

- 弹窗显示请求方 ID / 名称 / 平台 / 来源地址 / 请求类型（远程桌面或文件传输），
  以及 180 秒倒计时；超时、点「拒绝」、直接关窗口都算拒绝。
- **弹窗出现时，控制端仍停在它自己的密码输入框上** —— 两个界面并存，这正是上游的行为。
  本端不会回任何会把那个密码框顶掉的响应，所以控制端可以继续输密码；输对了立即
  建立会话（弹窗自动关闭），不必再点「同意」。
- 等待期间每 10 秒给控制端发一个空包保活：控制端发出登录请求后有一个 30 秒
  「无数据」看门狗（`src/client/io_loop.rs`），不发就会被它判超时断开。空包不触发
  任何 UI 变化，密码框照旧留在屏幕上。控制端若在等待期间断开，弹窗会立刻自动关闭。
- 拒绝后连接**立即关闭**，控制端看到的是「Closed manually by the peer」——
  与上游 RustDesk 里点 Cancel 时完全一致，且不会出现「重试」按钮（否则点一下又会
  重新弹窗）。
- 实现是 Win32（`RegisterClassW` + 消息循环 + `SetTimer` 超时），不引入任何 UI 框架；
  消息循环跑在阻塞线程上，不影响会话本身。
- 若弹窗建不出来（无交互桌面 / 资源耗尽 / win32 调用失败），**不算拒绝**：退回
  「只认密码」，控制端照常输密码即可。弹窗是密码的替代路径，缺了它不该把合法用户
  挡在门外。

**锁屏 / UAC 期间不弹窗**：那时活动桌面是 Winlogon，而窗口只能建在用户 Default 桌面
上 —— 用户既看不见也点不到，弹出来只会白等到 180 秒再拒绝，等于把「有人在场」误判成
「无人值守」。所以这两种情况（以及会话已断开，例如退出 RDP）**直接跳过弹窗**，
退回「密码即授权」：用户解锁后下一次连接就会正常弹窗。

由此得到一个符合直觉的组合：

| 机器状态 | 怎么连 |
| --- | --- |
| 已解锁、有人在场 | 点弹窗「同意」**或**输密码，二选一 |
| 锁屏 / UAC / 无人在场 | 只能输密码（不弹窗，不会白等 180s） |

> 这条判断只看**本机**状态（`capture::uac::secure_desktop` + 会话是否还有合成表面），
> 不看请求方，也不受「谁正在连接」影响。

### 接入协议（TCP / 中继 / WebRTC）

控制端能在自己的设置里挑传输方式，本端要能接受它挑的任意一种：

| 传输 | 状态 | 说明 |
| --- | --- | --- |
| TCP 打洞（直连） | 支持 | `net::rendezvous::handle_punch_hole` → `punch_tcp_and_serve`，先 IPv4 再 IPv6 |
| TCP 中继 | 支持 | 打洞不适用（对称 NAT / `force_relay`）或失败时改走 hbbr |
| **WebRTC** | 支持 | `net::webrtc_in`，UDP 上的 DTLS 通道，见下 |
| **UDP 打洞（KCP）** | 支持 | `net::kcp_in`，打通后数据走 KCP，见下 |

四条传输在 **SOS 侧全部默认启用，没有任何开关或配置文件项**：收到哪种请求就接哪种。
不需要在 SOS 上做任何配置，也不受 SOS 自身设置影响（唯一的例外是代理模式不答 WebRTC，
因为 ICE 会绕过代理暴露真实 IP）。

「能不能真用上」取决于两件本机无法控制的事：控制端是否请求该传输（`PunchHole` 里的
`udp_port` / `webrtc_sdp_offer`），以及 hbbs 是否把这些字段转发过来 —— 后者目前不转发，
详见下方「为什么 hbbs 永远不带 udp_port / webrtc_offer」。

#### WebRTC 入站流程

```text
控制端                          本端（sos）                       hbbs
  │  PunchHole{webrtc_sdp_offer}    │                                │
  ├───────────────────────────────►│                                │
  │                                │ WebRTCStream::new(offer)       │
  │                                │  └─ 得到 SDP answer + 本端候选 │
  │                                │ PunchHoleSent{webrtc_sdp_answer}│
  │                                ├───────────────────────────────►│（独立加密 TCP）
  │                                │ IceCandidate（本端候选）        │
  │                                ├───────────────────────────────►│
  │        PunchHoleResponse{answer}                                │
  │◄───────────────────────────────────────────────────────────────┤
  │  IceCandidate（对端候选）       │                                │
  ├───────────────────────────────►│                                │
  │◄══════ DTLS/ICE 打通后即 Stream::WebRTC ═══════════════════════►│
```

打通之后这条连接与 TCP 会话**没有任何区别**（`net::connection::handle` 同一套逻辑），
视频/输入/剪贴板/文件全部照常。，

#### 三条不能省的细节

1. **answer 与 ICE 候选必须走加密通道。** 候选里是本机每块网卡的地址，明文发等于
   自曝内网拓扑。所以本端单独开一条 TCP 连 hbbs 并强制
   `librustdesk::common::secure_tcp_required`；拿不到加密通道就放弃本次候选。
2. **重复的 offer 不能再建 answerer。** 控制端每次打洞重试都重发同一个 offer，而
   `WebRTCStream::new` 对相同 offer 有共享缓存：`take_local_ice_rx()` 每个 pc 只成功
   一次。第二次拿到 `None` 就只回 answer、不再起连接任务 —— 否则两个任务抢读同一条
   data channel，握手互相踩烂。
3. **签名身份里必须绑定 DTLS 指纹。** 本端把自己的 DTLS 证书指纹签进 `IdPk`
   （`net::connection::setup_encrypted_stream`），控制端会拿它和实际协商出的 DTLS 通道
   比对（`dtls_fingerprint_bound`），不一致说明 SDP 途中被换过，它会直接拒绝 —— 这
   正是防止信令服务器/中继替换指纹的机制，不能省。

#### UDP 打洞（KCP）

控制端也可以要求走 UDP：`PunchHole.udp_port > 0` 就是它的意愿表达，此时本端按
`net::kcp_in` 的流程打洞，打通后数据走 KCP（UDP 上的可靠有序传输）：

1. 新建一个直连 hbbs 的 UDP socket，**从它**发出 `PunchHoleSent`；
2. hbbs 对这个包做**源地址观测**，从包的源地址学到本端 NAT 映射，再转告控制端来打这个洞
   —— 所以这条包必须来自打洞用的同一个 socket。走中转的 UDP 信令通道会让 hbbs
   学到另一个端口，洞就白打了；
3. 重发两次（随机 10–30ms）：第一次常落在还没建立的映射上；
4. `connect(peer)` + 双向探针（`librustdesk::common::punch_udp`）打开两侧 NAT；
5. `KcpStream::accept` 等控制端的 KCP SYN，拿到 `Stream` 后交给会话层。

打洞是「能通就快、不通就回退」，所以它失败只记日志，不影响这条连接的其它传输。
同时在飞的打洞数上限为 2（`net::kcp_in::MAX_UDP_PUNCHES`），超限直接放弃而不是排队。

#### 为什么同一内网里测不到 WebRTC / UDP

实测确认过：如果控制端与被控端在**同一个内网**（同公网出口 / 同网段），hbbs 会把这一轮
判定为「本地」，直接回 `PunchHoleResponse.is_local = true`（这个字段只存在于
hbbs→控制端的响应里，被控端根本看不到），**不再把 `PunchHole` 转发给被控端**。

于是被控端日志里只会出现 `FetchLocalAddr`（控制端要内网地址）和 `RequestRelay`
（内网地址连不上就回退中继），**永远不会有 `message type=PunchHole`** —— 而 WebRTC 的
SDP offer 与 UDP 打洞的 `udp_port` 都挂在 `PunchHole` 里，所以这两条传输在
同内网场景下根本不会被触发。判据就是 `handle_punch_hole` 那行入口日志有没有出现，
以及其中的 `udp_port` / `webrtc_offer`。

要验证这两条传输，控制端必须来自**另一个网络**（不同公网 IP），并且控制端设置里
关了「总是使用中继」、开着对应的直连传输。另外本机若是对称 NAT，打洞本身也不会成功
（会正常回退中继）；WebRTC 在这种情况下需要配 TURN 服务器。

#### 为什么 hbbs 永远不带 udp_port / webrtc_offer

实测与源码双向确认：**问题在服务端，不在控制端设置**。截至 `rustdesk-server` master
（2026-11-07 的提交，其 `libs/hbb_common` 为 `69cea8da`），hbbs 做两件事使这两条传输
无法进入被控端：

1. **转发 `PunchHole` 时从零构造消息**，只填三个字段（`src/rendezvous_server.rs`，
   `handle_punch_hole_request` 的 else 分支）：

   ```rust
   msg_out.set_punch_hole(PunchHole {
       socket_addr,
       nat_type: ph.nat_type,
       relay_server,
       ..Default::default()      // udp_port / socket_addr_v6 / force_relay 都没拷
   });
   ```

   服务端源码里 `udp_port` 字符串**出现 0 次** —— 控制端在 `PunchHoleRequest` 里带的
   `udp_port` 虽然协议上有这个字段（`PunchHoleRequest.udp_port = 7`），但不会被转发。
2. **服务端那版协议根本没有 WebRTC 字段**。与本仓库的 `libs/hbb_common` 对比：

   | message | 本机（SOS 用） | hbbs 用（69cea8da） |
   | --- | --- | --- |
   | `PunchHoleRequest` | 有 `webrtc_sdp_offer = 12` | 止于 `switch_code = 11`，无 |
   | `PunchHole` | 有 `webrtc_sdp_offer = 10` | 止于 `controlled_context = 9`，无 |
   | `PunchHoleSent` | 有 `webrtc_sdp_answer = 8` | 止于 `socket_addr_v6 = 7`，无 |
   | `RelayResponse` | 有 `webrtc_sdp_answer = 12` | 止于 `upnp_port = 11`，无 |

   protobuf 解析时未知字段会被丢弃，所以控制端的 offer 到不了被控端，
   被控端的 answer 也回不去 —— 这就是日志里恒定 `udp_port=0 webrtc_offer=0B` 的原因。

（次要事实，与本条无关但容易混淆：控制端的 `enable-udp-punch` / `enable-webrtc` 在
自建服务器下**未设置**时默认关闭 —— `src/common.rs` 的 `get_local_option`，引入于
上游提交 `645a76d43`（udp/ipv6 punch option）与 `ae6af2de4`（Webrtc）。即使把它们
全打开，上面的服务端限制仍然成立。）

**结论：要真正验证这两条传输，必须换一个会转发 `udp_port` 且协议带 WebRTC 字段的
hbbs**（官方 master 目前不支持，需要自己给服务端打补丁或等上游跟进）。本仓库侧的
实现与判据单测是完备的，缺的是服务端这条链路。

顺带记一条服务端行为：`handle_punch_hole_request` 里若出现「一方在内网、另一方不在」
（`peer_is_lan ^ is_lan`）或 `ALWAYS_USE_RELAY`，hbbs 会把 `nat_type` 强改成
`SYMMETRIC` 来强制中继（注释原话 `will force relay`）。

> 这条路径需要上游把 `src/lib.rs` 的 `mod kcp_stream` 公开成 `pub mod`（本项目第 4 处
> 上游补丁，见 AGENTS §1）。`librustdesk::common::punch_udp` 上游本来就是公开的。

另有两条与上游一致的取舍：`force_relay` 单独出现时按 Relay-only 处理（除非 offer 信封
里声明 `ice_policy: all`），而 Relay-only 需要 TURN 服务器（`WebRTCStream::has_turn_server`），
没有就拒答、让控制端走别的传输；代理模式下不答 WebRTC（ICE 会绕过代理暴露真实 IP）。

#### 日志

| 关键字 | 含义 |
| --- | --- |
| `[CONN] WebRTC offer received` | 收到 offer，开始建 answer |
| `[CONN] PunchHoleSent（含 WebRTC answer）已通过加密通道发回` | answer 已送出 |
| `[WEBRTC] ICE connected, handing the session over` | ICE 打通，交给会话层 |
| `[WEBRTC] wait_connected failed` | 没打通（候选/TURN/防火墙问题），控制端会回退 |
| `[WEBRTC] duplicate offer` | 重试命中缓存，正常 |
| `[KCP] PunchHoleSent sent from ...` | UDP 打洞包已从打洞 socket 发出 |
| `[KCP] KCP session established` | KCP 打通，交给会话层 |
| `[KCP] declined a UDP punch` | 并发超限，本次放弃（控制端会回退） |

> **端到端验证需要控制端主动选 WebRTC**：把控制端的传输方式设成 WebRTC 再连一次，
> 日志里应能看到上面第 1、2、3 行依次出现。只有实在连不上时才会回退 TCP/中继。

### 画质 / 帧率是怎么下发的（上游协议，已核对）

控制端**没有独立的「画质调整」消息类型**，全部走 `Message.misc.option`（`OptionMessage`）：

| 字段 | 编号 | 含义 |
| --- | --- | --- |
| `image_quality` | 1 | 枚举 `NotSet=0` / `Low=2` / `Balanced=3` / `Best=4`（**1 被跳过**） |
| `custom_image_quality` | 6 | **UI 百分比 `<< 8`**（不是裸 0-100） |
| `custom_fps` | 11 | 目标帧率上限 |
| `supported_decoding` | 10 | 控制端解码能力与编码偏好（与画质无关，但常同包下发） |

自定义画质的换算（上游 `video_qos.rs` 与 SOS `ratio_of_image_quality` 逐位一致）：

```text
ratio = ((q >> 8) & 0xFFF) * 2 / 100      // 线性，斜率 0.02
50 → 1.0x      80 → 1.6x      100 → 2.0x      2000 → 40x（UI「更多画质」档）
```
三档枚举对应 `Best=1.5x` / `Balanced=0.67x` / `Low=0.5x`。最终码率 = `base_bitrate(w,h) * ratio`
（SOS 的基数与上游不同，见下）。

**发送时机只有两处**：握手（`LoginRequest.option`）与用户改档时的 `Misc.Option`，**不会周期性重发**。
所以两端都必须在握手期就把画质应用一次 —— SOS 曾经漏了这一步，表现为「用户默认档位是
best/low/自定义时，第一次连上仍是 Balanced，动一次画质菜单才生效」。

SOS 与上游的已知差异（都属设计取舍，不是 bug）：

| 项 | 上游 | SOS |
| --- | --- | --- |
| 码率基数 | `base_bitrate` 1080p≈2073kbps | 1080p≈2500kbps，链路充裕时再 ×2 |
| 自定义下限 | 0.2x | 0.1x |
| 拥塞时是否压画质比 | 会（`min_ratio()` 压到 0.6/0.335/0.1） | 不压，只降帧率 |
| 量化器是否随画质联动 | 会（`calc_q_values`） | 不联动，只改目标码率 |

### 单实例

程序持有 `Global\RustDeskSOS_main` 互斥体，重复启动会提示后退出。

## 服务器端

依赖 RustDesk 服务端组件（[rustdesk/rustdesk-server](https://github.com/rustdesk/rustdesk-server)）：

- 信令服务器 `hbbs`（UDP 21116）
- 中继服务器 `hbbr`（TCP 21117）

## 要求

- Windows 10+ x86_64
- 必须以管理员权限运行（否则直接弹窗退出）
- DXGI 捕获需要 DirectX 11 兼容 GPU；远程桌面会话下 DXGI 不可用，自动降级到 GDI

## 代码结构

`src/` 是「库 + 薄二进制」+ 按域分层：`src/lib.rs` 是库根（`pub mod` 七个域），
`src/main.rs` 只剩 GUI 子系统声明和 `#[tokio::main]` → `app::run()`。集成测试用
`use rustdesk_sos::…` 直接链接库，不再用 `#[path]` 把源文件重新编进每个测试二进制。
以前的结构是全部平铺的 `sos_*.rs` + 二进制 only，分层的理由与取舍见
[`AGENTS.md`](AGENTS.md) §2。

```
src/
  lib.rs              库根：pub mod app/capture/video/net/ipc/input/ui + 缺 libvpx 时的 compile_error!
  main.rs             二进制入口：GUI 子系统 + #[tokio::main] → app::run()
  ── 启动 / 进程（app）──
  app.rs              域根 + pub use run::run
  app/run.rs          原 main 主体：CLI、模式分发、主进程装配
  app/console.rs      调试控制台挂接（AttachConsole + CONOUT$ + SetStdHandle）
  app/bootstrap.rs    提权检查、单实例、日志
  app/config.rs       注册表配置、ID/密码
  app/constants.rs    全局常量与 Windows 对象名
  app/system.rs       SYSTEM 子进程入口与令牌提升
  app/tray.rs         托盘
  app/cpu.rs          CPU 占用采样（GetSystemTimes，不用 PDH）
  app/watchdog.rs     SYSTEM 子进程看门狗（WaitForSingleObject + 指数退避）
  app/secure_watch.rs 锁屏 / UAC 安全桌面监听线程（桌面切换 → SHMEM 采集切换）
  ── 捕获（capture）──
  capture.rs          域根 + SYSTEM 子进程的采集线程（写共享内存）
  capture/dxgi.rs     DXGI Desktop Duplication 捕获
  capture/gdi.rs      GDI 捕获（安全桌面自检/降级/状态图）
  capture/display.rs  显示器枚举与几何查询
  capture/uac.rs      UAC 安全桌面 / 会话连接状态检测
  ── 视频（video）──
  video.rs            域根：capture_loop（故意不拆）+ ActiveEncoder/CaptureState + 调优常量，
                      并对五个子模块 pub use（保住 crate::video::xxx 调用点）
  video/link.rs       链路档位与码率预算（LINK_TIER 迟滞、note_link_sample）
  video/hub.rs        订阅表、广播、每显示器刷新/关键帧标志
  video/ladder.rs     编码成本自适应与采集档位降级（MIN_TIER、ENC_LEVEL）
  video/ability.rs    控制端编码能力与当前编码格式（PEER_ABILITY）
  video/displays.rs   显示器枚举、命名缓存与切换消息
  video/vpx.rs        libvpx 封装、编码格式决策、码率/预算纯函数（可单测）
  video/hwenc.rs      硬件 H.264（hwcodec 的 FFmpeg 后端）
  video/qos.rs        按显示器的帧率/画质/码率状态机（纯逻辑可单测）
  ── 网络（net）──
  net.rs              域根
  net/connection.rs   单连接生命周期（鉴权、消息泵、订阅）
  net/rendezvous.rs   信令（UDP 注册、打洞、中继）
  net/relay.rs        中继候选解析与优先级；NAT 判定/中继决策纯函数（可单测）
  net/rtt.rs          RTT min-filter（RttFilter 窗口与可信上限，可单测）
  net/file_transfer.rs  文件传输
  ── 进程间（ipc）──
  ipc.rs              域根
  ipc/pipe.rs         命名管道（Main → System）
  ipc/shmem.rs        共享内存（帧数据通道）
  ── 输入 / 界面 ──
  input.rs            输入事件分派
  ui.rs               域根
  ui/consent.rs       本机「同意受控」弹窗
  bindings/vpx_ffi.h  bindgen 的输入
platform/windows.cc  SYSTEM 令牌提升、桌面切换钩子
.cargo/config.toml  cargo 别名（xtask）
xtask/              发布构建助手（编译 → 复制到 dist → UPX 压缩 → 校验）
tools/              本机工具（不入库）：WMI 诊断脚本、可选的 UPX 副本
dist/               交付物输出目录（不入库）
tests/              编码/捕获/QoS/CPU 的实测与回归测试（36 个测试二进制）
```

模块名就是路径（`video::vpx`、`ipc::shmem`）。域根与目录**同名同级**（`video.rs` +
`video/`），不是 `video/mod.rs`；`capture.rs` / `video.rs` / `input.rs` 这类域根本身也
放实现，不是纯模块清单 —— 但 `video.rs` 现在只留 `capture_loop`（732 行，故意不拆）、
`ActiveEncoder`/`CaptureState` 与全部调优常量，策略搬进了 `video/` 新增的五个子模块。
域根里那组 `pub use ability::*; …` 是刻意保留的再导出，保证 `crate::video::xxx` 与测试
调用点不变，**不要删**。集成测试不要再写指向 `../src/` 的 `#[path]`，理由见
[`AGENTS.md`](AGENTS.md) §9。

`libs/` 与仓库根 `src/`（上游 RustDesk）原则上不改，全部功能改动都在 `sos-client/` 内。
现有 3 处不可避免的偏离（可解析性所需的 `Cargo.toml`/`Cargo.lock`、`.gitignore`、
4 个模块的 `pub mod` 可见性）逐条列在 [`AGENTS.md`](AGENTS.md) §1，**只减不增**。
原本还有第 4 处（`libs/scrap/.../vpxcodec.rs` 的编码基准参数），已改成在测试侧用
`video::vpx::set_rc_override` 控制，上游那行代码已还原 —— 基准要公平不需要动上游。

改代码前请读 [`AGENTS.md`](AGENTS.md)：那里记录了上游依赖的现状（`hbb_common`/`base`
拆分、`Stream: Drop`）、采集与码率控制的实测结论、以及一批「不要凭直觉改回去」的坑。
仓库根的 `AGENTS.md` 属于上游，不要改它。

### 清理 debug 产物（`target/debug` 实测峰值 148.8 GB）

**一次干净构建就占 29 GB**，而 cargo **不回收**历史 hash 产物，重编几轮就翻几倍：

| 量 | 实测值 |
| --- | --- |
| 一次完整构建（含 36 个测试二进制） | **29.1 GB**（deps 24.0 + incremental 3.0 + build 1.5），414 秒 |
| 其中 `.pdb` | **73 个 / 约 24 GB** —— 每个测试二进制都链接了整个 `librustdesk` + WebRTC + FFmpeg 栈，单个 PDB 约 430 MB |
| 最大的单个文件 | `librustdesk.lib` 3.1 GB（MSVC 的 import library 也带 debuginfo）、`liblibrustdesk.rlib` 1.1 GB |
| 重构期间实测峰值 | **148.8 GB / 72336 个文件**：deps 93（pdb 72.6 + rlib 7.8 + exe 6.0 + lib 3.0）+ incremental 50.5（960 个会话目录，同一 crate 有几十个不同 hash）+ build 4.7 |
| 折算 | ≈ **5 代产物**；同一个测试名有 **10 份**历史 PDB 副本 |

根因两条，都不是 bug：

1. **debuginfo 太大**：debug 构建带完整 DWARF，而本项目的依赖树是整个 rustdesk +
   WebRTC + 静态 FFmpeg。每个测试二进制因此都有 ~430 MB 的 PDB。
2. **cargo 只加不减**：`deps/` 与 `incremental/` 都按 hash 累积，旧产物只能靠 `cargo clean`
   回收。改一次 `src/` 就会重链 36 个测试二进制（新一代 ~29 GB），大重构做几十轮就上百 GB。

处理方式：

- **日常**：大重构告一段落后跑一次，**只要 39 秒**就能删掉 debug 产物并保留 release：

  ```powershell
  cargo clean --profile dev --manifest-path d:\code\rustdesk\sos-client\Cargo.toml
  ```

  实测 `Removed 72336 files, 148.8GiB total` / 39s，磁盘剩余 168.7 → 308.7 GB。
  代价是下一次 `cargo test` 要从零编译（实测 414 秒，`-j 4`）。
- **想根治**就把 debuginfo 降级（本项目目前**没有**这么做，因为要牺牲变量级调试）：
  `[profile.dev] debug = 1`（只保留行号表）。PDB 能小一个数量级，backtrace 仍有
  `文件:行号`，但调试器里看不到变量值。是否值得由负责调试的人定。
- 别用 `git clean -xdf` 代替：它会把 `dist/`（交付物）和 `tools/upx/`（UPX 缓存）一起删掉。
