# GVCP/GigE 线格式事实表（已核对规范，附出处）

> 来源：`notes/GigE-Vision-Specification-3-0-00a.pdf`（AIA/A3）。所有多字节字段**网络字节序（big-endian）**（§15.3 R-137c）。
> 本文件是 gentl-gvcp crate 与 simulator 的共同契约。任何不符以规范原文为准并回改此表。
>
> 2026-11-26 与规范原文（`notes/markdown/gige-vision-3.0.md`）对账修订：
> flag ACK 值、DISCOVERY_ACK 载荷长度、WRITEREG_ACK 的 4 字节 SCD、WRITEMEM_CMD 布局、
> GVSP 3.0 leader body 36B（padding 各 u16），并补 Pending Timeout(0x0958) 行。
> 其余数值未变。

## 命令头（8 字节，§15.1）
| 字节 | 字段 | 说明 |
|---|---|---|
| 0 | key | 固定 0x42 |
| 1 | flag | ACKNOWLEDGE=0x01（§15.1 图例 "bit 0 (msb) on the left ... bit 7 (lsb) on the right"：印刷 bit7 即最低位，值 0x01；Q27 已裁定，实现为 `FLAG_ACK=0x01`），bit0-3（msb 侧）命令特定，其余保留 0 |
| 2-3 | command | u16 BE，命令码 |
| 4-5 | length | u16 BE，**载荷字节数（不含头）** |
| 6-7 | req_id | u16 BE，应用生成，从 1 递增，不得为 0 |

## 应答头（8 字节，§15.2）
| 字节 | 字段 |
|---|---|
| 0-1 | status u16 BE（见状态码表） |
| 2-3 | acknowledge u16 BE = 对应命令码 + 1 |
| 4-5 | length u16 BE（载荷字节数；出错可为 0） |
| 6-7 | ack_id u16 BE = 对应 req_id |

## 命令码（§16 字典）
| 命令 | 值 |
|---|---|
| DISCOVERY_CMD / ACK | 0x0002 / 0x0003 |
| FORCEIP_CMD / ACK | 0x0004 / 0x0005 |
| PACKETRESEND_CMD | 0x0040 |
| READREG_CMD / ACK | 0x0080 / 0x0081 |
| WRITEREG_CMD / ACK | 0x0082 / 0x0083 |
| READMEM_CMD / ACK | 0x0084 / 0x0085 |
| WRITEMEM_CMD / ACK | 0x0086 / 0x0087 |
| PENDING_ACK | 0x0089 |
| ACTION_CMD / ACK | 0x0100 / 0x0101 |

## 状态码（§19）
| 名称 | 值 |
|---|---|
| GEV_STATUS_SUCCESS | 0x0000 |
| GEV_STATUS_NOT_IMPLEMENTED | 0x8001 |
| GEV_STATUS_INVALID_PARAMETER | 0x8002 |
| GEV_STATUS_INVALID_ADDRESS | 0x8003 |
| GEV_STATUS_WRITE_PROTECT | 0x8004 |
| GEV_STATUS_BAD_ALIGNMENT | 0x8005 |
| GEV_STATUS_ACCESS_DENIED | 0x8006 |
| GEV_STATUS_BUSY | 0x8007 |
| GEV_STATUS_PACKET_UNAVAILABLE | 0x800C |
| GEV_STATUS_DATA_OVERRUN | 0x800D |

## DISCOVERY_CMD（§16.1.1）
仅 8 字节头（flag 的 ACK bit 置位），无载荷。发送到 UDP 3956。

## DISCOVERY_ACK（§16.1.2，Figure 16-2）
8 字节应答头 + **248** 字节载荷（Figure 16-2 字段合计；旧稿 232 是漏计 `user_defined_name` 16 字节的笔误）。载荷字段（全部 BE，字符串区定长、可不以 NUL 结尾）：
- u16 spec_version_major, u16 spec_version_minor
- u32 device_mode
- u16 reserved + u16 device_MAC_high（MAC 前 2 字节）
- u32 device_MAC_low
- u32 IP_config_options / u32 IP_config_current（bit31=Persistent_IP, bit30=DHCP, bit29=LLA，见 §36.5）
- u32 reserved×3 → u32 current_IP
- u32 reserved×3 → u32 current_subnet_mask
- u32 reserved×3 → u32 default_gateway
- manufacturer_name[32], model_name[32], device_version[32],
  manufacturer_specific_info[48], serial_number[16], user_defined_name[16]

