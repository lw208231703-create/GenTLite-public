# U3V Gadget 前端整机调试踩坑全记录（2026-10-26/27）

Orange Pi 5 Plus 上把 Linux 板子模拟成 USB3 Vision 相机的完整 bring-up 过程。
最终状态：**grab_u3v 500/500 帧 × 背靠背两连跑全绿**。本文按层归档全部坑位：
症状 → 根因 → 修法。对应 Q&A 见 `docs/00-governance/OPEN-QUESTIONS.md` Q39 补充二~五。

## 最终可用配置快照（复现基线）

- 内核：`linux-image-current-rockchip-rk3588`（6.12.0-current，**不要用 vendor 6.1 BSP**）
- overlay `rockchip-rk3588-custom-peripheral.dtbo`（target-path 用 **别名值** `/usb@fc000000`，
  mainline 无 usbdrd3_0 包裹节点）：
  `dr_mode="peripheral"` + `extcon=<&u2phy0>`(phandle 0xeb) + `maximum-speed="high-speed"`
  + `phys=<&u2phy0_otg>` + `phy-names="usb2-phy"` + 禁用 `/phy@fed80000`(usbdp_phy0)
- 板端：`vgige-cli --back null --front u3v --u3v-payload 1310720`（nohup 起，需 `</dev/null`）
- Windows：Zadig 对 MI_00/01/02 **三个接口逐个**绑 WinUSB
- 宿主分段：**256 KiB**（`stream.rs PAYLOAD_TRANSFER`；512 KiB 是早期稳定基线）

## 一、内核/设备树层

1. **vendor 6.1.115 BSP 内核 dwc3 gadget 恒 ENODEV**：UDC 在、configfs/functionfs 全通，
   enable 时 `dwc3_gadget_start -19`。换 Armbian mainline 6.12 后不再是绑定本身的障碍。
2. **overlay target-path 写法**：mainline dtb 里 dwc3 路径是别名值 `/usb@fc000000`
   （vendor 树才是 `/usbdrd3_0/usb@fc000000`），用错路径 overlay 静默不匹配。
3. **`extcon=<&u2phy0>` + `maximum-speed="high-speed"` + 禁 usbdp 是最后一块拼图**：
   不加时 `__dwc3_gadget_start` 的 ep0out DEPCFG 失败（`failed to enable ep0out`），
   主机枚举没有任何反应；加后 `configured/high-speed`，一次成功。
4. **fusb302 `cannot register tcpm port`**（mainline 上也存在，唯一 deferred 设备）：
   不影响强制 peripheral 模式；Type-C CC 方向/SS lane 换向需要它，属 v2 议题。
5. **调试利器**：`/sys/kernel/debug/usb/fc000000.usb/`（mode/link_state/regdump/DALEPENA/
   每端点 TRB 队列）+ dynamic_debug（`echo '+p' > control`，按 file 过滤，别全开——
   fsnotify 洪水会淹没输出）。

## 二、FunctionFS/ConfigFS 协议层（格式错误连环坑）

6. **FF v2 记录头**：`magic(4) length(4) flags(4)` 之后，**计数字段按 flags 位条件出现**
   （fs_count/hs_count/ss_count/os_count，uapi functionfs.h 原话 "Depending on which
   flags are set, various fields may be missing"）。多发一个计数字 = 被当描述符解析 = EINVAL。
7. **记录必须单次 write**：f_fs.c 每次 write 独立校验 `length == 本次 write 长度`，
   拆写（head 和 body 分开 write）第一个短 write 就 EINVAL。
8. **f_fs.c 白名单拒绝 CS_INTERFACE(0x24)**：只认 HID/CCID/DFU 的类特定描述符，
   U3V Device Info CS 进不了 FF 记录（最初 -22 的真凶），只能留在宿主侧 config blob——
   但标准 GET_DESCRIPTOR 根本不到用户态（见 13），所以发现层要换确认方式（见 18）。
9. **分速 MPS 铁律**：FS bulk=64 / HS bulk=512 / SS bulk=1024。把 SS 用的 1024 放进
   FS 集 → `usb_ep_autoconfig` 失败 → 整个 bind 报 **ENODEV(-19)**（最初 -19 的真凶）。
   端点地址跨速集还必须一致（f_fs.c eps_addrmap 校验）。
