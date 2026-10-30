# GenTLite 压力测试与真实相机验收手册

本文适用于 Windows 主机 + Orange Pi（`192.168.137.55`）环境，覆盖：

1. 无真实相机的板载虚拟 U3V / GigE 压测；
2. 长跑、重复启停和结果记录；
3. 后续真实 U3V / GigE 相机的参数、ROI、曝光和异常恢复验收。

除特别说明外，命令均在仓库根目录的 **Git Bash** 中执行。

## 1. 测试分级与通过标准

建议按顺序执行，前一级通过后再增加时长：

| 级别 | 建议帧数/时长 | 目的 |
|---|---:|---|
| 冒烟 | 50 帧 | 枚举、XML、开流和基本参数链路 |
| 基线 | 300–1000 帧 | 帧完整性、帧率、重复启停 |
| 短压测 | 15 分钟 | 持续传输、EPIPE、丢帧和内存趋势 |
| 长稳 | 2 小时 | 驱动、线程、温度和长时间资源泄漏 |
| 交付前 | 8 小时或更长 | 产品化长跑与异常恢复 |

帧数估算：`帧数 = 预期 fps × 秒数`。虚拟 `null` 后端约 30 fps：

- 15 分钟约 27,000 帧；
- 2 小时约 216,000 帧；
- 8 小时约 864,000 帧。

基本通过标准：

- 所有帧 `incomplete=0`，图像非零，数量达到预期；
- 无进程崩溃、接收线程退出、持续 `EPIPE`、`desync` 或异常日志；
- 帧率符合帧源或相机配置，长时间无明显持续下降；
- 内存无持续增长，板端服务线程和 USB/网络状态正常；
- 参数测试后恢复原值，PayloadSize 与 ROI 联动正确；
- 异常测试必须记录“自动恢复 / 重新打开恢复 / 拔插或重启恢复”，不能只写“恢复”。

## 2. 环境准备

### 2.1 从零开始：第一次部署必须按顺序做

> 原则：**每一步看到预期结果后再做下一步**。不要复制完整章节一次性执行，逐条命令操作。

#### 第 1 步：接好硬件

1. 板子接电并启动 Linux。
2. 网线连接板子和电脑，板子本例地址是 `192.168.137.55`，电脑侧通常是 `192.168.137.1`。
3. 如果马上测试 U3V，再把板子 Type-C 口接到电脑 USB 口。
4. 本阶段不需要接真实相机，`null` 后端会产生测试图案。

#### 第 2 步：准备电脑软件

- Git Bash：后续 Linux 风格命令都在这里执行。
- Rust/Cargo：能执行 `cargo --version`。
- Python 3：虚拟 GigE 脚本需要，能执行 `python --version`。
- Zadig：仅 Windows 使用 U3V 时需要。
- SSH 客户端：Git Bash 中执行 `ssh -V` 能显示版本即可。

#### 第 3 步：打开 Git Bash 并进入项目

```bash
cd /d/code-lib/GenTLite-public
pwd
```

`pwd` 应显示 `/d/code-lib/GenTLite-public`。然后设置板子地址，后面的命令会反复使用：

```bash
BOARD=root@192.168.137.55
BOARD_IP=192.168.137.55
```

注意：变量只在当前 Git Bash 窗口有效。**新开的窗口要重新执行上面三行。**

#### 第 4 步：确认电脑能连到板子

先测试网络：

```bash
ping 192.168.137.55
```

能看到 `时间<1ms` 或类似回复即可；若全部超时，先检查网线、IP 和 Windows 网络共享设置，不要继续部署。

再测试 SSH：

```bash
ssh "$BOARD"
```

第一次连接若询问 `Are you sure you want to continue connecting`，输入 `yes`。随后输入 root 密码；**输入密码时屏幕不会显示字符，这是正常的**。出现板子命令行后输入：

```bash
exit
```

如果显示 `Permission denied`，说明密码或账号不对；如果连接超时，说明网络或 SSH 服务不通。

#### 第 5 步：先在 Windows 构建并运行无硬件测试

```bash
cargo build --workspace --all-targets
```

等待出现 `Finished`，再执行：

