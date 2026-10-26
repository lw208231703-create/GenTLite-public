//! Shared test support for the front-u3v integration tests: in-memory
//! byte streams bridging the device traits and the host-side client's
//! `UsbPipe` surface, plus a stub `CameraBack` standing in for the
//! downstream camera (a real `GigeBack` cannot be linked into the same
//! test binary — the dual-GenTL-producer symbol collision, Q38-7; the
//! back seam itself is covered with the real adapter in
//! vgige-cli/tests/relay_e2e.rs).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gentl_core::{GentlError, GentlResult};
use gentl_u3v::client::UsbPipe;
use vgige_core::{BackIdentity, CameraBack, CanonicalFrame, FrameSource};
use vgige_front_u3v::device::{DeviceTransfers, HostCommands, StreamError};

pub const MONO8: u32 = 0x0108_0001;
pub const W: u32 = 64;
pub const H: u32 = 48;
pub const PAYLOAD: usize = (W * H) as usize;
pub const MPS: usize = 1024;

/// One direction of a bulk endpoint: a CHUNK queue modeling USB
/// transaction boundaries — each `push` is one device transfer (one or
/// more packets, one chunk), and `pop_one` hands the host at most one
/// chunk per read, split at `max` if the chunk exceeds the request
/// (usbfs short-read semantics: a read never spans two transfers).
#[derive(Default)]
pub struct ByteQueue {
    inner: Mutex<VecDeque<Vec<u8>>>,
    cv: Condvar,
}

impl ByteQueue {
    pub fn push(&self, bytes: &[u8]) {
        self.inner.lock().unwrap().push_back(bytes.to_vec());
        self.cv.notify_all();
    }

