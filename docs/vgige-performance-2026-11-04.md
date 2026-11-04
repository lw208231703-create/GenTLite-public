# vgige 性能基准（2026-11-04）

无真机条件下，用 Null 后端 + 真实 U3V/GigE 传输链路做端到端基准。
目标是确认第一轮优化后的 30 fps 余量、各传输路径的上限，以及是否值得继续做
jumbo / 零拷贝。

## 1. 环境与口径

- 板端：Orange Pi 5 Plus，8 核，vendor `6.1.115-rk35xx`。
- U3V：Type-C `super-speed`，`vgige.service`，Null 后端。
- GigE：1 Gbps 链路，板端 MTU `1500`，`vgige-gige.service`，legacy GVSP 1.2。
- 图像：`1280×1024 Mono16`，payload `2,621,440` B。
- 接收端：Windows，自写 GenTL producer。
  - `gentl_gige.dll` 直接走 GVSP/GVCP。
  - `gentl_u3v.dll` 需要 WinUSB；测完已恢复 NI-IMAQdx。
- CPU：板端 `top -b -n 4 -d 1 -p <MainPID>` 的稳态采样，
  百分比按单核计算；括号内为多次采样的范围。
- 每个 U3V 速率点前重启 `vgige.service`，避免上一次 GenTL 进程退出后的
  gadget 状态影响下一次采样。
- 丢帧口径：`EventGetData` 连续取得 N 帧；检查 `INCOMPLETE`、
  `SIZE_FILLED`、FrameID 连续性和 `STREAM_INFO_NUM_UNDERRUN`。

## 2. GigE 结果

请求帧率通过 GVCP 写 `FrameRate=0x0004101C`，每点采集完整帧。

| 请求 fps | 帧数 | 实测 fps | MB/s | incomplete | FrameID 缺口 | 板端 CPU（单核） |
|---:|---:|---:|---:|---:|---:|---:|
| 30 | 300 | 29.74 | 78.0 | 0 | 0 | 11–20% |
| 45 | 600 | 44.67 | 117.1 | 0 | 0 | 18–30% |
| 60 | 600 | 45.04 | 118.1 | 0 | 0 | ~20% |
| 90 | 300 | 44.70 | 117.2 | 0 | 0 | 未单独采样 |
| 120 | 300 | 44.69 | 117.1 | 0 | 0 | 未单独采样 |

结论：

- 1 Gbps 链路的实际上限约 **44.7 fps / 117 MB/s**（payload 加 UDP/GVSP 头后
  已接近线速）。
- 请求 60/90/120 fps 时不会产生 incomplete 或丢块；Null 源被 Relay 环形
  队列反压，实际发送自动降到链路可承受的速率。
- 30 fps 时还有约 3 倍带宽余量；GigE 当前不是 CPU 瓶颈。
- 1500 MTU 下约 1790 包/帧，45 fps 约 8 万包/s。jumbo 能降低包数和 CPU，
  但不能突破 1 Gbps，暂不做默认改动。

## 3. U3V 结果

请求帧率通过 GenTL port 写 `FrameRate`（little-endian）。

| 请求 fps | 帧数 | 实测 fps | MB/s | incomplete | FrameID 缺口 | 板端 CPU（单核） |
|---:|---:|---:|---:|---:|---:|---:|
| 30 | 300 | 30.07 | 78.8 | 0 | 0 | 9–14% |
| 60 | 600 | 60.03 | 157.4 | 0 | 0 | 25–27% |
| 90 | 900 | 89.11 | 233.6 | 0 | 0 | 27–38% |
| 120 | 900 | 95.08 | 249.2 | 0 | 0 | 30–44% |

结论：

- 30/60 fps 能精确跟随请求值，30 fps 余量充足。
- 90 fps 已接近 USB3/FFS 的稳定上限；请求 120 fps 时实际约 **95 fps /
  249 MB/s**，同样是反压而不是丢帧。
- 与之前 Mono8 `186 fps / 244 MB/s` 的结果一致：限制主要来自
  **每秒钟搬运的字节数**，不是帧数或 Null 图案生成。
- 继续提高 U3V 帧率需要 Mono8、缩小 ROI/Height，或优化 FFS/用户态
  搬运；单纯调大 256 KiB piece 的收益需要单独 A/B，当前不是 30 fps 的
  瓶颈。

### U3V + GigE 并发（30 fps）

- NI-IMAQdx U3V 300/300 帧，GenTL GigE 300/300 帧。
- U3V 进程约单核 16–20%，GigE 进程约 10–14%，合计约 30%。
- 两条流都没有 incomplete、FrameID 缺口或 underrun。
- 两个 Null 源仍各自生成一份 payload；并发 30 fps 余量充足，暂不需要
  共享帧源或零拷贝。

## 4. 第一轮优化内容

