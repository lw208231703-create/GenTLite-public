//! vgige-back-null: the camera-less `CameraBack` (CLI `--back null`).
//! Unlocks the gateway demos with no camera attached: the GigE/U3V
//! virtual device fronts a synthetic camera — a minimal but GenApi-legal
//! register view plus a 30 fps Mono16 test-pattern stream the host can
//! verify byte-for-byte (a 16-bit horizontal gradient with a per-frame
//! block phase).
//!
//! Parameter model:
//! - Width/Height/OffsetX/OffsetY, ExposureTimeAbs, Gain and FrameRate
//!   are writable and clamped to the XML ranges; unknown writes are no-ops.
//! - The frame source crops the test pattern from the current ROI and
//!   reports the corresponding payload size.
//! - The built-in XML is little-endian: the null demo targets the U3V
//!   device front (R-30cd wire). Served through the GigE front the
//!   numeric register reads byte-swap (pass-through transcoding) — a
//!   demo limitation, not worth a second document in v1.
//! - The XML deliberately carries NO SmartCamera group: the gateway's
//!   XmlMerger injects it on top, exactly like a real camera's XML.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gentl_gvcp::registers as reg;
use vgige_core::{BackIdentity, CameraBack, CanonicalFrame, FramePayload, FrameSource};

/// Default synthetic stream geometry: 1280x1024 Mono16. Width, height,
/// offsets, exposure, gain and frame rate remain writable through GenCP.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 1024;
pub const BYTES_PER_PIXEL: usize = 2;
pub const PAYLOAD: usize = WIDTH as usize * HEIGHT as usize * BYTES_PER_PIXEL;
pub const PFNC_MONO8: u32 = 0x0108_0001;
pub const PFNC_MONO16: u32 = 0x0110_0007;
pub const EXPOSURE_US: u32 = 10_000;
pub const GAIN: u32 = 0;
pub const FRAME_RATE: u32 = 30;
/// Upper bound accepted for FrameRate/AcquisitionFrameRate and advertised
/// in the XML. A virtual camera has no sensor limit; 1 MHz is high enough
/// that a high request leaves the actual frame rate to the generator and
/// transport instead of an artificial 240 Hz cap.
pub const MAX_FRAME_RATE: u32 = 1_000_000;
/// Reusable payload slots. Relay ring (4) + U3V emitter queue (4) + the
/// frame being generated gives at most 9 live buffers; 12 leaves margin.
const FRAME_POOL_DEPTH: usize = 12;

/// SFNC ReverseX/ReverseY: mirror the generated frame in place — pixel order
/// inside each row for X, row order for Y.
fn mirror_payload(
    payload: &mut [u8],
    w: u32,
    h: u32,
    bpp: usize,
    reverse_x: bool,
    reverse_y: bool,
) {
    if !reverse_x && !reverse_y {
        return;
    }
    let width = w as usize;
    let row_bytes = width * bpp;
    if reverse_x {
        let mut pixel = vec![0u8; bpp];
        for row in payload.chunks_exact_mut(row_bytes) {
            let (mut lo, mut hi) = (0usize, width.saturating_sub(1));
            while lo < hi {
                let (a, b) = (lo * bpp, hi * bpp);
                pixel.copy_from_slice(&row[a..a + bpp]);
                row.copy_within(b..b + bpp, a);
                row[b..b + bpp].copy_from_slice(&pixel);
                lo += 1;
                hi -= 1;
            }
        }
    }
    if reverse_y {
        let rows = h as usize;
        let (mut top, mut bottom) = (0usize, rows);
        while top + 1 < bottom {
            bottom -= 1;
            let (head, tail) = payload.split_at_mut(bottom * row_bytes);
            head[top * row_bytes..(top + 1) * row_bytes].swap_with_slice(&mut tail[..row_bytes]);
            top += 1;
        }
    }
}

fn frame_period(fps: u32) -> Duration {
    Duration::from_micros(1_000_000 / u64::from(fps.max(1)))
}

fn pattern_value(x: u32, y: u32, w: u32, h: u32, block: u64) -> u16 {
    let denom = u64::from(w.saturating_sub(1).max(1));
    let mut value = ((u64::from(x) * 65_535) / denom + block) & 0xffff;
    let bar_w = 32u32.min(w);
    let bar_x = ((block.wrapping_mul(64)) % u64::from(w.max(1))) as u32;
    if h >= 32 && y >= 32 && x >= bar_x && x < bar_x.saturating_add(bar_w).min(w) {
        value = 0xffff;
    }
    if w >= 512 && h >= 32 && y < 32 && x < 512 {
        let bit = (block >> (x / 32)) & 1;
        value = if bit != 0 { 0xffff } else { 0 };
    }
    value as u16
}

/// XML-advertised little-endian registers. The acquisition block addresses
/// mirror vgige-front-u3v's ACQ_BASE.
pub const REG_WIDTH: u32 = 0x0004_1000;
pub const REG_HEIGHT: u32 = 0x0004_1004;
pub const REG_PIXEL_FORMAT: u32 = 0x0004_1008;
pub const REG_OFFSET_X: u32 = 0x0004_100C;
pub const REG_OFFSET_Y: u32 = 0x0004_1010;
pub const REG_EXPOSURE_US: u32 = 0x0004_1014;
pub const REG_GAIN: u32 = 0x0004_1018;
pub const REG_FRAME_RATE: u32 = 0x0004_101C;
pub const REG_ACQUISITION_START: u32 = 0x0004_0000;
pub const REG_ACQUISITION_STOP: u32 = 0x0004_0004;
/// Dynamic block payload in bytes (8-byte XML register; READREG returns the
/// low 32 bits for hosts that probe it without READMEM).
pub const REG_PAYLOAD: u32 = 0x0004_0008;
/// SFNC 2.x AcquisitionMode: Continuous = 2.
pub const REG_ACQUISITION_MODE: u32 = 0x0004_000C;
pub const REG_TRIGGER_MODE: u32 = 0x0004_0010;
pub const REG_DEVICE_UPTIME: u32 = 0x0004_1020;
pub const REG_DEVICE_TEMPERATURE: u32 = 0x0004_1024;
pub const REG_TL_PARAMS_LOCKED: u32 = 0x0004_1028;
/// SFNC sensor limits: the virtual sensor is a fixed 4096x4096 area
/// (`Width`/`Height` clamp to the same bound). RO constants exposed through
/// the standard ImageFormatControl category (NI-IMAQdx references WidthMax /
/// HeightMax / SensorWidth / SensorHeight by name).
pub const SENSOR_MAX: u32 = 4096;
pub const REG_WIDTH_MAX: u32 = 0x0004_102C;
pub const REG_HEIGHT_MAX: u32 = 0x0004_1030;
pub const REG_SENSOR_WIDTH: u32 = 0x0004_1034;
pub const REG_SENSOR_HEIGHT: u32 = 0x0004_1038;
/// SFNC DeviceReset command register: writing the command value restores the
/// factory defaults (the same map `new()` seeds).
pub const REG_DEVICE_RESET: u32 = 0x0004_103C;
/// SFNC `TestPattern` (the synthetic image generator selects the pattern).
pub const REG_TEST_PATTERN: u32 = 0x0004_1040;
/// SFNC `ReverseX`/`ReverseY` (the generator mirrors the synthetic frame).
pub const REG_REVERSE_X: u32 = 0x0004_1054;
pub const REG_REVERSE_Y: u32 = 0x0004_1058;
/// SFNC `UserSetControl` registers.
pub const REG_USER_SET_SELECTOR: u32 = 0x0004_1044;
pub const REG_USER_SET_DEFAULT: u32 = 0x0004_1048;
pub const REG_USER_SET_SAVE: u32 = 0x0004_104C;
pub const REG_USER_SET_LOAD: u32 = 0x0004_1050;

