//! vgige-back-u3v: the [`CameraBack`]/[`FrameSource`] adapter for USB3
//! Vision cameras, built on the `gentl-u3v` client facade. This unlocks
//! the gateway matrix cell "GigE front ← U3V camera" (roadmap Step 2,
//! notes/linux-vgige-design.md §8): remote GigE Vision clients see and
//! control a U3V camera through the virtual device — register access
//! proxies 1:1 onto GenCP addresses (the served XML is the camera's own,
//! whose IntReg addresses are GenCP addresses), and the U3V image stream
//! relays as GVSP 3.0 blocks.
//!
//! Register semantics: U3V registers are little-endian (R-30cd) and the
//! GenCP address space is 64-bit; the gateway's 32-bit window forwards
//! verbatim — a GigE client writing vendor register 0x1000_0024 lands at
//! GenCP address 0x1000_0024 on the camera (the XML it was given carries
//! exactly those addresses). No endianness conversion happens here or in
//! the register space: both sides forward raw bytes for block access, and
//! the bootstrap registers the space intercepts (CCP/MAC/IP/capability)
//! are virtualized locally anyway.
//!
//! Status-code mapping (GenCP → GEV, the error-transparency contract):
//! GenCP 1.3.1 §4.3.2.1 Table 6 and GigE Vision §19 Table 19-1 share one
//! numeric namespace for the common codes (0x0000, 0x8001-0x8007 —
//! NOT_IMPLEMENTED through BUSY), so those pass through verbatim. GenCP-
//! specific codes GigE never assigned (0x800B MSG_TIMEOUT, 0x800E
//! INVALID_HEADER, 0x800F WRONG_CONFIG), the generic 0x8FFF, and the U3V
//! technology namespace (0xA0xx/0xA1xx, R-38cd) have no GigE meaning:
//! they are passed through numerically as well (a GigE client treats any
//! 0x8xxx/0xAxxx status as an error) and logged once — QoI, notes/
//! OPEN-QUESTIONS.md Q38. Local failures (USB I/O, malformed wire data,
//! window timeout) map to the generic Appendix B.21 status 0x8FFF, the
//! same policy as the GigE back (its camera-unreachable case).

use std::sync::Arc;
use std::time::Duration;

use gentl_core::{GentlError, GentlResult};
use gentl_u3v::client::{Block, U3vCamera, U3vError, U3vStream, UsbPipe};
use vgige_core::{BackIdentity, CameraBack, CanonicalFrame, FrameSource};

// Generic error (GigE Vision spec Appendix B.21); gentl-gvcp's wire module
// does not export it. Used for local failures (camera unreachable, USB
// I/O) — the same policy as the GigE back.
const GEV_STATUS_ERROR: u16 = 0x8FFF;

/// The downstream U3V camera.
pub struct U3vBack {
    cam: U3vCamera,
}

impl U3vBack {
    /// Open by the backend instance ID (Windows: CM device instance path;
    /// Linux: `<bus>:<address>` from `U3vCamera::enumerate`).
    pub fn open(instance_id: &str) -> GentlResult<Self> {
        U3vCamera::open(instance_id).map(|cam| Self { cam })
    }

    /// Build from pre-opened endpoint pipes (test fakes, embedded
    /// transports). See [`U3vCamera::from_pipes`].
    pub fn from_pipes(
        control_out: Arc<dyn UsbPipe>,
        control_in: Arc<dyn UsbPipe>,
        stream_in: Vec<Arc<dyn UsbPipe>>,
    ) -> GentlResult<Self> {
        U3vCamera::from_pipes(control_out, control_in, stream_in).map(|cam| Self { cam })
    }

    /// The facade camera (diagnostics, tests).
    pub fn camera(&self) -> &U3vCamera {
        &self.cam
    }
}

/// Map a facade error to the GEV status the remote client sees.
fn to_gev(e: U3vError) -> u16 {
    match e {
        // Device-answered status: verbatim (see the module doc for the
        // GenCP→GEV namespace table).
        U3vError::Status(s) => {
            if !(s == 0 || (0x8001..=0x8007).contains(&s) || s == 0x8FFF) {
                eprintln!("vgige: U3V status 0x{s:04X} passed through to the GigE client (no GEV meaning, Q38)");
            }
            s
        }
        // Local failure: generic Appendix B.21 (camera-unreachable class).
        U3vError::Timeout => GEV_STATUS_ERROR,
        U3vError::Transport(m) => {
            eprintln!("vgige: U3V transport: {m}");
            GEV_STATUS_ERROR
        }
    }
}

/// Map a facade startup error (GentlResult-shaped) to a GEV status.
fn gentl_to_gev(e: GentlError) -> u16 {
    match e {
        GentlError::GevStatus(s) => s,
        other => {
            eprintln!("vgige: U3V startup: {other}");
            GEV_STATUS_ERROR
        }
    }
}

impl CameraBack for U3vBack {
    fn read_reg(&mut self, addr: u32) -> Result<u32, u16> {
        // LE register via the GenCP transaction (R-30cd); the raw device
        // status crosses unchanged.
        self.cam.read_reg(u64::from(addr)).map_err(to_gev)
    }

    fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), u16> {
        self.cam.write_reg(u64::from(addr), value).map_err(to_gev)
    }

    fn read_block(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, u16> {
        let mut buf = vec![0u8; len as usize];
        self.cam.read_mem(u64::from(addr), &mut buf).map_err(to_gev)?;
        Ok(buf)
    }

    fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<(), u16> {
        self.cam.write_mem(u64::from(addr), data).map_err(to_gev)
    }

    fn fetch_xml(&mut self) -> Result<Vec<u8>, u16> {
        self.cam.fetch_xml().map_err(gentl_to_gev)
    }

    fn fetch_identity(&mut self) -> Result<BackIdentity, u16> {
        // The ABRM/SBRM walk already snapshot every identity string at
        // open — no wire reads needed.
        let b = self.cam.bootstrap();
        Ok(BackIdentity {
            manufacturer: b.manufacturer.clone(),
            model: b.model.clone(),
            device_version: b.device_version.clone(),
            serial_number: b.serial_number.clone(),
            user_defined_name: b.user_defined_name.clone(),
            // The virtual device claims a U3V-era spec version (GENCPC
            // version pair, ABRM 0x0000: minor@0, major@16).
            spec_version_major: (b.gen_cp_version >> 16) as u16,
            spec_version_minor: (b.gen_cp_version & 0xFFFF) as u16,
        })
    }

    fn open_stream(&mut self) -> Result<Box<dyn FrameSource>, u16> {
        let stream = self.cam.stream(0).map_err(to_gev)?;
        stream.start().map_err(to_gev)?;
        Ok(Box::new(U3vFrameSource {
            stream,
            dead: false,
        }))
    }

    fn close_stream(&mut self) -> Result<(), u16> {
        // U3V stream teardown rides the FrameSource's drop: its
        // `U3vStream::stop` drops Stream Enable (§5.4.1.2: 0 while active
        // aborts the stream). Unlike the GigE back there is no per-box
        // channel register left to write here.
        Ok(())
    }
}

/// U3V intake of one open channel: pulls blocks from the facade stream
/// (watchdog-bounded) and maps them to canonical frames.
pub struct U3vFrameSource {
    stream: U3vStream,
    /// Set on a transport/desync error: the pipe cannot re-synchronize
    /// (same policy as the GenTL receiver's desync); keep answering `None`
    /// after waiting out the timeout, mirroring the GigE source's contract.
    dead: bool,
}

impl FrameSource for U3vFrameSource {
    fn next_frame(&mut self, timeout: Duration) -> Option<CanonicalFrame> {
        if self.dead {
            // The driver contract: wait out the timeout even when no frame
            // can ever arrive, so its stop-flag polling keeps its cadence.
            std::thread::sleep(timeout);
            return None;
        }
        match self.stream.next_block(timeout) {
            // No block started within the window (idle poll / timeout).
            None => None,
            Some(Err(e)) => {
                eprintln!("vgige: u3v stream read failed: {e:?}");
                self.dead = true;
                std::thread::sleep(timeout);
                None
            }
            // Non-image payload or an error-status trailer (§5.5.4): the
            // pipe stayed in sync, there is just no frame to forward.
            Some(Ok(Block::Skipped)) => None,
            Some(Ok(Block::Image {
                leader, payload, ..
            })) => Some(CanonicalFrame {
                timestamp: leader.timestamp,
                pixel_format: leader.pixel_format,
                width: leader.size_x,
                height: leader.size_y,
                offset_x: leader.offset_x,
                offset_y: leader.offset_y,
                padding_x: u32::from(leader.padding_x),
                // U3V carries no padding_y (leader has a single padding,
                // §5.5.5.1.1 Table 5-7).
                padding_y: 0,
                payload,
            }),
        }
    }
}

impl Drop for U3vFrameSource {
    fn drop(&mut self) {
        // §5.4.1.2: Stream Enable = 0 aborts the stream; called by Drop as
        // well, so a forgotten stop still tears the device side down.
        self.stream.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GenCP→GEV namespace decision: common codes verbatim, local
    /// failures to the generic Appendix B.21 status.
    #[test]
    fn status_mapping_table() {
        assert_eq!(to_gev(U3vError::Status(gentl_gvcp::wire::GEV_STATUS_SUCCESS)), 0x0000);
        assert_eq!(
            to_gev(U3vError::Status(gentl_gvcp::wire::GEV_STATUS_INVALID_ADDRESS)),
            0x8003
        );
        assert_eq!(
            to_gev(U3vError::Status(gentl_gvcp::wire::GEV_STATUS_ACCESS_DENIED)),
            0x8006
        );
        // U3V technology namespace: numeric passthrough (Q38).
        assert_eq!(to_gev(U3vError::Status(0xA004)), 0xA004);
        // Local failures: generic error (Appendix B.21).
        assert_eq!(to_gev(U3vError::Timeout), GEV_STATUS_ERROR);
        assert_eq!(
            to_gev(U3vError::Transport("boom".into())),
            GEV_STATUS_ERROR
        );
        assert_eq!(gentl_to_gev(GentlError::GevStatus(0x8003)), 0x8003);
        assert_eq!(gentl_to_gev(GentlError::Timeout), GEV_STATUS_ERROR);
    }
}
