//! vgige-back-null: the camera-less `CameraBack` (CLI `--back null`).
//! Unlocks the gateway demos with no camera attached: the GigE/U3V
//! virtual device fronts a synthetic camera — a minimal but GenApi-legal
//! register view plus a 30 fps Mono8 test-pattern stream the host can
//! verify byte-for-byte (`(byte_index + block_id) mod 256`, the
//! simulator's pattern).
//!
//! v1 demo scope (design note §5):
//! - Register reads return 0 except the three registers the built-in
//!   XML advertises (Width/Height/PixelFormat, seeded with the stream
//!   geometry so a GenApi client's static view matches the stream);
//!   writes are accepted as no-ops.
//! - The built-in XML is little-endian: the null demo targets the U3V
//!   device front (R-30cd wire). Served through the GigE front the
//!   numeric register reads byte-swap (pass-through transcoding) — a
//!   demo limitation, not worth a second document in v1.
//! - The XML deliberately carries NO SmartCamera group: the gateway's
//!   XmlMerger injects it on top, exactly like a real camera's XML.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vgige_core::{BackIdentity, CameraBack, CanonicalFrame, FrameSource};

/// Synthetic stream geometry: 1280x1024 Mono8 (the sim's production
/// size, 1.25 MB per frame — a comfortable USB3/gigE demo load).
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 1024;
pub const PAYLOAD: usize = (WIDTH * HEIGHT) as usize;
pub const PFNC_MONO8: u32 = 0x0108_0001;

/// Frame period for the ~30 fps synthetic source.
pub const FRAME_PERIOD: Duration = Duration::from_micros(1_000_000 / 30);

/// The XML-advertised registers (little-endian GenCP-shaped addresses,
/// forwarded nowhere — served from the seed map). NOTE: the acquisition
/// block addresses MUST mirror vgige-front-u3v's ACQ_BASE (§4.2.2
/// relocation, 2026-10-28) — the XML is the host's map to the wire.
pub const REG_WIDTH: u32 = 0x0001_0000;
pub const REG_HEIGHT: u32 = 0x0001_0004;
pub const REG_PIXEL_FORMAT: u32 = 0x0001_0008;

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
\n\x20 <Group Comment=\"ImageFormat\">\n\
\x20   <Integer Name=\"Width\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Image width in pixels.</ToolTip>\n\
\x20     <DisplayName>Width</DisplayName>\n\
\x20     <Visibility>Beginner</Visibility>\n\
\x20     <pValue>WidthReg</pValue>\n\
\x20     <Min>1</Min>\n\
\x20     <Max>4096</Max>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"WidthReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00010000</Address>\n\
\x20     <Length>4</Length>\n\
\x20     <AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort>\n\
\x20     <Sign>Unsigned</Sign>\n\
\x20     <Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"Height\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Image height in pixels.</ToolTip>\n\
\x20     <DisplayName>Height</DisplayName>\n\
\x20     <Visibility>Beginner</Visibility>\n\
\x20     <pValue>HeightReg</pValue>\n\
\x20     <Min>1</Min>\n\
\x20     <Max>4096</Max>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"HeightReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00010004</Address>\n\
\x20     <Length>4</Length>\n\
\x20     <AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort>\n\
\x20     <Sign>Unsigned</Sign>\n\
\x20     <Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Enumeration Name=\"PixelFormat\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Pixel format of the stream.</ToolTip>\n\
\x20     <DisplayName>Pixel Format</DisplayName>\n\
\x20     <Visibility>Beginner</Visibility>\n\
\x20     <EnumEntry Name=\"Mono8\" NameSpace=\"Standard\">\n\
\x20       <DisplayName>Mono8</DisplayName>\n\
\x20       <Value>0x01080001</Value>\n\
\x20     </EnumEntry>\n\
\x20     <pValue>PixelFormatReg</pValue>\n\
\x20   </Enumeration>\n\
\x20   <IntReg Name=\"PixelFormatReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00010008</Address>\n\
\x20     <Length>4</Length>\n\
\x20     <AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort>\n\
\x20     <Sign>Unsigned</Sign>\n\
\x20     <Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20 </Group>\n\
\x20 <Group Comment=\"AcquisitionControl\">\n\
\x20   <Command Name=\"AcquisitionStart\" NameSpace=\"Standard\">\n\
\x20     <pValue>AcquisitionStartReg</pValue>\n\
\x20     <CommandValue>1</CommandValue>\n\
\x20   </Command>\n\
\x20   <IntReg Name=\"AcquisitionStartReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00040000</Address>\n\
\x20     <Length>4</Length>\n\
\x20     <AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort>\n\
\x20     <Sign>Unsigned</Sign>\n\
\x20     <Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Command Name=\"AcquisitionStop\" NameSpace=\"Standard\">\n\
\x20     <pValue>AcquisitionStopReg</pValue>\n\
\x20     <CommandValue>1</CommandValue>\n\
\x20   </Command>\n\
\x20   <IntReg Name=\"AcquisitionStopReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00040004</Address>\n\
\x20     <Length>4</Length>\n\
\x20     <AccessMode>RW</AccessMode>\n\
\x20     <pPort>Device</pPort>\n\
\x20     <Sign>Unsigned</Sign>\n\
\x20     <Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20   <Integer Name=\"PayloadSize\" NameSpace=\"Standard\">\n\
\x20     <ToolTip>Full padded frame payload in bytes.</ToolTip>\n\
\x20     <DisplayName>Payload Size</DisplayName>\n\
\x20     <Visibility>Beginner</Visibility>\n\
\x20     <pValue>PayloadSizeReg</pValue>\n\
\x20     <Min>1</Min>\n\
\x20     <Max>1073741824</Max>\n\
\x20   </Integer>\n\
\x20   <IntReg Name=\"PayloadSizeReg\" NameSpace=\"Custom\">\n\
\x20     <Address>0x00040008</Address>\n\
\x20     <Length>8</Length>\n\
\x20     <AccessMode>RO</AccessMode>\n\
\x20     <pPort>Device</pPort>\n\
\x20     <Sign>Unsigned</Sign>\n\
\x20     <Endianess>LittleEndian</Endianess>\n\
\x20   </IntReg>\n\
\x20 </Group>\n\
</RegisterDescription>\n";

