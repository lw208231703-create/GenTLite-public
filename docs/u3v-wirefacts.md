# U3V/GenCP 线格式事实表（已核对规范，附出处）

> 来源：`notes/USB3_Vision_Specification_V1.2.pdf`（USB3 Vision 1.2）与
> `notes/GenICam_Package_2025.10/GenCP/GenICam_GenCP_1.3.1.pdf`（GenCP 1.3.1）。
> **字节序：一切引导寄存器、协议字段、数据块字段均为小端**（U3V §4.1.2 R-30cd；
> 数据块 §5.5.1 R-75st）——与 GigE 全大端相反，写解码器时勿带 GVSP 习惯。
> 本文件是 gentl-u3v crate 的契约。任何不符以规范原文为准并回改此表。

## USB 描述符匹配（U3V 1.2 §3.2）

**接口级 class code 是 0xEF/0x05/0x00-0x02，不是 0xEF/0x02**（0xEF/0x02/0x01 是
*设备级* 代码，因 IAD 存在；README 早期行与任务简报有误，已按规范更正，见
OPEN-QUESTIONS UQ1）。

| 描述符 | class/subclass/protocol | 出处 |
|---|---|---|
| Device Descriptor | EF / 02 / 01 | Table 3-1，R-3cd、R-4cd |
| Interface Association | EF / 05 / 00，iFunction="USB3 Vision Device" | Table 3-2，R-7cd |
| Device Control Interface | EF / 05 / 00 | Table 3-3，R-9cd |
| Device Event Interface | EF / 05 / 01 | Table 3-6，CR-18cd |
| Device Streaming Interface | EF / 05 / 02 | Table 3-8，CR-23cd |

- 控制接口必须是 IAD 第一个接口（R-10cd）；恰好 2 个端点：bulk OUT + bulk IN
  （R-11cd）；SSECD bMaxBurst=00h（Table 3-4）；厂商不得建 alternate 控制接口
  （R-13cd），host 必须忽略 alternate（R-14ch）。
- 事件接口恰好 1 个 **bulk IN** 端点（CR-19cd）——**U3V 1.2 事件走 bulk，不是
  interrupt**；流接口恰好 1 个 bulk IN（CR-24cd）。
- 多流接口的描述符顺序 = 流索引 = SIRM 寄存器块顺序（CR-123st）。
- Device Info 描述符（§3.2.5 Table 3-5）：bDescriptorType=0x24、bDescriptorSubtype=
  0x01（U3V_DEVICEINFO）、bLength=20；字段：bGenCPVersion u32@3、bU3VVersion u32@7、
  iDeviceGUID@11、iVendorName@12、iModelName@13、iFamilyName@14、
  iDeviceVersion@15、iManufacturerInfo@16、iSerialNumber@17、
  iUserDefinedName@18（可 0）、bmSpeedSupport@19（bit3=SS Gen1x1，bit4=SS+ Gen2x1，
  bit5=Gen1x2，bit6=Gen2x2）。
  - 必须出现在默认控制接口描述符之后（R-15cd）；所有字符串必须 7-bit ASCII
    （R-16cd）；**禁止**用标准设备描述符的 iManufacturer/iProduct/iSerialNumber
    代替它识别设备（R-5ch）。
- U3V GUID（§3.1.2，R-1cd）：12 位大写十六进制 = `{USB-IF VID 4 位}{厂商唯一值 8
  位}`，例 `"ABCD12345678"`；标签印刷时前缀 "U3V "（CR-2cd）。

## GenCP 命令层（GenCP 1.3.1 §4，U3V §4.1.3）

**通用包布局**（§4.1）：`Prefix | CCD | SCD | Postfix`。
U3V Prefix 为 4 字节 magic，**Postfix 不使用**（R-31cd）：

| 通道 | Prefix 值 | ASCII（U 在 LSB） |
|---|---|---|
| 控制 | 0x43563355 | "U3VC" |
| 事件 | 0x45563355 | "U3VE" |

