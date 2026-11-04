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

use vgige_core::{BackIdentity, CameraBack, CanonicalFrame, FramePayload, FrameSource};

/// Default synthetic stream geometry: 1280x1024 Mono16. Width, height,
/// offsets, exposure, gain and frame rate remain writable through GenCP.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 1024;
pub const BYTES_PER_PIXEL: usize = 2;
pub const PAYLOAD: usize = WIDTH as usize * HEIGHT as usize * BYTES_PER_PIXEL;
pub const PFNC_MONO8: u32 = 0x0108_0001;
pub const PFNC_MONO16: u32 = 0x0110_0001;
pub const EXPOSURE_US: u32 = 10_000;
pub const GAIN: u32 = 0;
pub const FRAME_RATE: u32 = 30;
/// Reusable payload slots. Relay ring (4) + U3V emitter queue (4) + the
/// frame being generated gives at most 9 live buffers; 12 leaves margin.
const FRAME_POOL_DEPTH: usize = 12;

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
/// SFNC 2.x AcquisitionMode: Continuous = 2.
pub const REG_ACQUISITION_MODE: u32 = 0x0004_000C;

/// Minimal but GenApi-legal device description (v1 demo — placeholder
/// GUIDs in the simulator's style; SmartCamera nodes come from the
/// gateway's XmlMerger injection, not from this document).
pub const NULL_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<RegisterDescription xmlns=\"http://www.genicam.org/GenApi/Version_1_1\" \
xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
ModelName=\"GenTLite Virtual\" VendorName=\"GenTLite\" \
ToolTip=\"GenTLite null camera: camera-less gateway demo\" StandardNameSpace=\"None\" \
SchemaMajorVersion=\"1\" SchemaMinorVersion=\"1\" SchemaSubMinorVersion=\"0\" \
MajorVersion=\"1\" MinorVersion=\"0\" SubMinorVersion=\"0\" \
ProductGuid=\"5a1f3c8e-2b7d-4e9a-9c4d-1e2f3a4b5c6d\" \
VersionGuid=\"6b2e4d9f-3c8a-5f1b-8d5e-2f3a4b5c6d7e\">\n\
\x20 <Port Name=\"Device\" NameSpace=\"Standard\">\n\
\x20   <ToolTip>The GenCP device port (implemented by the GenTL producer).</ToolTip>\n\
\x20 </Port>\n\
\x20 <Group Comment=\"Categories\">\n\
\x20   <Category Name=\"Root\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Provides the Root of the GenICam features tree.</ToolTip>\n\
\x20     <Description>Provides the Root of the GenICam features tree.</Description>\n\
\x20     <DisplayName>Root</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <ImposedAccessMode>RO</ImposedAccessMode>\n\
\x20     <pFeature>DeviceVendorName</pFeature><pFeature>DeviceModelName</pFeature>\n\
\x20     <pFeature>DeviceVersion</pFeature><pFeature>DeviceFirmwareVersion</pFeature>\n\
\x20     <pFeature>DeviceSerialNumber</pFeature><pFeature>DeviceUserID</pFeature>\n\
\x20     <pFeature>Width</pFeature><pFeature>Height</pFeature>\n\
\x20     <pFeature>OffsetX</pFeature><pFeature>OffsetY</pFeature>\n\
\x20     <pFeature>PixelFormat</pFeature><pFeature>PayloadSize</pFeature>\n\
\x20     <pFeature>ExposureTimeAbs</pFeature><pFeature>Gain</pFeature>\n\
\x20     <pFeature>FrameRate</pFeature><pFeature>AcquisitionFrameRate</pFeature>\n\
\x20     <pFeature>AcquisitionStart</pFeature><pFeature>AcquisitionStop</pFeature>\n\
\x20     <pFeature>AcquisitionMode</pFeature><pFeature>TriggerMode</pFeature>\n\
\x20     <pFeature>SmartCamera</pFeature>\n\
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
\x20   <Enumeration Name=\"PixelFormat\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Pixel Format</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <EnumEntry Name=\"Mono8\" NameSpace=\"Standard\">\n\
\x20       <DisplayName>Mono8</DisplayName><Value>0x01080001</Value>\n\
\x20     </EnumEntry>\n\
\x20     <EnumEntry Name=\"Mono16\" NameSpace=\"Standard\">\n\
\x20       <DisplayName>Mono16</DisplayName><Value>0x01100001</Value>\n\
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
\x20     <pValue>ExposureTimeAbs_Reg</pValue><Min>0</Min><Max>100000</Max><Unit>us</Unit>\n\
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
\x20     <pValue>FrameRate_Reg</pValue><Min>0</Min><Max>240</Max><Unit>Hz</Unit>\n\
\x20   </Float>\n\
\x20   <IntReg Name=\"FrameRate_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x0004101C</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Float Name=\"AcquisitionFrameRate\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Acquisition Frame Rate</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>FrameRate_Reg</pValue><Min>0</Min><Max>240</Max><Unit>Hz</Unit>\n\
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
\x20     <EnumEntry Name=\"On\" NameSpace=\"Standard\"><DisplayName>On</DisplayName><Value>1</Value></EnumEntry>\n\
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
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20 </Group>\n\
</RegisterDescription>\n";