/// The synthetic camera: seeded register view + pattern stream.
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

    fn write_reg(&mut self, _addr: u32, _value: u32) -> Result<(), u16> {
        // Accepted, not stored (v1 demo: the register view is static).
        Ok(())
    }

    fn read_block(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, u16> {
        if len == 4 {
            return self.read_reg(addr).map(|v| v.to_le_bytes().to_vec());
        }
        Ok(vec![0u8; len as usize])
    }

    fn write_block(&mut self, _addr: u32, _data: &[u8]) -> Result<(), u16> {
        Ok(())
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
        Ok(Box::new(NullFrameSource::new()))
    }

    fn close_stream(&mut self) -> Result<(), u16> {
        Ok(())
    }
}

/// The ~30 fps test-pattern source: Mono8, byte `k` of block `b` is
/// `(k + b) mod 256` — verifiable against the simulator's formula from
/// any host consumer.
pub struct NullFrameSource {
    next_block: u64,
    next_at: Instant,
}

impl NullFrameSource {
    pub fn new() -> Self {
        Self {
            next_block: 1,
            next_at: Instant::now(),
        }
    }
}

impl Default for NullFrameSource {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameSource for NullFrameSource {
    fn next_frame(&mut self, timeout: Duration) -> Option<CanonicalFrame> {
        let now = Instant::now();
        if now < self.next_at {
            let wait = self.next_at - now;
            if wait > timeout {
                // No frame due inside the caller's window: honor the
                // FrameSource contract by waiting the timeout out.
                std::thread::sleep(timeout);
                return None;
            }
            std::thread::sleep(wait);
        }
        let block = self.next_block;
        self.next_block += 1;
        self.next_at = Instant::now() + FRAME_PERIOD;
        Some(CanonicalFrame {
            // Synthetic timestamp: 1 MHz ticks, 33.333 ms per block.
            timestamp: block * 33_333,
            pixel_format: PFNC_MONO8,
            width: WIDTH,
            height: HEIGHT,
            offset_x: 0,
            offset_y: 0,
            padding_x: 0,
            padding_y: 0,
            payload: (0..PAYLOAD).map(|k| (k as u64 + block) as u8).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_semantics_and_identity() {
        let mut back = NullBack::new();
        // Seeded registers advertise the stream geometry.
        assert_eq!(back.read_reg(REG_WIDTH).unwrap(), WIDTH);
        assert_eq!(back.read_reg(REG_HEIGHT).unwrap(), HEIGHT);
        assert_eq!(back.read_reg(REG_PIXEL_FORMAT).unwrap(), PFNC_MONO8);
        // Everything else reads 0; every write is accepted.
        assert_eq!(back.read_reg(0x0FF0).unwrap(), 0);
        back.write_reg(0x0FF0, 42).unwrap();
        assert_eq!(back.read_reg(0x0FF0).unwrap(), 0, "writes are no-ops");
        let block = back.read_block(0x0FF0, 16).unwrap();
        assert_eq!(block, vec![0u8; 16]);
        back.write_block(0x0FF0, &[1, 2, 3, 4]).unwrap();

        let id = back.fetch_identity().unwrap();
        assert_eq!(id.manufacturer, "GenTLite");
        assert_eq!(id.model, "Virtual");
        assert_eq!(id.serial_number, "NULL-0001");

        let xml = back.fetch_xml().unwrap();
        let text = String::from_utf8(xml).unwrap();
        assert!(text.contains("</RegisterDescription>"));
        assert!(text.contains("ModelName=\"GenTLite Virtual\""));
        assert!(text.contains("WidthReg"));
        assert!(text.contains("HeightReg"));
        assert!(text.contains("PixelFormatReg"));
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