**命令 CCD（8 字节，§4.3.1 Table 4）**：flags u16（bit14=RequestAck，bit15=
CommandResend，bit0-13=0）、command_id u16、length u16（**仅 SCD 长度**，不含
Prefix/Postfix/CCD）、request_id u16。
**应答 CCD（8 字节，§4.3.2 Table 5）**：status u16、command_id u16、length u16、
request_id u16。应答的 command_id = 命令码 | 1。

**命令码**（§4.3.3 Table 7）：

| 命令 | 值 |
|---|---|
| READMEM_CMD / READMEM_ACK | 0x0800 / 0x0801 |
| WRITEMEM_CMD / WRITEMEM_ACK | 0x0802 / 0x0803 |
| PENDING_ACK | 0x0805 |
| READMEM_STACKED_CMD/ACK | 0x0806 / 0x0807 |
| WRITEMEM_STACKED_CMD/ACK | 0x0808 / 0x0809 |
| EVENT_CMD / EVENT_ACK | 0x0C00 / 0x0C01 |

- **request_id**：主机连接后从 **0** 开始（R-35ch）；request_id=0 的命令重置序
  列且必须被接受（R-34cd）；应答必须回显同一 request_id（不符 = 失步）。
- U3V **只支持 RequestAck**；收到 CommandResend 置位的命令必须以
  U3V_STATUS_RESEND_NOT_SUPPORTED 应答（R-134cd）——**U3V 没有命令重传**，超时
  即硬错误。
- 不支持的命令码 → GENCP_NOT_IMPLEMENTED 应答；prefix/格式错误的命令设备必须
  完全忽略（R-124cd）。EVENT_ACK 不被 U3V 使用（§4.1.3.2）。
- **READMEM_CMD SCD（12 字节，§4.4.1 Table 8）**：register address u64@0、
  reserved u16@8=0、read length u16@10。长度 u16 → 单次最多 65535 字节；地址/长
  度按字节对齐（GenCP §4.4.1，U3V 未另定对齐规则）。
- **READMEM_ACK SCD**：data（x 字节）；返回字节数与命令不符时 status 必须说明
  原因（Table 9）。
- **WRITEMEM_CMD SCD（§4.4.3 Table 10）**：register address u64@0、data@8；
  写字数由 CCD.length 推得（= length − 8）。
- **WRITEMEM_ACK SCD（§4.4.4 Table 11）**：reserved u16@0（=0）、length_written
  u16@2；**仅当 Device Capability bit11（Written Length Field）=1 时发送**（U3V
  强制支持，见 ABRM 节）；CCD.length 相应为 0 或 4。
- **传输限制**：单条命令 ≤ Maximum Command Transfer Length（R-41ch）；设备不应
  答超过 Maximum Acknowledge Transfer Length 的数据（R-43cd）；命令/应答可分布
  在多个 USB 包中，双方必须支持（R-39c）。
- **IN 端点提交长度 = 最大可能应答总长**（prefix+CCD+最大载荷，可向上取整到
  wMaxPacketSize）（R-117ch）；应答总长为 wMaxPacketSize 整数倍且小于提交长度
  时，设备以 zero-length packet 终止（R-118cd）→ 宿主按"短包/ZLP 即应答结束"
  读即可。
- **PENDING_ACK**：处理时间超过 Maximum Device Response Time 设备必须发
  （R-45cd）；主机必须支持 PENDING_ACK（R-36ch）——收到后继续等真应答。
- 处理中收到新命令 → 以 BUSY 状态应答（§4.1.4.1.1）。
- 重新同步：对两端点 SET ENDPOINT_HALT 再 CLEAR（§4.1.4.1.2）。

## 状态码

**格式**（GenCP §4.3.2.1 / U3V Table 4-1）：bit0-11 = code，bit12 = 0，
bit13-14 = namespace（0=GenCP，1=技术特定，2=设备特定），bit15 = severity
（0=Warning/Info，1=Error）。**U3V 状态码 namespace 必须 = 1**（R-38cd）。

**GenCP 状态码**（§4.3.2.1 Table 6，逐值经 PDF 原页图像核对）：