## READREG_CMD / ACK（§16.3）
- CMD：头 + N×u32 寄存器地址（每个 32 位对齐）
- ACK：头 + N×u32 寄存器值；length 必须是 4 的倍数；出错时 length=0

## WRITEREG_CMD / ACK（§16.4）
- CMD：头 + N×(u32 地址 + u32 值)，length = N×8
- ACK（Figure 16-8 / R-174c）：status ≠ ACCESS_DENIED 时 = 8B 头 + 4B SCD（reserved u16=0 + index u16：成功 = 已写寄存器数，失败 = 出错寄存器 0 基索引）；ACCESS_DENIED 时仅 8B 头（length=0，R-138cd；旧稿「头，无载荷」只描述了 ACCESS_DENIED 分支）

## READMEM_CMD / ACK（§16.5）
- CMD：头 + u32 address（32 位对齐）+ u16 reserved(=0) + u16 count（4 的倍数）
- ACK：头 + u32 address + data[count]
- 仅当全部读出才算成功，否则 status≠0 且 length=0

## WRITEMEM_CMD / ACK（§16.6）
- CMD（Figure 16-11 / R-192c）：头 + u32 address（32 位对齐）+ data（长度 4 的倍数，见 length 字段）。**没有** reserved 字段——旧稿多写了一个 u32 reserved，已删。

## Bootstrap 寄存器（Table 36-1，接口#0）
| 地址 | 名称 | 访问 | 长度 |
|---|---|---|---|
| 0x0000 | Version（major<<16\|minor） | R | 4 |
| 0x0004 | Device Mode | R | 4 |
| 0x0008 / 0x000C | MAC High / Low | R | 4+4 |
| 0x0010 | Net If Capability（bit31=P, 30=D, 29=L） | R | 4 |
| 0x0014 | Net If Configuration（同上 + bit1 PAUSE_gen, bit0 PAUSE_rec） | R/W | 4 |
| 0x0024 | Current IP | R | 4 |
| 0x0034 | Current Subnet Mask | R | 4 |
| 0x0044 | Current Default Gateway | R | 4 |
| 0x0048 | Manufacturer Name | R | 32 |
| 0x0068 | Model Name | R | 32 |
| 0x0088 | Device Version | R | 32 |
| 0x00A8 | Manufacturer Info | R | 48 |
| 0x00D8 | Serial Number | R | 16 |
| 0x00E8 | User-defined Name | R/W | 16 |
| 0x0200 / 0x0400 | First / Second URL | R | 512+512 |
| 0x0600 | Number of Network Interfaces | R | 4 |
| 0x0900 | Number of Message Channels（0 或 1） | R | 4 |
| 0x0904 | Number of Stream Channels | R | 4 |
| 0x0934 | GVCP Capability | R | 4 |
| 0x0938 | Heartbeat Timeout（ms，默认 3000，最小 500） | R/W | 4 |
| 0x093C / 0x0940 | Timestamp Tick Freq High/Low | R | 4+4 |
| 0x0948 / 0x094C | Timestamp Value High/Low | R | 4+4 |
| 0x0958 | Pending Timeout（ms；Table 36-1 强制实现、只读，R-472cd；PENDING_ACK 阈值见 R-210cd） | R | 4 |
| 0x0A00 | CCP：bit31=EA 独占, bit30=CA 控制, bit29=CSE 切换, bit0-15=switchover key | R/W | 4 |
| 0x0D00 | SCP0 流通道端口：**印刷 bit0=方向(RO)、bit12-15=NII、bit16-31=host_port；按 MSB-first 约定 = host_port 在低 16 位（裸端口即规范写法）**（§36.57，Q31） | R/W | 4 |
| 0x0D04 | SCPS0 包大小：**印刷 bit0=fire_test_packet（=0x80000000）、bit1=do_not_fragment（=0x40000000）、bit16-31=packet_size=含 IP(20)+UDP(8)+GVSP(20) 头的总包长（按 MSB-first 约定 = 低 16 位）**（§36.58，Q31）；规范算例 0xC000219C=F+DF+8604，1.2 出厂默认 0x40000240=DF+576；payload 数据长 = packet_size−48 | R/W | 4 |
| 0x0D18 | SCDA0 目的 IP | R/W | 4 |

