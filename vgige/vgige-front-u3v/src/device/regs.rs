//! The device-side register file: ABRM/SBRM/SIRM answer the host's
//! GenCP reads/writes; the manifest table points at the injected XML
//! blob; the custom window (vgige-core slot constants) is served
//! locally; every other address forwards to the downstream camera via
//! [`vgige_core::CameraBack`] — the gateway resells the real camera's
//! register space, and the served GenICam XML's vendor nodes carry
//! exactly these addresses, so the host's GenApi traffic lands on the
//! camera 1:1.
//!
//! Layout (device QoI): ABRM @0x0000 (0x400), manifest @0x0001_0000,
//! SBRM @0x0001_1000, XML @0x0002_0000, SIRM @0x0003_0000 (0x48),
//! demo acquisition @0x0004_0000, custom window @0x2000_0000 (0x1000).
//! All multi-byte registers are little-endian (R-30cd).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use gentl_u3v::packets;
use vgige_core::{slot, SharedBack, CUSTOM_BASE, CUSTOM_SLOTS, VGIGE_VERSION};

use crate::device::sha1;
use crate::{DeviceConfig, MANIFEST_BASE, SBRM_BASE, SIRM_BASE, XML_BASE};

/// The XML blob size cap (mirrors the host-side sanity bound,
/// control.rs read_device_xml).
const XML_MAX: usize = 32 << 20;

/// Demo acquisition block (SFNC level, see build_regions): Start/Stop
/// pokes at +0x00/+0x04, PayloadSize (u64) at +0x08.
const ACQ_BASE: u64 = 0x0004_0000;

struct Region {
    base: u64,
    mem: Mutex<Vec<u8>>,
    writable: bool,
}

impl Region {
    fn contains(&self, addr: u64, len: u64) -> bool {
        addr >= self.base
            && addr + len <= self.base + self.mem.lock().expect("region lock").len() as u64
    }
}

/// Device-side ABRM/SBRM/SIRM register file with the gateway dispatch
/// (local ranges + custom window + camera pass-through).
pub struct RegisterFile {
    regions: Vec<Region>,
    config: DeviceConfig,
    back: SharedBack,
    sirm_control: Mutex<u32>,
    eirm_control: Mutex<u32>,
    event_in: Mutex<Option<Arc<dyn super::DeviceTransfers>>>,
    watcher: Mutex<Option<Box<dyn Fn(bool) + Send>>>,
    general_slots: Mutex<[u32; CUSTOM_SLOTS]>,
    ai_result: AtomicU32,
    ai_score: AtomicU32,
    /// FrameCountRelayed (custom-window slot 3): shared with the
    /// stream session, which increments it per forwarded frame.
    pub frames: Arc<AtomicU32>,
}

impl RegisterFile {
    pub fn new(config: DeviceConfig, back: SharedBack) -> Self {
        let regions = build_regions(&config);
        Self {
            regions,
            config,
            back,
            sirm_control: Mutex::new(0),
            eirm_control: Mutex::new(0),
            event_in: Mutex::new(None),
            watcher: Mutex::new(None),
            general_slots: Mutex::new([0; CUSTOM_SLOTS]),
            ai_result: AtomicU32::new(0),
            ai_score: AtomicU32::new(0),
            frames: Arc::new(AtomicU32::new(0)),
        }
    }

    pub(crate) fn config_ref(&self) -> &DeviceConfig {
        &self.config
    }

    pub fn set_event_in(&self, stream: Arc<dyn super::DeviceTransfers>) {
        *self.event_in.lock().expect("event-in lock") = Some(stream);
    }

    /// Watch Stream Enable transitions (§5.4.1.2): fired from the
    /// SI_CONTROL write path.
    pub fn set_stream_watcher(&self, watcher: impl Fn(bool) + Send + 'static) {
        *self.watcher.lock().expect("watcher lock") = Some(Box::new(watcher));
    }