| 值 | 名称 |
|---|---|
| 0x0000 | GENCP_SUCCESS |
| 0x8001 | GENCP_NOT_IMPLEMENTED |
| 0x8002 | GENCP_INVALID_PARAMETER |
| 0x8003 | GENCP_INVALID_ADDRESS |
| 0x8004 | GENCP_WRITE_PROTECT |
| 0x8005 | GENCP_BAD_ALIGNMENT |
| 0x8006 | GENCP_ACCESS_DENIED |
| 0x8007 | GENCP_BUSY |
| 0x800B | GENCP_MSG_TIMEOUT |
| 0x800E | GENCP_INVALID_HEADER |
| 0x800F | GENCP_WRONG_CONFIG |
| 0x8FFF | GENCP_ERROR |

**U3V 命令状态码**（Table 4-2）：

| 值 | 名称 | 场景 |
|---|---|---|
| 0xA001 | U3V_STATUS_RESEND_NOT_SUPPORTED | 收到 CommandResend 命令 |
| 0xA002 | U3V_STATUS_DSI_ENDPOINT_HALTED | Stream Enable 时流端点 halted |
| 0xA003 | U3V_STATUS_SI_PAYLOAD_SIZE_NOT_ALIGNED | SI 尺寸寄存器未按对齐 |
| 0xA004 | U3V_STATUS_SI_REGISTERS_INCONSISTENT | Stream Enable 参数非法 |
| 0xA005 | U3V_STATUS_DEI_ENDPOINT_HALTED | Event Enable 时事件端点 halted |

**U3V 数据块状态码**（进 trailer status，Table 4-3）：
0xA100 U3V_STATUS_DATA_DISCARDED（块内部分数据被丢弃）、0xA101
U3V_STATUS_DATA_OVERRUN（数据块装不下）。

## ABRM —— 技术无关引导寄存器（U3V Table 4-6 = GenCP §5.4）

| 地址 | 寄存器 | 长 | 访问 |
|---|---|---|---|
| 0x00000 | GenCP Version | 4 | R（minor@0 u16，major@16 u16） |
| 0x00004 | Manufacturer Name | 64 | R |
| 0x00044 | Model Name | 64 | R |
| 0x00084 | Family Name | 64 | R（CM） |
| 0x000C4 | Device Version | 64 | R |
| 0x00104 | Manufacturer Info | 64 | R |
| 0x00144 | Serial Number | 64 | R |
| 0x00184 | User Defined Name | 64 | RW（CM） |
| 0x001C4 | Device Capability | 8 | R |
| 0x001CC | Maximum Device Response Time | 4 | R，ms（≤300ms，GenCP §5.4.10） |
| 0x001D0 | Manifest Table Address | 8 | R |
| 0x001D8 | SBRM Address | 8 | R（CM：capability bit9） |
| 0x001E0 | Device Configuration | 8 | RW（MultiEvent enable bit0） |
| 0x001E8 | Heartbeat Timeout | 4 | —（U3V 不用） |
| 0x001EC | Message Channel channel_id | 4 | —（U3V 不用） |
| 0x001F0 | Timestamp | 8 | R，当前设备时间 ns |
| 0x001F8 | Timestamp Latch | 4 | W |
| 0x001FC | Timestamp Increment | 8 | R |
| 0x00204 | Access Privilege | 4 | —（U3V 不用，**无排他访问机制**） |
| 0x00208 | Protocol Endianess | 4 | （GenCP 1.3.1 已废弃） |
| 0x0020C | Implementation Endianess | 4 | R（仅 little-endian） |
| 0x00210 | Device Software Interface Version | 64 | R（CM） |
| 0x00250 | Reserved | 64944 | 访问保留/未用寄存器必须回 GENCP_INVALID_ADDRESS（R-125cd） |

字符串寄存器：定长、0 填充、填满时可无终止 NUL；编码由 Device Capability 指定
（GenCP §5.2）。U3V 强制 ASCII（R-111cd 列表）。

**Device Capability 位**（GenCP Table 21，经原页图像核对）：
bit0 UserDefinedName、bit1 AccessPrivilege、bit2 MessageChannel、bit3 Timestamp、
bit4-7 StringEncoding（0=ASCII，1=UTF8，2=UTF16）、bit8 FamilyName、bit9 **SBRM**、
bit10 EndiannessRegister、bit11 **WrittenLengthField**、bit12 MultiEvent、
bit13 StackedCommands、bit14 DeviceSoftwareInterfaceVersion。
U3V 要求 bit3/bit9/bit11=1 且 StringEncoding=0（R-111cd）。