/// Device timestamp counter unit advertised through the GigE bootstrap
/// registers. The null frame source timestamps frames in microseconds, so
/// 1 MHz makes the GVSP leader timestamps valid tick counts.
pub const TIMESTAMP_TICK_FREQ_HZ: u32 = 1_000_000;

/// Minimal but GenApi-legal device description (v1 demo — placeholder
/// GUIDs in the simulator's style; SmartCamera nodes come from the
/// gateway's XmlMerger injection, not from this document).
pub const NULL_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<RegisterDescription xmlns=\"http://www.genicam.org/GenApi/Version_1_1\" \
xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
ModelName=\"GenTLiteVirtual\" VendorName=\"GenTLite\" \
ToolTip=\"GenTLite null camera: camera-less gateway demo\" StandardNameSpace=\"None\" \
SchemaMajorVersion=\"1\" SchemaMinorVersion=\"1\" SchemaSubMinorVersion=\"0\" \
MajorVersion=\"1\" MinorVersion=\"0\" SubMinorVersion=\"1\" \
ProductGuid=\"5a1f3c8e-2b7d-4e9a-9c4d-1e2f3a4b5c6d\" \
VersionGuid=\"7c3f5e0a-4d9b-6a2c-9e6f-3a4b5c6d7e8f\">\n\
\x20 <Port Name=\"Device\" NameSpace=\"Standard\">\n\
\x20   <ToolTip>The GenCP device port (implemented by the GenTL producer).</ToolTip>\n\
\x20 </Port>\n\
\x20 <Group Comment=\"Categories\">\n\
\x20   <Category Name=\"Root\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Provides the Root of the GenICam features tree.</ToolTip>\n\
\x20     <Description>Provides the Root of the GenICam features tree.</Description>\n\
\x20     <DisplayName>Root</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <ImposedAccessMode>RO</ImposedAccessMode>\n\
\x20     <pFeature>DeviceControl</pFeature>\n\
\x20     <pFeature>ImageFormatControl</pFeature>\n\
\x20     <pFeature>AcquisitionControl</pFeature>\n\
\x20     <pFeature>AnalogControl</pFeature>\n\
\x20     <pFeature>TransportLayerControl</pFeature>\n\
\x20     <pFeature>SmartCamera</pFeature>\n\
\x20     <pFeature>UserSetControl</pFeature>\n\
\x20   </Category>\n\
\x20   <Category Name=\"DeviceControl\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Device information and status.</ToolTip>\n\
\x20     <Description>Device information and status.</Description>\n\
\x20     <DisplayName>Device Control</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pFeature>DeviceVendorName</pFeature>\n\
\x20     <pFeature>DeviceModelName</pFeature>\n\
\x20     <pFeature>DeviceVersion</pFeature>\n\
\x20     <pFeature>DeviceFirmwareVersion</pFeature>\n\
\x20     <pFeature>DeviceSerialNumber</pFeature>\n\
\x20     <pFeature>DeviceUserID</pFeature>\n\
\x20     <pFeature>DeviceUptime</pFeature>\n\
\x20     <pFeature>DeviceTemperature</pFeature>\n\
\x20     <pFeature>DeviceReset</pFeature>\n\
\x20   </Category>\n\
\x20   <Category Name=\"ImageFormatControl\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Image size and pixel format.</ToolTip>\n\
\x20     <Description>Image size and pixel format.</Description>\n\
\x20     <DisplayName>Image Format Control</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pFeature>Width</pFeature>\n\
\x20     <pFeature>Height</pFeature>\n\
\x20     <pFeature>OffsetX</pFeature>\n\
\x20     <pFeature>OffsetY</pFeature>\n\
\x20     <pFeature>PixelFormat</pFeature>\n\
\x20     <pFeature>WidthMax</pFeature>\n\
\x20     <pFeature>HeightMax</pFeature>\n\
\x20     <pFeature>SensorWidth</pFeature>\n\
\x20     <pFeature>SensorHeight</pFeature>\n\
\x20     <pFeature>TestPattern</pFeature>\n\
\x20     <pFeature>ReverseX</pFeature>\n\
\x20     <pFeature>ReverseY</pFeature>\n\
\x20   </Category>\n\
\x20   <Category Name=\"AcquisitionControl\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Acquisition start/stop, exposure and frame rate.</ToolTip>\n\
\x20     <Description>Acquisition start/stop, exposure and frame rate.</Description>\n\
\x20     <DisplayName>Acquisition Control</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pFeature>AcquisitionMode</pFeature>\n\
\x20     <pFeature>AcquisitionStart</pFeature>\n\
\x20     <pFeature>AcquisitionStop</pFeature>\n\
\x20     <pFeature>ExposureTime</pFeature>\n\
\x20     <pFeature>ExposureTimeAbs</pFeature>\n\
\x20     <pFeature>AcquisitionFrameRate</pFeature>\n\
\x20     <pFeature>FrameRate</pFeature>\n\
\x20     <pFeature>TriggerMode</pFeature>\n\
\x20   </Category>\n\
\x20   <Category Name=\"AnalogControl\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Analog gain.</ToolTip>\n\
\x20     <Description>Analog gain.</Description>\n\
\x20     <DisplayName>Analog Control</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pFeature>Gain</pFeature>\n\
\x20   </Category>\n\
\x20   <Category Name=\"TransportLayerControl\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Transport layer features (GigE Vision U3V front adds the GevSCP* nodes).</ToolTip>\n\
\x20     <Description>Transport layer features.</Description>\n\
\x20     <DisplayName>Transport Layer Control</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pFeature>TLParamsLocked</pFeature>\n\
\x20     <pFeature>PayloadSize</pFeature>\n\
\x20   </Category>\n\
\x20   <Category Name=\"UserSetControl\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Save and load the complete feature set (SFNC UserSetControl).</ToolTip>\n\
\x20     <Description>Save and load the complete feature set.</Description>\n\
\x20     <DisplayName>User Set Control</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pFeature>UserSetSelector</pFeature>\n\
\x20     <pFeature>UserSetDefault</pFeature>\n\
\x20     <pFeature>UserSetSave</pFeature>\n\
\x20     <pFeature>UserSetLoad</pFeature>\n\
\x20   </Category>\n\
\x20 </Group>\n\
\x20 <String Name=\"DeviceVendorName\" NameSpace=\"Standard\"><Value>GenTLite</Value></String>\n\
\x20 <String Name=\"DeviceModelName\" NameSpace=\"Standard\"><Value>Virtual</Value></String>\n\
\x20 <String Name=\"DeviceVersion\" NameSpace=\"Standard\"><Value>1.0.0</Value></String>\n\
\x20 <String Name=\"DeviceFirmwareVersion\" NameSpace=\"Standard\"><Value>1.0.0</Value></String>\n\
\x20 <String Name=\"DeviceSerialNumber\" NameSpace=\"Standard\"><Value>NULL-0001</Value></String>\n\
\x20 <String Name=\"DeviceUserID\" NameSpace=\"Standard\"><Value></Value></String>\n\
\n\x20 <Group Comment=\"ImageFormat\">\n\
\x20   <Integer Name=\"Width\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Width</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>Width_Reg</pValue><Min>1</Min><Max>4096</Max><Inc>1</Inc>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"Width_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041000</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"Height\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Height</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>Height_Reg</pValue><Min>1</Min><Max>4096</Max><Inc>1</Inc>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"Height_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041004</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"OffsetX\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Offset X</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>OffsetX_Reg</pValue><Min>0</Min><Max>4095</Max><Inc>1</Inc>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"OffsetX_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x0004100C</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"OffsetY\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Offset Y</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>OffsetY_Reg</pValue><Min>0</Min><Max>4095</Max><Inc>1</Inc>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"OffsetY_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041010</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"WidthMax\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Maximum width of the sensor in pixels.</ToolTip>\n\
\x20     <Description>Maximum width of the sensor in pixels.</Description>\n\
\x20     <DisplayName>Width Max</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <pValue>WidthMax_Reg</pValue><Min>1</Min><Max>4096</Max>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"WidthMax_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x0004102C</Address><Length>4</Length><AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"HeightMax\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Maximum height of the sensor in pixels.</ToolTip>\n\
\x20     <Description>Maximum height of the sensor in pixels.</Description>\n\
\x20     <DisplayName>Height Max</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <pValue>HeightMax_Reg</pValue><Min>1</Min><Max>4096</Max>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"HeightMax_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041030</Address><Length>4</Length><AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"SensorWidth\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Physical sensor width in pixels.</ToolTip>\n\
\x20     <Description>Physical sensor width in pixels.</Description>\n\
\x20     <DisplayName>Sensor Width</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <pValue>SensorWidth_Reg</pValue><Min>1</Min><Max>4096</Max>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"SensorWidth_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041034</Address><Length>4</Length><AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"SensorHeight\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Physical sensor height in pixels.</ToolTip>\n\
\x20     <Description>Physical sensor height in pixels.</Description>\n\
\x20     <DisplayName>Sensor Height</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <pValue>SensorHeight_Reg</pValue><Min>1</Min><Max>4096</Max>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"SensorHeight_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041038</Address><Length>4</Length><AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Enumeration Name=\"PixelFormat\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Pixel Format</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <EnumEntry Name=\"Mono8\" NameSpace=\"Standard\">\n\
\x20       <DisplayName>Mono8</DisplayName><Value>0x01080001</Value>\n\
\x20     </EnumEntry>\n\
\x20     <EnumEntry Name=\"Mono16\" NameSpace=\"Standard\">\n\
\x20       <DisplayName>Mono16</DisplayName><Value>0x01100007</Value>\n\
\x20     </EnumEntry>\n\
\x20     <pValue>PixelFormatReg</pValue>\n\
\x20   </Enumeration>\n\
\x20   <IntReg Name=\"PixelFormatReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041008</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20 </Group>\n\
\x20 <Group Comment=\"AcquisitionControl\">\n\
\x20   <Float Name=\"ExposureTimeAbs\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Exposure Time (us)</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>ExposureTimeAbs_Reg</pValue><Min>10</Min><Max>100000</Max><Unit>us</Unit>\n\
\x20   </Float>\n\
\x20   <Float Name=\"ExposureTime\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Exposure Time (us)</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>ExposureTimeAbs_Reg</pValue><Min>10</Min><Max>100000</Max><Unit>us</Unit>\n\
\x20   </Float>\n\
\x20   <IntReg Name=\"ExposureTimeAbs_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041014</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Float Name=\"Gain\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Gain</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>Gain_Reg</pValue><Min>0</Min><Max>100</Max><Unit>dB</Unit>\n\
\x20   </Float>\n\
\x20   <IntReg Name=\"Gain_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041018</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Float Name=\"FrameRate\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Frame Rate</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>FrameRate_Reg</pValue><Min>1</Min><Max>1000000</Max><Unit>Hz</Unit>\n\
\x20   </Float>\n\
\x20   <IntReg Name=\"FrameRate_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x0004101C</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Float Name=\"AcquisitionFrameRate\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Acquisition Frame Rate</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>FrameRate_Reg</pValue><Min>1</Min><Max>1000000</Max><Unit>Hz</Unit>\n\
\x20   </Float>\n\
\x20   <Command Name=\"AcquisitionStart\" NameSpace=\"Standard\">\n\
\x20     <pValue>AcquisitionStart_Reg</pValue><CommandValue>1</CommandValue>\n\
\x20   </Command>\n\
\x20   <IntReg Name=\"AcquisitionStart_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00040000</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Command Name=\"AcquisitionStop\" NameSpace=\"Standard\">\n\
\x20     <pValue>AcquisitionStop_Reg</pValue><CommandValue>1</CommandValue>\n\
\x20   </Command>\n\
\x20   <IntReg Name=\"AcquisitionStop_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00040004</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Enumeration Name=\"AcquisitionMode\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Acquisition Mode</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <EnumEntry Name=\"Continuous\" NameSpace=\"Standard\"><DisplayName>Continuous</DisplayName><Value>2</Value></EnumEntry>\n\
\x20     <pValue>AcquisitionModeReg</pValue>\n\
\x20   </Enumeration>\n\
\x20   <IntReg Name=\"AcquisitionModeReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x0004000C</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Enumeration Name=\"TriggerMode\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Trigger Mode</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <EnumEntry Name=\"Off\" NameSpace=\"Standard\"><DisplayName>Off</DisplayName><Value>0</Value></EnumEntry>\n\
\x20     <pValue>TriggerModeReg</pValue>\n\
\x20   </Enumeration>\n\
\x20   <IntReg Name=\"TriggerModeReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00040010</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"PayloadSize\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Payload Size</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>PayloadSize_Reg</pValue><Min>1</Min><Max>1073741824</Max>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"PayloadSize_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00040008</Address><Length>8</Length><AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort>\n\
\x20     <pInvalidator>Width</pInvalidator>\n\
\x20     <pInvalidator>Height</pInvalidator>\n\
\x20     <pInvalidator>PixelFormat</pInvalidator>\n\
\x20     <Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"DeviceUptime\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Device Uptime</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <pValue>DeviceUptime_Reg</pValue><Min>0</Min><Max>9223372036854775807</Max><Unit>ms</Unit>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"DeviceUptime_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041020</Address><Length>8</Length><AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Float Name=\"DeviceTemperature\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Device Temperature</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <pValue>DeviceTemperature_Reg</pValue><Unit>C</Unit>\n\
\x20   </Float>\n\
\x20   <FloatReg Name=\"DeviceTemperature_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041024</Address><Length>4</Length><AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort><Endianess>LittleEndian</Endianess>\n\
\x20   </FloatReg>\n\
\x20   <Integer Name=\"TLParamsLocked\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>TL Params Locked</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <pValue>TLParamsLocked_Reg</pValue><Min>0</Min><Max>1</Max>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"TLParamsLocked_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041028</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Command Name=\"DeviceReset\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Resets the device to factory defaults (SFNC DeviceReset).</ToolTip>\n\
\x20     <Description>Resets the device to factory defaults.</Description>\n\
\x20     <DisplayName>Device Reset</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <pValue>DeviceReset_Reg</pValue><CommandValue>1</CommandValue>\n\
\x20   </Command>\n\
\x20   <IntReg Name=\"DeviceReset_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x0004103C</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20 </Group>\n\
\x20 <Group Comment=\"TestPatternAndUserSets\">\n\
\x20   <Enumeration Name=\"TestPattern\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Selects a synthetic test image; the generator switches the pixel pattern.</ToolTip>\n\
\x20     <DisplayName>Test Pattern</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <EnumEntry Name=\"Off\" NameSpace=\"Standard\"><DisplayName>Off</DisplayName><Value>0</Value></EnumEntry>\n\
\x20     <EnumEntry Name=\"GreyHorizontalRamp\" NameSpace=\"Standard\"><DisplayName>Grey Horizontal Ramp</DisplayName><Value>1</Value></EnumEntry>\n\
\x20     <EnumEntry Name=\"GreyVerticalRamp\" NameSpace=\"Standard\"><DisplayName>Grey Vertical Ramp</DisplayName><Value>2</Value></EnumEntry>\n\
\x20     <pValue>TestPattern_Reg</pValue>\n\
\x20   </Enumeration>\n\
\x20   <IntReg Name=\"TestPattern_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041040</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Boolean Name=\"ReverseX\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Mirror the image horizontally; the synthetic generator flips the frame.</ToolTip>\n\
\x20     <Description>Mirror the image horizontally.</Description>\n\
\x20     <DisplayName>Reverse X</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>ReverseX_Reg</pValue>\n\
\x20   </Boolean>\n\
\x20   <IntReg Name=\"ReverseX_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041054</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Boolean Name=\"ReverseY\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Mirror the image vertically; the synthetic generator flips the frame.</ToolTip>\n\
\x20     <Description>Mirror the image vertically.</Description>\n\
\x20     <DisplayName>Reverse Y</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>ReverseY_Reg</pValue>\n\
\x20   </Boolean>\n\
\x20   <IntReg Name=\"ReverseY_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041058</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Enumeration Name=\"UserSetSelector\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Selects the user set to load or save.</ToolTip>\n\
\x20     <DisplayName>User Set Selector</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <EnumEntry Name=\"Default\" NameSpace=\"Standard\"><DisplayName>Default</DisplayName><Value>0</Value></EnumEntry>\n\
\x20     <EnumEntry Name=\"UserSet1\" NameSpace=\"Standard\"><DisplayName>User Set 1</DisplayName><Value>1</Value></EnumEntry>\n\
\x20     <pValue>UserSetSelector_Reg</pValue>\n\
\x20   </Enumeration>\n\
\x20   <IntReg Name=\"UserSetSelector_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041044</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Enumeration Name=\"UserSetDefault\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Selects the user set restored at power-on and by DeviceReset.</ToolTip>\n\
\x20     <DisplayName>User Set Default</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <EnumEntry Name=\"Default\" NameSpace=\"Standard\"><DisplayName>Default</DisplayName><Value>0</Value></EnumEntry>\n\
\x20     <EnumEntry Name=\"UserSet1\" NameSpace=\"Standard\"><DisplayName>User Set 1</DisplayName><Value>1</Value></EnumEntry>\n\
\x20     <pValue>UserSetDefault_Reg</pValue>\n\
\x20   </Enumeration>\n\
\x20   <IntReg Name=\"UserSetDefault_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041048</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Command Name=\"UserSetSave\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Saves the current feature set into the selected user set.</ToolTip>\n\
\x20     <DisplayName>User Set Save</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <pValue>UserSetSave_Reg</pValue><CommandValue>1</CommandValue>\n\
\x20   </Command>\n\
\x20   <IntReg Name=\"UserSetSave_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x0004104C</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Command Name=\"UserSetLoad\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Loads the selected user set into the active feature set.</ToolTip>\n\
\x20     <DisplayName>User Set Load</DisplayName><Visibility>Expert</Visibility>\n\
\x20     <pValue>UserSetLoad_Reg</pValue><CommandValue>1</CommandValue>\n\
\x20   </Command>\n\
\x20   <IntReg Name=\"UserSetLoad_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041050</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20 </Group>\n\
</RegisterDescription>\n";