    /// Emitter-fatal recovery: the stream transport died mid-block (e.g.
    /// a host-side pipe reset surfaces as EPIPE) — clear Stream Enable in
    /// the SIRM register and internally, and fire the watcher's close, so
    /// the next host enable is a real 0→1 transition with the relay and
    /// emitter restarted. Without this a crashed consumer leaves the
    /// virtual camera streaming into the void and every later acquisition
    /// joins mid-frame (observed 2026-10-27).
    pub fn abort_stream(&self) {
        let si_lo = SIRM_BASE + packets::sirm::SI_CONTROL;
        {
            let mut guard = self.sirm_control.lock().expect("sirm control lock");
            *guard &= !packets::sirm::CONTROL_STREAM_ENABLE;
        }
        if let Some(r) = self.region(SIRM_BASE) {
            let mut m = r.mem.lock().expect("region lock");
            let off = (si_lo - r.base) as usize;
            let v = u32::from_le_bytes(m[off..off + 4].try_into().expect("u32"))
                & !packets::sirm::CONTROL_STREAM_ENABLE;
            m[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        if let Some(w) = &*self.watcher.lock().expect("watcher lock") {
            w(false);
        }
    }

    pub fn set_ai_result(&self, v: u32) {
        self.ai_result.store(v, Ordering::Relaxed);
    }

    pub fn set_ai_score(&self, v: u32) {
        self.ai_score.store(v, Ordering::Relaxed);
    }

    /// Current Stream Enable bit (the emitter's gate).
    pub fn stream_enabled(&self) -> bool {
        *self.sirm_control.lock().expect("sirm control lock") & packets::sirm::CONTROL_STREAM_ENABLE
            != 0
    }

    /// The host-programmed transfer geometry (§5.2.1.3): transfer size,
    /// count, final1, final2. All zeros until the host programs.
    pub fn programmed_geometry(&self) -> (u32, u32, u32, u32) {
        let r = self.region(SIRM_BASE).expect("sirm region");
        let m = r.mem.lock().expect("sirm lock");
        let w = |off: u64| {
            let b = (off - SIRM_BASE) as usize;
            u32::from_le_bytes(m[b..b + 4].try_into().expect("u32"))
        };
        (
            w(SIRM_BASE + packets::sirm::SI_PAYLOAD_TRANSFER_SIZE),
            w(SIRM_BASE + packets::sirm::SI_PAYLOAD_TRANSFER_COUNT),
            w(SIRM_BASE + packets::sirm::SI_PAYLOAD_FINAL1_SIZE),
            w(SIRM_BASE + packets::sirm::SI_PAYLOAD_FINAL2_SIZE),
        )
    }

    fn region(&self, addr: u64) -> Option<&Region> {
        // Regions do not overlap and are stored in ascending base
        // order: the containing region is the last one with base ≤ addr.
        self.regions.iter().rev().find(|r| addr >= r.base)
    }

    /// One register/block read; Err is a GenCP status code (§4.3.2.1).
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), u16> {
        let len = buf.len() as u64;
        if (addr == ACQ_BASE + 0x08 || addr == SIRM_BASE + packets::sirm::SI_REQUIRED_PAYLOAD_SIZE)
            && (len == 4 || len == 8)
        {
            if let Ok(size) = self.back.lock().map_err(|_| packets::GENCP_ERROR)?.payload_size() {
                let bytes = size.to_le_bytes();
                buf.copy_from_slice(&bytes[..len as usize]);
                return Ok(());
            }
        }
        // Custom window first (it overlaps no device region, but the
        // dispatch is clearest ordered: device → window → camera).
        if addr >= u64::from(CUSTOM_BASE)
            && addr + len <= u64::from(CUSTOM_BASE) + (CUSTOM_SLOTS as u64) * 4
        {
            if len % 4 != 0 || addr % 4 != 0 {
                return Err(packets::GENCP_BAD_ALIGNMENT);
            }
            let mut pos = addr;
            for chunk in buf.chunks_exact_mut(4) {
                chunk.copy_from_slice(
                    &self
                        .slot_read(((pos - u64::from(CUSTOM_BASE)) / 4) as usize)
                        .to_le_bytes(),
                );
                pos += 4;
            }
            return Ok(());
        }
        if let Some(r) = self.region(addr).filter(|r| r.contains(addr, len)) {
            let m = r.mem.lock().expect("region lock");
            let off = (addr - r.base) as usize;
            buf.copy_from_slice(&m[off..off + buf.len()]);
            return Ok(());
        }
        self.back_read(addr, len)
            .map(|data| buf.copy_from_slice(&data))
    }

    /// One register/block write; Err is a GenCP status code.
    pub fn write(&self, addr: u64, data: &[u8]) -> Result<(), u16> {
        let len = data.len() as u64;
        if (addr == ACQ_BASE || addr == ACQ_BASE + 4) && len == 4 {
            self.back_write(addr, data)?;
        }
        if addr >= u64::from(CUSTOM_BASE)
            && addr + len <= u64::from(CUSTOM_BASE) + (CUSTOM_SLOTS as u64) * 4
        {
            if len != 4 || addr % 4 != 0 {
                return Err(packets::GENCP_BAD_ALIGNMENT);
            }
            return self.slot_write(
                ((addr - u64::from(CUSTOM_BASE)) / 4) as usize,
                u32::from_le_bytes(data[..4].try_into().expect("u32")),
            );
        }
        if let Some(r) = self.region(addr).filter(|r| r.contains(addr, len)) {
            if !r.writable {
                return Err(packets::GENCP_WRITE_PROTECT);
            }
            {
                let mut m = r.mem.lock().expect("region lock");
                let off = (addr - r.base) as usize;
                m[off..off + data.len()].copy_from_slice(data);
            }
            // SI_CONTROL side effect: fire the stream watcher.
            let si_lo = SIRM_BASE + packets::sirm::SI_CONTROL;
            if addr <= si_lo && addr + len > si_lo {
                let v = {
                    let m = r.mem.lock().expect("region lock");
                    let off = (si_lo - r.base) as usize;
                    u32::from_le_bytes(m[off..off + 4].try_into().expect("u32"))
                };
                let enabled = v & packets::sirm::CONTROL_STREAM_ENABLE != 0;
                let mut guard = self.sirm_control.lock().expect("sirm control lock");
                let was = *guard & packets::sirm::CONTROL_STREAM_ENABLE != 0;
                *guard = v;
                drop(guard);
                if enabled != was {
                    if let Some(w) = &*self.watcher.lock().expect("watcher lock") {
                        w(enabled);
                    }
                }
            }
            let ei_lo = crate::EIRM_BASE + packets::eirm::EI_CONTROL;
            if addr <= ei_lo && addr + len > ei_lo {
                let v = {
                    let m = r.mem.lock().expect("region lock");
                    let off = (ei_lo - r.base) as usize;
                    u32::from_le_bytes(m[off..off + 4].try_into().expect("u32"))
                };
                *self.eirm_control.lock().expect("eirm control lock") = v;
            }
            let test_lo = crate::EIRM_BASE + packets::eirm::EVENT_TEST_CONTROL;
            if addr <= test_lo && addr + len > test_lo {
                let v = {
                    let m = r.mem.lock().expect("region lock");
                    let off = (test_lo - r.base) as usize;
                    u32::from_le_bytes(m[off..off + 4].try_into().expect("u32"))
                };
                if v != 0 {
                    let enabled = *self.eirm_control.lock().expect("eirm control lock") & 1 != 0;
                    if enabled {
                        let stream = self.event_in.lock().expect("event-in lock").clone();
                        if let Some(stream) = stream {
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos() as u64;
                            stream
                                .send(&packets::encode_event(&packets::EventPacket {
                                    event_id: 0x4FFF,
                                    timestamp,
                                    data: Vec::new(),
                                }))
                                .map_err(|_| packets::GENCP_ERROR)?;
                        }
                    }
                    let mut m = r.mem.lock().expect("region lock");
                    let off = (test_lo - r.base) as usize;
                    m[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
                }
            }
            return Ok(());
        }
        self.back_write(addr, data)
    }

    fn slot_read(&self, idx: usize) -> u32 {
        match idx {
            slot::VERSION => VGIGE_VERSION,
            slot::AI_RESULT => self.ai_result.load(Ordering::Relaxed),
            slot::AI_SCORE => self.ai_score.load(Ordering::Relaxed),
            slot::FRAMES_RELAYED => self.frames.load(Ordering::Relaxed),
            _ => self.general_slots.lock().expect("slots lock")[idx],
        }
    }

    fn slot_write(&self, idx: usize, value: u32) -> Result<(), u16> {
        match idx {
            slot::VERSION | slot::AI_RESULT | slot::AI_SCORE | slot::FRAMES_RELAYED => {
                Err(packets::GENCP_WRITE_PROTECT)
            }
            slot::USER_COMMAND => {
                self.general_slots.lock().expect("slots lock")[idx] = value;
                eprintln!("vgige: UserCommand <= 0x{value:08X}");
                Ok(())
            }
            _ => {
                self.general_slots.lock().expect("slots lock")[idx] = value;
                Ok(())
            }
        }
    }

    fn back_read(&self, addr: u64, len: u64) -> Result<Vec<u8>, u16> {
        // The gateway's camera seam is 32-bit (GVCP register space);
        // wider addresses are unmapped here.
        let addr32 = u32::try_from(addr).map_err(|_| packets::GENCP_INVALID_ADDRESS)?;
        let mut back = self.back.lock().map_err(|_| packets::GENCP_ERROR)?;
        back.read_block(addr32, len as u32)
    }

    fn back_write(&self, addr: u64, data: &[u8]) -> Result<(), u16> {
        let addr32 = u32::try_from(addr).map_err(|_| packets::GENCP_INVALID_ADDRESS)?;
        let mut back = self.back.lock().map_err(|_| packets::GENCP_ERROR)?;
        back.write_block(addr32, data)
    }
}

fn put_str(out: &mut Vec<u8>, s: &str, len: usize) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&bytes[..bytes.len().min(len)]);
    out.resize(out.len() + (len - bytes.len().min(len)), 0);
}