## SBRM —— 技术相关引导寄存器（Table 4-7，偏移相对 SBRM 基址）

SBRM 必须位于 0xFFFF 之后的厂商地址空间。U3V Version 同 GenCP Version 格式
（minor@0，major@16）。

| 偏移 | 寄存器 | 长 | 访问 |
|---|---|---|---|
| +0x00 | U3V Version | 4 | R |
| +0x04 | U3VCP Capability | 8 | R：bit0 SIRM，bit1 EIRM，bit2 IIDC2 |
| +0x0C | U3VCP Configuration | 8 | RW（保留） |
| +0x14 | Maximum Command Transfer Length | 4 | R |
| +0x18 | Maximum Acknowledge Transfer Length | 4 | R |
| +0x1C | Number of Stream Channels | 4 | R（0=无流能力） |
| +0x20 | SIRM Address | 8 | R（CM：capability bit0） |
| +0x28 | SIRM Length | 4 | R（每个 SIRM 的字节长度） |
| +0x2C | EIRM Address | 8 | R（CM：capability bit1） |
| +0x34 | EIRM Length | 4 | R |
| +0x38 | IIDC2 Address | 8 | R（CM：capability bit2） |
| +0x40 | Current Speed | 4 | R（位布局同 Device Info 的 bmSpeedSupport，只报当前速度） |

第 2 个及以后的 SIRM = SIRM Address + i × SIRM Length。

## Manifest（GenCP §5.5 + U3V §4.1.5）

Manifest Table（地址在 ABRM 0x1D0）：entry count u64@0；entry i 在
8 + i×64，定长 64 字节：

| 偏移 | 字段 |
|---|---|
| +0 | GenICam File Version u32（subminor@0 u16，minor@16 u8，major@24 u8） |
| +4 | Schema/Filetype/Fileformat u32：bits0-2 File Type（0=Device XML，1=Buffer XML）；bits10-15 File Format（0=未压缩 XML，**1=ZIP 内含单个 XML**） |
| +8 | Register Address u64（文件在寄存器空间的地址） |
| +16 | File Size u64 |
| +24 | SHA1-Hash 20 字节（U3V 设备必须提供有效值，R-50cd） |
| +44 | Reserved 20 字节 = 0 |

- 缓存已下载文件的宿主在 hash 不符时必须重新下载（CR-51ch）。
- 设备必须提供 GenApi 兼容 XML（schema 1.1 起）（R-59cd）。

## 流数据块（§5.5）

块 = **leader + payload data + trailer**，全部小端（R-75st）。leader 提供
block_id（从 0 开始，Stream Enable 时复位，R-77st；已获取块必须连续 +1，
R-78st）。整块丢弃 = 跳 block_id，接收方数缺口（§5.5.4.3）。leader 与
trailer 永远成对（R-80st）。**leader/trailer 各自为单一传输**（§5.2.1.2 /
§5.2.1.4），宿主必须设 SI Maximum Leader/Trailer Size ≥ 设备 Required 值
（R-62sr/R-71sr）。

**Leader magic = 0x4C563355（"U3VL"）**（Table 5-4），**Trailer magic =
0x54563355（"U3VT"）**（Table 5-5）。

**Image Leader（52 字节，Table 5-7，逐字段经原页图像核对）**：

| 偏移 | 字段 | 宽 |
|---|---|---|
| 0 | magic_key u32 = 0x4C563355 | 4 |
| 4 | reserved | 2 |
| 6 | leader_size u16（leader 总字节数） | 2 |
| 8 | block_id u64 | 8 |
| 16 | reserved | 2 |
| 18 | payload_type u16 = 0x0001（Image） | 2 |
| 20 | timestamp u64（**单位 ns**） | 8 |
| 28 | pixel_format u32（PFNC，§5.5.7） | 4 |
| 32 | size_x u32（像素宽） | 4 |
| 36 | size_y u32（行数；可变帧时为最大行数） | 4 |
| 40 | offset_x u32 | 4 |
| 44 | offset_y u32 | 4 |
| 48 | padding_x u16（每行末尾填充字节） | 2 |
| 50 | reserved | 2 |

