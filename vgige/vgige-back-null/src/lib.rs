//! vgige-back-null: the camera-less `CameraBack` (CLI `--back null`).
//! Unlocks the gateway demos with no camera attached: the GigE/U3V
//! virtual device fronts a synthetic camera — a minimal but GenApi-legal
//! register view plus a 30 fps Mono8 test-pattern stream the host can
//! verify byte-for-byte (`(byte_index + block_id) mod 256`, the
//! simulator's pattern).
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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vgige_core::{BackIdentity, CameraBack, CanonicalFrame, FrameSource};

/// Default synthetic stream geometry: 1280x1024 Mono8. Width, height,
/// offsets, exposure, gain and frame rate remain writable through GenCP.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 1024;
pub const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
pub const PFNC_MONO8: u32 = 0x0108_0001;
pub const EXPOSURE_US: u32 = 10_000;
pub const GAIN: u32 = 0;
pub const FRAME_RATE: u32 = 30;

fn frame_period(fps: u32) -> Duration {
    Duration::from_micros(1_000_000 / u64::from(fps.max(1)))
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
\x20 <String Name=\"DeviceVendorName\" NameSpace=\"Standard\"><Value>GenTLite</Value></String>\n\
\x20 <String Name=\"DeviceModelName\" NameSpace=\"Standard\"><Value>Virtual</Value></String>\n\
\x20 <String Name=\"DeviceVersion\" NameSpace=\"Standard\"><Value>1.0.0</Value></String>\n\
\x20 <String Name=\"DeviceFirmwareVersion\" NameSpace=\"Standard\"><Value>1.0.0</Value></String>\n\
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
\x20     <pValue>PixelFormatReg</pValue>\n\
\x20   </Enumeration>\n\
\x20   <IntReg Name=\"PixelFormatReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041008</Address><Length>4</Length><AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20 </Group>\n\
\x20 <Group Comment=\"AcquisitionControl\">\n\
\x20   <Integer Name=\"ExposureTimeAbs\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Exposure Time (us)</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>ExposureTimeAbs_Reg</pValue><Min>10</Min><Max>100000</Max><Inc>1</Inc>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"ExposureTimeAbs_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041014</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"Gain\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Gain</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>Gain_Reg</pValue><Min>0</Min><Max>100</Max><Inc>1</Inc>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"Gain_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00041018</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"FrameRate\" NameSpace=\"Standard\">\n\
\x20     <DisplayName>Frame Rate</DisplayName><Visibility>Beginner</Visibility>\n\
\x20     <pValue>FrameRate_Reg</pValue><Min>1</Min><Max>240</Max><Inc>1</Inc>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"FrameRate_Reg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x0004101C</Address><Length>4</Length><AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
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
\x20     <EnumEntry Name=\"Continuous\" NameSpace=\"Standard\"><DisplayName>Continuous</DisplayName><Value>0</Value></EnumEntry>\n\
\x20     <pValue>AcquisitionModeReg</pValue>\n\
\x20   </Enumeration>\n\
\x20   <IntReg Name=\"AcquisitionModeReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x0004000C</Address><Length>4</Length><AccessMode>RO</AccessMode>\n\
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
                (REG_PIXEL_FORMAT, PFNC_MONO8),
                (REG_OFFSET_X, 0),
                (REG_OFFSET_Y, 0),
                (REG_EXPOSURE_US, EXPOSURE_US),
                (REG_GAIN, GAIN),
                (REG_FRAME_RATE, FRAME_RATE),
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
    fn read_reg(&mut self, addr: u32) -> Result<u32, u16> {
        Ok(self.regs.lock().expect("null regs lock").get(&addr).copied().unwrap_or(0))
    }

    fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), u16> {
        let mut regs = self.regs.lock().expect("null regs lock");
        match addr {
            REG_WIDTH | REG_HEIGHT => {
                regs.insert(addr, value.clamp(1, 4096));
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
        Ok(w * h)
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
        Ok(())
    }
}

/// Configurable test-pattern source. ROI pixels are sampled from the same
/// full-frame `(source_byte_index + block_id) mod 256` pattern.
pub struct NullFrameSource {
    regs: Arc<Mutex<std::collections::HashMap<u32, u32>>>,
    next_block: u64,
    next_at: Instant,
}

impl NullFrameSource {
    pub fn new(regs: Arc<Mutex<std::collections::HashMap<u32, u32>>>) -> Self {
        Self {
            regs,
            next_block: 1,
            next_at: Instant::now(),
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
        let (w, h, ox, oy, fps) = {
            let regs = self.regs.lock().expect("null regs lock");
            (
                regs.get(&REG_WIDTH).copied().unwrap_or(WIDTH),
                regs.get(&REG_HEIGHT).copied().unwrap_or(HEIGHT),
                regs.get(&REG_OFFSET_X).copied().unwrap_or(0),
                regs.get(&REG_OFFSET_Y).copied().unwrap_or(0),
                regs.get(&REG_FRAME_RATE).copied().unwrap_or(FRAME_RATE),
            )
        };
        let block = self.next_block;
        self.next_block += 1;
        self.next_at = Instant::now() + frame_period(fps);
        let mut payload = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                let source_index = u64::from(y + oy) * u64::from(WIDTH) + u64::from(x + ox);
                payload.push((source_index + block) as u8);
            }
        }
        Some(CanonicalFrame {
            timestamp: block * (1_000_000 / u64::from(fps)),
            pixel_format: PFNC_MONO8,
            width: w,
            height: h,
            offset_x: ox,
            offset_y: oy,
            padding_x: 0,
            padding_y: 0,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_semantics_and_identity() {
        let mut back = NullBack::new();
        assert_eq!(back.read_reg(REG_WIDTH).unwrap(), WIDTH);
        assert_eq!(back.read_reg(REG_HEIGHT).unwrap(), HEIGHT);
        assert_eq!(back.read_reg(REG_PIXEL_FORMAT).unwrap(), PFNC_MONO8);
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
        assert_eq!(back.payload_size().unwrap(), 640 * 480);
        assert_eq!(back.read_reg(REG_OFFSET_X).unwrap(), 40);
        assert_eq!(back.read_reg(REG_OFFSET_Y).unwrap(), 24);
        assert_eq!(back.read_reg(REG_EXPOSURE_US).unwrap(), 20_000);
        assert_eq!(back.read_reg(REG_GAIN).unwrap(), 8);
        assert_eq!(back.read_reg(REG_FRAME_RATE).unwrap(), 60);
        assert_eq!(frame_period(60), Duration::from_micros(16_666));

        let id = back.fetch_identity().unwrap();
        assert_eq!(id.manufacturer, "GenTLite");
        assert_eq!(id.model, "Virtual");
        assert_eq!(id.serial_number, "NULL-0001");

        let xml = back.fetch_xml().unwrap();
        let text = String::from_utf8(xml).unwrap();
        assert!(text.contains("</RegisterDescription>"));
        assert!(text.contains("ModelName=\"GenTLite Virtual\""));
        for node in ["Width_Reg", "Height_Reg", "OffsetX_Reg", "OffsetY_Reg", "ExposureTimeAbs_Reg", "Gain_Reg", "FrameRate_Reg", "AcquisitionMode"] {
            assert!(text.contains(node), "missing {node}");
        }
    }

    #[test]
    fn frames_are_sized_and_patterned() {
        let mut back = NullBack::new();
        let mut src = back.open_stream().unwrap();
        let f1 = src.next_frame(Duration::from_secs(2)).expect("frame 1");
        assert_eq!(f1.width, WIDTH);
        assert_eq!(f1.height, HEIGHT);
        assert_eq!(f1.pixel_format, PFNC_MONO8);
        assert_eq!(f1.payload.len(), PAYLOAD);
        assert!(
            f1.payload
                .iter()
                .enumerate()
                .all(|(k, &b)| b == (k as u64 + 1) as u8),
            "block 1 payload is (k + 1) mod 256, byte-exact"
        );
        let f2 = src.next_frame(Duration::from_secs(2)).expect("frame 2");
        assert!(
            f2.payload
                .iter()
                .enumerate()
                .all(|(k, &b)| b == (k as u64 + 2) as u8),
            "block 2 payload is (k + 2) mod 256, byte-exact"
        );
        assert!(f2.timestamp > f1.timestamp);
    }

    #[test]
    fn roi_changes_geometry_payload_and_offsets() {
        let mut back = NullBack::new();
        back.write_reg(REG_WIDTH, 2).unwrap();
        back.write_reg(REG_HEIGHT, 2).unwrap();
        back.write_reg(REG_OFFSET_X, 1).unwrap();
        back.write_reg(REG_OFFSET_Y, 1).unwrap();
        let mut src = back.open_stream().unwrap();
        let frame = src.next_frame(Duration::from_secs(2)).unwrap();
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!((frame.offset_x, frame.offset_y), (1, 1));
        assert_eq!(frame.payload, vec![2, 3, 2, 3]);
    }

    #[test]
    fn next_frame_honors_the_timeout_contract() {
        let mut back = NullBack::new();
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

        // ~30 fps pacing: at least 5 frames in 200 ms (loose bound for CI).
        let t0 = Instant::now();
        let mut n = 0;
        while t0.elapsed() < Duration::from_millis(200) {
            if src.next_frame(Duration::from_millis(50)).is_some() {
                n += 1;
            }
        }
        assert!(n >= 5, "paced frames: {n}");
    }

    #[test]
    fn close_stream_is_a_clean_noop() {
        let mut back = NullBack::new();
        back.close_stream().unwrap();
        // The stream can still be (re)opened — null has no teardown.
        assert!(back.open_stream().is_ok());
    }
}