/// The synthetic camera: a writable register view plus a pattern stream.
pub struct NullBack {
    regs: Arc<Mutex<std::collections::HashMap<u32, u32>>>,
}

impl NullBack {
    pub fn new() -> Self {
        Self {
            regs: Arc::new(Mutex::new(std::collections::HashMap::from([
                (REG_WIDTH, WIDTH),
                (REG_HEIGHT, HEIGHT),
                (REG_PIXEL_FORMAT, PFNC_MONO16),
                (REG_OFFSET_X, 0),
                (REG_OFFSET_Y, 0),
                (REG_EXPOSURE_US, EXPOSURE_US),
                (REG_GAIN, GAIN),
                (REG_FRAME_RATE, FRAME_RATE),
                (REG_ACQUISITION_MODE, 2), // SFNC Continuous
            ]))),
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
                regs.insert(addr, value.clamp(1, 240));
            }
            _ => {}
        }
        Ok(())
    }

    fn read_block(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, u16> {
        if addr == 0x0004_0008 && (len == 4 || len == 8) {
            let payload = self.payload_size()?.to_le_bytes();
            return Ok(payload[..len as usize].to_vec());
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
        Ok(Box::new(NullFrameSource::new(self.regs.clone())))
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
    next_block: u64,
    next_at: Instant,
    recycler: SyncSender<Vec<u8>>,
    pool: Receiver<Vec<u8>>,
}

impl NullFrameSource {
    pub fn new(regs: Arc<Mutex<std::collections::HashMap<u32, u32>>>) -> Self {
        let (recycler, pool) = sync_channel(FRAME_POOL_DEPTH);
        Self {
            regs,
            next_block: 1,
            next_at: Instant::now(),
            recycler,
            pool,
        }
    }
}

impl Default for NullFrameSource {
    fn default() -> Self {
        Self::new(NullBack::new().regs)
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
        let (w, h, ox, oy, fps, pixel_format) = {
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
        Some(CanonicalFrame {
            timestamp: block * (1_000_000 / u64::from(fps)),
            pixel_format,
            width: w,
            height: h,
            offset_x: ox,
            offset_y: oy,
            padding_x: 0,
            padding_y: 0,
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
        assert!(text.contains("ModelName=\"GenTLite Virtual\""));
        for node in ["Width_Reg", "Height_Reg", "OffsetX_Reg", "OffsetY_Reg", "ExposureTimeAbs_Reg", "Gain_Reg", "FrameRate_Reg", "AcquisitionMode", "TriggerMode", "TriggerModeReg", "AcquisitionFrameRate"] {
            assert!(text.contains(node), "missing {node}");
        }
        for node in ["ExposureTimeAbs", "Gain", "FrameRate"] {
            assert!(text.contains(&format!("<Float Name=\"{node}\"")), "{node} must be a GenApi Float");
            assert!(text.contains(&format!("<IntReg Name=\"{node}_Reg\"")), "{node}_Reg must be a GenApi IntReg");
        }
        assert!(text.contains("<EnumEntry Name=\"Mono8\""));
        assert!(text.contains("<EnumEntry Name=\"Mono16\""));
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
}