## 控制通道（§10）
- 权限四级：独占(EA) / 控制(CA) / 控制+切换(CSE) / 监视（只读，不写 CCP）
- 打开通道 = 写 CCP 寄存器；释放 = 写 0；独占优先于控制（Table 36-3）
- 心跳（§10.5）：主应用任何 GVCP 命令都复位心跳计数（PACKETRESEND/DISCOVERY/ACTION/FORCEIP 除外）；**推荐用心跳=读 CCP 寄存器**；超时 = Heartbeat Timeout 寄存器（默认 3s）
- 动态端口：应用侧任意端口，设备侧 GVCP 3956

## 发现（§16.1）
- DISCOVERY_CMD 广播到 **255.255.255.255:3956**（limited broadcast，§5.1.1 原文定义；曾误写为
  239.255.255.255，2026-10-12 审计更正 S-3）或子网定向广播 `ip|!mask`；设备回单播 DISCOVERY_ACK（同子网时）
- 应答延迟受 Discovery ACK Delay 寄存器（0x0950）约束

---

# GVSP（Part 3，§22–26）补充事实 — M2 依据

## GVSP 包头（所有包共用 20 字节，Figure 24-2，§24.2）
| 偏移 | 字段 | 说明 |
|---|---|---|
| 0-1 | status u16 BE | 流操作状态（0=成功） |
| 2-3 | flag u16 BE | bit15=PACKET_RESEND, bit14=PREV_BLOCK_DROPPED, bit13=RESEND_RANGE_ERROR；bit0-7 设备私有 |
| 4 | EI(1bit)+reserved(3bit)+packet_format(4bit) | EI 必须=1（64 位 block id）；packet_format: 1=leader, 2=trailer, 3=generic payload, 5=H.264, 7=multi-part, 8=GenDC |
| 5-7 | reserved u24 | |
| 8-15 | block_id64 u64 BE | 从 1 递增；通道打开时复位为 1（R-259st）；0 保留给 test packet |
| 16-19 | packet_id32 u32 BE | 块内从 0 开始；0=leader |

- GVSP 头**无长度字段**，接收方用 UDP 长度推导（R-257s 周边）；全部 big-endian（R-257s）
- 块内 payload 包 packet_id32 从 1 顺序递增（R-264st）；最后包可小于 SCPSx 指定的 payload 数据长（packet_size−48）

## Image payload 类型（payload_type=0x0001，§25.2）
- **Leader**（packet_id=0，20B 头 + **36B body**，Figure 25-2 / CR-288s）：field_id(4b)+field_count(4b)+reserved(8b) 合占 u16（渐进扫描恒 0）+ payload_type u16(0x0001) + timestamp u64 + pixel_format u32（PFNC 值）+ size_x u32 + size_y u32 + offset_x u32 + offset_y u32 + **padding_x u16 + padding_y u16**（旧稿把两个 padding 写成 u32，合计会算成 40B，已按规范图更正）
- **Payload**（packet_format=3）：20B 头 + 纯数据；除最后一包外数据长必须等于 SCPSx 指定的 payload 数据长（= packet_size − 48，R-263st）；包大小不得超过 SCPSx（R-573st）；接收方不得拷贝超出 PayloadSize 的填充（R-568sr）
- **Trailer**（packet_format=2，20B 头 + 8B）：reserved u16 + payload_type u16(0x0001) + size_y u32（本块实际行数）
- leader/trailer 单包 ≤576 字节（含 IP/UDP/GVSP 头），除非 SCCFGx 的 large_leader_trailer_enable

## 流通道打开与运行（§11.5–11.8、§36.57/§36.58）
- 打开通道：先写 **SCDAx=目的 IP**，最后写 **SCPx = 裸 host_port**（低 16 位，host_port≠0 激活，=0 关闭，R-087ca/§36.57，Q31 修正）；**SCPx 必须是开通道最后一个写的寄存器**（SCPSx 协商期写除外，§36.57/§11.8）
- 写纪律（R-561ca/§36.57）：host_port 非零时禁止写 SCDAx/SCCFGx/SCPSx（SCPSx 包大小协商期除外）；重配前先写 0 关闭
- 流式期间 SCPx 非零写必须回 **GEV_STATUS_BUSY**（R-586cd），只有写 0（关闭）被允许
- **SCPSx**（§36.58）：packet_size 在**低 16 位**（MSB-first 印刷 bit16-31），**含 IP(20)+UDP(8)+GVSP 头的总包长**；fire=0x80000000、DF=0x40000000；generic payload 包数据长 = packet_size−48；设备不支持的值会取整并更新寄存器，**写后必须读回验证**（R-562st）；流式期间禁写 SCPSx
- 包大小协商（§11.7 R-567s）：开通道后、正式流前用 test packet 搜索（写 SCPSx=裸 size，读回取整值，再置 fire_test_packet|do_not_fragment 发测试包）；M2 不实现（Q16），M3 留口
- 传输方源端口用动态端口（49152-65535，O-086st）
- 块 payload 总大小上限 = **SCMBSx**（64 位，0x0D34/0x0D38）
- 最大包数 SCMPCx（0x0D30）