**Image Trailer（32 字节，Table 5-8）**：

| 偏移 | 字段 | 宽 |
|---|---|---|
| 0 | magic_key u32 = 0x54563355 | 4 |
| 4 | reserved | 2 |
| 6 | trailer_size u16 | 2 |
| 8 | block_id u64（必须与 leader 相同） | 8 |
| 16 | status u16（0 或 Table 4-3 数据块状态码） | 2 |
| 18 | reserved | 2 |
| 20 | valid_payload_size u64（payload 有效字节数） | 8 |
| 28 | size_y u32（**实际行数** ≤ leader，可变帧高，CR-86st） | 4 |

**payload_type**（Table 5-6）：0x0001 Image、0x4000 Chunk、0x4001 Image
Extended Chunk（bit14 扩展 chunk 位）、0x1001 GenDC Container、0x1002 GenDC
Component Data。

**Payload 序列**（§5.2.1.3，Figure 5-2）：`equal sized blocks（SI Payload
Transfer Size × SI Payload Transfer Count）→ final transfer1 → final transfer2`。
- 设备发送不得超过 TS×Count + F1 + F2（R-63st）；顺序固定（R-64st）；
  TS/Count/F1/F2 任一为 0 则相应部分省略（R-65st–R-68st）。
- 设备数据少于编程量时以短包/ZLP/填充补完（R-69st）；**首个短包/ZLP 之后
  valid_payload_size 不得超出其偏移**（R-126st），其后的传输数据接收方必须丢
  弃（R-127st）；有效字节数必须写入 valid_payload_size（R-70st）。
- 块内部分数据被丢弃：剩余 payload 以短/ZLP 包填充 + trailer status=0xA100
  （§5.5.4.1）；溢出：status=0xA101（§5.5.4.2）。

## SIRM —— 流接口寄存器（Table 5-1 + §5.4.1，偏移相对 SIRM 基址）

| 偏移 | 寄存器 | 长 | 访问 |
|---|---|---|---|
| +0x00 | SI Info | 4 | R：bit0 Supports Additional Payload Modes；bits24-31 **Payload Size Alignment = 2^n**（所有 "SI Size" 寄存器值必须是 2^n 的倍数） |
| +0x04 | SI Control | 4 | RW：bit0 **Stream Enable**（1→enable；活动流上写 0 会 abort；参数非法→0xA004，端点 halted→0xA002） |
| +0x08 | SI Required Payload Size | **8**（UINT64；Table 5-1 该行列宽印成 4 是排版错位，以 §5.4.1.3 细节节"Length 8, UINT64"为准） | R |
| +0x10 | SI Required Leader Size | 4 | R |
| +0x14 | SI Required Trailer Size | 4 | R |
| +0x18 | SI Maximum Leader Size | 4 | RW* |
| +0x1C | SI Payload Transfer Size | 4 | RW* |
| +0x20 | SI Payload Transfer Count | 4 | RW* |
| +0x24 | SI Payload Final Transfer1 Size | 4 | RW* |
| +0x28 | SI Payload Final Transfer2 Size | 4 | RW* |
| +0x2C | SI Maximum Trailer Size | 4 | RW* |
| +0x30 | SI Supported Payload Modes | 4 | R（CM）：bit0 Standard，bit1 GenDC Container，bit2 GenDC Serial Flow，bit3 GenDC Parallel Flow |
| +0x34 | SI Payload Mode | 4 | RW*（CM）：同一编码，只允许置一位 |
| +0x38 | SI GSIRM Address | 8 | R（CM） |
| +0x40 | SI GSIRM Size | 4 | R（CM） |

- R 寄存器写入必须报错（R-72cd）；RW* 寄存器在 Stream Enable 置 1 时被设备
  捕获，活动流期间改动不生效（R-73st）。