```bash
cargo test --workspace
```

最后应是 `0 failed`。有编译错误时先解决错误，不要继续部署。

然后构建抓图工具：

```bash
cargo build --release \
  --example grab_u3v --example configure_u3v \
  --example grab_camera --example param_test --example dump_cti
```

这一步会生成后面的 `target/release/examples/grab_u3v.exe` 等程序。

#### 第 6 步：把最新源码同步到板子并构建

```bash
bash scripts/deploy-board.sh "$BOARD"
```

过程中可能多次询问 root 密码。脚本会做三件事：同步 `crates` 和 `vgige` 源码、检查文件数量、在板子上构建 release 程序。预期看到：

```text
file counts match: ...
Finished `release` profile ...
==> deploy done.
```

**这一步不会重启板端服务。** 仅部署不重启，板子仍可能运行旧程序。

#### 第 7 步：在板子上再跑一遍自动化测试

```bash
ssh "$BOARD" 'cd /root/GenTLite-public && cargo test --workspace'
```

等待全部测试结束，确认每个测试汇总都是 `0 failed`。这一步验证的是 aarch64 Linux 构建和协议测试，不代表 Windows 已经能采图。

#### 第 8 步：重启板端虚拟 U3V 服务

```bash
bash scripts/board-restart.sh "$BOARD"
```

该脚本会停止旧服务、清理旧 gadget、启动新服务，Windows 可能重新枚举 USB。预期最后看到：

```text
daemon processes: 1
VGIGE_READY front=u3v ... down=null ... stream=on
configured
high-speed
```

含义分别是：服务进程存在一个、最新服务启动成功、USB 已配置、当前是 USB 2.0 High-Speed。

如果 `daemon processes: 0`，立即查看日志：

```bash
ssh "$BOARD" 'tail -50 /tmp/vgige.log'
```

### 2.2 第一次使用虚拟 U3V：Windows 驱动与状态检查

#### 第 1 步：用 Zadig 绑定三个接口

1. 打开 Zadig。
2. 在设备列表中找到 `Virtual`；若列表为空，先确认 Type-C 已连接且上一步显示 `configured`。
3. 依次选择 **MI_00、MI_01、MI_02**，把驱动设为 **WinUSB**，点击安装或替换驱动。
4. 三个接口都要处理，只绑 MI_00 会导致控制或流接口不可用。

设备管理器里出现三个 `Virtual` 是正常的：它们分别是控制、事件和流接口，逻辑上仍是一台相机。真实 U3V 相机若原来使用厂商驱动，测试本项目 CTI 前也需切换 WinUSB；后续可按厂商流程恢复。

#### 第 2 步：复核板端服务和 USB 状态

```bash
ssh "$BOARD" 'pgrep -x vgige-cli; cat /sys/class/udc/fc000000.usb/state; cat /sys/class/udc/fc000000.usb/current_speed; head -3 /tmp/vgige.log'
```

预期能看到进程号、`configured`、`high-speed` 和 `VGIGE_READY`。如果 Windows 没有设备，优先检查 Type-C 线、Zadig 和板端日志，不要先改代码。

### 2.3 GigE 网络准备

确保 Windows 与板子/相机网络互通：

```bash
ping 192.168.137.55
```

Windows 防火墙需允许来自板子或真实相机 IP 的 UDP 入站。管理员终端示例：

```bat
netsh advfirewall firewall add rule name="GenTLite-GVSP-137" dir=in action=allow protocol=UDP remoteip=192.168.137.55
```

真实相机时将 `remoteip` 换成相机 IP。测试环境变量：

```bash
export GENTLITE_DISCOVERY_TARGETS=192.168.137.55:3956
export GENTLITE_GVCP_PORT=3956
# 自研相机 fire_test_packet 有已知固件问题时使用固定包长：
export GENTLITE_GEV_PACKET_SIZE=1500
```

## 3. 无真实相机：虚拟 U3V 压测

### 3.1 启动或恢复虚拟 U3V

```bash
bash scripts/board-restart.sh "$BOARD"
```

预期日志：

```text
VGIGE_READY front=u3v ... down=null ... stream=on
configured
high-speed
```

