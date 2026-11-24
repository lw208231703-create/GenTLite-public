# 实施疑问清单（OPEN QUESTIONS）

> 纪律：规范查不到/不确定的点**禁止猜测**，一律记录在此，由人核实后回填。
> 每条注明：涉及模块、规范出处（文档+章节）、当前代码中的临时处理、待确认的问题。
> 已解决的条目标记 `[RESOLVED]` 并写明结论。

## Q1. Version 寄存器（0x0000）的取值
- **涉及**：simulator regmap、GigE device 读版本
- **规范出处**：GigE Vision 3.0 §36 Table 36-1（"两个最高字节 major，两个最低字节 minor"）
- **待确认**：simulator 声明兼容哪个版本号？格式形如 `major<<16 | minor`（如 1.2 → 0x00010002）我理解无误，但 3.0 规范下"设备应报告 2.x 还是 3.0"未在已读章节中找到明确语句（3.0 设备多数报告 2.2 以保持兼容）。
- **临时处理**：simulator 报 `0x00020002`（2.2），主流相机值。

## Q2. DISCOVERY_ACK 中 spec_version_major/minor 字段
- **涉及**：gvcp server/client
- **规范出处**：GigE Vision 3.0 §16.1.2 Figure 16-2
- **待确认**：该字段填"本规范版本 3.0"还是"设备实现的版本 2.2"？Figure 16-2 标注此字段即 bootstrap 的 Version 寄存器语义，倾向与 Q1 同值。
- **临时处理**：与 Version 寄存器一致（2.2）。

## Q3. 网卡枚举作为 GenTL Interface 的 ID 命名 `[RESOLVED]`
- **涉及**：discovery.rs / TLGetInterfaceID
- **规范出处**：GenTL 1.6（Interface ID 格式未强制）
- **结论（2026-10-12，按三厂商实测）**：**结构两家完全一致——单聚合接口**（不区分网卡）；
  差异仅在字符串风格。最终采用 **Basler 风格**：
  - Interface ID = `GenTLite GEV GenTL Interface Module`（Basler 同款
    `"Basler GEV GenTL Interface Module"`；MVS 用 `"GigE Vision"`）
  - Interface DISPLAYNAME = `GigE Vision`（三家一致：Basler/MVS 的用户可见名都是它）
  - TLGetInfo：VENDOR=`GenTLite`、MODEL=`GEV`、ID/NAME=文件名 `gentl_gige.cti`、
    DISPLAYNAME=`GenTLite GEV GenTL Producer`（Basler 同款结构）
  - 发现语义不变：所有 NIC 定向广播+全局地址并集（去重），无子网过滤；
    NIC 增删经 `TLUpdateInterfaceList` 的 pbChanged 报告。
- **实测依据**（本机 dump_cti 工具，`crates/gentl-gige/examples/dump_cti.rs`）：
  | | Basler ProducerGEV.cti | MVS MvProducerGEV.cti | LDVisionTL.cti |
  |---|---|---|---|
  | 接口数 | 1（需先 TLUpdateInterfaceList） | 1 | 8（每网卡/USB 根一接口） |
  | Interface ID | `Basler GEV GenTL Interface Module` | `GigE Vision` | `LDVision::GEV::<网卡名>` |
  | DISPLAYNAME | `GigE Vision` | `GigE Vision` | `LDVision GigE ... [<名> @ IP]` |
  | TL VENDOR/MODEL | `Basler` / `GEV` | `GEV` / `GEV GenTL Producer` | `LDVision` / 长描述 |
  | 结论 | 国际主流=聚合+ID/显示名分离 | 结构同 Basler，字符串更简 | 少数派 |
- ~~临时处理~~：曾采用 LDVision 风格每网卡接口，后改 MVS 风格，最终按用户指示
  以 Basler 为准（国际通用性优先）。

## Q4. Device ID 采用什么（MAC / serial / user-defined name）
- **涉及**：discovery / IFGetDeviceID / IFOpenDevice
- **规范出处**：GenTL 1.6（DEVICE_INFO_ID 未规定内容格式）
- **待确认**：商用 producer 多用 MAC 地址（如 "00:11:22:33:44:55"）或序列号。选择影响 consumer 持久化设备的能力。
- **临时处理**：采用 MAC 地址字符串（冒号分隔十六进制），待 review。

## Q5. 迷你 XML 的 SchemaVersion 与节点最小集
- **涉及**：simulator regmap XML、consumer-test
- **规范出处**：GenApi Schema 1.1（notes/GenICam_Package_2025.10/GenApi/GenApiSchema_Version_1_1.xsd_.zip）、SFNC 2.7
- **待确认**：ExposureTime 按 SFNC 是 Float 节点，单位 us。最小节点集定为：DeviceVendorName/DeviceModelName/DeviceVersion/DeviceFirmwareVersion（String）、Width/Height（Integer）、PixelFormat（Enumeration：Mono8/Mono10/Mono12/Mono16）、ExposureTime（Float）、GevDeviceIPAddress 等可选。ExposureTest 读写值范围自定（如 10.0–100000.0 us）。
- **临时处理**：按上述集合实现，XML 合法性由 consumer-test 的 GenApi 加载验证。

## Q6. GCGetPortURL 返回的 URL 形式
- **涉及**：device.rs xml_url
- **规范出处**：GigE Vision 3.0 §（First URL bootstrap 0x0200，格式未在已读部分细查）
- **待确认**：相机 XML URL 形如 `Local:<hex_addr>;<hex_len>` 或 file/http URL。规范有 "URL scheme" 章节未细读，实现前需查 §“Device Description File”相关章节。
- **临时处理**：实现时先查规范该章节；simulator 用 `Local:` 形式指向自身寄存器空间。

## Q6-resolved. GCGetPortURL 返回的 URL 形式 `[RESOLVED]`
- **结论**：规范 §14.1.1 明确格式 `local:<文件名>.<扩展名>;<地址 hex>;<长度 hex>`（hex 无前缀，
  "local" 大小写不敏感）。XML 首地址必须 32 位对齐（R-119cd）。M1 用 uncompressed `.xml`。

---

## Q7. DeviceHandler trait 无 peer 上下文，CCP 无法区分 primary application
- **涉及**：simulator regmap CCP 状态机、gentl-gvcp `DeviceHandler` trait
- **规范出处**：GigE Vision 3.0 §10.1（R-068cd 要求设备记录 primary 的 IP/端口/权限；R-069cd
  primary 重复请求同权限 MUST 接受；R-070cd primary 可直接切换权限）、§10.3/§10.4、§36.46
- **待确认**：`DeviceHandler::read_reg/write_reg` 是否需要携带 peer (ip, port) 参数（或传入
  上下文对象），否则设备侧无法实现规范的完整 CCP 语义。改动 gentl-gvcp 超出本切片范围，
  留待下次可动该 crate 时处理。
- **临时处理**（simulator 简化模型，代码已注释）：无占用时按请求位授予 EA（优先）或 CA，
  connection_count（bits 16-19）每次打开 +1、释放后不归零（§36.46）；占用期间**相同**权限
  重写一律视为 R-069cd 刷新接受（无法分辨是不是 primary 本人），**不同**权限写拒绝
  `GEV_STATUS_ACCESS_DENIED`；写 0 一律释放（无法校验是否 primary）；不支持 CSE
  （GVCP Capability bit10=0，CSE 位读出 0，写入被丢弃）；独占期间的 monitor 读限制
  （§10.1 "no other application can control or monitor"）未实现，读路径永远不封锁。

## Q8. Version 寄存器（0x0000）——§36.1 明确语句找到（补充 Q1）
- **涉及**：simulator regmap、Q1 的格式与取值疑问
- **规范出处**：GigE Vision 3.0 §36.1："Version 3.0 of this specification shall return
  0x00030000. The previous version of this specification being 2.2, shall return 0x00020002."
- **待确认**：无——Q1 的两个疑点均可澄清：① 编码 `major<<16 | minor` 理解正确（0x00030000
  即 major=3 在高 16 位）；② 设备报告**其实现的**规范版本，3.0 设备应报 0x00030000。
  遗留的只是本项目策略选择：sim 目前只实现 2.2 兼容子集，故报 0x00020002（沿用 Q1 临时
  处理）；未来声明 3.0 支持时需同步改此值与 DISCOVERY_ACK 的 spec_version（Q2 同理）。
- **临时处理**：维持 `0x00020002`。

## Q9. PORT_INFO_PORTNAME 的取值
- **涉及**：gentl-gige lib.rs GCGetPortInfo（PORT_INFO_CMD=12）
- **规范出处**：GenTL 1.6 §6.4.5（PORT_INFO_PORTNAME "Name of the port as referenced in the XML description"）；SFNC 2.7 GenTL 端口命名
- **待确认**：远程设备端口在 GenApi XML 中的标准端口名。暂用 "Device"（多数 GigE producer 的惯例），未对照 SFNC GenTL 章节原文核实。
- **临时处理**：返回 "Device"。

## Q10. TL_INFO_PATHNAME 未实现
- **涉及**：gentl-gige lib.rs TLGetInfo（TL_INFO_CMD=6）
- **规范出处**：GenTL 1.6 §6.3.2.2（"Full path including file name and extension of the library"）
- **待确认**：需要用 GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS) + GetModuleFileNameW 取本 DLL 路径（windows-sys feature Win32_System_LibraryLoader），M1 未加该依赖。
- **临时处理**：返回 GC_ERR_NOT_AVAILABLE。

## Q11. 发现机制的测试/广域网钩子与 ACCESS_STATUS 启发式
- **涉及**：gentl-gige discovery.rs（GENTLITE_DISCOVERY_TARGETS 环境变量）、IFGetDeviceInfo ACCESS_STATUS
- **规范出处**：GenTL 1.6 §6.2（producer 可在 IFOpenDevice 时自行重跑发现）；GigE Vision 3.0 §16.1（定向广播）
- **待确认**：① 为 loopback 测试设备/子网外相机引入的 unicast 目标列表（逗号分隔 `ip:port`，
  绕过子网过滤）属实现自有扩展，review 是否保留为正式特性；② IFGetDeviceInfo 的
  ACCESS_STATUS 在未探测其他主机占用时报告 READWRITE（探测需要额外 GVCP 事务，M1 未做）。
- **临时处理**：环境变量仅在设置时生效；ACCESS_STATUS 按"本进程是否已打开"报
  READWRITE / OPEN_READWRITE。

---

## Q5-resolved. 迷你 XML Schema 1.1 节点集——GenApi 实际加载验证通过 `[RESOLVED]`
- **结论**：GenApi 3.5 参考实现（`GenApi_MD_VC141_v3_5.dll`，C++ consumer，见
  `consumer-test/test_genapi.cpp`）经 `CNodeMapFactory(ContentType_Xml, buf, size)` 成功解析
  sim XML：node map 建立，22 个节点。`DeviceVendorName`（StringReg，32B bootstrap 槽）
  经 GCReadPort 读回 "GenTLite"；`ExposureTime`（Float→FloatReg，f32 BE）默认值 10000.0
  读通、`SetValue(12345.0)` 写回后忽略缓存回读 12345.0（真实 GCWritePort/GCReadPort
  回环）。Q5 定下的最小节点集（4 个身份 StringReg + Width/Height/PixelFormat +
  ExposureTime + Root 三大 Category）Schema 1.1 合法性成立。

## Q12. 本机 GenICam SDK 拷贝不完整（已修复，记录备查）`[RESOLVED]`
- **涉及**：consumer-test 的 GENAPI_ROOT（默认 `D:/code-lib/camer_UI/GenICam`）
- **现象**：该目录 `library/CPP/include/` 只剩 4 个顶层文件（GenICam.h 等），`Base/`、
  `GenApi/` 等子目录缺失（不完整拷贝）；GenICam.h 的 `#include <GenApi/GenApi.h>` 无法解析。
- **修复**：从官方包
  `camera_prj/scripts/GenICam_Package_2025.10/.../GenICam_V3_5_0-Win64_x64_VC141-Release-SDK.zip`
  解出 `library/CPP/include/**` 补齐到 SDK 目录；zip 内 `lib/Win64_x64` 与已装 lib 逐字节
  一致，bin/ 运行时 DLL 原本就齐。SDK 现可正常编译/链接/运行。
- **遗留**：该 SDK 目录在仓库外，换机/重装需重做此补齐或指向完整 SDK（CMake 的
  `-DGENAPI_ROOT` 可覆盖）。

## Q13. GenApi 3.5 C++ consumer API 事实（以 SDK 真实头文件为准）`[RESOLVED]`
- **涉及**：consumer-test/test_genapi.cpp 的 GenApi 用法
- **规范出处**：`D:/code-lib/camer_UI/GenICam/library/CPP/include/GenApi/{NodeMapFactory.h, PortImpl.h, INodeMap.h, IFloat.h}`
- **更正一个既有假设**：GenApi 3.5 的 `CNodeMapFactory` **不处理 URL**——
  只有 FileName（文件系统路径，可含环境变量）/buffer/string 三种构造。`local:` URL 的
  解析与取数（解析 `local:<file>;<addr hex>;<len hex>` 后经 GCReadPort 拉 XML）由 consumer
  完成，再把 XML buffer 交给工厂。本实现即按此路径，GenApi 侧验证不变。