## 丢包重传（§16.7，M3-1 已实现 2026-10-19）
- **能力门控**：R-197ca——GVCP Capability（0x0934）PACKETRESEND 位（印刷 bit29 = mask 0x00000004，1.2 §27.28/3.0 §36.29，Aravis 1<<2）未声明时**禁止**发重传请求
- **PACKETRESEND_CMD = 0x0040**（CR-200c/Fig 16-13）：flag 字节 EI 位置 1（0x10，印刷 bit3）；载荷 20B = stream_channel_id u16 + reserved u16 + first_packet_id32 + last_packet_id32 + block_id64（EI 时）；last=0xFFFFFFFF 表示"到块尾（含 trailer）"。EI=0 的 legacy 形式载荷 14B（block_id16）
- **fire-and-forget**：CR-198ca 必须清 ACKNOWLEDGE 位；**PACKETRESEND_ACK 不存在**（CR-201cd）——不得等待应答；可异步发送（即使正等前条命令的应答）；CR-199cd primary/secondary 均可发
- **GVSP flag 位序**（§15.3/Table 24-1，msb-first）：RESEND_RANGE_ERROR=0x0004、PREVIOUS_BLOCK_DROPPED=0x0002、PACKET_RESEND=0x0001（旧文档曾把三者写成 0x2000/0x4000/0x8000，已纠正）。
- **重传包标记**（§16.7.2）：CR-204st——重发包必须置 flag bit15，MSB-first 即 **GEV_FLAG_PACKET_RESEND=0x0001**；status 首选 **GEV_STATUS_PACKET_RESEND=0x0100**（Table 19-1），接收端 SUCCESS 与 0x0100 都按正常包处理
- **错误应答**（CR-205st，Table 16-2）：设备无法重传时回**仅头无数据**包，status ∈ {0x800C PACKET_UNAVAILABLE, 0x8014 TEMPORARILY_UNAVAILABLE（可带 hold-off 扩展 CR-208st——v1 不解析，QoI）, 0x8010 NOT_YET_AVAILABLE, 0x8011 AND_PREV_REMOVED_FROM_MEMORY, 0x8012 REMOVED_FROM_MEMORY} → 该包按不可恢复处理
- **RESEND_RANGE_ERROR**（CO-206st/CR-207sr）：flag 0x0004，body 首 4 字节 = last_packet_id32、无数据——整个请求范围不可恢复
- **时序**（§16.7.3，全部 QoI，出处 Aravis arvgvstreamprivate.h）：尽早请求（设备内存有限）；保序拓扑可立即请求、乱序拓扑用超时机制——producer 采用乱序窗口：初始窗口 1ms（INITIAL_PACKET_TIMEOUT，参照 1000µs）、重发间隔 20ms（PACKET_TIMEOUT，参照 20000µs）、帧保留 100ms（FRAME_RETENTION，参照 100000µs，`GENTLITE_GEV_FRAME_RETENTION_MS` 可调 10-60000ms）；`GENTLITE_GEV_RESEND=0` 禁用
- **trailer 的 packet_id = 末 payload id+1**（R-264st 序）：接收端据此登记"尾部缺口"（尾部整段丢失时否则无法发现）
- **尾部缺口之外的知识来源**：payload id > highest+1 到达时登记 (highest+1..=id-1)
- sim：GVCP Capability 声明 0xC0000004（UN|SN|PACKETRESEND）；每通道缓存**最近一块**全部数据报（真机内存有限，sim 只留一块，§16.7.3 语义），重发时置 0x0001+0x0100（legacy 1.2 头无 flag 字段，仅 status）；未知块/越界回 0x800C 仅头包；`set_packet_drop` 测试钩子只丢 payload（leader/trailer 不掉）