10. **FF 字符串记录是 v2 格式**：`str_count lang_count` + 每语言 le16 langid + **NUL 结尾
    UTF-8** 字符串——不是 USB 字符串描述符（后者只用于 ep0 STRING 应答，且要按 wValue
    索引取单个描述符，整表回会拿到空串）。
11. **ep0 是事件流**：`struct usb_functionfs_event` 12 字节，type@8（BIND/UNBIND/ENABLE/
    DISABLE/SETUP=4/SUSPEND/RESUME），**不是裸 8 字节 setup**；每个 SETUP 都必须应答
    （数据或 ZLP，BOS 等未知类型回 ZLP），不应答=枚举卡死。
12. **端点文件 ep1..epN 在 bind（UDC write）时才创建**（ffs_epfiles_create）——先写描述符
    记录+字符串、再 enable、最后才 open ep 文件；顺序反了 ENOENT/ENODEV 连环。
13. **标准 GET_DESCRIPTOR 由 composite.c 应答，永远到不了 ffs 用户态**（ep0 日志零请求，
    但设备名/字符串都正常——来自 ffs strings 记录+框架组合）。宿主读到的 config 是
    usbccgp 按接口过滤的 32 字节版（无 IAD/无 CS）——别指望往里塞 CS。
14. **mount functionfs 两个坑**：① 挂载源是**裸实例名**（`mount -t functionfs vgige ...`，
    带 `ffs.` 前缀 ENOENT）；② **util-linux 会把"CWD 里存在的相对源"规范成绝对路径**
    （strace 实证），守护进程 CWD 恰好有 `vgige/` 目录时源被改写成绝对路径 → 内核按名
    查找 ENOENT。修法：libc::mount 直调 mount(2)。
15. configfs function 目录内不能挂载 functionfs（EPERM）；挂载点用 `/dev/usb-ffs/<名>`。
    清理要按逆序 rmdir（属性文件 rm 不掉，rmdir 组会递归删）。

## 三、Windows/WinUSB 主机层

16. **Zadig 逐接口绑定后，各 MI 是独立设备节点**：`WinUsb_GetAssociatedInterface`
    **一个都看不到**。修法：枚举时收集 MI_00/01/02 全部实例路径，逐個 CreateFile+
    WinUsb_Initialize（`open_device_all`），再按 bInterfaceProtocol 分类管道。
17. **发现层身份确认**：内核组合 config 没有 Device Info CS（坑 8/13），`read_identity`
     fallback：解析 config 按 **EF/05 接口形状**（§3.2）确认 U3V 放行 openable，
    GUID 缺失降级 `vid:pid:serial` 设备 ID，身份由 open 后 SBRM bootstrap 补。
18. **【本机最大坑】1 MiB payload 分段 = 端点必挂**：`PAYLOAD_TRANSFER=1<<20` 时
    WinUSB/xHCI 在**第一个 1MiB piece** 就 halt bulk-IN 端点（设备端 leader 之后立刻
    EPIPE，百分之百复现）；**512 KiB（1<<19）无限稳定**（500/500×2）。确切驱动层机制
    留档待查（疑似 RAW_IO 单请求上限类约束），512 KiB 是经验安全值。
19. **读超时 → AbortPipe+ResetPipe（audit B11）会把端点 reset 传到设备侧 = EPIPE
    mid-write**。流接收的轮询读绝不能对正常帧间停顿（亚秒级）做 abort/reset。
    （曾试图做"挂起复用"让超时的读保持 outstanding——staging 方案引发 n=0/短应答/
    内存错误，整体放弃，流读维持经典 cancel-on-timeout。）
20. RAW_IO 设置时机（采集时 vs open 时）、开关本身都试过——**都不是** 18 的根因；
    RAW_IO 保留在 start 时设置。
21. **Windows 会"拉黑"失败次数过多的 USB 设备/口**（PnP 里消失或"设备未发挥作用"），
    拔插/重启系统才恢复；`Disable-PnpDevice` 软件重置需要提权（未验证）。守护进程
    每次重启 = Windows 看一次"设备换新"，失败积累快，这是当时反复要求拔插的原因。
22. **崩溃的采集进程不留情面**：设备端 SIRM Stream Enable 残留、发射器线程已死，
    下一次采集接到旧字节流/没有发射器。修法：`abort_stream()`（清 SIRM 寄存器+
    内部状态+关继电器）+ **发射器永不退出**（channel 比 relay 会话长寿，断开就等
    下一个使能）。