- **已核实签名**（GenApi 3.5, GENAPI_NAMESPACE=GenApi_3_5，别名 `namespace GenApi`）：
  - `CNodeMapFactory(EContentType_t, const void* pData, size_t DataSize, ...)`；
    `INodeMap* CreateNodeMap(const gcstring& DeviceName = "Device", ...)`；
    析构 nodemap：`GenApi::CastToIDestroy(pNodeMap)->Destroy()`（NodeMapRef.h，Win 下 inline）。
  - 端口接入：自定义类继承 `GenApi::CPortImpl`（实现 `GetAccessMode()/Read/Write`，
    `Read(void*, int64_t Address, int64_t Length)`），`IPortConstruct : virtual public IPort`
    故可直接 `nodeMap->Connect(&port)`（默认端口名 "Device"，与 XML 的 `<Port Name="Device">` 对应）。
  - `IFloat::SetValue(double, Verify=true)`；`double GetValue(Verify=false, IgnoreCache=false)`
    —— `IgnoreCache=true` 才强制走端口回读（验证写回必须用它）。
  - SDK 自带 `GenTL.h` 在 C++ 下把所有函数与 `GC_ERROR` 类型放进 `namespace GenTL`
    （`bool8_t` 留在全局）；PGC* 函数指针 typedef 可直接用于 GetProcAddress 强转。
- **ABI 观察**：VC141 (VS2017) MD 导入库 + DLL 在 VS18 (cl 19.51) 下编译/链接/运行通过
  （MSVC 自 VS2015 起二进制兼容）；GenApi 头文件打印 "untested compiler version 1951"
  提示，属参考实现的版本白名单噪声，非错误。consumer 必须 /MD
  （`MSVC_RUNTIME_LIBRARY MultiThreadedDLL`），否则跨 DLL 堆管理不一致。

---

## Q14. SCPx/SCPSx 寄存器语义：sim 用"原始值"约定，与规范 §36.57/§36.58 的位布局不同 `[RESOLVED]`
- **涉及**：gentl-simulator regmap/emitter、gentl-gige receiver（对接约定）、
  docs/10-protocol/gvcp-wirefacts.md（记载与规范冲突，待回填修正）
- **规范出处**：GV 3.0 §11.7（"bit 16 to 31 provides the packet size"；test packet
  总大小 "including IP and UDP headers"）、§36.57 SCPx（bits 16-31 = host_port，
  bit0 = direction 只读）、§36.58 SCPSx（"total packet size, including all headers
  (IP header = 20 bytes, UDP header = 8 bytes and GVSP header)"；packet_size 在
  bits 16-31；bit0 = fire_test_packet、bit1 = do_not_fragment；Factory Default =
  device-specific）
- **结论（2026-10-10 按规范原文更正，全项目已统一为规范布局）**：
  - **SCPx = host_port << 16**（gentl-gvcp `registers::scp_make/scp_host_port`）；
    host_port=0 关闭通道；流式期间非零写回 GEV_STATUS_BUSY（R-586cd）；SCPx 是
    开通道最后一个写的寄存器（§36.57，SCPSx 协商例外）。
  - **SCPSx = packet_size << 16**（bits16-31 含 IP(20)+UDP(8)+GVSP(20) 头的总
    包长；`scps_make/scps_packet_size`）；generic payload 包数据长 = packet_size
    − 48（`GVSP_HEADERS_OVERHEAD`）；写后须读回（设备可取整，R-562st）。
  - sim SCPS0 出厂值 = `scps_make(1400+48)`（即总包长 1448，payload 1400，
    IPv4 总 1448 < 1500 MTU）；SCPS0=0 或小于头的非法值由 producer/emitter
    各自回退（见 Q21）。
  - sim SCP0 钩子按 `scp_host_port(v)` 解析；**bits16-31=0 但值非零的裸端口
    写法**（如直接写 50000）→ GEV_STATUS_INVALID_PARAMETER——这是对规范布局
    的校验，让旧裸值写法的失败足够响亮。
  - producer（gentl-gige stream.rs）开通道顺序：确认 SCP0 host_port=0（R-561ca，
    残留非零先写 0 关闭）→ 写 SCDA0 → 写 SCP0=scp_make(port) → 读回验证
    host_port 非零；接收 stride = scps_packet_size(读回 SCPS0) − 48。
  - wirefacts.md "流通道打开与运行" 一节已按 §36.57/§36.58/§11.5/§11.7/§11.8
    回填更正。
- ~~临时处理~~（已被上述结论取代，勿恢复）：sim 按 wirefacts/任务约定实现：SCP0 非零 → 打开通道（>0xFFFF
  拒绝 GEV_STATUS_INVALID_PARAMETER，使规范风格写法的失败足够响亮）；SCPS0=0
  → 1400。fire_test_packet / do_not_fragment 未实现——按规范写 (size<<16)|1
  会被当作巨大 SCPS 值 clamp 而非触发 test packet。接收端（gentl-gige）
  对接时按 sim 约定写字典：裸 16 位端口、裸 payload 字节数。

## Q15. sim 的流语义：通道打开即连续发块（无 AcquisitionStart 门控）
- **涉及**：gentl-simulator emitter、GenApi XML、gentl-gige 接收端集成测试
- **规范出处**：SFNC AcquisitionControl（真实相机由 consumer 写
  AcquisitionStart 才出图）；GV 3.0 §11.5（打开流通道）
- **待确认**：真实相机出图由 AcquisitionStart/AcquisitionStop 门控，sim 简化为
  "SCP0≠0 即连续发假图块，帧间隔 ~1ms，SCP0=0 即停"，XML 也无
  AcquisitionStart/AcquisitionStop Command 节点。接收端集成测试不要期待
  AcquisitionStart 语义，打开通道后应直接收块。
- **临时处理**：M2 保持此简化；`SimCamera::with_stream` 的 StreamConfig 会同时
  播种 Width/Height/PixelFormat 用户寄存器与 XML Width/Height Max，保证
  XML、寄存器、流三者一致。

## Q16. 包大小协商缺失：M2 保持相机 SCPS0 默认值，producer 不写字典
- **涉及**：gentl-gige stream.rs（`GevDataStream::open/start`）
- **规范出处**：GV 3.0 §11.7（包大小协商流程，R-567s）、§36.58 SCPSx（fire_test_packet
  自检包；SCPSx 布局已按 Q14 resolved 的规范位布局实现）
- **待确认**：规范流程是 app 打开通道后用 test packet 迭代搜索（二分），每轮
  "写 SCPSx（不带 fire 位）→ 读回取整值 → 或上 fire_test_packet|do_not_fragment
  发测试包"。M2 的 producer 只读 SCPS0 推出 payload stride（packet_size−48，
  首包可再推断），不写寄存器；真机网卡 MTU 小于相机默认包大小时会整帧丢失（丢包
  计数可见但不重传——重传是 M3）。与 sim 对接无此问题（1448 < 1500）。
  代码结构已在 `GevDataStream::start` 标注 M3 挂接点（SCP0 写后、receiver 启动前）。
- **临时处理**：注释标 Q16；M3 实现 PACKETRESEND 时一并做协商。sim 侧
  fire_test_packet / do_not_fragment 也未实现（写入被 scps_packet_size 掩盖，
  无 test packet 行为）。

## Q17. STREAM_INFO_PAYLOAD_SIZE 的 GEV 语义：用"最近一帧的期望值"
- **涉及**：gentl-gige lib.rs DSGetInfo（cmd 7/9）
- **规范出处**：GenTL 1.6 §6.4.4 STREAM_INFO_CMD_LIST（PAYLOAD_SIZE "Size of the
  expected data in bytes"；DEFINES_PAYLOADSIZE "this data stream defines a
  payload size independent from the remote device"）
- **待确认**：商用 GEV producer 一般从设备 nodemap 的 PayloadSize 特性取值，
  本 producer 无 GenApi，无法在首帧前得知期望负载。当前实现：
  DEFINES_PAYLOADSIZE=0（GEV 负载由远程设备定义），PAYLOAD_SIZE 返回最近一帧
  leader 推出的 width×height×bpp（含 padding；首帧前为 0）。consumer 若依赖
  首帧前的 PAYLOAD_SIZE 会拿到 0——待 review 是否更保守地返回 NO_DATA。
- **临时处理**：按上述实现，集成测试只断言流稳定后 PAYLOAD_SIZE == 实际帧大小。
- **2026-11-11 修复**：`DevOpenDataStream` / `DSStartAcquisition` 前增加最小
  GenApi 解析器（`gentl/gige/src/payload.rs`）：从 First URL 的 `local:` XML
  解析 `Integer Name="PayloadSize"` 的 `<pValue>` 链到 `IntReg`/`MaskedIntReg`，
  经 GVCP 读取真实寄存器；支持 zip XML，`http:`/`file:` URL 不抓取。XML 无
  可用特性或读取失败时仍回退到首帧 leader。LD 的启动前检查由 0 变为真实
  payload。缓存的是特性地址，不是值；ROI/PixelFormat 变化后下一次
  `STREAM_INFO_PAYLOAD_SIZE` 会重读寄存器。

## Q18. DSStopAcquisition（default）对块内帧的等待上限
- **涉及**：gentl-gige gvsp_receiver.rs（receiver_loop 的 STOP_GRACE=2s）
- **规范出处**：GenTL 1.6 §6.4.4.3 ACQ_STOP_FLAGS（default = "待填充中的 buffer
  等当前任务完成后再停"）；GVSP 无块取消机制
- **待确认**：GVSP 没有"取消当前块"的信令，default 停止只能等 trailer 自然到
  达。若设备恰在块中途停机/断流，无上限会一直等到 trailer。当前实现等待
  2 秒（STOP_GRACE）后将未完成块按 incomplete 交付（IS_INCOMPLETE=1，与 KILL
  同路径）。2 秒为拍脑袋值，待 review。
- **临时处理**：注释标 Q18；超时路径已有单测覆盖（abandon incomplete）。

## Q19. Data Stream ID 字符串格式
- **涉及**：gentl-gige stream.rs STREAM_ID、DevGetDataStreamID/DevOpenDataStream
- **规范出处**：GenTL 1.6 §3（"IDs of the data streams must be fix between all
  sessions"；格式未强制）、§6.3.4.3
- **待确认**：规范只要求会话间固定，未规定格式。当前取 "Stream0"。商用
  producer 有 "0"/"Stream0" 两种惯例（Vimba 系 "Stream0"、部分 GigE CTI "0"）。
  选择影响 consumer 持久化/按 ID 打开的行为。
- **临时处理**：STREAM0="Stream0"；DevOpenDataStream 只认该 ID（其它 →
  GC_ERR_INVALID_ID）。

## Q20. 不完整块的处置策略 ~~M2 直接丢弃~~ → M3-1 重传等待窗口 `[RESOLVED 2026-10-19，M3-1 实现]`
- **涉及**：gentl-gige gvsp_receiver.rs（on_trailer 的 gap/size 检查）
- **规范出处**：GenTL 1.6 §6.4.4.4/§6.3.5.12（producer 可选交付不完整 buffer
  并以 BUFFER_INFO_IS_INCOMPLETE 标记）；GV 3.0 §16.7 重传
- **结论**：M3-1 落地——trailer 到达时仍有缺口 → 块进入重传等待（不再即弃）：
  初始窗口 1ms 后对每缺口发 PACKETRESEND（§16.7.3 QoI，Aravis 值），20ms 间隔
  重发，FRAME_RETENTION（默认 100ms）后仍缺才按 incomplete 丢弃。trailer 的
  packet_id（=末包 id+1，R-264st 序）登记尾部缺口——尾部整段丢失也可恢复。
  设备未声明 PACKETRESEND 能力或 `GENTLITE_GEV_RESEND=0` 时保持 M2 行为
  （trailer 有缺口即弃）。IS_INCOMPLETE 仍只在 KILL 停止时产生（不交付半帧）
- ~~临时处理~~：M2 的"丢弃"实现已被上述取代；接收统计保留所有 dropped 计数

## Q21. SCPS0 读回为 0 / 非法小时的 producer 与 sim 回退值（1500 MTU 假设）
- **涉及**：gentl-gige stream.rs（`GevDataStream::open`）、gentl-simulator
  emitter.rs（`StreamParams::new`）
- **规范出处**：GV 3.0 §36.58（packet_size 为含头总包长；SCPS0 出厂默认
  device-specific；设备对不支持的值取整 R-562st）。规范未规定"寄存器读回 0
  或小于头开销时应用该怎么办"——真机应总有合法出厂默认。
- **待确认**：producer 端 SCPS0 读回 0 或 packet_size ≤ 48（头都装不下）时，
  当前回退为 **1500−48 = 1452 payload**（假设 1500 字节 MTU）；sim emitter 端
  同样情况回退 **1400 payload**（= 其出厂默认）。两个回退值不同（producer 按
  MTU 上限、sim 按自身默认），且 1500 假设对巨型帧/jumbo MTU 网卡非最优——
  M3 协商实现后此回退只剩"设备出厂默认非法"的防御路径，届时是否保留待 review。