1. Null 行模板生成
   - 旧路径：每个像素做 64 位除法、取模和条件判断。
   - 新路径：先生成一行 16-bit 模板和一行 8-bit 模板；32 行块号标记行
     复用同一模板；其余行 `extend_from_slice` 复制。
   - 保留 Mono8/Mono16、ROI、移动亮条和帧号标记语义。
2. GigE 发送缓冲复用
   - `GvspSession` 持有一个 `scratch: Vec<u8>`。
   - 每个 leader/payload/trailer 包原地 `clear + extend`，不再每包
     `Vec::with_capacity`。
   - 1500 MTU、45 fps 时约 8 万包/s，去掉了每包的堆分配。
3. U3V `SIZE_FILLED`
   - U3V 没有 GVSP trailer 字节数；接收端现在把实际写入的 payload
     `written` 作为 `FrameMeta.payload_bytes`。
   - 未知/非标准 PixelFormat 也能返回真实 `BUFFER_INFO_SIZE_FILLED`。

## 5. 验证

- `cargo test --workspace --offline`：全绿。
- GenTL GigE Mono16 5/5 帧，与生成图逐字节 0 差异。
- GenTL U3V Mono16 5/5 帧，首帧/末帧与生成图逐字节 0 差异。
- NI GigE 5/5 帧；NI U3V 5/5 帧（恢复 NI-IMAQdx 后复测）。
- 所有基准点 `incomplete=0`、FrameID 连续、`underrun=0`。

## 6. 决策

- 保持 legacy GVSP 1.2 和 1500 MTU 为默认，NI 兼容路径不变。
- 暂不启用 jumbo：当前 1 Gbps 已满速，收益是 CPU 而非帧率，且需要
  交换机/双端 MTU 同时支持。
- 已做安全的 frame ownership 转移和 `FramePayload` 回收池；未做
  `DsCore` 裸指针直通/真零拷贝，留给真机验收后再评估。
- U3V piece 保持 256 KiB；后续可做 512 KiB A/B。
- 达到 1 Gbps/250 MB/s 以上需要改传输假设（Mono8、ROI 或更高链路），
  不是当前单帧 2.6 MB Mono16 的软件调参能突破的。

## 7. 第二轮：去掉整帧克隆 + 可回收 payload

这一轮只做内存安全的优化，不引入裸指针所有权转移。

1. **U3V 前端不再克隆整帧**
   - `FrameSession::send_frame` 改为按值接收 `CanonicalFrame`。
   - GigE 前端同步发完即释放；U3V 前端把整个 frame 直接 move 进 emitter
     队列，不再执行 `payload.clone()`。
   - 原来每帧额外复制 2.6 MB，30 fps 就是约 78 MB/s；120 fps 基准下
     不再产生这部分内存流量。
2. **可回收 `FramePayload`**
   - 新增 `FramePayload`：拥有 `Vec<u8>`，可选绑定一个有界
     `SyncSender<Vec<u8>>`；`Drop` 时清空并归还分配。
   - Null 后端和 GigE 后端各使用 12 个槽位的 payload 池：
     `尝试取回 -> 填充 -> Relay -> 前端发送 -> Drop 归还`。
   - Drop 归还使用 `try_send`，池满或接收端已销毁时直接释放；不会阻塞、
     不会泄漏，也不存在 use-after-free。
3. **保持已有的安全边界**
   - 没有把 `DsCore` 的裸指针/16 MiB 槽位直接交给 Relay。
   - 没有新增 `sendmsg` / `io_uring` / AF_XDP 等平台相关 unsafe 路径。
   - GigE 后端仍保留一次从重组槽到 `CanonicalFrame` 的复制；它是下一步
     真正需要“零拷贝”时才会动的部分。

### 第二轮结果

- NI-IMAQdx U3V 30 fps：300/300 帧，CPU 从约 13.4% 降到约 10%
  （单核，同一采样口径）。
- GenTL U3V 120 fps 请求：900/900 帧，实际 93.7 fps / 245.7 MB/s，
  CPU 37–40%；仍受 USB 字节率上限约束。
- GenTL GigE 45 fps：600/600 帧，44.66 fps / 117.1 MB/s，
  CPU 19–22%；仍是 1 Gbps 线速上限。
- GigE 30 fps：300/300 帧，29.74 fps，CPU 15–16%。
- 所有点位 `incomplete=0`、FrameID 连续、`underrun=0`；
  GenTL U3V 首末帧与生成图 0 差异。

### 当前结论

- 30 fps 目标下 CPU 余量充足；这轮优化主要减少内存流量和分配抖动。
- 帧率没有提升是因为 GigE 已满 1 Gbps、U3V 已接近 USB3/FFS 字节率上限，
  不是软件还没有“跑满”。
- 继续追求更高吞吐应改传输假设（Mono8、ROI、更大链路）或做更底层的
  真零拷贝；在当前 30 fps 目标下继续加 unsafe 复杂度收益不成比例。