## 四、进程/线程健壮性

23. **FunctionFS bulk ep 读在 USB 重枚举/复位时瞬时 ESHUTDOWN**：读线程一退就会 drop
    channel sender → GenCP 服务器在断开通道上**空转 100% CPU**（recv_timeout 对
    Disconnected 立即返回）。修法：读循环永不退出，错/空都短sleep重试。
24. `recv_timeout` 的 Timeout 与 Disconnected 在 trait 上**不可区分**：给 None 分支加
    "累计 1000 次退出"的保险会把正常空闲的相机 100 秒后杀掉（实测踩过）。
25. **发射器严禁重发 block**：EPIPE 发生在 piece 中途时重试会把 leader 发两遍，
    宿主立刻 desync。EPIPE 后只能 abort 整条流。
26. **regions 必须按 base 升序存储**（查找=最后一个 base≤addr）：插入 ACQ 区域打乱
    顺序后 PayloadSize 落到 SBRM 判定之外，读出 0。
27. **线程名 /proc/comm 15 字符截断**：`vgige-u3v-emitter` 显示为 `vgige-u3v-emitt`，
    未命名线程显示进程名——看线程列表数人头时要注意。
28. **崩溃进程的发射器死了不会再起**（线程只在 Device::new 建一次）——这是 22 里
    "不死发射器"方案的直接动机。

## 五、调试纪律/工程流程坑（自省）

29. **pgrep -f 会匹配查询命令自身**（ssh 的 bash -c cmdline 含模式串）："守护进程
    活着"的确认曾因此全是假阳性， Waste 了大量排查方向。状态确认用 `pgrep -x` 或
    多手段交叉。
30. **板子网络随时断**：ssh 长命令串一断全乱（pkill 杀了、restart 没跑）。用户规定：
    **单步执行**。守护进程启动用 `nohup ... </dev/null &`（否则 stdin 占着 ssh 通道，
    会话挂到超时）。
31. **同步必须 md5 核验再构建**：tar 管道被断过一次，残件导致板端编译报"本地能过
    板端不能过"的灵异错误。
32. **先污染后诊断是大忌**：多轮失败把 Windows 设备状态/板端流状态都弄脏后，
    每轮测试结论都不可靠（"消失的设备""中间的流"互相纠缠）。教训：发现异常先
    恢复干净基线（重启守护进程/等自愈/换口），再测再下结论。
33. python `open(path,'w')` **打开即截断**，参数校验失败文件也没了——用脚本改文件
    前想清楚；毁过 grab_u3v.rs，靠 `git show HEAD:` 恢复+重放当日小改动。
34. dmesg/dynamic_debug 是免费的时间戳：用户态日志没有时间轴对照时，`dmesg` 的
    内核时间戳 + `/sys/kernel/debug` 端点 TRB 队列是最可靠的"发生了什么"证据链。
35. 每个假设只做**一个变量**的改动再测；一次叠多个"修复"后出问题，只能全部回滚
    重走（本轮 WinUSB staging、RAW_IO 时机、STREAM_POLL 三个改动叠测浪费的轮次）。

## 六、遗留（诚实清单）

- 1 MiB 分段被 halt 的确切驱动机制（RAW_IO 上限？xHCI MaxTransferSize？）——
  已绕开（512 KiB），根因留 Q39 待查。
- fusb302 tcpm deferred（SS/Type-C 方向属 v2）。
- pylon 对虚拟相机验证未做（U-B 收尾项）。
- 板端建议加 systemd 服务自启 vgige-cli。
- Windows 设备软件重置（提权 Disable/Enable-PnpDevice）未验证，当前靠自愈+重枚举。

## 七、Type-C/USB3 专项（2026-10-27 晚，第二层进展）

### 已解决：fusb302 "cannot register tcpm port" 永久 deferral 的根因

- **根因链**（源码+实证）：overlay 把 `dr_mode` 钉成 `"peripheral"` → dwc3 在
  peripheral-only 模式下**不注册 USB role-switch** → fusb302 的
  `tcpm_register_port` 走到 `fwnode_usb_role_switch_get()` 等一个永远不来的
  依赖 → 返回 -EPROBE_DEFER；又因为 boot 时 fw_devlink 报
  "Fixed dependency cycle(s)" 拆过依赖边，重触发路径丢失 → **永久卡死**。