- **临时处理**：按上述回退实现；gvsp_receiver 若 stride 为 0 仍可按首包推断
  （双来源兼容，见 gvsp_receiver.rs Block::stride 注释）。

## Q22. 未知像素格式帧：截尾检测丧失 + BUFFER_INFO_SIZE_FILLED=0
- **涉及**：gentl-gige gvsp_receiver.rs（on_leader/on_trailer）、gentl-core
  ds.rs（`on_frame` 零拷贝路径的 filled_size）
- **规范出处**：GV 3.0 §25.2（image leader 的 pixel_format 可为任意 PFNC 值）；
  GenTL 1.6 §6.4.4 BUFFER_INFO_SIZE_FILLED
- **待确认**：leader 的像素格式无已知 bytes_per_pixel 时（不在
  `pixfmt::SUPPORTED`），M2 起**不再拒收**：按 stride 组帧、trailer 行数判定
  完成、容量校验保留（存不下即丢）。两个已知能力损失：
  ① **截尾不可检**——trailer 的累计字节数无从比对（on_trailer 的 size 检查对
  未知格式跳过），尾部丢包只能通过 trailer 行数/后续块间接发现；
  ② **BUFFER_INFO_SIZE_FILLED=0**——零拷贝交付时 `filled_size` 取
  `meta.payload_bytes().unwrap_or(0)`，未知格式恒为 0，而实际 payload 字节已
  写入 buffer。要把真实字节数（receiver 的 bytes_written）贯通到 filled_size
  需改 gentl-core `FrameSink::on_frame` 签名（本次改动范围之外），待 M3 一并
  处理或接受此限制。
- **临时处理**：按"接受 + trailer 判定 + filled_size=0"实现，有单测
  （unknown_pixel_format_reassembles_and_completes_via_trailer）。
- **进展（2026-10-14）**：已大幅收窄——PFNC 全表 273 个格式经生成器
  （`scripts/generate_pixfmt.py` 从 include/PFNC.h 机械提取，显式枚举而非
  推导，用户裁定 2026-10-14）纳入 pixfmt_table，`SIZE_FILLED`/截尾检测对
  全部 273 个格式精确成立；U3V 侧未知格式不再丢弃（leader 推不出尺寸时
  回退 SIRM SI Required Payload Size，§5.4.1.3）。仅剩真·未知值
  （CUSTOM/未分配）走 GigE=trailer 判定 / U3V=SIRM 尺寸 的降级路径。

## Q23. sim emitter 的填充图案仅对 Mono8/Mono16 像素级精确
- **涉及**：gentl-simulator emitter.rs（`fill_pattern`/`StreamParams::bytes_per_pixel`）
- **规范出处**：无规范约束（sim 内部测试图案）；PFNC 字节宽度见 include/PFNC.h
- **待确认**：emitter 的渐变图案按像素编号生成：Mono8（1 B/px）与 Mono16
  （2 B/px，R-344st 小端）是像素级精确的；StreamConfig 若配 Bayer8 系列
  （1 B/px）图案同样逐字节成立；但 **RGB8/BGR8（3 B/px）/YUV422_8（2 B/px）**
  目前按 1 B/px 生成，图像内容无语义、组帧不受影响（接收端按字节重组）。
  如需对彩色格式做端到端像素断言，应扩展 `fill_pattern` 到任意 bpp。
- **临时处理**：保持 Mono8/Mono16 精确、其余格式字节级确定性；新格式的接收
  端语义由 gvsp_receiver 单测（leader 改写像素格式）覆盖，不经 sim 流。

---

*新增疑问按 Q12、Q13… 追加，勿删旧条目。*

## Q24. READMEM 在 bootstrap 寄存器区被相机拒绝（真机实测）`[RESOLVED 按规范]`
- **涉及**：gentl-gige device.rs `ControlChannel::read_mem`
- **规范出处**：GV 3.0 §16.3/§16.5/§19；**GV 1.2 §14.3/§14.5/§14.7（用户提供的
  1.2 规范，2026-10-12 查证）——两版规范均无地址分区规则**，两条命令都是合法读
  手段，设备对不支持的地址返回 0x8001 NOT_IMPLEMENTED（§19 明确定义）
- **现象（2026-10-12，仿制相机实测）**：READREG 全区可用；READMEM 在 0x0000/0x0004
  返回 0x8001，在 0x0200/XML 区正常；READREG 在 0x0200 静默返回 0
- **结论**：按规范实现——**默认 READMEM，收到 0x8001 回退逐寄存器 READREG**
  （规范状态码驱动，无启发式边界）。曾用 0x0200/64B 启发式路由，已删除。


## Q25. ~~非标相机的~~ SCPx/SCPSx 编码（真机实测）~~`[RESOLVED 纯规范，不做兼容]`~~ **`[SUPERSEDED by Q31 2026-10-16：该相机合规，是我方误读]`**
- **裁定反转**：该相机（Version=1.2）行为 = Aravis mask 全集 = 规范 MSB-first 正确解读；当时作为
  "非标证据"的"裸端口生效/<<16 被忽略/0xC000 顶位"全部符合规范。Q25 补充决策（纯规范零兼容立场）
  本身保留且继续成立——修正位布局后立场自动贯彻：合规相机（含此相机）零改动可工作。
- **涉及**：gentl-gige stream.rs 通道打开；gentl-gvcp registers.rs 互操作辅助
- **规范出处**：GV 3.0 §36.57/§36.58；**GV 1.2 §27.45/§27.46（2026-10-12 查证）——
  两版布局完全一致：host_port/packet_size 均在 bits 16-31，从未变过**（1.2 出厂
  默认 0x40000240）
- **现象（仿制相机，自称 GigE 1.2，实测 Version=0x00010002）**：SCP0 写裸端口生效、
  写 port<<16 被静默忽略；SCPS0=0xc0001f40 按规范解析 packet_size=0xc000 无效，
  实际 size=0x1f40(8000) 在低 16 位。**该设备违反 1.2 §27.45/§27.46 原文**；
  MVS 能驱动它说明 MVS 有同款兼容
- **结论**：主路径严格按规范（<<16）；打开读回验证失败时做一次裸值回退重试
  （interop fallback，代码注释+Q25 标注）；SCPS0 提取值超过 Ethernet jumbo 上界
  (9018) 时尝试低 16 位解释。守规范设备永不走回退路径。


### Q25 补充决策（2026-10-12，用户裁定）
**项目立场：不做任何非标设备兼容，只实现最标准的规范。** 曾实现的 SCP 裸值回退
与 SCPS 低 16 位重解释已删除（`legacy` 辅助函数与测试一并移除）。对非标设备的
行为是**干净失败**：开通道读回验证不符即返回明确错误。修复方向是修改相机固件
使其合规（本相机违反条款清单见 Q25 现象节），改好后本 producer 零改动即可工作，
Basler 等其它 consumer 同样受益。

## U3V 疑问（M4，2026-10-12）

> U3V producer（crates/gentl-u3v）实施中规范未覆盖或需要人裁定的点。
> 线格式事实见 docs/10-protocol/u3v-wirefacts.md。

### UQ1. U3V 匹配代码：0xEF/0x02 是设备级，接口级是 0xEF/0x05
- **涉及**：discovery.rs 匹配规则、README 计划表
- **规范出处**：U3V 1.2 §3.2.1 Table 3-1（Device Descriptor 强制 EF/02/01，
  因 IAD 存在）、Table 3-2（IAD：EF/05/00）、Table 3-3/3-6/3-8（控制/事件/流
  接口：EF/05/00、EF/05/01、EF/05/02）
- **背景**：任务简报与 README 计划行写的是 "bInterfaceClass=0xEF,
  SubClass=0x02"——与规范原文不符（0x02 是设备级 subclass，源于 IAD）。
- **决策（2026-10-12）**：按规范实现。枚举匹配用 SetupAPI 兼容 ID
  `USB\Class_EF&SubClass_02`（设备级，纯注册表路径可得）；能打开的设备再用
  WinUSB 读配置描述符，确认 IAD EF/05/00 + 控制接口 EF/05/00 + Device Info
  描述符（R-15cd）。打不开的设备列入清单但 ACCESS_STATUS=NOACCESS。
  README 计划行已改正。

### UQ2. Device ID 策略
- **涉及**：discovery.rs `FoundDevice::id`
- **规范出处**：§3.1.2 R-1cd（GUID = 12 位十六进制，VID+厂商唯一值）
- **决策**：可打开（读到 Device Info 描述符）→ ID = GUID 字符串本身；打不开
  → 退化 `VID:PID:Serial`（实例路径解析，仅供用户辨认，ACCESS_STATUS 已是
  NOACCESS）。R-5ch 禁止用标准设备描述符字符串代替 Device Info 字段，故
  字符串一律来自 Device Info 描述符/ABRM，不用 iManufacturer/iProduct。

### UQ3. Interface 呈现：每 USB 根集线器一接口（LDVision 风格）
- **涉及**：TLOpen/TLGetInterfaceID、discovery::list_root_hubs
- **规范出处**：GenTL 1.6 未规定接口划分；实测依据：本机 dump_cti 观察
  LDVisionTL.cti 的 U3V 部分 = 每个 XHCI 根集线器一个接口
  （ID=`LDVision::U3V::<PCIROOT…#USBROOT(0)>`，DISPLAYNAME=
  "LDVision USB3 Vision Interface [USB 根集线器(USB 3.0) (XHCI)]"），
  与 GigE 侧 Basler 单聚合风格不同
- **决策**：沿用 LDVision 每根一接口。理由：根集线器是 Windows 上稳定可枚举
  的 USB 拓扑单元，设备→根的归属可用 CM_Get_Parent 链确定；聚合风格对 U3V
  没有"多 NIC 统一发现"的对等收益。接口 ID = 根集线器设备实例 ID，
  DISPLAYNAME = `GenTLite USB3 Vision Interface [<总线报告名>]`，TLTYPE=U3V。

### UQ4. GCGetPortURL 的 URL 形式
- **涉及**：control.rs `xml_url()`、GCGetPortURL/GCGetNumPortURLs/GCGetPortURLInfo
- **规范出处**：U3V 无 URL 寄存器（XML 经 Manifest 的地址+长度获取）；GenTL.h
  定义 URL_SCHEME_LOCAL=0 "XML can be read from the local register map"
- **决策**：返回 GenApi local 惯例
  `Local:<Model>.xml;<地址大写十六进制>;<长度大写十六进制>`（地址/长度是
  Manifest 事实；文件名段是占位——消费者经端口读 XML 时不使用该段）。
  URL_INFO_SCHEME = URL_SCHEME_LOCAL。待真机验证 Basler/LDVision 消费者的
  解析行为。

### UQ5. GenTL Exclusive 访问在 U3V 上无对应机制
- **涉及**：IFOpenDevice（iOpenFlag=4）
- **规范出处**：ABRM Access Privilege（0x00204）标注 "Not used for this
  standard"（Table 4-6）
- **决策**：Exclusive 暂按 Control 处理（打开后不附加排他语义），代码注释标注。

### UQ6. 控制通道超时后的重新同步未实现
- **涉及**：control.rs `transaction` 错误路径
- **规范出处**：§4.1.4.1.2（ENDPOINT_HALT set/clear 重同步）、§4.1.4.1.1
- **现状**：失败事务后仅 WinUsb_FlushPipe(IN) 清残留应答；不做 ENDPOINT_HALT
  序列（WinUSB 无直接 Set/ClearFeature 封装，需经 control transfer）。持续失步
  的设备会干净报错而非恢复。

### UQ7. DEI 事件通道未接 GenTL 事件投递
- **涉及**：stream/device 事件路径；packets.rs 事件编解码（已实现+测试）
- **规范出处**：§4.1.4.2（EIRM、Event Enable）、R-28ch（host MUST 使能 DEI）、
  §6.3.7（GenTL 事件）
- **现状**：codec 完整（U3VE prefix + EVENT_CMD SCD），但 Event Enable 保持 0、
  无事件读取线程、EVENT_FEATURE_INVALIDATE/Event Port 投递未实现。R-28ch 是
  对"控制设备的宿主软件"的强制，与 GenTL 事件对象模型（gentl-core 事件队列
  目前只服务 DS 的 EVENT_NEW_BUFFER）接好后再开。

### UQ8. GenDC payload 模式未实现
- **涉及**：stream.rs `start()`
- **规范出处**：§5.2.2（Container/Serial Flow/Parallel Flow）、§5.4.1.13
- **现状**：SI Payload Mode ≠ Standard（bit0）时 DSStartAcquisition 返回
  NOT_IMPLEMENTED，不做静默降级。

### UQ9. §5.2.1.3 例子的 "WinUSB 1KB 倍数/≤64KB" 限制无法从规范核实
- **涉及**：stream.rs `plan_geometry`（TS = min(1 MiB, 按对齐向上取整的
  required)，count/final 取商余）
- **规范出处**：§5.2.1.3 例子（"limitations enforced by the WinUSB driver on
  Windows"，非规范性文字，且其算式 4×64K+63K+1K=327680 与所述 326680 不符）
