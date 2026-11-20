# U3V 带宽瓶颈定位（2026-11-04）

## 1. 现象

`1280×1024 Mono16`（payload `2,621,440` B）请求 120 fps 时，实际约
**94–100 fps / 245–262 MB/s**，明显低于 USB3 SuperSpeed 的理论
`5 Gbps ≈ 500 MB/s` payload。

## 2. 交叉测量

| 测试 | 帧率 | 吞吐 | 说明 |
|---|---:|---:|---|
| NI-IMAQdx Mono16 | 93.95–97.4 fps | 246–255 MB/s | `.icd` 请求 120 fps，512 KiB transfer |
| 自写 GenTL Mono16 | 93.2–100.1 fps | 244–262 MB/s | 512 KiB transfer，900/1800 帧完整 |
| NI-IMAQdx Mono8 120 | 120.09 fps | 157.4 MB/s | 未碰到上限 |
| NI-IMAQdx Mono8 240 | 189.27 fps | 248.1 MB/s | 再次撞到约 250 MB/s |

关键结论：**上限是字节率，不是帧率、像素格式或 GenTL 解析**。
Mono8 可以跑满 120 fps；继续请求 240 fps 时，又停在约 248 MB/s。

## 3. 排除项

- **板端应用 CPU**：120 fps 时 U3V emitter 线程约 30% 单核，relay 约 5%，
  总进程约 36%，没有跑满。
- **Host 应用 CPU**：NI / GenTL 接收进程约 4–6% 单核，没有跑满。
- **Null 图像生成 / GVSP 解析**：Mono8 240 fps 仍撞同一字节率；
  Null 生成和协议解析都不是限制。
- **主机 producer 实现**：NI-IMAQdx 和自写 GenTL 两套独立接收路径
  都停在约 245–255 MB/s，说明不是某一个 producer 的 bug。
- **USB 链路速率**：板端 `current_speed=super-speed`，`maximum_speed=super-speed`，
  协商为 SuperSpeed。

## 4. dwc3 请求级 trace

在 120 fps、512 KiB transfer 下抓取 `dwc3_ep_queue` /
`dwc3_gadget_giveback`，统计 1002 个 512 KiB 请求：

```text
transfer_ms avg=1.694  min=1.452  max=2.002
gap_ms      avg=0.302  min=0.144  max=0.724
```

- 每个 512 KiB 请求从入队到完成约 **1.7 ms**，对应约 **310 MB/s**
  的“请求传输期”速率。
- 请求完成后到下一次入队还有约 **0.3 ms** 的用户态/调度空档。
- 当前 emitter 是**同步 write**：一次只有一个 512 KiB 请求在途，
  所以总吞吐 = `512 KiB / (1.7 + 0.3) ms ≈ 262 MB/s`。

trace 还显示：

- 512 KiB 请求按 `512K + 32B + 52B` 的 leader/payload/trailer 形态发送；
- 640 KiB 和 1 MiB 请求会触发 EPIPE（首块后 endpoint halt），
  512 KiB 是当前 host/driver 下最大稳定请求（1800/1800 帧通过）。

## 5. 当前最可能的瓶颈

按可能性排序：

1. **USB3 host 控制器/端口/线缆**
   - 当前板子接在 Windows 的 **ASMedia USB 3.20 控制器**
     （`PCI\VEN_1B21&DEV_2426`）根端口上，不是 AMD 原生 xHCI。
   - NI 和自写 GenTL 都走这条链路，因此无法区分。
   - 需要换到 AMD 原生 USB3 口、换一根确认过的 USB3 线复测。
2. **FunctionFS 单请求同步写，没有 pipeline**
   - 每个 512 KiB 请求完成后才提交下一个，0.3 ms 空档无法隐藏。
   - 如果改成 AIO/io_uring 或内核侧多 URB 队列，理论上可把 1.7 ms
     传输期连续起来；但当前传输期本身也只有约 310 MB/s。
3. **dwc3 / RK3588 UDC 或 xHCI 调度**
   - 310 MB/s 只有 5 Gbps 链路理论值的约 62%，可能包含
     burst、TRB、LPM/U1/U2、控制器调度开销。
   - 需要 raw bulk 基准（不经过 U3V/Null）才能把 UDC 和 host 分开。