- **修复**：overlay 改 `dr_mode="otg"` + `extcon=<&u2phy0>` + 恢复 usbdp_phy0
  （去掉 maximum-speed 限制）。效果：role-switch 注册 ✓、fusb302 正常绑定 ✓、
  `/sys/class/typec/port0` 出现且 partner 自动识别 ✓、CC 方向 normal ✓、
  角色协商 sink/device ✓。
- **连带收获**：OTG 模式下第二个 dwc3（fc400000，USB-A 3.0 口）同样可做
  gadget（不经过 Type-C 那套），是无 fusb302 依赖的备选 USB3 试验田
  （需 A 公→C 公全功能线）。

### SS 描述符已实现（vgige-front-u3v）

- FF 记录三速集（fs 64 / hs 512 / ss 1024 + 伴生描述符，flags 位条件计数）；
- ep0 按 UDC `current_speed` 应答 SS 设备描述符（bcdUSB 3.2、MPS0 2^9）、
  SS 配置（1024MPS+companions）、BOS（USB2.0-Ext + SS Device Capability）；
- 1 MiB 分段会 halt 端点的教训在 SS 下同样适用，分段保持 512 KiB。

### 遗留：SS 链路不训练（当前卡点）

- 现象：一切就绪后 `current_speed` 仍 high-speed，热插拔无效（排除冷启动时序）。
- 源码级已确认：usbdp phy `mode=UDPHY_MODE_USB`（无 dp-lane-mux 时纯 USB）、
  orientation-switch 已注册且事件到达（orientation=normal）、
  usb3 phy_init→power_on 路径完整（含 LCPLL/CDR 锁定检查）。
- **下一步候选**（按性价比）：
  1. 读 PMA 运行寄存器确认（CMN_LANE_MUX@0x288 / LCPLL@0x350 / 复位@0x38c）
     —— 注意 `/sys/kernel/debug/regmap/*/registers` 对 fast_io regmap 会挂死，
     用 `register` 单寄存器文件，且挂死后可能需要重启板子；
  2. 用 dmesg dynamic_debug 跟一次 dwc3 `phy_power_on` 全程，确认
     `rk_udphy_u3_port_disable(false)`（U3 端口使能位）真的执行了；
  3. 对照 RK 官方 BSP dts 的 usbdp 节点属性（`rockchip,dp-lane-mux`、
     vbus-supply 等）找 mainline dts 缺项；
  4. 备选路线：fc400000（USB-A 口）+ 全功能 A→C 线，绕开 Type-C 全部复杂度。
- **教训**：debugfs regmap 整库 dump 在 fast_io regmap 上会卡死内核读侧，
  已坑过一次（板子疑似挂起需断电重启）。

### 角色抽签问题与修复（OTG 模式的第二个坑）

- **现象**：OTG overlay 后偶发"设备消失/不枚举"，`data_role=host`、`power_role=source`，
  dmesg 出现板上 xhci 枚举 PC 的记录（板子把自己当主机了）。
- **根因**：dtb 连接器默认 `try-power-role="source"`，板子和 PC 都是 DRP 时按
  时序抽签，板子可能赢成 source；PC 的口若支持 UFP 会顺势当设备，链路方向整个反了，
  且每次重启结果不定。
- **修复**：连接器覆盖 `power-role="sink"; data-role="device"`——相机固定为
  UFP/sink，不再抽签。此后枚举稳定（2.0 业务验证 20/20 帧）。
- **遗留**：SS 链路仍不训练。PMA 寄存器读不到（regmap 只有整库 dump 且会挂内核；
  /dev/mem 该段 Bad address；单板 register 文件不存在），下一步候选：
  对照 RK BSP dts 查 usbdp 节点缺项 / 用 BSP 内核对照试验 / fc400000 USB-A 口
  绕开 Type-C（需全功能 A→C 线）。

### BSP dts 对照结果（B1-①，阴性）

vendor dtb 与 mainline dtb 的 usbdp_phy0/fusb302/connector/dwc3 节点逐项对比：
**mainline 什么都不缺**——orientation-switch/mode-switch/svid(0xff01)/sbu-gpios/
port 端点链接全部在位（早前 diff 被截断造成误判，实际完整）。差距为零，
SS 不训练与 DT 无关，问题在运行态 PHY 层。