- **决策**：只遵守规范性规则（SIRM alignment，0xA003 报错）。1 MiB 传输在
  现代 WinUSB 上可用；若真机出现传输失败，再按 1KB/64KB 粒度重编程并回本条。

### UQ10. GenCP Stacked 命令（0x0806-0x0809）未接 GenTL v1.1 端口接口
- **涉及**：GCReadPortStacked/GCWritePortStacked 仍为桩
- **规范出处**：GenCP §4.3.3 Table 7；GenTL.h v1.1
- **现状**：Device Capability bit13 报告设备能力，producer 侧暂不实现。

### UQ11. STREAM_INFO_BUF_ALIGNMENT 的取值依据
- **涉及**：DSGetInfo
- **规范出处**：GenTL.h（BUFFER alignment，生产者定义）；U3V SI Info 的
  Payload Size Alignment 只约束 SIRM 尺寸寄存器（§5.4.1.1），不约束宿主缓冲
- **决策**：返回 1（不做额外基址对齐要求）；SI Info 对齐只用于传输规划。

## Q27. GVCP flag 字节 ACKNOWLEDGE 位编码：0x80 还是 0x01？
- **涉及**：gentl-gvcp wire.rs `FLAG_ACK`
- **规范出处**：GV 3.0 §15.1 Figure 15-8 示例原文：flag=0x01，"bit 7 is SET… Since
  bit 7 is the lsb, the resulting value will be 0000 0001b (0x01)"——规范图采用
  msb 先行位编号，按该编号 ACK 位值=0x01
- **反证（实测）**：2026-10-12 真机（LeadingOE 仿制相机）会话中，producer 以
  flag=0x80 发出数十条 READREG/WRITEREG/READMEM，相机**全部正常应答**——若
  相机严格按规范图编号实现（0x80=ACK 清零），规范明文"NO ack message is sent"，
  不应有任何应答。说明实机生态按 lsb 编号（bit7=msb=0x80）实现或忽略该位
- ~~**当前处理**：维持 0x80~~ **【2026-10-16 更正】已改 0x01**：规范图例（Fig 15-8
  flag=0x01）+ Aravis（ACK_REQUIRED=0x01）+ pyGigEVision（FLAG_ACK=0x01）三方一致，
  我方 0x80 是孤例；原"真机实证"更可能是该相机忽略 flag 位（非应答编码证据）。
  仍保留真机复核项：合规相机到货后用 probe_camera 双编码各发一条 READREG 对比。
- 审计报告 F-1（判"致命"）经此实测证据降级为"编号歧义记录"，非缺陷

---

## 上真机前合规审计处置记录（2026-10-12）
双路径审计报告见 `compliance/preflight-gige-u3v.md`。处置摘要：
- **GigE**：F-2（重试改 req_id，违反 R-047ca）已修；S-3（发现地址 239.255.255.255
  无规范出处）已改 255.255.255.255（§5.1.1）；F-1（flag 0x80/0x01 编号歧义）经真机
  实测证据裁决维持 0x80，记 Q27；S-9/S-10 注释债务已清。另审计漏网项"接收线程死亡
  引擎悬挂"（同 U3V C16）已一并修复（`DsCore::abort_engine` + 事件 kill）。
- **U3V**：D18（GCInitLib 双调/GCGetLastError 空参语义，§6.3.1.3/§6.3.1.4，两 crate
  同病）已修；D19（DevClose 对流线程 WinUSB 句柄 UAF）已修（接收线程持 Arc 设备）；
  C16（接收线程死亡悬挂）已修（同 GigE 方案）；B11（AbortPipe 后未 ResetPipe）已修。
- 审计提出的真机验证项全部转入报告第 5 章清单。

## UQ12. 复合设备枚举匹配策略（真机驱动，2026-10-13）
- **涉及**：gentl-u3v discovery.rs `enumerate_devices`
- **规范出处**：U3V §3.1/§3.2——设备级 EF/02 声明在 IAD 中，主机侧只能通过
  描述符或操作系统伪影推断
- **实测（海康 U3V 相机，VID_2BDF）**：复合设备的注册表 CompatibleIds 为
  DevClass_00（IAD 不体现在注册表）；`USB\Class_EF&SubClass_02` 只在声明
  device-class 的设备上出现；接口子设备（MI_00）携带 Class_EF&SubClass_05
- **实现**：两级匹配——直接声明 EF/02 的设备，或"COMPOSITE + 存在 EF/05
  接口子设备"的复合设备（父子关系必须经 CM_Get_Parent，实例 ID 字符串无法
  推导——子接口实例后缀是 hub 端口路径，父的是序列号）；打开后仍以描述符
  三重确认（§3.2.5）。REG_MULTI_SZ 曾只读首段导致漏匹配，已修。
- **驱动绑定**：相机绑海康私有 MvisionUSB 驱动时枚举可见但 openable=false
  （与 LDVision 实测行为一致）——GenTL producer 经 WinUSB 打开，需要设备
  绑定 WinUSB 驱动（解绑方法见 docs/00-governance/PENDING.md 与报告）。

## UQ13. Windows 上 U3V 设备打开的绑定相关路径链（真机驱动，2026-10-13）
- **涉及**：gentl-u3v discovery.rs `device_interface_paths` / `open_device`
- **规范出处**：U3V 规范不规定主机侧打开方式（producer 自由）；以下全部为
  Windows 平台实测结论（海康 MV-CS016-10UC，VID_2BDF）
- **结论（按尝试顺序，前者优先）**：
  1. SetupDi 全局枚举 `GUID_DEVINTERFACE_WINUSB`（{F72FE0D4-...}，winusb.inf
     的 Needs=WINUSB.NT 绑定注册）——匹配设备实例（含 MI_* 子设备）
  2. SetupDi 全局枚举 `GUID_DEVINTERFACE_USB_DEVICE`（复合设备由 usbccgp 注册）
  3. **注册表 DeviceClasses 扫描**（绑定无关的终极方法）：HKLM\SYSTEM\
     CurrentControlSet\Control\DeviceClasses\<GUID>\##?#USB#... 子键名即设备
     路径——Zadig/libwdi 风格 INF 用**自定义 DeviceInterfaceGUID**（本机实测
     {2F3C3EC2-...}），固定 GUID 列表无法覆盖
  4. 规范路径合成（\?\USB#...#{a5dcbf10...}）——CreateFile 复合父设备句柄成功
     但 WinUsb_Initialize 报 error 8：**复合父句柄不能用于 WinUSB，必须打开
     接口（MI_00 控制接口）路径**
- **附带发现**：`WinUsb_GetAssociatedInterface` 似乎一次性——同一 handle 上
  重复调用失败（诊断工具与 collect 竞争时暴露）；生产路径只调一次无碍

## UQ14. ABRM 可选字段必须按 Device Capability 门控（真机驱动）
- **涉及**：gentl-u3v control.rs `read_bootstrap`
- **规范出处**：GenCP §5.2 Table（Device Capability 位定义可选字段存在性）
- **实测**：海康相机未实现 FAMILY_NAME，读 0x84 回 GENCP_INVALID_ADDRESS
  (0x8003)——曾导致 bootstrap 整体失败。修复：先读 DEVICE_CAPABILITY(0x01C4)，
  FAMILY_NAME(bit8)/USER_DEFINED_NAME(bit0) 按位门控
- **GenApi XML 间接寻址**（grab 工具层面）：相机用 MaskedIntReg +
  `<pAddress>REF</pAddress>` → Integer 常量 `<Value>0x..</Value>` 的地址间接
  链（AcquisitionStart_Reg → 0x30804 等），解析器需支持多级间接

## UQ15. Device Capability 位表提取陷阱与相机实证（2026-10-13）
- **涉及**：gentl-u3v packets.rs `abrm::CAP_*` 常量
- **经过**：真机验收后复核规范符合性时，`pdftotext -layout` 对 GenCP Table 21
  （跨栏位图表）的列错位导致**误读为另一套位编号**（FamilyName=bit14/SBRM=bit15），
  据此"修正"了本来就正确的常量；相机实证（capability=0x0e09 且 SBRM 功能正常）
  暴露矛盾后，用 `pdftotext -raw` 重提规范页 54 得到正确行序：
  **bit0 UserDefinedName / bit1 AccessPrivilege / bit2 MessageChannel / bit3
  Timestamp / bits4-7 StringEncoding / bit8 FamilyName / bit9 SBRM / bit10
  EndiannessReg / bit11 WrittenLength / bit12 MultiEvent / bit13 Stacked /
  bit14 DevSWVersion**。原实现常量正确，已恢复并加双重验证注释
- **教训（流程级）**：跨栏位表格必须用 `-raw` 模式提取或双模式交叉验证；
  单模式提取 + 字面"修正"是纪律明文禁止的猜测变体。相机 0x0e09 与规范位图
  逐位吻合（UserDefinedName+Timestamp+SBRM+EndiannessReg+WrittenLength），
  构成第三方的三重证据（规范原文 / 实机寄存器 / 实机行为）
- **复核结论（本轮全部 U3V 改动）**：枚举匹配、打开路径链、PIPE_TYPE 修正
  （USBD bulk=2）、ABRM 能力门控（先读 0x01C4 再按位读可选字符串寄存器）——
  除已记录修正外均符合规范；打开路径链为规范沉默的主机侧实现细节（UQ13）

## Q28. 自研 GigE 相机（V4 改造版）合规复测（2026-10-13）`[2026-10-16 更正：整改项 #1/#2 方向错误，已撤销]`
- **更正（Q31）**：#1（SCPx host_port）与 #2（SCPSx packet_size）的"违规"判定系我方
  误读规范印刷位（MSB-first）；该相机行为符合规范，**固件无需整改**。producer 位布局
  已修正，该相机（含 MVS 生态行为一致的设备）零改动即可工作。表中 #3/#4/#5 及其余
  结论不受影响。
- **对象**：LeadingOE LD-V4-SW640170590S-C-G，S/N 252036，192.168.1.203，MAC 00:30:53:9c:db:03
- **对照清单**：Q25/Q26 五条固件整改项

| # | 整改项 | 规范依据 | 结果 | 实测证据 |
|---|---|---|---|---|
| 1 | SCPx host_port 写 bits 16-31 | 1.2 §27.45 / 3.0 §36.57 | ❌ **未整改** | 写 49152<<16 读回 0（被忽略）；写裸 49153 读回 0xe2ef=58095≠49153 验证写法差异见注 |
| 2 | SCPSx packet_size 写 bits 16-31 | §36.58 / 1.2 §27.46 | ❌ **未整改** | 出厂值 0xc0001f40，规范解析 size=0xc000 无效，实际语义 size=0x1f40(8000) 在低 16 位 |
| 3 | GVSP 帧结构 | §24.2/§24.3（3.0）、1.2 §24 | ✅ **合规（以 1.2 设备身份）+ 整改生效** | 防火墙放行后诊断抓包 55254 个：leader(485)+payload(54285)+trailer(484) 成对出现（旧固件全 payload 无 leader/trailer——整改生效）；8 字节头 EI=0、16 位 block_id 块内恒定、trailer 闭合——与其 Version=1.2 声明一致（1.2 设备本用 16 位 block id，3.0 §24.2 指向 2.0 规范）；payload_type=0x0001、pixel_format=0x01100005(Mono12) 可解析；SCPS=1500 被尊重（包 1472B） |
| 3a | **我方缺口**：receiver 只解析 EI=1 二十字节头 | 1.2 §24（用户提供的 1.2 规范） | ⚠️ **producer 侧待实现** | 对 1.2 式 8 字节头（16 位 block_id）的接收支持是规范内特性（非非标兼容），利于所有守规范 1.2 相机；不解决本相机开流（仍被 #1 阻塞） |
| 4 | 响应广播 DISCOVERY_CMD | §5.1.1/R-140c | ❌ **未整改** | 255.255.255.255 与 192.168.1.255 定向广播均无应答；仅单播有。MVS 可见（驱动层）不构成合规证据 |
| 5 | PayloadSize 端序与 XML 一致 | —（一致性整改） | ✅ **已整改** | XML 声明 BigEndian，实际大端（读 8389120 合理），与 Q25 记录的旧行为（XML 标 LE 实为 BE）相反 |
| 附 | Version 寄存器 | §36.1 | ✅ 合规 | 0x00010002（1.2） |

- **注**：裸写 49153 读回 0x0000c001 = 49153 生效——相机的 SCP 语义仍是"host_port 在低 16 位"。
- **producer 侧行为**：按"纯规范零兼容"立场，SCP0 读回验证失败 → 干净报错
  "stream channel did not open"，不做任何回退（Q25 决策不变）。
- **遗留**：#3 的 GVSP 验证需防火墙放行 192.168.1.203 后补测（裸值通道仅作诊断，
  不进 producer）。