4. **U1/U2 / USB 电源管理**
   - 把 Windows 电源计划切到“高性能”后，单次 900 帧结果从
     93.95 提升到 96.05 fps（约 2%），不是主因，但会叠加。
   - 已恢复原电源计划。

## 6. 建议的下一步

1. **物理换口**：把板子从 ASMedia 口换到 AMD 原生 USB3 口，重跑
   Mono16 120 fps 和 Mono8 240 fps。
2. **换线**：用确认支持 5 Gbps 的 Type-C 线，排除线缆/接触问题。
3. **raw UDC 基准**：在板端直接向 stream endpoint 写固定块（绕过
   U3V 协议和 Null），分别测同步单请求、2–4 个异步请求。
   - 如果异步能超过 300 MB/s，瓶颈是 FFS 单请求同步写；
   - 如果仍约 250–300 MB/s，瓶颈在 dwc3/xHCI/链路。
4. 如果确认是 FFS 单请求：再评估 AIO/io_uring 双缓冲或内核模块
   多 URB 队列；这部分需要单独做内存安全设计，不能直接塞进当前
   `FileTransfers::send`。
5. 如果换口后能到 350–400 MB/s，则不需要改协议栈，只需记录推荐
   host 端口/线缆。

## 7. 结论

**当前不是“软件没跑满”，而是 U3V bulk-IN 传输路径的上限约
250 MB/s。** 具体是 Host 端口/线缆、FFS 单请求同步写、还是 dwc3/xHCI
调度，需要再做一次换口和 raw 基准才能最终定责。现有代码侧已经确认：

- CPU 不是瓶颈；
- Null 生成和 GenTL 解析不是瓶颈；
- 512 KiB 是当前最大稳定 transfer；
- 250 MB/s 对应 Mono16 约 95 fps、Mono8 约 189 fps。

## 8. 2026-11-20 更新：板端 AIO pipeline 实测

> 旧的 “250 MB/s 上限” 是同步单请求写的上限，不是 USB3 链路上限。

### 8.1 改动

- 板端 `virtual/u3v/src/gadget.rs` 的 stream `FileTransfers` 改用 Linux native AIO（`io_submit` / `io_getevents`）。
- `AIO_DEPTH = 4`，每次仍是 512 KiB；最多 4 笔在途。
- `DeviceTransfers::reset()` 在 host 清除 Stream Enable 时销毁/重建 AIO context，并在新 generation 的第一帧前再确认一次（覆盖 0→1 切换太快、emitter 没观察到 disabled 的情况），避免上一会话的完成事件污染下一次开流。
- 主机侧未改：GenTL producer 仍是 512 KiB、单 pending read。

### 8.2 结果（Orange Pi 5 Plus, dwc3, Windows host, 1280x1024）

| 模式 | 请求 | 实测 | 带宽 |
|---|---:|---:|---:|
| Mono16 | 120 fps | 120.00 fps | ~315 MB/s |
| Mono16 | 160 fps | 160.00 fps | ~419 MB/s |
| Mono8 | 240 fps | 240.00 fps | ~315 MB/s |

- Mono16 160 fps 连续 3 次 4 秒稳定；Mono8 240 fps 连续 2 次 4 秒稳定。
- 板端 U3V emitter 单核约 40%，整机 CPU 约 10%，不是 CPU 瓶颈。
- 190/240 fps Mono16 请求不稳定（190 偶发 0 帧，240 失败）；160 fps Mono16 是当前稳定点。
- 1 MiB 单次 FFS 写仍 EPIPE；1 MiB SIRM + 512 KiB 拆分写不加 AIO 时仍约 96 fps。AIO 的收益来自隐藏同步等待，不是 1 MiB。

### 8.3 结论修正

- 旧结论 “U3V 上限约 250 MB/s” 仅适用于同步 512 KiB 单请求路径。
- 板端 AIO pipeline 把 1280x1024 Mono16 从约 96 fps 提升到 160 fps / 约 419 MB/s，约为 USB3 5 Gbps 理论 payload 的 84%。
- 继续提高需要主机侧多 outstanding read 或更深的板端队列/UDC 调优；Mono8 240 fps 是当前更稳的高帧率配置。