### A1 上游调研结论

f_fs CS 白名单（HID/CCID/DFU）无上游放宽讨论；本机补丁约 5 行（
`case USB_TYPE_CLASS | 0x01` 对 EF 类放行），但 usb_f_fs 是 in-tree 模块、
armbian headers 不含驱动源码，DKMS 不可行——需要完整内核构建链
（armbian-build 或 apt source），属于基础设施级投入。**决策：先用
ld/pylon 互通实测确认有严格 producer 因此拒认，再立项内核补丁。**

### USB3 不训练的根因定论：线材（2026-10-27）

用户用 M.2 NVMe 硬盘盒+现有 C-to-C 线实测传输仅 30+MB/s——USB 2.0 实际吞吐
水位（USB3 应 300MB/s 起）。证实手中线无 SS 差分对，SS 链路不训练与板端
软件无关。软件侧（fusb302/OTG/角色锁定/SS 三速描述符/BOS）全部就绪，
**只差一根全功能 C-to-C 线**（认准 SS 三叉戟/5-20Gbps/USB4/雷电标识；
纯"快充线"常为 USB2 芯）。线到即测：读 current_speed，super-speed 即跑流。
主机模式 SSD 判别实验因此取消（硬盘盒测试已覆盖）。

### 幽灵调试事件复盘（deploy 基础设施三连坑，2026-10-27/28）

症状：configfs bDeviceClass 写入"不生效"，反复改代码验证均 0x00，疑似内核
configfs 怪癖。**真相是板子一直跑旧二进制**，三层部署管道问题叠加：

1. **GNU tar 默认不覆盖"mtime 更新"的文件**——板子时钟比 PC 快约 9 小时
   （时区差），板上文件 mtime 看起来"更新"，tar 静默跳过 → 源码陈旧；
   解药：tar x 加 `--overwrite`。
2. **cargo 指纹只看 mtime**——tar 带过来的未来/错位 mtime 让 cargo 认为
   无变化，"Finished"但不重编；解药：sync 后 `find -name '*.rs' -exec touch`。
3. **管道掩错**——`cmd | tail && next` 的退出码是 tail 的，deploy 失败
   restart 照跑；解药：显式 rc 判断。

教训：远程"改代码不生效"先验证**跑的到底是不是新二进制**（/proc/PID/exe
md5 对比 + strace 确认写入发生），再怀疑内核。之前"pre-bind 写被 configfs
清零"的猜想**未证实**（实际是旧二进制从未执行新代码）；现改为 bind 后写
的实现在新二进制上验证有效（0xef 落盘）。

### Aravis 互通测试结论（2026-10-28，自环拓扑：板子 A 口 ↔ 板子 Type-C）

- **发现关**：Aravis `_usb_device_to_device_ids` 以**设备描述符类 EF/02/01**
  为门槛（configfs bDeviceClass/SubClass/Protocol 属性，composite 用它们组建设备
  描述符）→ 修复后通过（此前 0/0/0 被拒）。
- **打开关**：Aravis 按 GUID 打开设备；GUID 字符串索引来自控制接口的
  **Device Info CS 描述符**（内核组合 config 中没有，f_fs 白名单所致）→
  guid 为空串 → **匹配到总线上第一个 guid 为空的任意设备**（_hub_）→
  端点全 0 → LIBUSB_ERROR_IO。这是 **A1（CS 描述符）的正式立项证据**：
  开源严格实现精确死在 CS 依赖上。
- 内核服务侧 config 本身完好（lsusb -v：3 接口 EF/05/{0,1,2}，端点
  0x01/0x81/0x82/0x83，MPS 512）。

### 部署脚本第四坑：解包路径错误造成"影子树"（幽灵调试的真终点）

`deploy-board.sh` 的 tar 解包 `-C /root`，而工作区在 `/root/GenTLite-public`——
连续一天所有 deploy 都把新代码同步到了 **/root/{crates,vgige} 影子树**，
真正的工作区只吃到了早期手动单文件同步的内容。文件数校验对此免疫（两边
数量一致），表现为"改代码不生效"的终极幽灵。症状特征：板端源码个别文件
新、个别旧，无规律。解药：解包 `-C $WS`；清理 `rm -rf /root/crates /root/vgige`。
此前归给"tar mtime 跳过/cargo 指纹"的解释都是影子树的次生现象。