## UQ16. WinUSB 管道策略与"笔记本丢帧"类问题（2026-10-14）
- **背景**：用户 UVC 开发经验——WinUSB 在笔记本上等时传输丢帧，开启 RAW_IO 解决
- **分析**（规范沉默区的主机侧实现细节；U3V 规范全文无 RAW_IO/管道策略条款）：
  1. UVC 是**等时传输**，RAW_IO 对等时管道是必需的——该经验不直接迁移
  2. U3V 是 **bulk 传输**；WinUSB 默认 `ALLOW_PARTIAL_READS=TRUE`（MSDN），
     读请求"有多少返多少"——而本接收机按精确几何读（leader/payload/trailer
     定长），**部分主控（笔记本类）上的部分返回会直接破坏组帧**，这是更可能
     的丢帧机制
  3. 处置（已实现，`usbio::set_stream_pipe_policy`）：流管道设
     `ALLOW_PARTIAL_READS=FALSE`（读满或短包终止才返回，语义与接收机精确
     匹配）；**RAW_IO 刻意不用**（它把完成粒度变为每 USB 事务，bulk 无收益
     且需全面改造读累积逻辑）
  4. 控制通道保持默认策略：GenCP ACK 以短包终止，单次读即得完整应答
- **验证状态（2026-10-14 更新）**：**RAW_IO 已实施并真机验证**——流管道
  启用 RAW_IO（`usbio::set_raw_io`），接收端遵守 MPS 对齐契约（请求长度
  对齐 wMaxPacketSize：leader/trailer 读到对齐缓冲、非对齐尺寸的 payload
  片走暂存+拷贝、对齐片直读进 consumer buffer 零拷贝）；组帧状态机不变
  （"短包终止 → actual_length 即片长"语义 RAW_IO 下保持）。海康相机
  200/200 帧复验通过，回归 102 全绿、0 警告。
- **累积读取修正（2026-10-14，ROI 测试暴露）**：RAW_IO 下"一次 ReadPipe
  返回 = 一个 U3V transfer"的假设被证伪——设备自身会在传输中段发送短事务
  （1MiB 请求返回 203180 字节的实据），部分返回曾被当作完整部件导致
  帧尾留白（间歇零帧）。修正：payload 走**计划尺寸精确累积**
  （read_piece_exact：ZLP 跳过、超长判 Desync），leader/trailer 走
  **容量上限 + 短事务/ZLP 终止**（read_piece_capped），所有请求经
  aligned_window 保证 MPS 对齐。验证：ROI 开窗全流程（基线 30/30、开窗
  720x540@40,24 读回精确、PayloadSize 联动 1555200→388800、开窗流 50/50、
  曝光读写、恢复 30/30）+ RAW_IO 10000/10000 + 非 RAW_IO 5000/5000 A/B +
  回归 106 全绿。附带发现：该相机 Size 写入会复位 Offset（写序：先 Size
  后 Offset；恢复时先 Offset 清零再 Size）。完整档（overlapped 读池）
  仍挂 docs/00-governance/PENDING.md R1 按触发条件执行

## Q29. Streaming Configuration 写保留值（streaming_protocol = 2-3）的设备行为
- **涉及**：gentl-simulator regmap（§36.38 写语义）
- **规范出处**：GV 3.0 §36.38——streaming_protocol 字段定义 "0: GVSP, 1: GVRSP,
  2-3: reserved. Ignore when reading"；对**写**保留值的行为规范沉默
- **待确认**：规范只说了"读时忽略"，未定义设备收到写 2-3 该怎么办
- **临时处理**（sim 侧 QoI 裁定）：写 2-3 回 GEV_STATUS_INVALID_PARAMETER
  （与"写不支持的模式"同一错误面，失败得足够响亮）；producer 永不写保留值。
  真机若实测某相机接受/忽略写保留值，回本条修正 sim

## Q30. GVRSP 模式下 GVSP 通道寄存器的访问门控范围（CR-671cd 字面外的外推）
- **涉及**：gentl-simulator regmap（GVRSP 模式门控）
- **规范出处**：CR-671cd 字面只要求"device operating in GVRSP mode 时对
  **SCPx** register 回 GEV_STATUS_ACCESS_DENIED"；对 SCPSx/SCPDx/SCDAx
  在 GVRSP 模式下的访问行为，规范沉默（§11.12 只定义了反向：GVRSP 寄存器
  在非 GVRSP 模式不可访问）
- **当前处理**（外推裁定）：整个 GVSP 通道块（SCPx/SCPSx/SCPDx/SCDAx，
  含 READMEM 路径）在 GVRSP 模式下一律 ACCESS_DENIED——依据 §11.12 精神
  （两种协议的寄存器集互斥），但超出 CR-671cd 字面条文
- **待确认**：严格抠字面应只挡 SCPx，其余按普通规则（未实现通道回
  INVALID_ADDRESS）；真机 GVRSP 设备实测后可回填，或用户裁定为收窄

## Q31. 规范寄存器表的位编号约定 = MSB-first（"bit 0 是最高有效位"）——Q14/Q25 系统性误读 `[RESOLVED 2026-10-16 已修复，用户批准]`
- **修复结论**：全部位常量按规范算例/Aravis/pyGigEVision 修正（registers.rs、wire.rs FLAG_ACK=0x01、
  sim regmap 同步）；112 测试全绿、0 警告；回归锚定测试 = 规范算例 0xC000219C（scps_packet_size
  == 8604）与 1.2 出厂默认 0x40000240（== 576）。Q14/Q25/Q27/Q28 结论由本条取代
- **涉及**：gentl-gvcp 全部位常量（scp/scps/IP_CFG/CCP/GVCP Capability/SCAP/SCONF…）、
  sim regmap、producer CCP/通道打开序列；Q14/Q25/Q28 的结论
- **规范出处（原文亲验）**：
  - GV 3.0 规范约定章（pdftotext 行 3482-3483）："Whenever a byte represents a
    numeric quantity the leftmost bit in the figure is the most significant bit.
    **Therefore, the bit labeled 0 is the most significant bit**"
  - GV 1.2 Figure 13-8 同款例句："bit 0 is the most significant bit"（写 SCPS0 =
    0xC000219C：bit0=F→0x80000000、bit1=D→0x40000000、bits16-31=包长→**低 16 位**）
  - 3.0 §15.x 同款（flag 字节例，Q27 已录）
- **误读机制**：Q14/Q25 把规范印刷的"bits 16-31"按 LSB-first 实现成 `<<16`；
  正确含义（MSB-first 编号）= 低 16 位（mask 0xFFFF）。"bit 0/1" = 0x80000000/
  0x40000000 而非 0x1/0x2
- **四重独立证据**：
  1. 规范自身的 SCPS0 算例（1.2 Fig 13-8 / 3.0 同文）：0xC000219C 只有按
     MSB-first 才自洽（8604 = 0x219C 在低 16 位）
  2. 1.2 §27.46 出厂默认 0x40000240：DF(0x40000000) + size=576(0x240 低 16 位)，
     按 LSB-first 读则 size=0 且保留位非零、不可解释
  3. Aravis（与数百款相机互通的事实标准，main 分支 arvgvcpprivate.h 亲验）：
     SCPS size mask=0xFFFF、FIRE=1<<31、DF=1<<30、endian=1<<29；CCP EXCLUSIVE=
     1<<0、CONTROL=1<<1；IP_CFG=1<<0/1<<1/1<<2；GVCP CAP PACKET_RESEND=1<<2
  4. Q25 的"仿制 1.2 相机"实测行为（接受裸端口、SCPS 低 16 位 + 0xC000 顶位）
     = Aravis mask 全集——**原"非标"裁定的证据方向反了**
  5. 旁证：3.0 GVCP Capability 印刷"bit29 PACKETRESEND"↔ Aravis 1<<2，
     逐位满足 31-x 镜像（concat 31↔0、serial 1↔30、heartbeat 2↔29…）
- **受影响常量现状**（registers.rs 亲查）：`IP_CFG_*=1<<31/30/29`、
  `CCP_*=1<<31/30/29`、`scp_make=port<<16`、`scps_make=size<<16`、
  `FIRE=1<<0`、`DF=1<<1`、Q30 的 SCAP/SCONF 位——全部与线格式真实值镜像反转
- **结论反转**：Q25 相机是合规 1.2 设备；Q28 固件整改项 #1/#2（要求厂家改
  port<<16）方向错误，厂家无错；我方 producer 对任何合规真机的 CCP/SCP 写
  入都不成立（从未真机验证所以未暴露）
- **待用户裁定**：按 Aravis/规范算例修正全部位常量（含 sim 同步、测试、
  Q14/Q25/Q28 结论改写；"纯规范零兼容"立场不变——修正后合规相机全兼容，
  Q25 相机零改动即可工作）
- **双引擎复核（2026-10-16，回应"文本抽取是否精准"的质疑）**：关键寄存器页
  用 pdftotext 的第二种排版引擎 **-lineprinter**（固定字符网格，保留几何列
  位置）重提，与 -raw 结论逐项一致：
  - 3.0 §36.57 SCPx（物理页 322）：位图 D 在印刷 bit0 列、NII 在 14-15、
    host_port 横跨印刷 16-31 右半 → 低 16 位 ✓（并修正注释：3.0 NII 是
    14-15，1.2 是 12-15）
  - 3.0 §36.58 SCPSx（物理页 325）：bits 0=F、1=D、2=deprecated（2.0 前的
    pixel_endianness，3.0 已废弃——注释已更新）、3-15 reserved、16-31=
    packet_size ✓
  - 3.0 §36.46 CCP（物理页 312-313）：0-15=switchover_key、16-19=
    connection_count（→0xF000 ✓）、29=CSE、30=CA、31=EA ✓
  本机无 PDF 渲染器（无 pdftoppm/gs/mutool，PyPI 不可达装不了 PyMuPDF），
  -lineprinter 的字符网格几何是当前可用的最接近"看图"的验证手段；渲染级
  验证留待有网络环境时补（Q31 复核清单）。
- **渲染级复核（2026-10-16 第二轮，回应 pdfplumber 要求）**：pip 索引进程损坏
  （curl/urllib 均 200 但 pip 对任何包返回 "versions: none"），pdfplumber 的
  依赖树手动下载受阻；改用 **pypdfium2 5.9.0（PDFium 引擎，独立于 Xpdf）**
  安装成功并**渲染关键页为图片直接目视**（D:/tmp/gv*.png，可复查）：
  - 3.0 页 114：约定章原图"bit 0 (msb) on the left… the bit labeled 0 is the
    most significant bit" + Fig 15-4/5/6（msb 在左、lsb 在右）✓
  - 3.0 页 322 SCPx 图：D 在印刷位 0、NII 在 14-15、host_port 横跨印刷 16-31
    右半 → 低 16 位 ✓
  - 3.0 页 323 SCPSx 图：F/D/deprecated 在印刷位 0/1/2、packet_size 占印刷
    16-31 → FIRE=0x80000000、DF=0x40000000、size=低 16 位 ✓
  - 3.0 页 311 CCP 图：switchover_key 0-15、connection_count 16-19、CSE/CA/EA
    在印刷位 29/30/31 → 0x4/0x2/0x1、count mask 0xF000 ✓
  - 1.2 页 254：SCPx 表 + SCPSx 图 + **出厂默认 0x40000240 同页目视** ✓
  至此 Q31 修复链达到六重：渲染图 + 双引擎文本 + 规范算例 + Aravis +
  pyGigEVision + 真机行为。

## Q32. pdfplumber 全量核验（2026-10-16 第三轮，三引擎终审）发现的两项
> 用 pdfplumber 0.11.3（pdfminer.six 引擎）对两份 GigE 规范全量重提，逐项核对
> 代码后的剩余发现。Table 36-1 全部地址、§19 状态码表、1.2 §14 命令字典
> 逐项通过（见会话报告）；本条记录未修的偏差。

### Q32-1. sim 的 NET_IF Capability 未声明 DHCP —— 偏离 Table 36-1 的强制要求 `[待用户裁定]`
- **规范出处**：3.0 Table 36-1（0x0010）："DHCP and LLA bits must be on"；1.2 §27.5 同
- **现状**：sim 广告 persistent|LLA（0x5），DHCP 位（0x2）置 0；sim 不运行 DHCP 客户端
- **两难**：声明未实现的能力会误导 consumer（设 DHCP 位无行为）；不声明违反 MUST
- **倾向**：保持 0x5（诚实不声明），sim 作为测试台以"不撒谎"优先；真机不受此影响
  （producer 只读该寄存器）。用户可裁定改为 0x7。

### Q32-2. 3.0 在 GVCP Capability / Device Mode / SCPSx 增设 deprecated always-1 位 `[已修正/已记录]`
- **规范出处**：3.0 §36.29 print bits 9/13/26/30 deprecated=1；§36.2 print bit 0 deprecated=1；
  §36.58 print bit 2（原 pixel_endianness）deprecated
- **处置**：sim 自称 Version 2.2 → Device Mode 弃用位**已修**（0x00000001 → 0x80000001，
  §36.2 "must be set to 1 since 2.0"，Aravis 同位旁证，112 测试复验通过）；
  GVCP Capability 的 3.0 deprecated 位（13/26/30）按 1.2 §27.28 布局**不设**（2.2 身份
  正确）；若未来 sim 升 3.0 需补（0x00040000|0x20|0x2）。


---

## Q33. sim 接受流式中带 FIRE 的 SCPSx 写 + producer 的 FIRE-only 协商回退 `[sim QoI 裁定 + producer QoI 偏离，2026-10-16]`
- **涉及**：gentl-simulator regmap `write_scps` / emitter 测试包发送；
  producer `stream.rs` §11.7 协商（`negotiate_packet_size`）