    /// Pop up to `max` bytes as ONE transfer; `None` if nothing arrived
    /// within `timeout`.
    pub fn pop_one(&self, max: usize, timeout: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut q = self.inner.lock().unwrap();
        loop {
            if !q.is_empty() {
                let front_len = q.front().expect("front").len();
                let n = front_len.min(max);
                if n == front_len {
                    return Some(q.pop_front().expect("front"));
                }
                // Bigger than the request: split — hand back the first
                // n bytes, keep the remainder queued for the next read.
                let front = q.front_mut().expect("front");
                let rest = front.split_off(n);
                return Some(std::mem::replace(front, rest));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let (guard, _) = self.cv.wait_timeout(q, remaining).unwrap();
            q = guard;
        }
    }
}

/// Device side of the host→device control stream.
pub struct DevCommands(pub Arc<ByteQueue>);

impl HostCommands for DevCommands {
    fn recv(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.0.pop_one(4096, timeout)
    }
}

/// Device side of a device→host stream: writes are split into
/// MPS-sized chunks (one chunk = one transaction, so the host's UQ16
/// accumulation is genuinely exercised).
pub struct DevTransfers(pub Arc<ByteQueue>, pub u16);

impl DeviceTransfers for DevTransfers {
    fn send(&self, bytes: &[u8]) -> Result<(), StreamError> {
        for chunk in bytes.chunks(self.1 as usize) {
            self.0.push(chunk);
        }
        Ok(())
    }
    fn max_packet(&self) -> u16 {
        self.1
    }
}

/// Client (host) side of the control OUT endpoint: commands in.
pub struct ClientOut(pub Arc<ByteQueue>);

impl UsbPipe for ClientOut {
    fn read_bulk(&self, _buf: &mut [u8], _timeout: Duration) -> GentlResult<u32> {
        Err(GentlError::Timeout)
    }
    fn write_bulk(&self, buf: &[u8], _timeout: Duration) -> GentlResult<()> {
        self.0.push(buf);
        Ok(())
    }
    fn flush(&self) {}
    fn set_raw_io(&self) -> GentlResult<()> {
        Ok(())
    }
    fn max_packet(&self) -> u16 {
        MPS as u16
    }
}

/// Client side of a device→host IN endpoint.
pub struct ClientIn(pub Arc<ByteQueue>);

impl UsbPipe for ClientIn {
    fn read_bulk(&self, buf: &mut [u8], timeout: Duration) -> GentlResult<u32> {
        let Some(chunk) = self.0.pop_one(buf.len(), timeout) else {
            return Err(GentlError::Timeout);
        };
        buf[..chunk.len()].copy_from_slice(&chunk);
        Ok(chunk.len() as u32)
    }
    fn write_bulk(&self, _buf: &[u8], _timeout: Duration) -> GentlResult<()> {
        Ok(())
    }
    fn flush(&self) {}
    fn set_raw_io(&self) -> GentlResult<()> {
        Ok(())
    }
    fn max_packet(&self) -> u16 {
        MPS as u16
    }
}

/// One Mono8 test frame with the sim's pattern: byte `k` of block `b` is
/// `(k + b) mod 256` — the payload assertion shape shared with the
/// gateway e2e suites.
pub fn mono8_frame(block_seed: u64, timestamp: u64) -> CanonicalFrame {
    CanonicalFrame {
        timestamp,
        pixel_format: MONO8,
        width: W,
        height: H,
        offset_x: 0,
        offset_y: 0,
        padding_x: 0,
        padding_y: 0,
        payload: (0..PAYLOAD).map(|k| (k as u64 + block_seed) as u8).collect(),
    }
}

/// The stub downstream camera behind the `CameraBack` seam: canned
/// identity/XML, a writable vendor register, and a scripted frame
/// source for the stream tests.
pub struct StubBack {
    pub vendor: Arc<Mutex<HashMap<u32, u32>>>,
    pub frames: Vec<CanonicalFrame>,
    pub closed: Arc<std::sync::atomic::AtomicBool>,
}

pub const VENDOR_WIDTH: u32 = 0x0010_0000;

impl Default for StubBack {
    fn default() -> Self {
        Self::new()
    }
}

impl StubBack {
    pub fn new() -> Self {
        let vendor = HashMap::from([(VENDOR_WIDTH, W)]);
        Self {
            vendor: Arc::new(Mutex::new(vendor)),
            frames: Vec::new(),
            closed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub fn xml(&self) -> Vec<u8> {
        br#"<?xml version="1.0" encoding="UTF-8"?>
<RegisterDescription xmlns="http://www.genicam.org/GenApi/Version_1_1" ModelName="StubCam" VendorName="StubVendor">
  <Group Comment="ImageFormat">
    <Integer Name="Width" NameSpace="Standard"><pValue>WidthReg</pValue><Min>1</Min><Max>512</Max></Integer>
    <IntReg Name="WidthReg" NameSpace="Custom"><Address>0x00100000</Address><Length>4</Length><AccessMode>RW</AccessMode><pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>
  </Group>
</RegisterDescription>
"#
            .to_vec()
    }

    pub fn identity(&self) -> BackIdentity {
        BackIdentity {
            manufacturer: "StubVendor".into(),
            model: "StubCam".into(),
            device_version: "1.0.0".into(),
            serial_number: "STUB-0001".into(),
            user_defined_name: String::new(),
            spec_version_major: 2,
            spec_version_minor: 2,
        }
    }
}

impl CameraBack for StubBack {
    fn read_reg(&mut self, addr: u32) -> Result<u32, u16> {
        self.vendor
            .lock()
            .unwrap()
            .get(&addr)
            .copied()
            .ok_or(gentl_gvcp_status(GentlError::InvalidAddress))
    }

    fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), u16> {
        let mut v = self.vendor.lock().unwrap();
        if v.contains_key(&addr) {
            v.insert(addr, value);
            Ok(())
        } else {
            Err(0x8003)
        }
    }

    fn read_block(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, u16> {
        if len == 4 {
            return self.read_reg(addr).map(|v| v.to_le_bytes().to_vec());
        }
        Err(0x8003)
    }

    fn write_block(&mut self, addr: u32, data: &[u8]) -> Result<(), u16> {
        if data.len() == 4 {
            return self.write_reg(addr, u32::from_le_bytes(data[..4].try_into().unwrap()));
        }
        Err(0x8005)
    }

    fn fetch_xml(&mut self) -> Result<Vec<u8>, u16> {
        Ok(self.xml())
    }

    fn fetch_identity(&mut self) -> Result<BackIdentity, u16> {
        Ok(self.identity())
    }

    fn open_stream(&mut self) -> Result<Box<dyn FrameSource>, u16> {
        self.closed
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(StubSource {
            frames: Mutex::new(std::mem::take(&mut self.frames).into()),
            closed: Arc::clone(&self.closed),
        }))
    }

    fn close_stream(&mut self) -> Result<(), u16> {
        self.closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

fn gentl_gvcp_status(e: GentlError) -> u16 {
    match e {
        GentlError::InvalidAddress => 0x8003,
        _ => 0x8FFF,
    }
}

struct StubSource {
    frames: Mutex<VecDeque<CanonicalFrame>>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl FrameSource for StubSource {
    fn next_frame(&mut self, timeout: Duration) -> Option<CanonicalFrame> {
        if let Some(f) = self.frames.lock().unwrap().pop_front() {
            return Some(f);
        }
        std::thread::sleep(timeout);
        None
    }
}

/// Drop the dead-code warning on the unused `closed` read path (the
/// flag is asserted from the test side).
#[allow(dead_code)]
fn _touch(s: &StubSource) -> bool {
    s.closed.load(std::sync::atomic::Ordering::SeqCst)
}