### 3.2 第一次采图：先做 50 帧，再做 1000 帧

#### 第 1 步：确认当前窗口不再占用相机

关闭其他 MVS、pylon、抓图程序。**同一时间只能有一个采集程序打开这台设备**，否则会出现 `ACCESS_DENIED`。

#### 第 2 步：采集 50 帧冒烟

```bash
target/release/examples/grab_u3v.exe 1209:7D01:NULL-0001 50
```

其中 `1209:7D01:NULL-0001` 是虚拟相机 ID，`50` 是帧数。程序会依次枚举、打开设备、读取 XML、启动采集、逐帧检查并停止。

最后必须同时看到类似内容：

```text
frames complete & non-zero: 50/50
GRAB OK: 50/50 frames
```

如果中途报错，先保留完整报错，不要直接增加帧数。

#### 第 3 步：采集 1000 帧并测帧率

先执行：

```bash
target/release/examples/grab_u3v.exe 1209:7D01:NULL-0001 1000
```

预期约半分钟完成，最后为 `GRAB OK: 1000/1000 frames`。需要程序自动计算平均 fps 时，改用下面整段：

```bash
N=1000
start=$(date +%s%3N)
target/release/examples/grab_u3v.exe 1209:7D01:NULL-0001 "$N" || exit $?
end=$(date +%s%3N)
awk -v ms=$((end-start)) -v n="$N" 'BEGIN { printf "elapsed=%.3fs fps=%.3f\n", ms/1000, n*1000/ms }'
```

输出类似 `elapsed=33.540s fps=29.815`。该耗时包含打开、XML 和启动开销，短测时平均 fps 会略低于稳态值，约 30 fps 即符合虚拟帧源配置。

### 3.3 重复启停：确认程序能反复打开和关闭

确认上一轮已经退出后，执行：

```bash
for i in $(seq 1 20); do
  echo "U3V START/STOP $i"
  target/release/examples/grab_u3v.exe 1209:7D01:NULL-0001 50 || exit $?
done
```

它会自动重复 20 次，每次采 50 帧。看到 `U3V START/STOP 1` 到 `20`，且每轮都是 `GRAB OK`，才算通过。不要在另一个终端同时抓图。

### 3.4 15 分钟及长稳

先跑 15 分钟：

```bash
target/release/examples/grab_u3v.exe 1209:7D01:NULL-0001 27000
```

15 分钟通过后，再跑约 2 小时：

```bash
target/release/examples/grab_u3v.exe 1209:7D01:NULL-0001 216000
```

**采集窗口要一直保持打开**，期间不要睡眠电脑、拔 Type-C 线或关闭板端服务。另开一个 Git Bash 窗口，按第 7 节查看日志和资源趋势；新窗口记得重新执行 `BOARD=root@192.168.137.55`。

如果人为按 Ctrl+C 中止，先执行下面的命令恢复干净状态，再重新开始：

```bash
bash scripts/board-restart.sh "$BOARD"
```

## 4. 无真实相机：虚拟 GigE 压测

### 4.1 构建双前端程序

```bash
ssh "$BOARD" 'cd /root/GenTLite-public && cargo build --release --no-default-features --features back-null,front-gige,front-u3v -p vgige-cli'
```

### 4.2 切换到虚拟 GigE

以下操作会停止 U3V 服务并清理 gadget：

```bash
ssh "$BOARD" 'pkill -x vgige-cli 2>/dev/null || true; sleep 1;
cd /sys/kernel/config/usb_gadget/vgige 2>/dev/null && {
  echo "" > UDC 2>/dev/null || true
  rm configs/c.1/* 2>/dev/null || true
  rmdir functions/* strings/0x409 configs/c.1 strings 2>/dev/null || true
  cd /sys/kernel/config/usb_gadget
  rmdir vgige 2>/dev/null || true
}
umount /dev/usb-ffs/vgige 2>/dev/null || true
rm -rf /dev/usb-ffs/vgige
cd /root/GenTLite-public
nohup ./target/release/vgige-cli --back null --front gige --up 192.168.137.55 \
  > /tmp/vgige-gige.log 2>&1 </dev/null &
sleep 3
pgrep -xc vgige-cli
head -2 /tmp/vgige-gige.log'
```