- **规范出处**：
  - §11.7（3.0 页 83 逐字核实）："This packet size negotiation must
    happen before any 'regular' stream packets are issued... Test packet
    negotiation occurs right after opening the stream channel"；
    R-567s（MUST：协商在通道打开后、常规流前）；
    "If a test packet is requested while regular streaming is active, the
    device **should** return a GEV_STATUS_BUSY status code"（小写 should，
    非强条款）；"If a test packet is requested while the stream channel
    is closed, then the device can either ignore the request or return a
    GEV_STATUS_INVALID_PARAMETER status code"
  - §36.58：SCPSx 流式中写封锁是 sim 的 QoI 外推（继承 R-586cd 的 SCPx 规则），
    非规范逐字要求
- **矛盾**：sim 无 AcquisitionStart 门控（Q15）——SCPx 一打开调度器即持续发
  帧，"打开后、常规流前"的协商窗口在 sim 上不存在。若 sim 对打开后的 SCPSx
  写一律 BUSY，则 producer 的 §11.7 三步探测第一步（无 FIRE 写）永远失败，
  协商对 sim 不可测——而 sim 正是本阶段唯一的端到端测试台
- **sim 裁定（QoI）**：带 FIRE 位（0x80000000）的 SCPSx 写**永远接受**（即使
  流式中）并立即发一个 §24.5 测试包；不带 FIRE 的写仍 BUSY（§36.58 外推规则
  保持，现有断言不动）。FIRE 写携带的取整后尺寸被**运行中的发射器采纳**
  （`update_packet_size`）：真实设备上 SCPSx 即流的包尺寸，"打开时冻结"
  快照只在值不变时成立；sim 的自流使值可变，故同步更新
- **producer 配套（QoI，偏离简报"静默放弃"的字面指令）**：首个无 FIRE 写被
  拒（transaction error）时**不**直接回退现状，而是整轮搜索改用 FIRE-only
  探测（每个探测写都带 `fire|DF`，读回取整值）——FIRE 写是"容忍测试包的设备
  在协商窗口内仍必须接受的写"（§11.7 三步的第三步；closed-channel fire 规范
  明确允许忽略，open-channel fire 无拒绝条款）。FIRE-only 写也被拒、或下界
  测试包不到达，才静默回退（读 SCPS 推 stride，老设备/只读）。真机走标准
  三步路径不受影响（协商窗口内设备非流式，无 FIRE 写本就被接受）
- **待确认**：真机若实测"打开后、采集前"对无 FIRE SCPSx 写回 BUSY（即设备
  把"打开"等同"流式开始"），producer 的 FIRE-only 回退已覆盖；对 FIRE 写也
  BUSY 的设备（严格 §36.58 + §11.7 should-BUSY 解读）→ 静默回退，行为安全。
  实测后回填本条


---

## Q34. 2.0 式 EI=0 GVSP 头布局未经 2.0 规范核验；1.2 布局以规范图实测为准 `[已按 1.2 图 + Aravis 双证据实现，2.0 待回填]`
- **涉及**：gentl-gvcp `gvsp.rs`（legacy 8 字节头 / 36 字节 leader body 编解码）、
  producer `gvsp_receiver.rs`（decode_auto 接收判别）、sim legacy 发射模式
- **规范出处**：3.0 §24.2（EI 位：EI=1 → 64 位 block id；"For information on
  16-bit block IDs, refer to the GigE Vision Specification 2.0"）；1.2 §24
  Fig 24-1/24-3/24-15（8 字节头、leader/trailer body）
- **2.0 原文缺失风险**：3.0 明确把 EI=0 的解释权指向 2.0 规范，而仓库无 2.0
  文档。若 2.0 在 EI=0 头中引入过 1.2 没有的字段（如 flag 字段），则 2.0 相机
  会被误解析。当前实现按 **1.2 §24 布局**解析 EI=0（真机 1.2 相机抓包 Q28 一致）；
  拿到 2.0 规范或 2.0 相机后回填本条
- **1.2 布局的三重核验**（2026-10-19，简报原描述与规范图冲突后的复核链）：
  1. **规范渲染图目视**：pypdfium2 渲染 1.2 Fig 24-1/24-3/24-15 为图直接读
     （D:/tmp/gv12_*.png）+ pdftotext -lineprinter/-layout 双引擎文本：
     - 头 8 字节：`status u16@0-1 | block_id u16@2-3 | packet_format **整个
       字节4**（像素级测量约 8 bit 宽）| packet_id **u24@5-7**`（Fig 24-3 标注
       "packet_id = 0x000000" 明示 24 位；**无 flag 字段**）
     - leader body **36 字节**：`reserved u16 | payload_type u16 | timestamp u64 |
       pixel_format/size_x/size_y/offset_x/offset_y（各 u32）| padding_x/y（各 u16）`
       （末行 "padding x | padding y" 合占 32 位 → 各 u16）
     - trailer body 8 字节：`reserved u16 | payload_type u16 | size_y u32`
       （与 3.0 现行编解码一致，复用）
  2. **Aravis 0.8.30 独立确认**（main 分支 arvgvspprivate.h 亲验）：legacy
     `ArvGvspHeader{frame_id u16, packet_infos u32}`（packet_infos = format 在
     字节4 低 7 位 + packet_id 24 位，掩码 0x7f000000/0x00ffffff），leader
     body {flags u16, pt u16, ts 8B} + {pf,w,h,xo,yo u32 + xpad/ypad u16}
     = 与规范图逐字节一致；Aravis 与数百款 1.2 相机互通
  3. **Q28 真机抓包**（55254 包）：8 字节头 EI=0、16 位 block_id 块内恒定、
     pt/pixel_format 可解析、SCPS=1500 → 1472B UDP 载荷（=1500−28，36 字节
     GVSP 头开销语义，1.2 §27.46）
- **更正记录**：M3-9 简报原引布局（packet_format@字节2高半字节、block_id@4-5、
  packet_id u16@6-7、leader body 42 字节无 reserved、padding u32）**与规范图
  不符**，已按"不符则以规范图为准"的简报规则修正为上图布局；简报担心的
  "block_id 高位置位导致字节4顶位=1 误判"系该错误布局的假象——真实布局字节4
  是 format 字节（值 ≤0x7f 顶位恒 0），decode_auto 判别无歧义
- **SCPS 语义**：1.2 设备 packet_size 计数 IP20+UDP8+GVSP8 = **36** 头开销
  （Q28 算例核实）；3.0 为 48。producer 的 §11.7 协商对此无感（只读写 SCPSx
  数值），组帧 stride 统一由首 payload 包推断，不消费 SCPS 语义

## Q35. 自研相机（LeadingOE 改良版）fire_test_packet 中毒（真机实测 2026-10-19）`[固件缺陷，报厂家]`
- **对象**：00:30:53:9b:db:6f @ 192.168.1.92（S/N 262041，新版固件）
- **现象（可复现）**：§11.7 协商向 SCPSx 写入带 FIRE 位（0x80000000）的值后，
  相机对**后续所有 WRITEREG 不再回 ACK**（超时 -1011），控制通道永久失应，
  直至相机整体挂死（MVS 也无法发现，需断电重启）。flag 0x01/0x80 均如此，
  排除 Q27 编码问题；不带 FIRE 的 SCPS 写、读、开通道全部正常
- **对照**：固定包长模式（GENTLITE_GEV_PACKET_SIZE=1500，无 FIRE 写）全流程正常
- **处置**：producer 不改（行为符合 §11.7/R-093st——设备侧缺陷）；
  本机用固定包长验收；**建议报厂家修复 fire_test_packet 路径**
- **验收记录**：1000/1000 帧完整（1280x1024 Mono8，1.2 式 8 字节头，
  EI=0 legacy 布局经真机三重确认 = Q34 实现），~38fps

## Q34 补充（2026-10-19 真机第三重确认）
真机 leader 逐字节：`status=0 block=1 fmt=1 | reserved=0 pt=1 | ts | pf=0x01080001
sx=0x500 sy=4`——与 Q34 实现（渲染图 + Aravis 双确认）的 36B 布局**逐字段一致**，
含 u16 reserved 前缀与 u16 paddings。1.2 式接收路径真机毕业。

## Q27 补充（2026-10-19 真机裁决）
flag 0x01 与 0x80 对同一相机的 READREG/WRITEREG **都应答**（flag_probe 双发实测）——
维持 0x01（规范图例 + Aravis + pyGigEVision）不变，相机大概率忽略该位。
真机复核项销账。

## Q36. Phase A Linux 移植的主机侧 QoI 裁定组（2026-10-21，规范沉默区）
- **涉及**：`gentl-gige/src/sys.rs`、`gentl-simulator/src/sys.rs`、
  `gentl-gige/tests/consumer_dll.rs`
- **规范出处**：GigE Vision 3.0 / GenTL 1.6 对主机侧网卡枚举、socket 调优、
  线程绑核、测试包 DF 位设置**均无条款**——全部为 QoI（实现质量）决定，
  按"不猜"纪律记录于此（代码内均有同名 QoI 注释）。
- **裁定**：
  1. **Unix 网卡枚举**用 `getifaddrs`（与 Windows GetAdaptersAddresses 同
     地位的主机侧实现细节）。差异：getifaddrs 不携带硬件 MAC，Windows 侧的
     "≥6 字节物理地址"过滤在 Unix 不可复刻——放宽为"up + 非 loopback +
     有 AF_INET 地址 + 有 netmask"（无掩码则跳过：无法算定向广播）。
     按接口名聚合取首个 IPv4，与 Windows "每适配器取首个单播"对齐。
  2. **测试包 DF 位（§24.5）**：Linux 无逐包 DF 套接字选项，用
     `IP_MTU_DISCOVER=IP_PMTUDISC_DO`（setsockopt 失败静默）——DF 是
     尽力而为，与 Windows 侧"setsockopt 失败不阻塞测试包"同语义。
  3. **线程绑核**：`sched_setaffinity(0, ...)` + CPU_SET；u64 mask 只覆盖
     低 64 核（两平台一致）；mask=0 不调用（Windows 侧 SetThreadAffinityMask
     传 0 语义不同，显式规避）。
  4. **SO_RCVBUF 读回**：Linux 会把请求钳到 `net.core.rmem_max` 且按内核
     记账翻倍读回——"读回值才是真实生效值"的契约两平台一致（压测基线需
     先把 rmem_max/wmem_max 提到 32MB，已记入 docs/20-design/gige-100g-rx-design.md §8）。
  5. **consumer_dll 回归测试双平台化**：同一测试在 Unix 加载
     `libgentl_gige.so`（libloading 跨平台），导出完整性 + 端到端控制面
     在 Linux 得到与 Windows 等价的验证。
- **遗留（Phase B）**：gentl-u3v 的 rusb/libusb 后端（docs/00-governance/PENDING.md
  "Linux 可移植"）。

## Q35 补充（2026-10-22，相机 00:30:53:9b:db:6c @192.168.1.89）：IP 分片是控制面失能的确切触发器
- **发现**：linux-vgige v1 用单次 65524B READMEM 拉 XML → 13KB 数据报在 1500 MTU 链路上 IP 分片
  → 相机不应答且控制面逐级失能直至假死（ICMP 仍活）。producer 全程 512B 分块（无分片）故从未触发；
  环回 MTU 65535 无分片，模拟器 e2e 因此漏检
- **修复**：linux-vgige READMEM_CHUNK=512（与 producer 一致），注释已载明因果链
- **固件缺陷定位更精确**：该相机不能处理 IP 分片的 UDP（GigE Vision 相机通病之一——其 RX 路径
  不做分片重组）。报厂家建议条目："IP 分片的 GVCP 数据报导致控制面失能"（比 Q35 原述更精确）
- **影响面复查**：producer 现存代码路径（READMEM_CHUNK=512、SCPS 协商 test packet ≤ 64KB 总包长
  但在 1500 MTU 链路上协商本身限制在 ≤1500）均无分片风险；标记为已排查

## Q35 补充 2（2026-10-22，相机 00:30:53:9b:db:73 @192.168.1.96）：写小 SCPS 值杀出流
- **现象（可复现）**：SCPS0 写 1500（或自动协商的更小值）后，AcquisitionStart 正常应答但
  **零数据报出流**；写回出厂值 8000 立即恢复（100/100、1000/1000 复验）。该单元出厂 SCPS0=8000
- **用户工作流解释**：MVS 里"切比特位再切回"（PixelFormat 写两遍）的民间疗法，副作用恰好重置
  包大小状态机——与本发现一致
- **固件缺陷定位**：该单元固件不能处理 SCPS 向小值重配（疑似 DMA/buffer 重配 bug）。
  报厂家条目 #6："SCPSx 写小值后 GVSP 出流停止，需复位恢复"
- **工程侧处置**：GENTLITE_GEV_PACKET_SIZE=8000 绕开（operator override 本就是设计好的逃生舱）；
  producer 无需改动——§11.7 协商 + 读回验证是规范正确路径，固件缺陷由 env 兜底