/// The synthetic camera: a writable register view plus a pattern stream.
pub struct NullBack {
    regs: Arc<Mutex<std::collections::HashMap<u32, u32>>>,
    /// Saved feature set for SFNC `UserSet1` (UserSetSave/UserSetLoad have
    /// real semantics: the whole feature map is snapshotted and restored).
    saved: Arc<Mutex<std::collections::HashMap<u32, u32>>>,
    /// Device clock for `DeviceUptime` and the bootstrap timestamp counter.
    epoch: Instant,
}

impl NullBack {
    /// Factory-default register map, shared by `new()` and the SFNC
    /// `DeviceReset` command.
    fn default_regs() -> std::collections::HashMap<u32, u32> {
        std::collections::HashMap::from([
            (REG_WIDTH, WIDTH),
            (REG_HEIGHT, HEIGHT),
            (REG_PIXEL_FORMAT, PFNC_MONO16),
            (REG_OFFSET_X, 0),
            (REG_OFFSET_Y, 0),
            (REG_EXPOSURE_US, EXPOSURE_US),
            (REG_GAIN, GAIN),
            (REG_FRAME_RATE, FRAME_RATE),
            (REG_ACQUISITION_MODE, 2), // SFNC Continuous
            (REG_TRIGGER_MODE, 0),     // Off; the synthetic source is free-running
            (REG_DEVICE_TEMPERATURE, 25.0f32.to_bits()),
            (REG_TL_PARAMS_LOCKED, 0),
            // Static virtual-sensor geometry (SFNC ImageFormatControl).
            (REG_WIDTH_MAX, SENSOR_MAX),
            (REG_HEIGHT_MAX, SENSOR_MAX),
            (REG_SENSOR_WIDTH, SENSOR_MAX),
            (REG_SENSOR_HEIGHT, SENSOR_MAX),
            // SFNC TestPattern: Off (the deterministic synthetic pattern) or
            // a grey ramp generated by the frame source.
            (REG_TEST_PATTERN, 0),
            // SFNC UserSetControl: selector/default plus the command
            // registers (they read 0 outside a command).
            (REG_USER_SET_SELECTOR, 0),
            (REG_USER_SET_DEFAULT, 0),
            (REG_USER_SET_SAVE, 0),
            (REG_USER_SET_LOAD, 0),
            // SFNC ReverseX/ReverseY: the frame generator mirrors the frame.
            (REG_REVERSE_X, 0),
            (REG_REVERSE_Y, 0),
        ])
    }