先验证发现：

```bash
GENTLITE_DISCOVERY_TARGETS=192.168.137.55:3956 \
  target/release/examples/dump_cti.exe target/debug/gentl_gige.dll
```

### 4.3 采帧、启停和长跑

`grab_camera` 写死了项目真实相机的寄存器，**不要用于 null 虚拟 GigE**。使用仓库提供的虚拟 GigE 压测脚本：

```bash
# 冒烟 / 基线
python scripts/stress_virtual_gige.py --frames 50
python scripts/stress_virtual_gige.py --frames 1000

# 重复启停
for i in $(seq 1 20); do
  python scripts/stress_virtual_gige.py --frames 50 || exit $?
done

# 约 15 分钟 / 2 小时
python scripts/stress_virtual_gige.py --frames 27000
python scripts/stress_virtual_gige.py --frames 216000
```

脚本只使用 Python 标准库和当前 GenTL DLL，输出总帧数、耗时与平均 fps。

### 4.4 恢复虚拟 U3V

```bash
bash scripts/board-restart.sh "$BOARD"
target/release/examples/grab_u3v.exe 1209:7D01:NULL-0001 50
```

## 5. 真实 U3V 相机验收流程

> ROI/ExposureTime 需要相机 XML 和寄存器真实支持。`null` 后端已实现 Width/Height/OffsetX/OffsetY、ExposureTimeAbs、Gain 和 FrameRate，可用于无真机流程验证；其结果仍不能代替真实相机的电气与固件兼容性验收。

### 5.1 基础链路

1. 相机连接 USB，按需要切换为 WinUSB。
2. 执行 50 帧冒烟，再执行 1000 帧基线：

```bash
# 自动选择第一台真实 U3V 相机
target/release/examples/grab_u3v.exe

# 或指定 GUID / 设备 ID 与帧数
target/release/examples/grab_u3v.exe '<device-id>' 1000
```

### 5.2 ROI、曝光和恢复

```bash
# 自动从相机 XML 解析 Width/Height/OffsetX/OffsetY、ExposureTime、PayloadSize
target/release/examples/configure_u3v.exe '<device-id>'

# 指定 ROI；省略时使用原图的一半宽高
CFG_ROI=720x540 target/release/examples/configure_u3v.exe '<device-id>'
```

工具依次验证：

- 原始几何下采帧；
- 修改 Width、Height、OffsetX、OffsetY 并读回；
- PayloadSize 随 ROI 改变；
- ROI 下采帧；
- ExposureTime 写入与读回；
- 恢复原始 ROI 和曝光后再次采帧。

只有输出 `CONFIGURE OK` 才算通过。若某个品牌节点命名不同，应先修正 XML 解析适配，不能直接判定协议失败。

### 5.3 长跑与启停

将虚拟相机命令中的 ID 换成真实相机 ID，执行第 3.3、3.4 节。帧数按真实相机帧率计算，并关注相机自身是否限速、曝光时间是否大于帧周期。

## 6. 真实 GigE 相机验收流程

### 6.1 发现与基线

```bash
export GENTLITE_DISCOVERY_TARGETS=<相机IP>:3956
export GENTLITE_GEV_PACKET_SIZE=1500

# 查看枚举与 Stream0
target/release/examples/dump_cti.exe target/debug/gentl_gige.dll

# 项目自研/LeadingOE 相机，MAC 可省略以自动选择第一台
target/release/examples/grab_camera.exe '<MAC>' 1000
```

`grab_camera`、`param_test` 目前包含项目自研相机的寄存器地址，不能直接当作所有品牌的通用参数工具。其他品牌应使用 GenApi consumer 或按该相机 XML 适配地址。

### 6.2 参数、ROI 和曝光

适用于当前项目自研相机寄存器表：

```bash
target/release/examples/param_test.exe '<MAC>'
```

覆盖 ExposureTime、Gain、FrameRate、Height/ROI、PayloadSize、OffsetX/OffsetY 及恢复。输出 `PARAM TEST OK` 后，再进行与第 3 节相同级别的长跑。

### 6.3 异常与恢复