- **旁证**：探测工具显式序列（不动 SCPS）在该单元 29906 数据报/1.5s 全正常；8KB 大包在千兆链路上
  IP 分片、Linux 接收端重组无碍（100/100 内容校验通过）

## Q37. Linux 后端的宿主侧身份裁定组（Phase B，2026-10-23）`[QoI 组裁定]`
- **涉及**：`gentl-u3v/src/linux/discovery.rs`、`usbio/libusb.rs`、
  `usbio/mod.rs`
- **规范出处**：U3V 1.2 §3 只规定设备侧描述符与 Device Info（R-1cd：
  GUID 是设备身份）；**主机侧枚举/持久标识规范沉默**——全部按 QoI 裁定
  记录（代码内同名 QoI 注释；事实细节回填 docs/10-protocol/u3v-wirefacts.md
  "Linux 后端事实"节）。
- **裁定**：
  1. **Root hub 呈现**：libusb 枚举设备不建模 hub。按"当前有设备的
     USB 总线"合成一 GenTL Interface（UQ3 一-root-hub-一-interface 模型
     的最贴近映射），ID 保持 `USB\ROOT_HUB30\BUS<n>` 家族形状。
  2. **设备 instance ID**：`<bus>:<address>`（十进制），**会话级**有效
     （重枚举/重插后变）；跨会话持久身份 = 打开后读到的 GUID。摄像头
     插电验收脚本用 `dump_cti`/`grab_u3v` 现读现用，不跨会话缓存。
  3. **打不开设备的退化 ID**：Windows 用实例 ID 里的 serial；Linux 的
     serial 字符串不开设备读不到 → 退化 `VID:PID:bus:address` +
     ACCESS_STATUS=NOACCESS（权限不足时装 udev 规则或 root 运行）。
  4. **描述符匹配一次完成**：Linux 见真描述符，Q12 的 Windows 注册表
     两级匹配在描述符层一次做完；Device Info 从控制接口 `extra()`
     重序列化 blob 喂共享解析器（双端同一套单测）。
  5. **错误恢复/策略映射**：flush→clear_halt（UQ6 在 usbfs 的等价物）；
     RAW_IO→no-op（usbfs 无此策略；UQ16 累积层必要且充分，双端单测
     锁定语义）。
- **待真机回填**：海康相机插板子 USB3 口后，核对枚举 GUID 与
  grab_u3v 全流程（下轮验收）。

## Q38. vgige-back-u3v 的 QoI 裁定组（Step 2，2026-10-26）`[QoI 组裁定]`
- **涉及**：`crates/gentl-u3v/src/client.rs`（client facade）、
  `vgige/vgige-back-u3v/`、`vgige/vgige-front-gige`（assemble_identity）、
  `vgige/vgige-cli`（features/netid）。
- **背景**：网关矩阵 "GigE 前端 ← U3V 相机" 格（无真机，fake GenCP
  设备全链路 e2e 见 vgige-cli/tests/gateway_u3v_e2e.rs）。
- **裁定**：
  1. **GenCP→GEV 状态码映射**：GenCP 1.3.1 §4.3.2.1 Table 6 与 GigE
     §19 Table 19-1 在公共码（0x0000、0x8001-0x8007）上同数值（命名
     空间有意对齐）——原样透传（错误透传契约）。GenCP 特有
     （0x800B/0x800E/0x800F）、0x8FFF、U3V 技术命名空间（0xA0xx/0xA1xx，
     R-38cd）GigE 无定义：数值透传 + 一次性日志（GigE 客户端对任何
     0x8xxx/0xAxxx 都按错误处理）。本地失败（USB IO/线格式错/窗口超时）
     → 0x8FFF 附录 B.21（与 GigeBack 相机不可达策略一致）。
  2. **原始状态码保留**：`GenCpClient` 加 `transaction_raw` /
     `read_mem_raw` / `write_mem_raw`（非成功状态以
     `GentlError::GevStatus(raw)` 出），producer 路径的 `transaction`
     不变（仍经 status_error → GenTL 错误码）——producer 零行为变化，
     库面获得逐位精确的状态码。
  3. **库形态块读取**：`read_block_alloc` 是 stream.rs 零拷贝
     `read_block` 的**内存汇姊妹实现**（同 §5.2.1.3 走片、R-127sr
     短片语义、§5.5.4 trailer 状态、CR-86st 高度修正），协议编解码与
     UQ16 累积层全部复用既有 pub(crate) 件；刻意不重构零拷贝路径
     （其逐错销账/DsCore abort 语义与 GenTL 事件耦合，共享会过度侵入）。
  4. **超时=看门狗**：`next_block(timeout)` 的取消源只有调用方超时，
     以每调用一条看门狗线程置停止旗标表达（UQ16 累积层是停止旗标驱
     动）。网关切面 ≤92 fps 下开销可忽略；若将来有高频调用方，换成
     deadline 变体累积层。
  5. **虚拟发现身份合成**：`assemble_identity` 改为
     `back.fetch_identity()`（各后端读本机 bootstrap：GigE=Table 36-1；
     U3V=ABRM 快照零线上读取）+ **本地合成** device_mode（0x8000_0001
     发送方类）与 ip_config（persistent+LLA / persistent）——这些字段
     描述的是**虚拟设备自己**（住在对外网口上），镜像相机网络值是
     旧行为（还向下游网络泄露信息）；子网随之从"相机"改为"对外网口"
     （netid 现返回 MAC+掩码），网关 0。**这是 GigE 格 discovery 载荷
     的一处有意行为变更**（§19 信息性字段；客户端不据其操作）。
  6. **U3V 无 close_stream 语义**：`CameraBack::close_stream` 在 U3V
     适配器为 no-op——SIRM Stream Enable=0（§5.4.1.2）由
     `U3vFrameSource::drop` → `U3vStream::stop` 完成（Facade Drop 双
     保险）；无 GigE SCP0 那样的常驻寄存器可写。
  7. **双 producer 同图链接冲突**：vgige-cli 二进制按编译特性二选一
     （`--no-default-features --features back-u3v`）——两个 GenTL
     producer 的 #[no_mangle] 导出同图链接必然符号冲突（MSVC
     LNK2005 / GNU ld multiple definition）；集成测试是独立链接单元，
     经 dev-dependencies 双后端齐备，与特性的取舍无关。
  8. **MockPipe 不提升**：fake 设备在测试内直接实现 `UsbPipe`
     （trait 已随 facade 导出）；MockPipe 的"脚本块长+图案字节"行为
     不是 fake 需要的（fake 要解析 GenCP 命令），保持 pub(crate)。
- **待真机回填**：U3V 相机插板子后跑
  `vgige --no-default-features --features back-u3v --back u3v
  --down <bus:addr> --up <up-IP>` 全流程（枚举/open/SIRM/出流经
  中继到 GigE 客户端）。

## Q39. vgige-front-u3v 的 QoI 裁定组（Step 3，2026-10-26）`[QoI 组裁定]`
- **涉及**：`vgige/vgige-front-u3v/`（device/ + gadget.rs）、
  `crates/gentl-u3v/src/packets.rs`（设备侧编解码下沉）。
- **背景**：网关矩阵最后一格——Linux 盒子经 USB gadget 口对 PC 呈现
  标准 U3V 相机。无真机：协议核心用 gentl-u3v 自己的客户端配对测试
  （vgige-front-u3v/tests/pairing.rs），整机集成待验。
- **裁定**：
  1. **设备侧编解码下沉 gentl-u3v**：packets.rs 本就"编解码双向"，
     Step 3 补齐设备侧缺件——`CommandCcd::decode`、`AckCcd::encode`、
     `decode_readmem_scd`/`decode_writemem_scd`、`encode_ack`/
     `encode_readmem_ack`/`encode_writemem_ack`（Written-Length 字段）、
     `encode_image_leader`/`encode_image_trailer`（§5.5.5.1.1/5.1.3，
     全部 round-trip 单测锁定）。packets/descriptors 模块转 pub
     （纯线格式件；描述符 Device Info 经 gentl-u3v 自家解析器往返）。
  2. **字节流抽象**：device/ 只依赖两条 bulk 字节流 trait
     （HostCommands=控制 OUT、DeviceTransfers=IN，含 MPS）。gadget.rs
     是 FunctionFS 实现；测试是内存 chunked 队列——**chunk=事务粒度**
     （一次读永不跨两个 chunk），否则主机 UQ16 累积会被"拼接读"
     绕过（开发中实际抓到并修复的竞态）。
  3. **SI Required Payload Size 来源**：设备在主机询问时不可能知道
     每帧大小 → 由操作者经 `--u3v-payload` 提供（写入 SIRM 0x08），
     主机按它规划传输几何（§5.2.1.3）。变高帧（CR-86st）在
     front-u3v 格不转发：设备按已编程几何出块，尾短补零（与真机
     固定几何模型一致，Q39 记录）。
  4. **开流触发=主机 Stream Enable**：front-u3v 没有 GVCP SCPx——
     设备的 SI_CONTROL 写回调（寄存器文件 watcher）触发
     `Relay::open()/close()`（Relay 新增的编程式开关）；FrameSession
     的 payload_hint 对 U3V 无语义（切片由主机 SIRM 编程决定），
     保留在签名中给固定切片前端。
  5. **GUID 合成**：U3V 身份是 GUID 不是 MAC——`synthesize_guid` =
     相机序列号+型号的确定性 FNV-1a 折叠 + RFC 4122 version/variant
     位；唯一性依托序列号。ep0 字符串与 ABRM 同源（同一
     DeviceConfig）。
  6. **gadget MPS/速度**：dwc3 superSpeed、bulk MPS 1024、bcdUSB
     3.2、Device Info bmSpeedSupport=0x08；单 full-speed 描述符集 +
     fs_count=1（hs/ss/os=0）——superSpeed 伴随描述符在 bring-up
     时补齐（待验项）。UDC 取排序第一个（多 UDC 主机应显式选）。
  7. **VID/PID**：pid.codes 私有测试分配 0x1209/0x7D01（0x7D01 为
     本工程自分配子项）；量产需自有 USB-IF id。
  8. **事件接口**：描述符保留 EF/05/01 事件接口（端点存在），设备
     不出事件（UQ7 同款后续）。
  9. **FunctionFS 头格式**：functionfs_descs_head_v2（magic=3 +
     length + flags + fs/hs/ss/os counts）按内核 uapi 结构编码；
     整机 bring-up 验证字节序与 counts 对齐（待验，代码注释已标）。
- **待整机回填**：Orange Pi Type-C 口 dwc3 + configfs 挂载 +
  libcomposite 加载后，`vgige --no-default-features --features
  back-u3v,front-u3v --back u3v --front u3v --down <bus:addr>
  --u3v-payload <N>`，PC 侧 pylon/MVS 枚举到虚拟相机并抓流。

## Q39 补充（2026-10-26，Orange Pi 5 Plus gadget 整机调试实录）
- **代码侧成果（已全部修复入库，双端 174/174）**：①configfs gadget 根目录未创建 ②FunctionFS
  必须挂载在 /dev/usb-ffs/<名>（configfs 函数目录内挂载 EPERM）③挂载源是裸实例名非 ffs.<名>
  （vendor 内核 6.1.115 实测语义）④CLI 纯 front-u3v 构建的默认前端选择
- **硬件阻塞（内核层，dwc3 设备模式起不来）**：UDC 注册/configfs/functionfs 全通，挂载成功
  ep0 出现，但 enable 时 `dwc3_gadget_start` 恒 -19(ENODEV)——stock overlay、自定义
  dr_mode="peripheral" overlay、maximum-speed="high-speed" 三案皆然
- **根因嫌疑链**（实测证据）：usb2-phy 缺 vbus-supply（启动报错）＋fusb302 TCPC 驱动已绑定但
  /sys/class/typec 为空（角色切换不发生）＋内核"Fixed dependency cycle(s)"可能拆断了
  fc000000.usb↔fusb302 的调节器依赖；vendor 6.1.115 BSP 内核
- **已验证无效**：内核/dtb 升级 26.8.3（同症状）；dr_mode 定点 overlay；HS 限速
- **下一步候选（按性价比）**：①Armbian mainline/edge 内核（社区 dwc3/Type-C 栈，apt 换源+
  重启即可试）②换一块 gadget 口碑好的板子（如 Radxa/RPi5 验证路径）③搁置（本项本就是
  Q39 记录的"整机待验"格，协议核心已内存配对全验证，不阻塞其他成果）

## Q39 补充二（2026-10-26 晚，mainline 6.12 打通至"待插线"）
- **内核方案落地**：装 `linux-image-current-rockchip-rk3588`（6.12.0）→ 自定义 overlay
  `rockchip-rk3588-custom-peripheral.dtbo`（target-path 用别名值 `/usb@fc000000`；mainline
  无 usbdrd3_0 包裹节点）：仅 `dr_mode="peripheral"` + `phys=<0x23>`(u2phy0_otg) +
  `phy-names="usb2-phy"`（甩掉 usbdp/fusb302 依赖链）。vendor 6.1.115 的 ENODEV 根因到
  6.12 上不再是绑定本身。
