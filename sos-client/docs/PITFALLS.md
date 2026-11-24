# 已踩实的坑（实测结论）

`sos-client` 在实机上踩出来并验证过的结论，按主题分节。**写代码前先读对应主题**，
这些都不要凭直觉改回去。

> 从 [`../AGENTS.md`](../AGENTS.md) §7 拆出来的：那份文件是自动加载的开发指引，
> 有字节预算，而这一份体量太大。规则与目录仍在 AGENTS.md，这里是「为什么这么写」。

## 采集

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
- GDI 路径 `BitBlt` 固定开销 7-8ms（win32k 往返）；自建 DIB 直写省掉了 `scrap`
  原版的 GetDIBits + 全帧镜像 + 全帧旋转 + 每帧 8MB 分配与 memcmp。

## 硬件编码（H.264/H.265）

- **`hwcodec` 是默认 feature**（`sos-client/Cargo.toml` 的 `default = ["hwcodec"]`）：
  普通 `cargo xtask` 出的就是带硬件编码的版本（约 28.6 MB，UPX 后 9.1 MB）。
  纯软件、不依赖 FFmpeg 静态库的小体积版本用 `--no-default-features`（13.6 MB）。
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
  `available()` 会被 `[VIDEO:FPS]` 这类周期性日志间接调用，所以必须走原子量。
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
## 码率控制（libvpx）

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

## 码率 / 帧率耦合

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

## 链路档位 / QoS 采样

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

## 关键帧 / 静止画面

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

## 发送队列

`Conn` 拆分后 `tx` 是 `unbounded_channel()` ⇒ 有界视频通道的背压被完全旁路，
网络慢时帧无限堆在写队列（实测约 2 秒画面），表现为延迟持续增大而不是丢帧。
`MAX_VIDEO_BACKLOG = 6`（30fps 下 ~200ms，与 `RC_BUFFER_MS` 对齐），**超限只丢
`VideoFrame`**；控制消息（鼠标/键盘/剪贴板）必须入队，否则会丢输入。

## 锁屏 / UAC / 会话

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

## 输入

- 注入直接调 `handle_mouse_simulation_(&me, 1)` / `handle_key_`，**不要**改用
  `handle_mouse_` wrapper：后者多一个 `handle_mouse_show_cursor_`（白板/远程光标渲染），
  在 SYSTEM 子进程里既无订阅者也在画面合成中没用途。`handle_mouse_simulation_` 内部会
  做 `try_change_desktop()`，绕过它 ⇒ 锁屏下打不了字。`conn = 1` 是单控制端模型的常量，
  只影响 `LATEST_PEER_INPUT_CURSOR` 的归属（传 -1 会记录无效 ID）。
- 双进程下 `LATEST_PEER_INPUT_CURSOR` 不共享（SYSTEM 子进程更新的是自己的副本），
  `run_pos` 的 exclude 逻辑永远不生效，控制端会收到自己的光标回显 —— 已知且可接受。

## 连接确认弹窗（默认开启，无开关）

- **语义与上游 `approve-mode=both` 一致：点击是密码的「替代」，不是附加条件。**
  两条路径谁先完成谁生效（`src/server/connection.rs`）：
  * 密码正确（`:3091`）→ 直接放行，不弹窗；
  * 空密码（`:3066` `lr.password.is_empty()`）→ 弹窗，**一个包都不回**；
  * 密码错误（`:3083`）→ 回 `Wrong Password` + **也弹窗**（本机用户仍可同意）。
  所以弹窗出现时，控制端**正停在它自己的密码输入框上** —— 两个 UI 并存，这也是上游
  的行为。**没有开关**：SOS 恒走 both 语义，唯一的例外是下面那条「用户看不见就不弹」。
- 因此 `wait_for_local_approval` 在等弹窗的同时**必须继续读这条连接**：输对了走密码
  路径并 `cancel_pending()` 收窗；**输错了也必须回 `Wrong Password`**（与主循环共用
  `report_wrong_password` —— 漏一处用户就完全无感知）。
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

## CPU / 进程

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

## 环境事实

- `scrap::codec_thread_num()` 在 4GB 机器上恒返回 1（`available_memory()/2` 是整数
  GB 除法，1G/2=0）⇒ **不要用它的值做诊断**。
- 线程数由 `recommended_threads()` 决定：`(cpus*3/4).clamp(2, 8)`，只有可用内存低于
  `ENCODER_MIN_FREE_BYTES`（512MB）时才退回 2 线程。内存每个线程只增约 4MB，开销由
  分辨率决定 ⇒ 线程数只看 CPU，不用内存卡。
- Windows 默认定时器粒度 ~15.6ms，`thread::sleep(5ms)` 会睡满一个 tick。帧节奏必须
  `timeBeginPeriod(1)` + **绝对时间轴**（`next_frame_at += spf`），落后超过一帧就重置，
  避免补帧式背靠背发送。

---