在确认可恢复现场后逐项执行，每次只改变一个变量：

1. 收流中拔插网线；
2. 停流后再拔插网线；
3. 结束宿主进程，再重新打开；
4. 重启 `vgige-cli`；
5. 连续执行至少 20 次开流/停流；
6. 修改 ROI 和包长后重复启停。

记录断点、错误码、是否自动恢复以及是否需要拔插或重启。GigE 断线自动重建目前仍有未完成项，不能预先承诺所有场景都能无干预恢复。

## 7. 长跑监控

### 7.1 板端

```bash
# USB、进程和日志
ssh "$BOARD" 'watch -n 5 "pgrep -x vgige-cli; cat /sys/class/udc/fc000000.usb/state 2>/dev/null; cat /sys/class/udc/fc000000.usb/current_speed 2>/dev/null; tail -20 /tmp/vgige.log"'

# GigE 前端日志改用 /tmp/vgige-gige.log
ssh "$BOARD" 'tail -f /tmp/vgige-gige.log'

# CPU、内存和运行时间
ssh "$BOARD" 'watch -n 5 "ps -o pid,comm,%cpu,%mem,rss,etime -p \$(pgrep -x vgige-cli)"'

# 温度（不同 thermal_zone 类型需结合名称判断）
ssh "$BOARD" 'watch -n 5 "for z in /sys/class/thermal/thermal_zone*; do echo -n \"\$z: \"; cat \$z/temp; done"'
```

### 7.2 Windows

PowerShell 中定期检查采集进程：

```powershell
Get-Process grab_u3v,grab_camera -ErrorAction SilentlyContinue |
  Select-Object Name,CPU,WorkingSet64,StartTime
```

建议每 5–15 分钟记录一次：帧数、fps、进程内存、板端内存、CPU、温度、USB 状态和最后 20 行日志。判断泄漏看趋势，不看单点波动。

## 8. 结果记录模板

| 项目 | 内容 |
|---|---|
| 日期 / 测试人 | |
| Git 提交 | |
| Windows / Linux 版本 | |
| 板端二进制与部署时间 | |
| 相机或后端 | null / U3V 型号 / GigE 型号 |
| 接口和速率 | HS / SS / GigE / 其他 |
| 分辨率、格式、包长、ROI | |
| 帧数 / 实际时长 / 平均 fps | |
| 完整帧 / 不完整帧 / 丢帧 | |
| 启停次数与失败次数 | |
| CPU / 内存 / 温度趋势 | |
| ROI / 曝光 / Gain / FrameRate | |
| 异常与恢复方式 | |
| 日志路径 | |
| 结论 | PASS / FAIL / 有条件通过 |

历史参考报告位于 `compliance/`，包括 U3V gadget 15 分钟、GigE 中继和 Linux 2 小时长稳报告。

## 9. 常见故障处理

- **找不到 U3V**：确认 `configured`、WinUSB 三个接口、Type-C 线和板端进程；反复重枚举失败时重新拔插。
- **reverse 插法只有 high-speed**：Linux 6.12 的 USBDP 驱动可能未在方向变化时重新初始化 PHY。板端 udev 规则会自动重绑 DWC3；也可手动执行 `/root/typec-ss-recover.sh`，随后复查 `orientation` 和 `current_speed`。
- **ACCESS_DENIED**：关闭其他 MVS、pylon、抓帧终端；同一设备不要并发打开，必要时重启服务后重试。
- **EPIPE / desync**：保留当前 512 KiB 分段，检查 `/tmp/vgige.log`；不要在流中途重发 block。
- **GigE 能发现但无图**：检查防火墙、`GENTLITE_DISCOVERY_TARGETS`、SCDA 对应本机网卡及固定包长设置。
- **修改代码后行为不变**：重新部署并重启，确认新的 `VGIGE_READY` 和构建时间。
- **`Width_Reg in XML`**：当前 null 已支持 ROI/曝光；若仍出现此错误，优先确认部署的是最新 XML 和二进制，而不是 USB 采帧链路故障。
- **长时间帧率下降**：对照温度、CPU、内存、USB 速度和相机帧周期，区分宿主丢帧与相机端限速。