- SI Required Payload Size 必须与设备 XML 的 PayloadSize 特性一致（R-114st；
  R-74st 要求反映当前配置）——宿主用它决定缓冲大小。

## EIRM —— 事件接口寄存器（Table 4-5，偏移相对 EIRM 基址）

| 偏移 | 寄存器 | 长 | 访问 |
|---|---|---|---|
| +0x00 | EI Control | 4 | RW：bit0 **Event Enable**（halted 时置 1 → 0xA005） |
| +0x04 | Maximum Event Transfer Length | 4 | R |
| +0x08 | Event Test Control | 4 | RW：bit0 TriggerEventTest（自清），Event Enable=1 时发送 U3V_EVENT_TEST |

- 事件**无应答**（单向端点，USB 自身重试；§4.1.2 NOTE）；EVENT_ACK 不被
  U3V 使用（§4.1.3.2）。
- 事件包 = U3VE prefix + CCD(EVENT_CMD) + SCD：**event_size u16@0**（MultiEvent
  时 = 事件数据对象总长，否则 0）、**event_id u16@2**、**timestamp u64@4
  （ns）**、data@12（§4.1.3.3.2 + GenCP Table 17）。SCD 从 CCD 之后开始映射进
  事件端口地址空间（读 timestamp 的节点偏移 = 4）。
- U3V 技术特定事件 ID：0x4FFF = U3V_EVENT_TEST（Table 4-4）。
- 事件 IN 提交长度 = Maximum Event Transfer Length 向上取整到
  wMaxPacketSize（R-119ch）。

## 宿主侧规则速查

- 控制通道：连接后 request_id 从 0 开始；IN 按最大应答尺寸提交；PENDING_ACK
  继续等；超时/失步无重传（CommandResend 被拒），恢复手段是 ENDPOINT_HALT
  再清除（§4.1.4.1.2）。
- 读/写按 Max Ack/Command Transfer Length 分块（同时受 u16 长度字段限制）。
- 流：先写 SI Maximum Leader/Payload 序列/Maximum Trailer，最后写 Stream
  Enable（§5.2.1 顺序；SI_REGISTERS_INCONSISTENT 表示被拒）；停流写 0。
- GenTL 映射（本 producer 决策，非规范强制）：DEVICE_INFO_TIMESTAMP_FREQUENCY
  = 1 GHz（leader timestamp 已是 ns）；端口小端；BUFFER pixel namespace =
  PFNC 32-bit。

---

# 真机实测补充（2026-10-13，海康 MV-CS016-10UC / Hikrobot，VID_2BDF&PID_0001）

## Windows 设备打开（规范不规定，实测链）
见 OPEN-QUESTIONS UQ13。要点：复合父设备句柄 CreateFile 可开但
WinUsb_Initialize 拒绝（error 8）；必须打开 MI_00 控制接口的设备接口路径；
Zadig 风格 INF 用自定义 DeviceInterfaceGUID，需 DeviceClasses 注册表扫描兜底。

## GenCP 会话（真机验证通过）
- ABRM 从地址 0 开始：GENCPC_VERSION(0x0000)=0x00010001（GenCP 1.1）
- 字符串寄存器 64B 定长 NUL 截断 ✓；厂商 "Hikrobot"、型号 "MV-CS016-10UC"
- **ABRM 可选字段必须按 DEVICE_CAPABILITY(0x01C4) 位门控**（UQ14）：
  海康相机无 FAMILY_NAME（读回 0x8003）
- SBRM 地址 0x60000；U3V Version/U3VCP Capability 走读正常
- XML：Manifest → `Local:MV-CS016-10UC.xml;100000;12700`（ZIP 封装，
  §5.5.1.2 解压后 829591B）；相机寄存器用 `<pAddress>` 间接寻址
- 接口枚举：control(EF/05/00, ep 0x81/0x01) + event(0x82) + stream(0x83)，
  全 bulk（PipeType=2），max packet 1024

## 数据流（真机验证通过，grab_u3v 50/50 + 200/200 帧）
- 相机：1440×1080 BayerRG8（0x01080009），PayloadSize=1555200（经 XML
  PayloadSize_Reg 间接地址读出，LE）