    pub fn new() -> Self {
        let defaults = Self::default_regs();
        Self {
            regs: Arc::new(Mutex::new(defaults.clone())),
            // A factory-fresh camera has a valid user set: saving without a
            // prior UserSetSave would otherwise have nothing to restore.
            saved: Arc::new(Mutex::new(defaults)),
            epoch: Instant::now(),
        }
    }
}

impl Default for NullBack {
    fn default() -> Self {
        Self::new()
    }
}

impl CameraBack for NullBack {
    fn register_endian(&self) -> vgige_core::RegisterEndian {
        vgige_core::RegisterEndian::Little
    }

    fn read_reg(&mut self, addr: u32) -> Result<u32, u16> {
        match addr {
            // 8-byte XML registers: READREG has no 64-bit form, so serve the
            // low 32 bits here and the full value through read_block.
            REG_PAYLOAD => return Ok(self.payload_size()? as u32),
            REG_DEVICE_UPTIME => return Ok(self.epoch.elapsed().as_millis() as u32),
            reg::TIMESTAMP_TICK_FREQ_HIGH => return Ok(0),
            reg::TIMESTAMP_TICK_FREQ_LOW => return Ok(TIMESTAMP_TICK_FREQ_HZ),
            reg::TIMESTAMP_VALUE_HIGH => {
                let ticks = self.epoch.elapsed().as_micros() as u64;
                return Ok((ticks >> 32) as u32);
            }
            reg::TIMESTAMP_VALUE_LOW => {
                return Ok(self.epoch.elapsed().as_micros() as u64 as u32);
            }
            _ => {}
        }
        Ok(self.regs.lock().expect("null regs lock").get(&addr).copied().unwrap_or(0))
    }

    fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), u16> {
        let mut regs = self.regs.lock().expect("null regs lock");
        match addr {
            REG_ACQUISITION_START => {
                regs.insert(addr, u32::from(value != 0));
            }
            REG_ACQUISITION_STOP => {
                regs.insert(REG_ACQUISITION_START, 0);
            }
            REG_ACQUISITION_MODE => {
                // Only Continuous (SFNC value 2) is supported.
                regs.insert(addr, 2);
            }
            REG_TRIGGER_MODE => {
                // No trigger source/software-trigger command is implemented;
                // keep the synthetic source free-running and report Off.
                regs.insert(addr, 0);
            }
            REG_WIDTH | REG_HEIGHT => {
                regs.insert(addr, value.clamp(1, 4096));
            }
            REG_PIXEL_FORMAT => {
                if value == PFNC_MONO8 || value == PFNC_MONO16 {
                    regs.insert(addr, value);
                }
            }
            REG_OFFSET_X | REG_OFFSET_Y => {
                regs.insert(addr, value.min(4095));
            }
            REG_EXPOSURE_US => {
                regs.insert(addr, value.clamp(10, 100_000));
            }
            REG_GAIN => {
                regs.insert(addr, value.min(100));
            }
            REG_FRAME_RATE => {
                regs.insert(addr, value.clamp(1, MAX_FRAME_RATE));
            }
            REG_TL_PARAMS_LOCKED => {
                regs.insert(addr, u32::from(value != 0));
            }
            REG_TEST_PATTERN => {
                // SFNC TestPattern enumeration: Off / GreyHorizontalRamp /
                // GreyVerticalRamp. The frame source reads this and switches
                // the generated pixels, so the feature has real semantics.
                if value <= 2 {
                    regs.insert(addr, value);
                }
            }
            REG_USER_SET_SELECTOR | REG_USER_SET_DEFAULT | REG_REVERSE_X | REG_REVERSE_Y => {
                if value <= 1 {
                    regs.insert(addr, value);
                }
            }
            REG_USER_SET_SAVE => {
                // SFNC UserSetSave: snapshot the active feature set into the
                // selected user set (1 = UserSet1; Default is read-only).
                let selector = regs.get(&REG_USER_SET_SELECTOR).copied().unwrap_or(0);
                if value != 0 && selector == 1 {
                    *self.saved.lock().expect("null saved lock") = regs.clone();
                }
                regs.insert(addr, 0);
            }
            REG_USER_SET_LOAD => {
                // SFNC UserSetLoad: restore the selected user set.
                let selector = regs.get(&REG_USER_SET_SELECTOR).copied().unwrap_or(0);
                if value != 0 && selector == 1 {
                    let saved = self.saved.lock().expect("null saved lock").clone();
                    *regs = saved;
                }
                regs.insert(addr, 0);
            }
            REG_DEVICE_RESET => {
                // SFNC DeviceReset: the command value restores factory
                // defaults (a virtual reset — the process keeps running).
                if value != 0 {
                    let defaults = Self::default_regs();
                    *regs = defaults.clone();
                    *self.saved.lock().expect("null saved lock") = defaults;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn read_block(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, u16> {
        if addr == REG_PAYLOAD && (len == 4 || len == 8) {
            let payload = self.payload_size()?.to_le_bytes();
            return Ok(payload[..len as usize].to_vec());
        }
        if addr == REG_DEVICE_UPTIME && (len == 4 || len == 8) {
            let uptime = (self.epoch.elapsed().as_millis() as u64).to_le_bytes();
            return Ok(uptime[..len as usize].to_vec());
        }
        if len == 4 {
            return self.read_reg(addr).map(|v| v.to_le_bytes().to_vec());
        }
        Ok(vec![0u8; len as usize])
    }

    fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<(), u16> {
        if data.len() == 4 {
            return self.write_reg(addr, u32::from_le_bytes(data[..4].try_into().expect("u32")));
        }
        Ok(())
    }

    fn payload_size(&mut self) -> Result<u64, u16> {
        let regs = self.regs.lock().expect("null regs lock");
        let w = u64::from(regs.get(&REG_WIDTH).copied().unwrap_or(WIDTH));
        let h = u64::from(regs.get(&REG_HEIGHT).copied().unwrap_or(HEIGHT));
        let bpp = if regs.get(&REG_PIXEL_FORMAT).copied() == Some(PFNC_MONO8) {
            1
        } else {
            BYTES_PER_PIXEL
        };
        Ok(w * h * bpp as u64)
    }

    fn fetch_xml(&mut self) -> Result<Vec<u8>, u16> {
        Ok(NULL_XML.as_bytes().to_vec())
    }

    fn fetch_identity(&mut self) -> Result<BackIdentity, u16> {
        Ok(BackIdentity {
            manufacturer: "GenTLite".into(),
            model: "Virtual".into(),
            device_version: "1.0.0".into(),
            serial_number: "NULL-0001".into(),
            user_defined_name: String::new(),
            spec_version_major: 2,
            spec_version_minor: 2,
        })
    }

    fn open_stream(&mut self) -> Result<Box<dyn FrameSource>, u16> {
        Ok(Box::new(NullFrameSource::new(self.regs.clone(), self.epoch)))
    }

    fn close_stream(&mut self) -> Result<(), u16> {
        self.regs
            .lock()
            .expect("null regs lock")
            .insert(REG_ACQUISITION_START, 0);
        Ok(())
    }
}

/// Configurable test-pattern source. ROI pixels are sampled from the same
/// full-frame `(source_byte_index + block_id) mod 256` pattern.
pub struct NullFrameSource {
    regs: Arc<Mutex<std::collections::HashMap<u32, u32>>>,
    /// Same clock as the bootstrap timestamp registers, so GVSP leader
    /// timestamps use the same epoch/unit the host reads from the device.
    epoch: Instant,
    next_block: u64,
    next_at: Instant,
    recycler: SyncSender<Vec<u8>>,
    pool: Receiver<Vec<u8>>,
}

impl NullFrameSource {
    pub fn new(regs: Arc<Mutex<std::collections::HashMap<u32, u32>>>, epoch: Instant) -> Self {
        let (recycler, pool) = sync_channel(FRAME_POOL_DEPTH);
        Self {
            regs,
            epoch,
            next_block: 1,
            next_at: Instant::now(),
            recycler,
            pool,
        }
    }
}

impl Default for NullFrameSource {
    fn default() -> Self {
        let back = NullBack::new();
        Self::new(back.regs, back.epoch)
    }
}

impl FrameSource for NullFrameSource {
    fn next_frame(&mut self, timeout: Duration) -> Option<CanonicalFrame> {
        let now = Instant::now();
        if now < self.next_at {
            let wait = self.next_at - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return None;
            }
            std::thread::sleep(wait);
        }
        if self
            .regs
            .lock()
            .expect("null regs lock")
            .get(&REG_ACQUISITION_START)
            .copied()
            .unwrap_or(0)
            == 0
        {
            if !timeout.is_zero() {
                std::thread::sleep(timeout);
            }
            return None;
        }
        let (w, h, ox, oy, fps, pixel_format, test_pattern, reverse_x, reverse_y) = {
            let regs = self.regs.lock().expect("null regs lock");
            (
                regs.get(&REG_WIDTH).copied().unwrap_or(WIDTH),
                regs.get(&REG_HEIGHT).copied().unwrap_or(HEIGHT),
                regs.get(&REG_OFFSET_X).copied().unwrap_or(0),
                regs.get(&REG_OFFSET_Y).copied().unwrap_or(0),
                regs.get(&REG_FRAME_RATE).copied().unwrap_or(FRAME_RATE),
                regs.get(&REG_PIXEL_FORMAT)
                    .copied()
                    .unwrap_or(PFNC_MONO16),
                regs.get(&REG_TEST_PATTERN).copied().unwrap_or(0),
                regs.get(&REG_REVERSE_X).copied().unwrap_or(0) != 0,
                regs.get(&REG_REVERSE_Y).copied().unwrap_or(0) != 0,
            )
        };
        let block = self.next_block;
        self.next_block += 1;
        let period = frame_period(fps);
        self.next_at += period;
        let now = Instant::now();
        if self.next_at < now {
            self.next_at = now + period;
        }
        let bpp = if pixel_format == PFNC_MONO8 {
            1
        } else {
            BYTES_PER_PIXEL
        };
        // SFNC TestPattern: a real generator switch. The ramps replace the
        // synthetic pattern (and its block marker) for every row.
        if test_pattern != 0 {
            let mut payload = self.pool.try_recv().unwrap_or_default();
            payload.clear();
            payload.reserve((w * h) as usize * bpp);
            let horizontal = test_pattern == 1;
            let denom = if horizontal {
                u64::from(w.saturating_sub(1).max(1))
            } else {
                u64::from(h.saturating_sub(1).max(1))
            };
            for y in 0..h {
                for x in 0..w {
                    let position = if horizontal { u64::from(x) } else { u64::from(y) };
                    let value = ((position * 65_535) / denom) as u16;
                    if bpp == 1 {
                        payload.push((value >> 8) as u8);
                    } else {
                        payload.extend_from_slice(&value.to_le_bytes());
                    }
                }
            }
            mirror_payload(&mut payload, w, h, bpp, reverse_x, reverse_y);
            return Some(CanonicalFrame {
                timestamp: self.epoch.elapsed().as_micros() as u64,
                pixel_format,
                width: w,
                height: h,
                offset_x: ox,
                offset_y: oy,
                padding_x: 0,
                padding_y: 0,
                chunk: None,
                payload: FramePayload::with_recycle(payload, self.recycler.clone()),
            });
        }
        let row_y = if h >= 32 { 32 } else { 0 };
        let mut base_row = Vec::with_capacity(w as usize);
        base_row.extend((0..w).map(|x| pattern_value(x, row_y, w, h, block)));
        let mut row_bytes = Vec::with_capacity(w as usize * bpp);
        for value in &base_row {
            if bpp == 1 {
                row_bytes.push((*value >> 8) as u8);
            } else {
                row_bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        // Rows 0..31 are the gradient plus, for a wide enough ROI, the
        // 16-bit block-id marker over x < 512. Build one shared row so the
        // frame copy stays row-granular.
        let top_bytes = if h >= 32 {
            let denom = u64::from(w.saturating_sub(1).max(1));
            let mut top = Vec::with_capacity(w as usize * bpp);
            for x in 0..w {
                let value = ((u64::from(x) * 65_535) / denom + block) & 0xffff;
                if bpp == 1 {
                    top.push((value >> 8) as u8);
                } else {
                    top.extend_from_slice(&(value as u16).to_le_bytes());
                }
            }
            let marker_px = if w >= 512 { 512 } else { 0 };
            for bit in 0..marker_px / 32 {
                let value = if (block >> bit) & 1 != 0 { 0xff } else { 0 };
                let start = bit as usize * 32 * bpp;
                let end = start + 32 * bpp;
                top[start..end].fill(value);
            }
            Some(top)
        } else {
            None
        };
        let mut payload = self.pool.try_recv().unwrap_or_default();
        payload.clear();
        payload.reserve((w * h) as usize * bpp);
        for y in 0..h {
            if y < 32 {
                if let Some(top) = &top_bytes {
                    payload.extend_from_slice(top);
                    continue;
                }
            }
            payload.extend_from_slice(&row_bytes);
        }
        mirror_payload(&mut payload, w, h, bpp, reverse_x, reverse_y);
        Some(CanonicalFrame {
            timestamp: self.epoch.elapsed().as_micros() as u64,
            pixel_format,
            width: w,
            height: h,
            offset_x: ox,
            offset_y: oy,
            padding_x: 0,
            padding_y: 0,
            chunk: None,
            payload: FramePayload::with_recycle(payload, self.recycler.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_pixel(x: u32, y: u32, w: u32, h: u32, block: u64) -> [u8; 2] {
        pattern_value(x, y, w, h, block).to_le_bytes()
    }

    #[test]
    fn register_semantics_and_identity() {
        let mut back = NullBack::new();
        assert_eq!(back.read_reg(REG_WIDTH).unwrap(), WIDTH);
        assert_eq!(back.read_reg(REG_HEIGHT).unwrap(), HEIGHT);
        assert_eq!(back.read_reg(REG_PIXEL_FORMAT).unwrap(), PFNC_MONO16);
        assert_eq!(back.read_reg(REG_PAYLOAD).unwrap(), PAYLOAD as u32);
        assert_eq!(
            back.read_reg(reg::TIMESTAMP_TICK_FREQ_LOW).unwrap(),
            TIMESTAMP_TICK_FREQ_HZ
        );
        assert_eq!(back.read_reg(0x0FF0).unwrap(), 0);
        back.write_reg(0x0FF0, 42).unwrap();
        assert_eq!(back.read_reg(0x0FF0).unwrap(), 0, "unknown writes are no-ops");
        let block = back.read_block(0x0FF0, 16).unwrap();
        assert_eq!(block, vec![0u8; 16]);
        back.write_block(0x0FF0, &[1, 2, 3, 4]).unwrap();

        back.write_reg(REG_WIDTH, 640).unwrap();
        back.write_reg(REG_HEIGHT, 480).unwrap();
        back.write_reg(REG_OFFSET_X, 40).unwrap();
        back.write_reg(REG_OFFSET_Y, 24).unwrap();
        back.write_reg(REG_EXPOSURE_US, 20_000).unwrap();
        back.write_reg(REG_GAIN, 8).unwrap();
        back.write_reg(REG_FRAME_RATE, 60).unwrap();
        assert_eq!(back.payload_size().unwrap(), 640 * 480 * 2);
        assert_eq!(back.read_reg(REG_OFFSET_X).unwrap(), 40);
        assert_eq!(back.read_reg(REG_OFFSET_Y).unwrap(), 24);
        assert_eq!(back.read_reg(REG_EXPOSURE_US).unwrap(), 20_000);
        assert_eq!(back.read_reg(REG_GAIN).unwrap(), 8);
        assert_eq!(back.read_reg(REG_FRAME_RATE).unwrap(), 60);
        assert_eq!(frame_period(60), Duration::from_micros(16_666));
        // No 240 Hz demo cap: a high request is accepted so the test sees
        // the generator/transport limit, not the register limit.
        back.write_reg(REG_FRAME_RATE, 1000).unwrap();
        assert_eq!(back.read_reg(REG_FRAME_RATE).unwrap(), 1000);
        assert_eq!(frame_period(1000), Duration::from_millis(1));
        back.write_reg(REG_FRAME_RATE, MAX_FRAME_RATE + 1).unwrap();
        assert_eq!(
            back.read_reg(REG_FRAME_RATE).unwrap(),
            MAX_FRAME_RATE,
            "only the virtual-camera upper bound remains"
        );
        let payload = back.read_block(0x0004_0008, 8).unwrap();
        assert_eq!(
            u64::from_le_bytes(payload.try_into().unwrap()),
            640 * 480 * 2
        );

        let id = back.fetch_identity().unwrap();
        assert_eq!(id.manufacturer, "GenTLite");
        assert_eq!(id.model, "Virtual");
        assert_eq!(id.serial_number, "NULL-0001");

        let xml = back.fetch_xml().unwrap();
        let text = String::from_utf8(xml).unwrap();
        assert!(text.contains("</RegisterDescription>"));
        assert!(text.contains("ModelName=\"GenTLiteVirtual\""));
        // SFNC standard category tree: GenApi hosts (NI MAX, pylon, MVS)
        // group the attribute tree by Category and derive persisted
        // attribute paths from it (CameraAttributes::ImageFormatControl::
        // Width). A flat Root feature list shows an ungrouped grid instead.
        for category in [
            "Root",
            "DeviceControl",
            "ImageFormatControl",
            "AcquisitionControl",
            "AnalogControl",
            "TransportLayerControl",
        ] {
            assert!(
                text.contains(&format!("<Category Name=\"{category}\"")),
                "missing category {category}"
            );
        }
        for feature in [
            "DeviceControl",
            "ImageFormatControl",
            "AcquisitionControl",
            "AnalogControl",
            "TransportLayerControl",
        ] {
            assert!(
                text.contains(&format!("<pFeature>{feature}</pFeature>")),
                "Root does not reference {feature}"
            );
        }
        assert!(
            text.contains("<Max>1000000</Max>"),
            "XML must advertise the same high FrameRate bound"
        );
        for node in ["Width_Reg", "Height_Reg", "OffsetX_Reg", "OffsetY_Reg", "ExposureTimeAbs_Reg", "Gain_Reg", "FrameRate_Reg", "AcquisitionMode", "TriggerMode", "TriggerModeReg", "AcquisitionFrameRate"] {
            assert!(text.contains(node), "missing {node}");
        }
        for node in ["ExposureTimeAbs", "Gain", "FrameRate"] {
            assert!(text.contains(&format!("<Float Name=\"{node}\"")), "{node} must be a GenApi Float");
            assert!(text.contains(&format!("<IntReg Name=\"{node}_Reg\"")), "{node}_Reg must be a GenApi IntReg");
        }
        assert!(text.contains("<EnumEntry Name=\"Mono8\""));
        assert!(text.contains("<EnumEntry Name=\"Mono16\""));
        // SFNC sensor constants and the DeviceReset command (NI-IMAQdx's
        // fixed attribute table references these by name).
        for node in [
            "WidthMax",
            "HeightMax",
            "SensorWidth",
            "SensorHeight",
            "DeviceReset",
        ] {
            assert!(
                text.contains(&format!("Name=\"{node}\"")),
                "missing SFNC node {node}"
            );
        }
        for feature in ["WidthMax", "HeightMax", "SensorWidth", "SensorHeight"] {
            assert!(
                text.contains(&format!("<pFeature>{feature}</pFeature>")),
                "{feature} not attached to a category"
            );
        }
        assert!(text.contains("<pFeature>DeviceReset</pFeature>"));
    }

    #[test]
    fn sensor_constants_and_device_reset() {
        let mut back = NullBack::new();
        assert_eq!(back.read_reg(REG_WIDTH_MAX).unwrap(), SENSOR_MAX);
        assert_eq!(back.read_reg(REG_HEIGHT_MAX).unwrap(), SENSOR_MAX);
        assert_eq!(back.read_reg(REG_SENSOR_WIDTH).unwrap(), SENSOR_MAX);
        assert_eq!(back.read_reg(REG_SENSOR_HEIGHT).unwrap(), SENSOR_MAX);

        // DeviceReset restores factory defaults after a change.
        back.write_reg(REG_WIDTH, 640).unwrap();
        assert_eq!(back.read_reg(REG_WIDTH).unwrap(), 640);
        back.write_reg(REG_DEVICE_RESET, 1).unwrap();
        assert_eq!(
            back.read_reg(REG_WIDTH).unwrap(),
            WIDTH,
            "DeviceReset restores the factory width"
        );
        // The command register must not retain a stale 1 that a later read
        // would interpret as an active reset request.
        assert_eq!(back.read_reg(REG_DEVICE_RESET).unwrap(), 0);
    }

    #[test]
    fn frames_are_sized_and_patterned() {
        let mut back = NullBack::new();
        back.write_reg(REG_ACQUISITION_START, 1).unwrap();
        let mut src = back.open_stream().unwrap();
        let f1 = src.next_frame(Duration::from_secs(2)).expect("frame 1");
        assert_eq!(f1.width, WIDTH);
        assert_eq!(f1.height, HEIGHT);
        assert_eq!(f1.pixel_format, PFNC_MONO16);
        assert_eq!(f1.payload.len(), PAYLOAD);
        assert!(
            f1.payload
                .chunks_exact(2)
                .enumerate()
                .all(|(k, px)| {
                    px == expected_pixel(
                        (k % WIDTH as usize) as u32,
                        (k / WIDTH as usize) as u32,
                        WIDTH,
                        HEIGHT,
                        1,
                    )
                }),
            "block 1 payload is the Mono16 gradient, byte-exact"
        );
        let f2 = src.next_frame(Duration::from_secs(2)).expect("frame 2");
        assert!(
            f2.payload
                .chunks_exact(2)
                .enumerate()
                .all(|(k, px)| {
                    px == expected_pixel(
                        (k % WIDTH as usize) as u32,
                        (k / WIDTH as usize) as u32,
                        WIDTH,
                        HEIGHT,
                        2,
                    )
                }),
            "block 2 payload is the Mono16 gradient, byte-exact"
        );
        assert!(f2.timestamp > f1.timestamp);
    }

    #[test]
    fn frame_marker_and_moving_bar_follow_the_block_id() {
        assert_eq!(pattern_value(0, 0, WIDTH, HEIGHT, 1), 0xffff);
        assert_eq!(pattern_value(32, 0, WIDTH, HEIGHT, 1), 0);
        assert_eq!(pattern_value(32, 0, WIDTH, HEIGHT, 2), 0xffff);
        assert_eq!(pattern_value(64, 32, WIDTH, HEIGHT, 1), 0xffff);
        assert_ne!(
            pattern_value(64, 32, WIDTH, HEIGHT, 1),
            pattern_value(64, 32, WIDTH, HEIGHT, 2)
        );
        assert_eq!(pattern_value(128, 32, WIDTH, HEIGHT, 2), 0xffff);
    }

    #[test]
    fn mono8_mode_updates_payload_and_pattern() {
        let mut back = NullBack::new();
        back.write_reg(REG_ACQUISITION_START, 1).unwrap();
        back.write_reg(REG_PIXEL_FORMAT, PFNC_MONO8).unwrap();
        assert_eq!(back.payload_size().unwrap(), 1280 * 1024);
        let mut src = back.open_stream().unwrap();
        let frame = src.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(frame.pixel_format, PFNC_MONO8);
        assert_eq!(frame.payload.len(), 1280 * 1024);
        assert_eq!(
            frame.payload[..4],
            [
                (pattern_value(0, 0, WIDTH, HEIGHT, 1) >> 8) as u8,
                (pattern_value(1, 0, WIDTH, HEIGHT, 1) >> 8) as u8,
                (pattern_value(2, 0, WIDTH, HEIGHT, 1) >> 8) as u8,
                (pattern_value(3, 0, WIDTH, HEIGHT, 1) >> 8) as u8,
            ]
        );
        back.write_reg(REG_PIXEL_FORMAT, PFNC_MONO16).unwrap();
        assert_eq!(back.payload_size().unwrap(), 1280 * 1024 * 2);
    }

    #[test]
    fn roi_changes_geometry_payload_and_offsets() {
        let mut back = NullBack::new();
        back.write_reg(REG_WIDTH, 2).unwrap();
        back.write_reg(REG_HEIGHT, 2).unwrap();
        back.write_reg(REG_OFFSET_X, 1).unwrap();
        back.write_reg(REG_OFFSET_Y, 1).unwrap();
        back.write_reg(REG_ACQUISITION_START, 1).unwrap();
        let mut src = back.open_stream().unwrap();
        let frame = src.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!((frame.offset_x, frame.offset_y), (1, 1));
        assert_eq!(frame.pixel_format, PFNC_MONO16);
        let mut row = Vec::new();
        row.extend_from_slice(&expected_pixel(0, 0, 2, 2, 1));
        row.extend_from_slice(&expected_pixel(1, 0, 2, 2, 1));
        assert_eq!(frame.payload.as_ref(), [row.clone(), row].concat().as_slice());
    }

    #[test]
    fn dropped_frames_reuse_the_payload_allocation() {
        let mut back = NullBack::new();
        back.write_reg(REG_ACQUISITION_START, 1).unwrap();
        let mut src = back.open_stream().unwrap();
        let first = src.next_frame(Duration::from_secs(2)).unwrap();
        let payload_ptr = first.payload.as_ptr();
        drop(first);
        let second = src.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(
            second.payload.as_ptr(),
            payload_ptr,
            "the frame source must recycle its payload allocation"
        );
    }

    #[test]
    fn narrow_roi_top_rows_match_the_pattern() {
        let mut back = NullBack::new();
        let (w, h) = (320u32, 64u32);
        back.write_reg(REG_WIDTH, w).unwrap();
        back.write_reg(REG_HEIGHT, h).unwrap();
        back.write_reg(REG_ACQUISITION_START, 1).unwrap();
        let mut src = back.open_stream().unwrap();
        let frame = src.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!((frame.width, frame.height), (w, h));
        for y in 0..h {
            for x in 0..w {
                let k = (y * w + x) as usize * 2;
                assert_eq!(
                    frame.payload[k..k + 2],
                    expected_pixel(x, y, w, h, 1),
                    "pixel ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn next_frame_honors_the_timeout_contract() {
        let mut back = NullBack::new();
        back.write_reg(REG_ACQUISITION_START, 1).unwrap();
        let mut src = back.open_stream().unwrap();
        // First frame is due immediately.
        let f1 = src.next_frame(Duration::from_secs(2)).expect("frame 1");
        assert_eq!(f1.payload.len(), PAYLOAD);

        // Contract piece (host-scheduling tolerant — a host may stall
        // between calls for longer than a frame period, making the next
        // frame legitimately due inside any window): a None answers
        // only after the caller's window is waited out; a Some carries
        // a valid frame.
        let t0 = Instant::now();
        match src.next_frame(Duration::from_millis(5)) {
            None => assert!(
                t0.elapsed() >= Duration::from_millis(5),
                "a None answers only after the window is waited out"
            ),
            Some(f2) => assert_eq!(f2.width, WIDTH, "a due frame is the next block"),
        }

        // ~30 fps pacing: at least 4 frames in 200 ms (loose bound for CI).
        let t0 = Instant::now();
        let mut n = 0;
        while t0.elapsed() < Duration::from_millis(200) {
            if src.next_frame(Duration::from_millis(50)).is_some() {
                n += 1;
            }
        }
        assert!(n >= 4, "paced frames: {n}");
    }

    #[test]
    fn close_stream_is_a_clean_noop() {
        let mut back = NullBack::new();
        let mut src = back.open_stream().unwrap();
        assert!(src.next_frame(Duration::from_millis(1)).is_none());
        back.write_reg(REG_ACQUISITION_START, 1).unwrap();
        back.close_stream().unwrap();
        assert_eq!(back.read_reg(REG_ACQUISITION_START).unwrap(), 0);
        assert!(back.open_stream().is_ok());
    }

    /// SFNC TestPattern has real semantics: the frame generator switches
    /// from the synthetic pattern to a grey ramp (both directions).
    #[test]
    fn test_pattern_switches_the_generated_pixels() {
        let mut back = NullBack::new();
        back.write_reg(REG_PIXEL_FORMAT, PFNC_MONO8).unwrap();
        back.write_reg(REG_WIDTH, 4).unwrap();
        back.write_reg(REG_HEIGHT, 2).unwrap();
        back.write_reg(REG_FRAME_RATE, 1000).unwrap();
        back.write_reg(REG_ACQUISITION_START, 1).unwrap();
        let mut src = NullFrameSource::new(back.regs.clone(), back.epoch);

        // Horizontal ramp: row-independent 0,80,170,255 (65535/3 >> 8).
        back.write_reg(REG_TEST_PATTERN, 1).unwrap();
        let frame = src.next_frame(Duration::from_secs(2)).expect("ramp frame");
        assert_eq!((frame.width, frame.height), (4, 2));
        assert_eq!(frame.payload.len(), 8);
        for row in 0..2 {
            assert_eq!(
                &frame.payload[row * 4..row * 4 + 4],
                &[0, 85, 170, 255],
                "horizontal ramp row {row}"
            );
        }
        // Vertical ramp: two flat rows.
        back.write_reg(REG_TEST_PATTERN, 2).unwrap();
        let frame = src.next_frame(Duration::from_secs(2)).expect("ramp frame");
        assert_eq!(&frame.payload[..4], &[0, 0, 0, 0]);
        assert_eq!(&frame.payload[4..], &[255, 255, 255, 255]);
        // Off restores the block-dependent synthetic pattern.
        back.write_reg(REG_TEST_PATTERN, 0).unwrap();
        let frame = src.next_frame(Duration::from_secs(2)).expect("pattern frame");
        assert_ne!(&frame.payload[..4], &[0, 0, 0, 0]);
    }

    /// SFNC ReverseX/ReverseY really mirror the generated frame.
    #[test]
    fn reverse_x_and_y_mirror_the_generated_frame() {
        let mut back = NullBack::new();
        back.write_reg(REG_PIXEL_FORMAT, PFNC_MONO8).unwrap();
        back.write_reg(REG_WIDTH, 4).unwrap();
        back.write_reg(REG_HEIGHT, 2).unwrap();
        back.write_reg(REG_FRAME_RATE, 1000).unwrap();
        back.write_reg(REG_ACQUISITION_START, 1).unwrap();
        // Horizontal ramp makes the mirroring observable per row.
        back.write_reg(REG_TEST_PATTERN, 1).unwrap();
        let mut src = NullFrameSource::new(back.regs.clone(), back.epoch);
        let frame = src.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(&frame.payload[..4], &[0, 85, 170, 255]);
        back.write_reg(REG_REVERSE_X, 1).unwrap();
        let frame = src.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(&frame.payload[..4], &[255, 170, 85, 0]);
        assert_eq!(&frame.payload[4..], &[255, 170, 85, 0], "both rows mirrored");
        // Vertical ramp exposes ReverseY: the ramp runs top-down, so the
        // bottom row carries the high value.
        back.write_reg(REG_TEST_PATTERN, 2).unwrap();
        let frame = src.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(&frame.payload[..4], &[0, 0, 0, 0], "top row = ramp start");
        assert_eq!(&frame.payload[4..], &[255, 255, 255, 255], "bottom row = ramp end");
        back.write_reg(REG_REVERSE_Y, 1).unwrap();
        let frame = src.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!(&frame.payload[..4], &[255, 255, 255, 255], "ReverseY flips the rows");
        assert_eq!(&frame.payload[4..], &[0, 0, 0, 0]);
    }

    /// SFNC UserSetControl has real semantics: save snapshots the feature
    /// set, load restores it, and DeviceReset clears it back to factory.
    #[test]
    fn user_set_save_and_load_restore_the_feature_set() {
        let mut back = NullBack::new();
        back.write_reg(REG_WIDTH, 640).unwrap();
        back.write_reg(REG_HEIGHT, 480).unwrap();
        back.write_reg(REG_USER_SET_SELECTOR, 1).unwrap();
        back.write_reg(REG_USER_SET_SAVE, 1).unwrap();
        assert_eq!(back.read_reg(REG_USER_SET_SAVE).unwrap(), 0, "command self-clears");

        back.write_reg(REG_WIDTH, 320).unwrap();
        back.write_reg(REG_HEIGHT, 240).unwrap();
        back.write_reg(REG_USER_SET_LOAD, 1).unwrap();
        assert_eq!(back.read_reg(REG_WIDTH).unwrap(), 640);
        assert_eq!(back.read_reg(REG_HEIGHT).unwrap(), 480);

        // DeviceReset restores factory defaults and clears the saved set.
        back.write_reg(REG_DEVICE_RESET, 1).unwrap();
        assert_eq!(back.read_reg(REG_WIDTH).unwrap(), WIDTH);
        back.write_reg(REG_USER_SET_SELECTOR, 1).unwrap();
        back.write_reg(REG_USER_SET_LOAD, 1).unwrap();
        assert_eq!(
            back.read_reg(REG_WIDTH).unwrap(),
            WIDTH,
            "a reset user set loads factory defaults"
        );
    }

    /// The XML exposes the SFNC features the register model implements, in
    /// their standard categories (the gateway injects this XML into the
    /// virtual device, so GenApi hosts see them).
    #[test]
    fn xml_exposes_test_pattern_and_user_sets() {
        let mut back = NullBack::new();
        let xml = String::from_utf8(back.fetch_xml().unwrap()).unwrap();
        for needle in [
            "<Enumeration Name=\"TestPattern\"",
            "<Boolean Name=\"ReverseX\"",
            "<Boolean Name=\"ReverseY\"",
            "<Enumeration Name=\"UserSetSelector\"",
            "<Enumeration Name=\"UserSetDefault\"",
            "<Command Name=\"UserSetSave\"",
            "<Command Name=\"UserSetLoad\"",
            "<Category Name=\"UserSetControl\"",
            "<pFeature>TestPattern</pFeature>",
            "<pFeature>ReverseX</pFeature>",
            "<pFeature>ReverseY</pFeature>",
            "<pFeature>UserSetControl</pFeature>",
        ] {
            assert!(xml.contains(needle), "missing {needle}");
        }
    }
}