- **vgige-front-u3v 连环修复（均有内核源码/uapi 依据，非猜测）**：
  1. UDC 使能必须在 ep0 描述符握手之后（f_fs 无描述符拒绝 bind）——provision 拆出
     enable()/disable()，open() 先写描述符再使能；
  2. ep0 必须 O_RDWR 打开（写应答 EBADF）；
  3. mount 改 libc::mount 直调——util-linux 会把"在 CWD 存在的相对源"规范成绝对路径
     （strace 实证 `/root/GenTLite-public/vgige` → 内核按名查找 ENOENT）；
  4. **FF v2 记录真实格式**（uapi functionfs.h）：头=magic/length/flags(12B)，计数字段
     **按 flags 位出现**（多发一个词被当描述符解析 EINVAL）；length==单次 write 大小
     （f_fs.c 每次 write 独立校验，拆写必 EINVAL）；记录必须单次 write；
  5. **f_fs.c 白名单拒绝 CS_INTERFACE(0x24)**（仅 HID/CCID/DFU）——U3V Device Info CS
     不进 FF 记录，只留在 ep0 应答的 config blob（host-facing）里；
  6. **分速 MPS**：FS bulk=64 / HS bulk=512（FS 写 1024 → autoconfig 失败整 bind 报
     ENODEV——最初的 -19 真凶）；端点地址跨速集必须一致；
  7. **FF 字符串记录是 v2 格式**（str_count/lang_count/语言块内 NUL 结尾 UTF-8，非 USB
     字符串描述符——后者只用于 ep0 应答）：DescriptorSet 拆 ffs_strings/usb_strings；
  8. **ep0 是事件流**（struct usb_functionfs_event 12B，type@8，SETUP=4），非裸 8B setup；
  9. 端点文件 ep1..ep4 在 **bind 时**才创建（ffs_epfiles_create）——先 enable 再开 ep；
  10. 描述符集全面改正：host 侧 device/config 用 HS 形态（bcdUSB 0x0210、MPS0 64、
     bulk 512），provision 写 bcdUSB=0x0210 + max_speed=high-speed（U3V Table 3-1 是
     SS profile，HS profile 属规范沉默处裁定，留档）。
- **当前状态**：`vgige-cli --back null --front u3v` 整机绑定成功（dmesg 无 -19，ep0-ep4
  齐，VGIGE_READY），UDC state=not attached。**待办：PC 插板子 Type-C 线看枚举**。残留
  警告 `dwc3: failed to enable ep0out`（HID 对照实验同样报、PineTab2 案例带此报文仍可
  半枚举）；若实机枚举失败，下一张牌=Rockchip《RK3588 USB 开发指南》Type-C-USB2.0-only
  配方：`extcon=<&u2phy0>` + 禁用 usbdp_phy0{,_dp,_u3}。
- **fusb302 遗留**：mainline 6.12 上仍 `cannot register tcpm port`（唯一 deferred 设备），
  原因未查明（不影响 peripheral 强制定向；Type-C CC 方向/SS lane 换向需其修复，SS 属 v2）。

## Q39 补充三（2026-10-27，U3V gadget 整机验证通过：grab_u3v 50/50 帧）
- **整机毕业**：Windows `grab_u3v`（自家 GenTL producer，WinUSB 后端）对板载虚拟 U3V
  相机完成 50/50 帧收流（1280×1024 Mono8，GenCP 控制面 + SIRM 流全链路）。这是 2×2
  矩阵最后一个实机格。overlay 最终生效组合：`dr_mode="peripheral"` + `extcon=<&u2phy0>`
  （phandle 0xeb）+ `maximum-speed="high-speed"` + usb2-only phys + 禁用 usbdp_phy0
  （"failed to enable ep0out" 随之消失，枚举 configured/high-speed）。
- **关键机制澄清（全部源码/实证）**：
  1. 标准 GET_DESCRIPTOR 由 composite 层应答（composite.c），**永远不到 ffs 用户态**——
     设备名/字符串来自 ffs strings 记录，config 由框架按 ffs 记录组合（无 CS）；我们
     ep0_loop 里的描述符应答分支是死代码但保留（防御性，应答正确）。Device Info CS
     因此不在宿主读到的 config 里（WinUsb_GetDescriptor 返回 usbccgp 按接口过滤的
     32 字节 config）。
  2. gentl-u3v 发现层据此改为：CS 缺失时按 §3.2 EF/05 接口形状确认 U3V 并放行
     openable（GUID 缺失降级 vid:pid:serial ID，身份由 open 后的 SBRM bootstrap 补）。
  3. Zadig 逐接口绑 WinUSB 时各 MI 是独立设备节点：**WinUsb_GetAssociatedInterface
     一个都看不到**；InterfaceSet::collect 改为逐 MI 打开句柄（open_device_all）。
  4. FunctionFS bulk ep 读在 USB 重枚举/复位时会瞬时 ESHUTDOWN——bridge 必须重试
     （退出会 drop sender 导致 GenCP 服务器在断开通道上空转 100% CPU）；recv 超时与
     断开在 trait 上不可区分，勿在 None 分支加累计退出逻辑（踩过）。
  5. ffs 记录拒绝 CS_INTERFACE（仅 HID/CCID/DFU 白名单，f_fs.c），且计数字段按 flags
     位条件出现、记录必须单次 write（详见补充二）。
- **vgige-front-u3v 新增**：demo 采集块 ACQ_BASE=0x0002_0000（Start/Stop RW + PayloadSize
  RO u64，null 后端的 XML 借此广告采集节点；真实后端用相机自己的节点 1:1 转发）。
  注意 regions 必须**按 base 升序**存储（查找取最后一个 base≤addr）。
- **工程纪律教训**：① pgrep -f 会匹配查询命令自身（用 pgrep -x）；② 板端串长命令
  网络一断全乱（用户要求：单步执行）；③ sync 后必须 md5 核验再构建；④ Windows 侧
  设备反复失败会禁用 USB 口/需要重启或换口。
- **遗留**：① 偶发一次流中途 EPIPE（host 侧 stall，重枚举后未复现；若复现抓 [rx]
  日志——已埋好错误路径）；② fusb302 tcpm 仍 deferred（SS/Type-C 方向属 v2）；
  ③ pylon 对虚拟相机验证未做（U-B）；④ 板端建议加 systemd 服务自启 vgige-cli。

## Q39 补充四（2026-10-27 收尾：流稳定性诚实结论）
- **里程碑已达成**：U3V gadget 前端整机验证通过（多次 grab_u3v 50/50 帧，GenCP+SIRM
  全链路）。2×2 矩阵最后一个实机格闭合。
- **长跑稳定性遗留**：连续推流约 2-5 帧后偶发宿主侧管道停滞——设备端 write 报
  EPIPE（端点被 halt）。宿主侧复位源已排查：非 STREAM_POLL 超时（2s 内必有数据）、
  非 RAW_IO 设置时机（start 时/open 时都试过）、非 set_raw_io 本身（关掉也复现）。
  嫌疑指向 xHCI/WinUSB 驱动层对该 dwc3 设备的传输故障处理（可能含选择性挂起误触发），
  **需 USB 分析仪级工具或换 USB 口/线/宿主对照实验**，禁止再盲改。记入 backlog：
  docs/00-governance/PENDING.md 新增"U3V 前端长跑 EPIPE 根因调查"。
- **本次试错沉淀（都已回滚到成功态，勿重蹈）**：
  1. WinUSB 读挂起复用（staging pending）方案引发 n=0/短应答/内存错误——**已整体
     放弃**；流管道读保持经典 cancel-on-timeout（audit B11）。
  2. 设备端发射器重试会重复发送 leader 导致宿主 desync——EPIPE 后禁止重发 block。
  3. 发射器死后流状态残留（Stream Enable 不清）→ 后续采集接到旧字节流——恢复方案
     （abort_stream + 不死发射器）方向正确但因 2 的干扰未验证，**代码已回滚**，下次
     按"失败即 abort_stream + 发射器继续等下一使能（不重试）"重新实现。
  4. 对照组合=当前 HEAD 工作区：宿主经典读+RAW_IO@start+STREAM_POLL=100ms；板端
     emit.rs 裸 break；regs.rs 无 abort_stream。
- **运维**：守护进程重启后 Windows 侧需拔插（或重启板子+拔插）；Disable-PnpDevice
  软件重置需提权，未验证。板端建议后续加 systemd 自启。

## Q39 补充五（2026-10-27 终局：EPIPE 根因找到并修复）
- **根因**：payload 分段大小 = 1 MiB（`PAYLOAD_TRANSFER = 1<<20`）时，WinUSB/xHCI
  在**第一个分段**就 halt bulk-IN 端点（设备端表现为 leader 之后第一个 1 MiB piece
  写入即 EPIPE）。512 KiB 分段（`1<<19`）同环境下无限稳定（500/500 帧背靠背两轮 +
  此前多轮 50/50）。确切驱动层机制留档待查（疑似 RAW_IO/WinUSB 单请求上限类约束），
  512 KiB 为经验安全值（stream.rs PAYLOAD_TRANSFER 注释有完整记录）。
- **配套固化（已入库）**：①宿主侧流接收读维持经典 cancel-on-timeout（挂起复用
  方案整体放弃，教训见补充四）；②板端发射器 EPIPE 后不重试（防 leader 双发 desync）、
  立即 abort_stream 清理（SIRM Stream Enable 归位 + 关继电器），且发射器永不退出
  （通道比继电器会话长寿，下一次使能自动重启）——实测连续失败-恢复循环正常；
  ③grab_u3v 的 _Reg/Reg 双命名兼容保留。
- **最终验收**：`grab_u3v 500 帧 × 2 连跑 = 500/500 + 500/500`（Windows 宿主 ←USB2.0
  HS→ Orange Pi 5 Plus 虚拟 U3V 相机，GenCP+SIRM 全链路）。**U3V gadget 前端整机
  验证完成，2×2 矩阵收官。**

（踩坑全记录归档：见 docs/20-design/u3v-gadget-bringup-pitfalls.md——按内核/FF协议/Windows宿主/线程健壮性/调试纪律五层整理，含最终可用配置快照。）

## Q40: FORCEIP on the virtual GigE device (relay boundary)

**Spec:** GigE Vision 3.0 §§16.2.1–16.2.2, R-147cd ("The FORCEIP command MUST
be supported by all devices"), R-149cd (MAC mismatch → silent discard),
R-150cd (an open control channel → silent discard), R-151cd (static_IP = 0 →
close streams and restart the IP configuration without an ACK), R-152cd
(static_IP != 0 → change the device address, then ACK).

**Situation:** the virtual device's MAC/IP are the gateway host's up-facing
NIC. Honouring a forced non-zero address would mean rebinding the host
interface (root-only, breaks other host networking, and the gateway would
become unreachable on the old address while nothing else reconfigures it).

**Decision (v1):** the command is parsed and ordered per the spec
(`DeviceHandler::force_ip`, simulator implements the full device behaviour),
but the relay's virtual device **silently discards** a request for its own MAC
— with a warning on stderr — instead of reconfiguring the host NIC.
`static_IP = 0` still performs the required stream close (R-151cd). Hosts that
want to recover a gateway with a lost address must reconfigure the NIC by
other means. R-147cd is therefore deliberately not honoured on the relay
front (protocol handling is present; the re-addressing side effect is out of
scope for a proxy device).

**Verification:** `gentl/simulator/tests/e2e.rs`
(`force_ip_command_end_to_end`: ACK, applied address, foreign-MAC and
occupied-channel discards) and `gateway/cli/tests/relay_e2e.rs`
(`force_ip_to_the_virtual_device_is_discarded`).

**Compliance status (2026-11-24):** this is a *known mandatory-requirement
gap* of the relayed virtual GigE device (R-147cd/R-152cd). It is listed
explicitly in `compliance/spec-audit/2026-11-gap-closure.md` §26 "known
unsupported mandatory requirements"; the project must not claim full GigE
Vision conformance for the relay front while this decision stands.

## Q41: 网关事件中继的 ACK/重传（MCTT/MCRC）v1 取舍

**规范**：GigE Vision 3.0 §12.3（CR-101cd/CR-102cd/CR-103cd）：应用写 MCTT 后设备必须为消息通道消息置 ACKNOWLEDGE 位并按 MCRC 重传，直到收到 EVENT_ACK。

**情况**：网关把下游相机的事件**原样转发**给宿主，转发路径中没有事件重传缓冲；若声明 MCTT 生效，就必须实现"等 ACK + 超时重传"，否则会在丢包时静默丢失事件且违反 §12.3。

**v1 决策**：虚拟设备的 MCTT/MCRC 写入被接受但读回 0，转发的事件数据报不置 ACK 位（等价于 MCTT=0 的设备：CR-103cd 允许应用不要求 ACK）。收益：宿主不会等待不存在的 ACK，事件丢失可由相机侧/宿主侧重开通道恢复。代价：事件通道没有逐事件可靠性保证。

**后续**：真实相机/宿主有明确事件可靠性需求时，给网关加事件重传队列（按 MCRC 重试、收 EVENT_ACK）后再放开 MCTT 语义。验证脚本：`relay_e2e::device_events_are_relayed_to_the_opened_message_channel`。