- AcquisitionStart=1 @0x30804（MaskedIntReg bit0，LE）——DSStartAcquisition
  写 SIRM Stream Enable 在前、相机 armed 在后，与 GigE 流程同序
- leader/trailer 重组、EVENT_NEW_BUFFER、IS_INCOMPLETE=0 全链路正确


---

# Linux 后端事实（Phase B rusb/libusb 移植，2026-10-23，代码完成待真机）

对应实现：`crates/gentl-u3v/src/linux/discovery.rs`、`usbio/libusb.rs`、
共享件 `usbio/mod.rs`（`UsbPipe` trait + UQ16 累积读层）、`descriptors.rs`
（纯描述符解析，双端同一套测试）。除本节列出的宿主侧事实外，线格式
语义（GenCP 事务/SIRM/重组）与 Windows 完全一致，无任何平台分叉。

## 设备匹配（§3）——描述符层一次完成
- libusb 直接给出真描述符，Windows 注册表两级舞（Q12：设备级兼容 ID
  → MI 子设备接口类确认）坍缩为一次检查：非复合设备看 device class
  EF/02（Table 3-1）；复合设备（class 0）看任一接口 EF/05
  （Tables 3-3/3-6/3-8）。不需要打开设备即可匹配。
- Device Info 描述符（§3.2.5）在控制接口描述符的 `extra()` 里（rusb 不
  暴露 IAD，也不需要）；`linux/discovery.rs::config_blob` 把 rusb 的解析
  描述符重序列化成共享解析器（`descriptors.rs`）消费的 blob 形状。

## 端点完成语义——与 RAW_IO 合同等价
- usbfs/libusb 同步 bulk：buffer-full 或短包完成；RAW_IO（MSDN）：
  同样 buffer-full/short-packet 完成 + 请求长必须 MPS 整倍数。差异仅
  "任意数据即返回"的时序（WinUSB RAW_IO 对 MPS 整倍数的中间数据也立即
  返回，usbfs 会继续等到短包）——对 UQ16 累积层只影响到达时刻，不影响
  组帧正确性；leader/trailer 是设备侧单传输（短包或 ZLP 收尾），两侧
  都正确结束。30s 饥饿死线兜底。
- 超时语义两侧一致：超时读已传数据被丢弃（WinUSB AbortPipe+ResetPipe
  丢弃在途 URB；libusb 同步传输超时即取消），poll 层重试。

## 错误恢复映射
- `UsbPipe::flush`：WinUSB = FlushPipe（清可能残留的迟到的 ACK）；
  libusb = `clear_halt`（同步传输无残留 URB 概念，剩余失败模式是端点
  halt——即 UQ6 ENDPOINT_HALT 重同步在 usbfs 的等价物，§4.1.4.1.2）。
- RAW_IO：WinUSB 真设置；libusb no-op（usbfs 无请求排队旁路策略，
  同步传输本身就是无排队路径；UQ16 累积层必要且充分）。

## 宿主侧枚举/打开
- GenTL Interface：libusb 不建模 root hub——按"当前有设备的 USB 总线"
  合成一接口（QoI，Q37），ID 保持 `USB\ROOT_HUB30\BUS<n>` 家族形状。
- instance ID = `<bus>:<address>`（十进制），会话级有效；打不开
  （权限/占用）的设备以 `VID:PID:bus:address` 退化 ID 列名 +
  ACCESS_STATUS=NOACCESS（serial 字符串不开设备读不到，QoI，Q37）。
- 打开：`set_auto_detach_kernel_driver(true)`——usbfs 无 WinUSB 式驱动
  独占，内核驱动占着接口时由 libusb 剥离、释放时重挂（尽力而为）。
- claim：open 路径（IFOpenDevice）一次 claim 全部 EF/05 接口（含 event，
  UQ7 未接消费）；`LibUsbDevice::drop` 逆序 release。
- 构建：`libusb1-sys` 链系统 libusb（无 vendored C 源码），构建期需
  libusb-1.0 开发包（pkg-config `libusb-1.0` + 头文件）；运行期
  rusb::devices()/open() 失败按"无设备/打不开"降级，永不致命。