### Type-C 正反插降速与 udev 自动恢复（2026-10-30）

> 本文开头的 `high-speed` 配置快照属于早期 USB 2.0 bring-up 阶段。当前 SuperSpeed
> 运行与恢复方式以本节为准。此前的线材问题已经单独验证排除，本节记录的是
> **确认支持 USB 3.x 的线材在正反插切换后仍然降速**的另一类问题。

#### 1. normal 插入变 high-speed：不是线材，也不是 normal 不支持 USB 3.x

- **现象**：`orientation=normal`、`state=configured`，但
  `current_speed=high-speed`。reverse 曾经正常，翻回 normal 后降速。
- **根因**：Linux 6.12 的 `rockchip-usbdp` 在方向变化时更新了 lane/orientation
  状态，却没有重新初始化已经运行的 PHY。原来的 udev 规则又只匹配
  `reverse`，所以 normal 方向没有执行恢复。
- **修复**：重绑 `fc000000.usb` 的 DWC3，让 PHY 按当前方向重新初始化；
  udev 对 `normal` 和 `reverse` 都触发同一恢复脚本。
- **判别方法**：若两个方向从冷启动始终只有 USB 2.0，先查线材和链路；若某个
  方向曾经达到 SuperSpeed、翻插后才降速，优先查方向切换后的 PHY 状态。

#### 2. 写了 `SYSTEMD_WANTS` 不等于服务真的会启动

曾使用：

```udev
ACTION=="change", SUBSYSTEM=="typec", KERNEL=="port0", ATTR{orientation}=="normal", ENV{SYSTEMD_WANTS}="vgige-typec-recover.service"
```

`udevadm info` 能看到 `SYSTEMD_WANTS`，但 `systemctl show` 中
`ExecMainStartTimestamp` 始终为空，journal 也没有启动记录。仅设置
`SYSTEMD_WANTS` 不足以构成可靠恢复机制；设备标签、事件动作和单元状态都要核验，
不能只看规则文件存在或 `Result=success`（未运行过的单元也可能显示 success）。

最终不再依赖 systemd 的 wants 队列，而是由 udev 直接调用幂等脚本；systemd
单元只保留给手动操作。当前规则：

```udev
ACTION=="change", SUBSYSTEM=="typec", KERNEL=="port0", ATTR{orientation}=="normal", RUN+="/root/typec-ss-recover.sh"
ACTION=="change", SUBSYSTEM=="typec", KERNEL=="port0", ATTR{orientation}=="reverse", RUN+="/root/typec-ss-recover.sh"
```

#### 3. 重绑会再次触发事件：恢复必须幂等

- **现象**：每次恢复约 9 秒后再次启动，日志反复出现
  `Starting/Finished vgige-typec-recover.service`，随后进入
  `start-limit-hit`。
- **根因**：DWC3 重绑和服务重启产生新的 udev/systemd 事件，恢复服务被重复排队。
- **错误方向**：只关闭 `StartLimitIntervalSec` 能消除失败状态，却不能消除重复
  启动本身，不能作为根本修复。
- **最终修复**：脚本先检查当前速度，已经 `super-speed` 就立即成功退出，不再
  重绑、不再重启相机服务；udev 直接运行脚本，避免 systemd 启动请求继续排队。

```bash
if [[ $(cat "/sys/class/udc/$UDC/current_speed" 2>/dev/null || true) == super-speed ]]; then
    echo "SuperSpeed is already active; recovery is not needed"
    exit 0
fi
```

#### 4. 恢复后 `state=not attached`：先检查二进制功能集

一次恢复失败并不是 PHY 没有恢复，而是双 producer 构建覆盖了板端
`vgige-cli`，新二进制不含 `back-null`。systemd 反复打印：

```text
--back: `null` is not available in this build
```

因此 gadget 根本没有创建，UDC 显示 `not attached/UNKNOWN`。不同功能组合要保留
独立二进制（如 `/root/vgige-null`、`/root/vgige-gige-u3v`），不要在测试组合后
让 systemd 指向不含所需后端/前端的同名文件。

#### 5. 推荐排查顺序

