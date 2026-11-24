# 已知限制（不要当 bug 修）

这些是**刻意留下的**行为或无法在应用层解决的问题。看到它们不要当成回归、也不要
顺手「修」掉 —— 每一条后面都写了原因，或在哪次实测里定的。

> 从 [`../AGENTS.md`](../AGENTS.md) §11 拆出来的：那份文件是自动加载的开发指引，
> 有字节预算，而这一套表格体量太大。规则与目录仍在 AGENTS.md。

| 限制 | 原因 |
| --- | --- |
| `WTSDisconnected` 会话黑屏 | 系统不为该会话合成表面，应用层无解；已改为显示状态图 |
| 产物含 WebRTC 栈（+5MB） | 上游把 WebRTC 做成 `Stream` 变体，`drop` 分发到 `close_webrtc()`，链接器无法裁剪。**传输本身已支持**（见 README「接入协议」） |
| 音频 / 端口转发 / 摄像头 / 隐私模式 / 远程打印 / 终端服务 | 有意移除 |
| 4 核机上 VP9 1080p 编不到 30fps | 软件编码物理上限；由 `ENC_LEVEL` 逐级降 `cpu-used`，实测不划算才试 VP8 |
| ~~AMD（AMF）机器上没有硬件 H.264~~ **已修复** | 三层原因叠在一起，缺一层都不行：(1) `hwcodec` 给 AMF 设私有选项 `query_timeout`，**FFmpeg 7.1 已移除该选项** ⇒ `Option not found`；(2) 更关键的是 FFmpeg 侧还要有上游的 `0008-remove-amf-loop-query` 补丁，否则 AMF 的 `QueryOutput` 阻塞轮询会让**试编码超时**（实测 `Encoder h264_amf test attempt 1 returned error: -1`），编码器建得出来但探测判它不可用；(3) `hwcodec` 把「设调优项失败」当致命错误，直接放弃编码器。上游能用是因为它自己的 vcpkg overlay 给 FFmpeg 打了这两个补丁（`res/vcpkg/ffmpeg/patch/0001`、`0008`）。SOS 全部在 `sos-client/` 内、**上游一行没改**：本地 FFmpeg 端口（含这两补丁）+ 本地 `vendor/hwcodec` 把调优项失败降级为警告。实测（Radeon 780M）：8 核机只快 11%-17%，收益主要在 4 核机；`max` 24.9→10.8ms（旧记录 9.5/12.0ms 是上游每帧 `flush()` 的数，不能当基准）。 |
| 硬件编码没有「编码器内部丢帧」 | 没有 `rc_dropframe_thresh` 的等价物 ⇒ `take_dropped_frames()` 恒 0，拥塞保护只剩发送侧 `MAX_VIDEO_BACKLOG`。`dropframe_thresh_for_link()` 对硬件后端是空转。 |
| 硬件编码没有「强制 IDR」 | hwcodec 只暴露 `new/encode/set_bitrate` ⇒ `force_keyframe()` 实现为**重建编码器**（新序列首帧必是 IDR），代价是一次编码器初始化。 |