/// Build the device-local regions (ABRM/manifest/SBRM/XML/SIRM).
fn build_regions(config: &DeviceConfig) -> Vec<Region> {
    assert!(
        config.xml.len() <= XML_MAX,
        "XML blob exceeds the device cap"
    );
    // ABRM (GenCP §5.4 / U3V Table 4-6) at the spec offsets, including
    // the capability-gated slots as zeros (FAMILY_NAME / MANUFACTURER_INFO
    // / USER_DEFINED_NAME bits are unset in Device Capability). Strings
    // are 64-byte slots; the host reads fixed addresses, so the layout
    // must be exact.
    let mut abrm = Vec::new();
    abrm.extend_from_slice(&0x0001_0003u32.to_le_bytes()); // GenCP 1.3
    put_str(&mut abrm, &config.manufacturer, 64); // 0x0004
    put_str(&mut abrm, &config.model, 64); // 0x0044
    put_str(&mut abrm, "", 64); // 0x0084 FAMILY_NAME (unsupported)
    put_str(&mut abrm, &config.device_version, 64); // 0x00C4
    put_str(&mut abrm, "", 64); // 0x0104 MANUFACTURER_INFO (unsupported)
    put_str(&mut abrm, &config.serial_number, 64); // 0x0144
    put_str(&mut abrm, "", 64); // 0x0184 USER_DEFINED_NAME (unsupported)
    abrm.resize(packets::abrm::DEVICE_CAPABILITY as usize, 0);
    abrm.extend_from_slice(
        &(packets::abrm::CAP_SBRM
            | packets::abrm::CAP_ENDIANNESS_REGISTER
            | packets::abrm::CAP_WRITTEN_LENGTH_FIELD)
            .to_le_bytes(),
    );
    abrm.extend_from_slice(&500u32.to_le_bytes()); // max response time, ms
    abrm.extend_from_slice(&MANIFEST_BASE.to_le_bytes());
    abrm.extend_from_slice(&SBRM_BASE.to_le_bytes());
    abrm.resize(0x400, 0);

    // Manifest (GenCP §5.5): one entry, raw XML, pointing at the blob.
    // R-50cd: the entry MUST carry a valid SHA1 of the file — at
    // offset 24, 20 bytes (field layout matches the host-side parser in
    // gentl-u3v packets::decode_manifest_entry, bytes 24..44 were
    // previously zero = deviation, fixed 2026-10-28).
    let mut manifest = Vec::new();
    manifest.extend_from_slice(&1u64.to_le_bytes());
    let mut entry = vec![0u8; packets::MANIFEST_ENTRY_LEN];
    entry[4..8].copy_from_slice(&packets::FILE_FORMAT_RAW_XML.to_le_bytes()); // raw, device XML
    entry[8..16].copy_from_slice(&XML_BASE.to_le_bytes());
    entry[16..24].copy_from_slice(&(config.xml.len() as u64).to_le_bytes());
    entry[24..44].copy_from_slice(&sha1::sha1(&config.xml));
    manifest.extend_from_slice(&entry);

    // SBRM (U3V Table 4-7): one streaming channel, 1 KiB transfers.
    let mut sbrm = vec![0u8; 0x44];
    sbrm[0..4].copy_from_slice(&0x0001_0002u32.to_le_bytes()); // U3V 1.2
    sbrm[0x04..0x0C].copy_from_slice(&((packets::sbrm::CAP_SIRM | packets::sbrm::CAP_EIRM) as u64).to_le_bytes());
    sbrm[0x14..0x18].copy_from_slice(&crate::MAX_TRANSFER.to_le_bytes());
    sbrm[0x18..0x1C].copy_from_slice(&crate::MAX_TRANSFER.to_le_bytes());
    sbrm[0x1C..0x20].copy_from_slice(&1u32.to_le_bytes()); // channels
    sbrm[0x20..0x28].copy_from_slice(&SIRM_BASE.to_le_bytes());
    sbrm[0x28..0x2C].copy_from_slice(&0x48u32.to_le_bytes());
    sbrm[0x2C..0x34].copy_from_slice(&crate::EIRM_BASE.to_le_bytes());
    sbrm[0x34..0x38].copy_from_slice(&0x0Cu32.to_le_bytes());

    // SIRM (Table 5-1/5-2): alignment 1 (SI_INFO=0), standard mode;
    // RW transfer sizes zeroed (host programs at Stream Enable).
    let mut sirm = vec![0u8; 0x48];
    sirm[0x08..0x10].copy_from_slice(&config.payload_size.to_le_bytes());
    sirm[0x10..0x14].copy_from_slice(&(packets::IMAGE_LEADER_LEN as u32).to_le_bytes());
    sirm[0x14..0x18].copy_from_slice(&(packets::IMAGE_TRAILER_LEN as u32).to_le_bytes());

    let mut eirm = vec![0u8; 0x0C];
    eirm[0x04..0x08].copy_from_slice(&4096u32.to_le_bytes());

    // Demo acquisition block (SFNC level): AcquisitionStart/Stop pokes
    // plus the frame PayloadSize the host sizes its buffers against.
    // The U3V stream itself is gated by SIRM Stream Enable (§5.4.1.2);
    // these are served locally so a camera-less (null-back) device still
    // completes a standard GenApi acquisition sequence. Real backends
    // advertise their own acquisition nodes, forwarded 1:1.
    let mut acq = vec![0u8; 0x14];
    acq[0x08..0x10].copy_from_slice(&config.payload_size.to_le_bytes());

    // Regions MUST be listed in ascending base order: the lookup takes
    // the last region with base <= addr.
    vec![
        Region {
            base: crate::ABRM_BASE,
            mem: Mutex::new(abrm),
            writable: false,
        },
        Region {
            base: MANIFEST_BASE,
            mem: Mutex::new(manifest),
            writable: false,
        },
        Region {
            base: SBRM_BASE,
            mem: Mutex::new(sbrm),
            writable: false,
        },
        Region {
            base: XML_BASE,
            mem: Mutex::new(config.xml.clone()),
            writable: false,
        },
        Region {
            base: SIRM_BASE,
            mem: Mutex::new(sirm),
            writable: true,
        },
        Region {
            base: crate::EIRM_BASE,
            mem: Mutex::new(eirm),
            writable: true,
        },
        Region {
            base: ACQ_BASE,
            mem: Mutex::new(acq),
            writable: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesize_guid;
    use vgige_core::{BackIdentity, CameraBack};

    struct NullBack;
    impl vgige_core::CameraBack for NullBack {
        fn read_reg(&mut self, _a: u32) -> Result<u32, u16> {
            Ok(0)
        }
        fn write_reg(&mut self, _a: u32, _v: u32) -> Result<(), u16> {
            Ok(())
        }
        fn read_block(&mut self, _a: u32, len: u32) -> Result<Vec<u8>, u16> {
            Ok(vec![0; len as usize])
        }
        fn write_block(&mut self, _a: u32, _d: &[u8]) -> Result<(), u16> {
            Ok(())
        }
        fn fetch_xml(&mut self) -> Result<Vec<u8>, u16> {
            Ok(vec![])
        }
        fn fetch_identity(&mut self) -> Result<BackIdentity, u16> {
            Ok(BackIdentity {
                manufacturer: "m".into(),
                model: "m".into(),
                device_version: "1".into(),
                serial_number: "s".into(),
                user_defined_name: String::new(),
                spec_version_major: 1,
                spec_version_minor: 2,
            })
        }
        fn open_stream(&mut self) -> Result<Box<dyn vgige_core::FrameSource>, u16> {
            Err(0x8001)
        }
        fn close_stream(&mut self) -> Result<(), u16> {
            Ok(())
        }
    }

    #[test]
    fn abrm_walk_reads() {
        let cfg = DeviceConfig::from_parts(
            BackIdentity {
                manufacturer: "m".into(),
                model: "m".into(),
                device_version: "1".into(),
                serial_number: "s".into(),
                user_defined_name: String::new(),
                spec_version_major: 1,
                spec_version_minor: 2,
            },
            b"<x/>",
            3072,
            synthesize_guid("s"),
        );
        let rf = RegisterFile::new(cfg, std::sync::Arc::new(std::sync::Mutex::new(NullBack)));
        let mut b = [0u8; 8];
        for (addr, len) in [
            (0x0000u64, 4usize),
            (0x01C4, 8),
            (0x0004, 64),
            (0x0044, 64),
            (0x00C4, 64),
            (0x0144, 64),
            (0x01CC, 4),
            (0x01D0, 8),
            (0x01D8, 8),
        ] {
            let mut buf = vec![0u8; len];
            rf.read(addr, &mut buf)
                .unwrap_or_else(|s| panic!("ABRM read 0x{addr:04X}/{len} -> 0x{s:04X}"));
        }
        // SBRM region reads.
        for (off, len) in [
            (0u64, 4usize),
            (0x04, 8),
            (0x14, 4),
            (0x18, 4),
            (0x1C, 4),
            (0x20, 8),
            (0x28, 4),
        ] {
            let mut buf = vec![0u8; len];
            rf.read(crate::SBRM_BASE + off, &mut buf)
                .unwrap_or_else(|s| panic!("SBRM read +0x{off:02X}/{len} -> 0x{s:04X}"));
        }
        let _ = &mut b;
    }

    /// Spec offsets are exact: the serial string sits at ABRM 0x144
    /// (fixed-address reads — a padded-build mistake must fail HERE,
    /// not get masked by a permissive test back).
    #[test]
    fn abrm_strings_at_spec_offsets() {
        let cfg = DeviceConfig::from_parts(
            BackIdentity {
                manufacturer: "Manu".into(),
                model: "Modl".into(),
                device_version: "9.9.9".into(),
                serial_number: "SER-77".into(),
                user_defined_name: String::new(),
                spec_version_major: 1,
                spec_version_minor: 2,
            },
            b"<x/>",
            3072,
            synthesize_guid("SER-77"),
        );
        let rf = RegisterFile::new(cfg, std::sync::Arc::new(std::sync::Mutex::new(NullBack)));
        let read_str = |addr: u64| {
            let mut buf = vec![0u8; 64];
            rf.read(addr, &mut buf).unwrap();
            let end = buf.iter().position(|&b| b == 0).unwrap_or(64);
            String::from_utf8(buf[..end].to_vec()).unwrap()
        };
        assert_eq!(read_str(0x0004), "Manu");
        assert_eq!(read_str(0x0044), "Modl");
        assert_eq!(read_str(0x00C4), "9.9.9");
        assert_eq!(read_str(0x0144), "SER-77");
    }

    #[test]
    fn payload_size_tracks_backend_roi_for_32_and_64_bit_reads() {
        let back = std::sync::Arc::new(std::sync::Mutex::new(vgige_back_null::NullBack::new()));
        let cfg = DeviceConfig::from_parts(
            BackIdentity {
                manufacturer: "m".into(),
                model: "m".into(),
                device_version: "1".into(),
                serial_number: "s".into(),
                user_defined_name: String::new(),
                spec_version_major: 1,
                spec_version_minor: 2,
            },
            b"<x/>",
            1280 * 1024,
            synthesize_guid("s"),
        );
        let rf = RegisterFile::new(cfg, back.clone());
        back.lock().unwrap().write_reg(vgige_back_null::REG_WIDTH, 640).unwrap();
        back.lock().unwrap().write_reg(vgige_back_null::REG_HEIGHT, 512).unwrap();

        let mut b32 = [0u8; 4];
        rf.read(ACQ_BASE + 0x08, &mut b32).unwrap();
        assert_eq!(u32::from_le_bytes(b32), 640 * 512);
        let mut b64 = [0u8; 8];
        rf.read(ACQ_BASE + 0x08, &mut b64).unwrap();
        assert_eq!(u64::from_le_bytes(b64), 640 * 512);
        rf.read(SIRM_BASE + packets::sirm::SI_REQUIRED_PAYLOAD_SIZE, &mut b64)
            .unwrap();
        assert_eq!(u64::from_le_bytes(b64), 640 * 512);
        rf.write(ACQ_BASE, &1u32.to_le_bytes()).unwrap();
        assert_eq!(back.lock().unwrap().read_reg(ACQ_BASE as u32).unwrap(), 1);
        rf.write(ACQ_BASE + 4, &0u32.to_le_bytes()).unwrap();
        assert_eq!(back.lock().unwrap().read_reg(ACQ_BASE as u32).unwrap(), 0);
    }
}