```bash
# 方向、枚举状态和链路速度
cat /sys/class/typec/port0/orientation
cat /sys/class/udc/fc000000.usb/state
cat /sys/class/udc/fc000000.usb/current_speed

# 相机服务和恢复服务是否真的运行过
systemctl is-active vgige.service
systemctl show vgige-typec-recover.service \
  -p ActiveState -p Result -p ExecMainStartTimestamp
journalctl -u vgige-typec-recover.service -n 30 --no-pager

# udev 属性与规则命中情况
udevadm info --query=property --path=/sys/class/typec/port0
udevadm test --action=change /sys/class/typec/port0
```

手动恢复与验收：

```bash
/root/typec-ss-recover.sh
# 板端应输出 orientation=normal|reverse、state=configured、speed=super-speed
# Windows 在项目根目录运行：
target/release/examples/grab_u3v.exe 1209:7D01:NULL-0001 50
```

最终 normal 实测恢复为 `super-speed`，采集 `50/50` 帧通过；reverse 方向此前也
已完成 SuperSpeed 与采流验证。临时 SSH 密码脚本在测试结束后删除，不写入文档或
仓库。


#### 6. high-speed 自环拓扑还需要文件锁和“按方向去重”

仅判断 `super-speed` 并不足以阻止恢复循环：当 Type-C 通过 A-to-C 线接回板子的
USB-A 口、链路本身只有 `high-speed` 时，每次重绑都会再次触发恢复，速度条件永远
无法满足。直接 udev `RUN` 还可能在恢复窗口内重入，造成 vgige 服务反复停止。

最终脚本同时保留两层保护：

1. `flock /run/vgige-typec-recover.lock`：串行化并发恢复，后触发者等待先触发者完成。
2. `/run/vgige-typec-last-orientation`：记录已经恢复过的方向。相同方向的后续事件
   直接退出，只有方向真正变化或板子重启（`/run` 清空）才再次恢复。

这意味着 high-speed 自环可以保持稳定，不会再周期性断开。若排查时需要强制重试，
先停止相关事件源，再删除 `/run/vgige-typec-last-orientation` 并手动运行恢复脚本；
不要把反复重绑当作恢复手段。


#### 7. ROI 后全零帧：GenICam PayloadSize 与 SIRM 几何不一致

- **现象**：1280×1024 正常，改为 640×512 后 GenICam `PayloadSize` 显示 327680，
  但宿主收到全零帧；小分辨率 fps 也没有提高。
- **证据**：板端帧源首字节为 `01..10`，发射日志却仍使用
  `ts=524288,count=2,f1=262144`，即旧的 1310720 字节几何。
- **根因**：只动态更新了 GenICam `PayloadSize`，SIRM 的
  `SI_REQUIRED_PAYLOAD_SIZE` 仍保留启动时的旧值。宿主按旧几何编程，缓冲区大小与
  分段计划不一致，小帧还会被零填充到旧大小。
- **修复**：`RegisterFile::read` 对 ACQ `PayloadSize` 和 SIRM
  `SI_REQUIRED_PAYLOAD_SIZE` 都调用后端 `payload_size()`，ROI 后重新开流让宿主按
  新几何编程。回归测试同时覆盖 4/8 字节读取和 SIRM 动态读取。
- **结果**：640×512 恢复 2400/2400 帧非零，240 fps 设置实测 238.830 fps；
  1280×1024 大帧经后续 burst/分段优化约 197–208 fps，瓶颈主要是载荷传输而不是帧源调度。

#### 8. 大帧只有 160–170 fps：SS companion 禁止突发 + 分段不优

- `ss_endpoint_companion()` 原来把 `bMaxBurst` 写为 0，导致 SuperSpeed bulk 每轮只允许
  一个包，1280×1024 Mono8 长期只有约 160–170 fps。改为 `bMaxBurst=15`（16 包突发）
  后提升到约 204 fps。
- 分段 A/B：512 KiB 约 204 fps，256 KiB 约 208 fps，128 KiB 降到约 186 fps，
  768 KiB 约 208 fps。最终采用 256 KiB；继续增大分段没有收益，说明瓶颈不只是
  每次传输的固定开销，不能承诺“跑满 USB 3.0 标称带宽”。
- GigE 另有独立问题：CCP 在异常退出后残留占用，且 `connection_count` 会让宿主把
  自己误判为其他连接。修复为单宿主计数 0、心跳超时释放；重复打开流通道仍按规范
  返回 `BUSY`，不再为某个宿主做非标准重开。


#### 9. 只连接未采集就满速发流：把“开通道”误当成“开始采集”

- **现象**：宿主刚连接 GigE，还没点击采集，网络已经有满速数据；点击采集反而失败。
- **根因**：`SCP0` 打开后 relay 立即创建并发送 null 帧，没有等待 `AcquisitionStart`。
- **修复**：null 后端增加采集门控。SCP 只负责打开/关闭通道；`AcquisitionStart=1`
  才产生帧，`AcquisitionStop`/关闭通道复位为停止。front-u3v 的本地 ACQ 写入也要
  同时转发到后端，否则 U3V 会因后端收不到 Start 而超时。
- **验证**：连接后 `FrameCountRelayed` 不增长；Start 后才增长；Stop 后停止。自有
  `stress_virtual_gige.py` 也已显式执行 Start/Stop，避免把模拟器自动发流当成标准行为。


#### 10. 第一次采集 BUSY、第二次成功：继承旧 CCP/SCP 会话

- **现象**：第一次打开并采集返回 `0x80000204`，第二次点击又能成功。
- **根因**：旧客户端关闭后可能遗留非零 `SCP0` 和采集状态。新连接第一次打开流通道时
  撞上旧状态，第二次才完成清理。单纯补 `TriggerMode` 不能覆盖这种残留。
- **修复**：在 CCP 释放或心跳超时时调用 `Relay::close()` 清理旧流通道，而不是在新
  授权时关闭，避免误伤宿主刚打开的新流。同一连接的重复 CCP 刷新不受影响。
- **验证**：手工制造 `SCP0!=0, CCP=0` 的旧会话后，第一次 MVS 打开即收到
  `100/100` 帧；新增 `control_release_closes_stream_but_new_grant_does_not` 回归测试。


#### 11. 大帧性能分析：先看写入/读取耗时，不盲目加并发

- 板端 `strace -f -c -e trace=write -p <pid>`：约 16859 次 write 平均 152 µs，
  说明 gadget 发射调用本身不是主要瓶颈。
- WinUSB 平均每次读取等待约 710 µs，一帧约 7 次读取，正好对应约 188 fps。
  这解释了 1280×1024 大帧长期只能约 160–208 fps，而不是 CPU 生成图像不足。
- 曾实现 payload 阶段 2 个及 8 个 overlapped 预读请求，平均等待均没有下降，
  已完整撤回。说明当前 WinUSB/控制器路径不能靠简单并发读取获得线性收益。
- 另写独立 libusb 接收器绕过 GenTL，500 帧、655360000 字节仍只有约 190 fps，
  说明不是 CTI 接收实现的问题，而是底层 WinUSB/控制器调度路径。
- 目前有效优化仍是 `bMaxBurst=15` 与 256 KiB 分段；大帧实测约 197–208 fps，
  小帧 640×512 可达约 239 fps。不能把结果写成“跑满 USB 3.0 标称带宽”。


#### 12. 对照 UVC 的 100-transfer 模型：不能直接套到 U3V 跨块预读

- `UVC_Camera` 使用 libuvc/libusb 的 WinUSB backend 和 RAW_IO，默认
  `LIBUVC_NUM_TRANSFER_BUFS=100`，启动时全部 submit，回调处理后立即 resubmit；
  其 FT602 设备发送连续的大 UVC payload，和当前 U3V 的 Leader/Payload/Trailer
  块序列不同。
- 新增过仅在实验构建启用的 WinUSB 固定 256 KiB 请求池，完成一个立即补一个，
  并按提交序号交付。深度 1 可完整收到 52 字节 Leader、5 个 256 KiB Payload、
  32 字节 Trailer，证明固定大请求和 short-packet 块边界本身可用。
- 深度 16 预挂后首项连续出现 ZLP，随后 Leader 丢失，256 KiB Payload 被送到
  Leader 窗口（262144 > 1024），接收器按协议正确中止。该实验没有有效 fps
  结果，不能写成“深度异步没有性能收益”，也不能把失败版本保留为默认实现。
- 因此 FT602 UVC 实测的 370 MB/s 不能直接作为 Linux FunctionFS U3V 的保证值。
  若继续沿此方向，应先通过 FunctionFS/DWC3 请求队列与 URB 时序查明多请求下
  ZLP/Leader 丢失的来源，再谈 16/32/64 深度性能；实验代码已完整撤回。
