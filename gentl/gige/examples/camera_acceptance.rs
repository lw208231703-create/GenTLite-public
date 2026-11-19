//! Real-camera acceptance run for the GigE Vision front end.
//!
//! Usage:
//!   GENTLITE_DISCOVERY_TARGETS=<ip:3956> cargo run -p gentl-gige \
//!       --example camera_acceptance -- [device-id] [output-dir]
//!
//! Each test configures the camera, captures a small burst, and saves the
//! last complete frame as an uncompressed baseline TIFF. The sequence opens
//! the GVSP channel, then writes AcquisitionStart; this firmware does not
//! ACK the start write once it is streaming, so that write is best-effort
//! and arriving frames are the confirmation.
//!
//! All register addresses below come from the camera's own XML
//! (LeadingOE/QSe SFNC 4.0.2, all IntRegs BigEndian).

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gentl_core::types::*;
use gentl_gige::*;

const VERSION: u64 = 0x0000_0000;
const DEVICE_MODE: u64 = 0x0000_0004;
const SENSOR_WIDTH: u64 = 0x1000_0000;
const SENSOR_HEIGHT: u64 = 0x1000_0004;
const WIDTH: u64 = 0x1000_0008;
const HEIGHT: u64 = 0x1000_000C;
const OFFSET_X: u64 = 0x1000_0010;
const OFFSET_Y: u64 = 0x1000_0014;
const PIXEL_FORMAT: u64 = 0x1000_0018;
const TEST_PATTERN: u64 = 0x1000_001C;
const ACQ_MODE: u64 = 0x1000_0020;
const ACQ_START: u64 = 0x1000_0024;
const TRIGGER_MODE: u64 = 0x1000_0028;
const EXPOSURE_MODE: u64 = 0x1000_002C;
const EXPOSURE: u64 = 0x1000_0030;
const EXPOSURE_AUTO: u64 = 0x1000_0034;
const GAIN: u64 = 0x1000_0038;
const PAYLOAD_SIZE: u64 = 0x1000_0050;
const FRAME_RATE_ENABLE: u64 = 0x1000_00B4;
const FRAME_RATE: u64 = 0x1000_0098;
const REVERSE_X: u64 = 0x1000_00AC;
const REVERSE_Y: u64 = 0x1000_00B0;
const TL_PARAMS_LOCKED: u64 = 0x1000_0084;
const MIN_WIDTH: u64 = 0x1000_0208;
const MIN_HEIGHT: u64 = 0x1000_020C;
const MAX_WIDTH: u64 = 0x1000_0210;
const MAX_HEIGHT: u64 = 0x1000_0214;
const WIDTH_INC: u64 = 0x1000_00C4;
const HEIGHT_INC: u64 = 0x1000_00C8;
const EXPOSURE_MIN: u64 = 0x1000_009C;
const EXPOSURE_MAX: u64 = 0x1000_0094;
const GAIN_MIN: u64 = 0x1000_005C;
const GAIN_MAX: u64 = 0x1000_0060;
const FRAME_RATE_MIN: u64 = 0x1000_00C0;
const FRAME_RATE_MAX: u64 = 0x1000_00A8;
const SCP0: u64 = 0x0D00;
const SCPS0: u64 = 0x0D04;
const SCDA0: u64 = 0x0D18;

const PFNC_MONO8: u32 = 0x0108_0001;
const PFNC_MONO12: u32 = 0x0110_0005;

const BUFFER_INFO_NEW_DATA: i32 = 4;
const BUFFER_INFO_IS_INCOMPLETE: i32 = 7;
const BUFFER_INFO_SIZE_FILLED: i32 = 9;
const BUFFER_INFO_WIDTH: i32 = 10;
const BUFFER_INFO_HEIGHT: i32 = 11;
const BUFFER_INFO_FRAMEID: i32 = 16;
const BUFFER_INFO_PIXELFORMAT: i32 = 20;

const DEVICE_INFO_MODEL: i32 = 2;
const DEVICE_INFO_SERIAL_NUMBER: i32 = 7;
const DEVICE_INFO_VERSION: i32 = 8;

const NUM_BUFFERS: usize = 4;
const FRAMES_PER_TEST: usize = 5;

#[derive(Clone, Copy)]
struct Config {
    width: u32,
    height: u32,
    offset_x: u32,
    offset_y: u32,
    pixel_format: u32,
    exposure: u32,
    gain: u32,
    frame_rate: u32,
    reverse_x: u32,
    reverse_y: u32,
    trigger_mode: u32,
    test_pattern: u32,
    exposure_mode: u32,
    exposure_auto: u32,
    frame_rate_enable: u32,
    locked: u32,
    /// True for the fps test: write ExposureTime before AcquisitionFrameRate.
    /// The two features invalidate each other on this camera; the test
    /// writes the value it wants to observe last.
    frame_rate_last: bool,
}

impl Config {
    fn describe(&self) -> String {
        format!(
            "{}x{} off=({}, {}) pf=0x{:08x} exp={}us gain={} fps={} rev=({}, {}) trig=0x{:08x} pattern={} expmode={} expauto={} fpsen={} lock={}",
            self.width,
            self.height,
            self.offset_x,
            self.offset_y,
            self.pixel_format,
            self.exposure,
            self.gain,
            self.frame_rate,
            self.reverse_x,
            self.reverse_y,
            self.trigger_mode,
            self.test_pattern,
            self.exposure_mode,
            self.exposure_auto,
            self.frame_rate_enable,
            self.locked
        )
    }
}

struct Session {
    tl: TLHandle,
    iface: IFHandle,
    dev: DevHandle,
    ds: DSHandle,
    ev: EventHandle,
    port: PortHandle,
    buffers: Vec<Vec<u8>>,
    handles: Vec<BufferHandle>,
    closed: bool,
}

impl Session {
    fn open(wanted: Option<String>) -> Result<Self, String> {
        let mut tl: TLHandle = ptr::null_mut();
        if GCInitLib() != GC_ERR_SUCCESS {
            return Err("GCInitLib failed".into());
        }
        if TLOpen(&mut tl) != GC_ERR_SUCCESS {
            let _ = TLClose(tl);
            return Err("TLOpen failed".into());
        }
        let iface_id = get_string(|s, n| TLGetInterfaceID(tl, 0, s, n));
        let c_iface = std::ffi::CString::new(iface_id.clone()).map_err(|e| e.to_string())?;
        let mut iface: IFHandle = ptr::null_mut();
        if TLOpenInterface(tl, c_iface.as_ptr(), &mut iface) != GC_ERR_SUCCESS {
            let _ = TLClose(tl);
            return Err("TLOpenInterface failed".into());
        }
        let mut changed = 0u8;
        if IFUpdateDeviceList(iface, &mut changed, 2000) != GC_ERR_SUCCESS {
            let _ = IFClose(iface);
            let _ = TLClose(tl);
            return Err("IFUpdateDeviceList failed".into());
        }
        let mut count = 0u32;
        if IFGetNumDevices(iface, &mut count) != GC_ERR_SUCCESS || count == 0 {
            let _ = IFClose(iface);
            let _ = TLClose(tl);
            return Err("no camera discovered (set GENTLITE_DISCOVERY_TARGETS?)".into());
        }
        let dev_id = wanted.unwrap_or_else(|| get_string(|s, n| IFGetDeviceID(iface, 0, s, n)));
        let c_dev = std::ffi::CString::new(dev_id.clone()).map_err(|e| e.to_string())?;
        let mut dev: DevHandle = ptr::null_mut();
        let r = IFOpenDevice(iface, c_dev.as_ptr(), 3, &mut dev); // 3 = control access
        if r != GC_ERR_SUCCESS {
            let _ = IFClose(iface);
            let _ = TLClose(tl);
            return Err(format!("IFOpenDevice({dev_id}): {}", last_error()));
        }
        let mut port: PortHandle = ptr::null_mut();
        if DevGetPort(dev, &mut port) != GC_ERR_SUCCESS {
            let _ = DevClose(dev);
            let _ = IFClose(iface);
            let _ = TLClose(tl);
            return Err("DevGetPort failed".into());
        }

        // Best-effort cleanup of a previous crashed session before doing
        // anything else. A zero write to SCP0 is always legal (R-091ca).
        let _ = wr32(port, ACQ_START, 0);
        let _ = wr32(port, SCP0, 0);

        let mut ds: DSHandle = ptr::null_mut();
        let c_sid = std::ffi::CString::new("Stream0").unwrap();
        if DevOpenDataStream(dev, c_sid.as_ptr(), &mut ds) != GC_ERR_SUCCESS {
            let _ = DevClose(dev);
            let _ = IFClose(iface);
            let _ = TLClose(tl);
            return Err("DevOpenDataStream failed".into());
        }
        let mut ev: EventHandle = ptr::null_mut();
        if GCRegisterEvent(ds, EVENT_NEW_BUFFER, &mut ev) != GC_ERR_SUCCESS {
            let _ = DSClose(ds);
            let _ = DevClose(dev);
            let _ = IFClose(iface);
            let _ = TLClose(tl);
            return Err("GCRegisterEvent failed".into());
        }

        let sensor_w = rd32(port, SENSOR_WIDTH).unwrap_or(0);
        let sensor_h = rd32(port, SENSOR_HEIGHT).unwrap_or(0);
        if sensor_w == 0 || sensor_h == 0 {
            let _ = GCUnregisterEvent(ds, EVENT_NEW_BUFFER);
            let _ = DSClose(ds);
            let _ = DevClose(dev);
            let _ = IFClose(iface);
            let _ = TLClose(tl);
            return Err("camera did not report sensor dimensions".into());
        }
        // Mono12 is a 16-bit container: two bytes per pixel. Allocate for
        // the largest payload any test can produce.
        let max_payload = sensor_w as usize * sensor_h as usize * 2;
        let mut buffers: Vec<Vec<u8>> = (0..NUM_BUFFERS).map(|_| vec![0u8; max_payload]).collect();
        let mut handles = Vec::with_capacity(NUM_BUFFERS);
        for buf in buffers.iter_mut() {
            let mut bh: BufferHandle = ptr::null_mut();
            let r = DSAnnounceBuffer(
                ds,
                buf.as_mut_ptr().cast(),
                buf.len(),
                ptr::null_mut(),
                &mut bh,
            );
            if r != GC_ERR_SUCCESS {
                let _ = GCUnregisterEvent(ds, EVENT_NEW_BUFFER);
                let _ = DSClose(ds);
                let _ = DevClose(dev);
                let _ = IFClose(iface);
                let _ = TLClose(tl);
                return Err(format!("DSAnnounceBuffer failed: {}", last_error()));
            }
            handles.push(bh);
        }
        for bh in &handles {
            if DSQueueBuffer(ds, *bh) != GC_ERR_SUCCESS {
                let _ = GCUnregisterEvent(ds, EVENT_NEW_BUFFER);
                let _ = DSClose(ds);
                let _ = DevClose(dev);
                let _ = IFClose(iface);
                let _ = TLClose(tl);
                return Err("DSQueueBuffer failed".into());
            }
        }

        Ok(Self {
            tl,
            iface,
            dev,
            ds,
            ev,
            port,
            buffers,
            handles,
            closed: false,
        })
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let _ = wr32(self.port, ACQ_START, 0);
        let _ = wr32(self.port, SCP0, 0);
        let _ = DSStopAcquisition(self.ds, 0);
        for bh in &self.handles {
            let _ = DSRevokeBuffer(self.ds, *bh, ptr::null_mut(), ptr::null_mut());
        }
        let _ = GCUnregisterEvent(self.ds, EVENT_NEW_BUFFER);
        let _ = DSClose(self.ds);
        let _ = DevClose(self.dev);
        let _ = IFClose(self.iface);
        let _ = TLClose(self.tl);
    }

    fn dev_string(&self, cmd: i32) -> String {
        let mut dtype: InfoDataType = 0;
        let mut buf = vec![0u8; 1024];
        let mut size = buf.len();
        let r = DevGetInfo(
            self.dev,
            cmd,
            &mut dtype,
            buf.as_mut_ptr().cast(),
            &mut size,
        );
        if r != GC_ERR_SUCCESS {
            return String::new();
        }
        String::from_utf8_lossy(&buf[..size.saturating_sub(1)]).into_owned()
    }

    fn read_config(&self) -> Result<Config, String> {
        Ok(Config {
            width: rd32(self.port, WIDTH)?,
            height: rd32(self.port, HEIGHT)?,
            offset_x: rd32(self.port, OFFSET_X)?,
            offset_y: rd32(self.port, OFFSET_Y)?,
            pixel_format: rd32(self.port, PIXEL_FORMAT)?,
            exposure: rd32(self.port, EXPOSURE)?,
            gain: rd32(self.port, GAIN)?,
            frame_rate: rd32(self.port, FRAME_RATE)?,
            reverse_x: rd32(self.port, REVERSE_X)?,
            reverse_y: rd32(self.port, REVERSE_Y)?,
            trigger_mode: rd32(self.port, TRIGGER_MODE)?,
            test_pattern: rd32(self.port, TEST_PATTERN)?,
            exposure_mode: rd32(self.port, EXPOSURE_MODE)?,
            exposure_auto: rd32(self.port, EXPOSURE_AUTO)?,
            frame_rate_enable: rd32(self.port, FRAME_RATE_ENABLE)?,
            locked: rd32(self.port, TL_PARAMS_LOCKED)?,
            frame_rate_last: false,
        })
    }

    fn apply(&self, cfg: &Config) -> Result<(), String> {
        // TLParamsLocked is a transient stream-state lock, not persistent
        // configuration. A previous failed stream may have left it set;
        // clear it before touching any feature register.
        wr32(self.port, TL_PARAMS_LOCKED, 0)?;
        // Clear offsets first so a smaller width/height never conflicts
        // with an old offset, then set the new size and new offsets.
        wr32(self.port, OFFSET_X, 0)?;
        wr32(self.port, OFFSET_Y, 0)?;
        wr32(self.port, PIXEL_FORMAT, cfg.pixel_format)?;
        // Do not touch the TestPattern register: writing it on this
        // firmware left the camera unable to stream even after writing 0.
        wr32(self.port, WIDTH, cfg.width)?;
        wr32(self.port, HEIGHT, cfg.height)?;
        wr32(self.port, OFFSET_X, cfg.offset_x)?;
        wr32(self.port, OFFSET_Y, cfg.offset_y)?;
        // Writing the mode/auto/enable registers when they already hold
        // the right value resets this firmware's other values, so only
        // change them when necessary.
        if cfg.frame_rate_enable != 1 {
            wr32(self.port, FRAME_RATE_ENABLE, 1)?;
        }
        if cfg.exposure_mode != 1 {
            wr32(self.port, EXPOSURE_MODE, 1)?;
        }
        if cfg.exposure_auto != 0 {
            wr32(self.port, EXPOSURE_AUTO, 0)?;
        }
        wr32(self.port, GAIN, cfg.gain)?;
        wr32(self.port, REVERSE_X, cfg.reverse_x)?;
        wr32(self.port, REVERSE_Y, cfg.reverse_y)?;
        // TriggerMode is bits 16..23 of this register. Force Off so the
        // camera free-runs; preserve the other (reserved) bits.
        let trigger_raw = rd32(self.port, TRIGGER_MODE)?;
        wr32(self.port, TRIGGER_MODE, trigger_raw & !0x00FF_0000)?;
        wr32(self.port, ACQ_MODE, 2)?; // continuous
        // ExposureTime and AcquisitionFrameRate invalidate each other.
        // Write the feature the current test is observing last.
        if cfg.frame_rate_last {
            wr32(self.port, EXPOSURE, cfg.exposure)?;
            wr32(self.port, FRAME_RATE, cfg.frame_rate)?;
        } else {
            wr32(self.port, FRAME_RATE, cfg.frame_rate)?;
            wr32(self.port, EXPOSURE, cfg.exposure)?;
        }
        Ok(())
    }

    fn stop_stream(&self) {
        // Close the GVSP channel first: that is what makes the camera
        // leave the "streaming" state even if the AcquisitionStop ACK is
        // dropped. Then stop acquisition and release TLParamsLocked.
        let _ = wr32(self.port, SCP0, 0);
        let _ = wr32(self.port, ACQ_START, 0);
        let _ = DSStopAcquisition(self.ds, 0);
        let _ = wr32(self.port, TL_PARAMS_LOCKED, 0);
        // Repeat the stop once after the receiver thread is gone: some
        // firmware revisions only accept AcquisitionStop after the stream
        // channel close has been processed.
        let _ = wr32(self.port, ACQ_START, 0);
        let _ = wr32(self.port, SCP0, 0);
        // Return filled buffers to the input pool; discarding them here
        // would leave the next acquisition with zero queued buffers.
        let _ = DSFlushQueue(self.ds, 2); // all to input
        // Give the firmware time to leave the stream state before the next
        // test writes registers and reopens the channel.
        std::thread::sleep(Duration::from_millis(200));
    }

    fn start_stream(&mut self) -> Result<(), String> {
        self.stop_stream();
        // The spec order is open the GVSP channel, then start the camera.
        // This firmware does not ACK the start write once it is streaming,
        // so the write is best-effort: frames are the real confirmation.
        let r = DSStartAcquisition(self.ds, 0, u64::MAX);
        if r != GC_ERR_SUCCESS {
            return Err(format!("DSStartAcquisition: {}", last_error()));
        }
        let start_result = wr32(self.port, ACQ_START, 1);
        println!(
            "stream regs: SCP0=0x{:08x} SCDA0=0x{:08x} SCPS0=0x{:08x} start={:?}",
            rd32(self.port, SCP0).unwrap_or(0),
            rd32(self.port, SCDA0).unwrap_or(0),
            rd32(self.port, SCPS0).unwrap_or(0),
            start_result
        );
        Ok(())
    }

    fn capture_frame(&mut self) -> Result<Frame, String> {
        let mut retried_start = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            if std::time::Instant::now() >= deadline {
                return Err("timeout waiting for a complete frame".into());
            }
            let mut raw = [0u8; std::mem::size_of::<EventNewBufferData>()];
            let mut n = raw.len();
            let timeout = if retried_start { 1500 } else { 2500 };
            let r = EventGetData(self.ev, raw.as_mut_ptr().cast(), &mut n, timeout);
            if r == GC_ERR_TIMEOUT {
                if !retried_start {
                    // Firmware may require start after the channel open.
                    let _ = wr32(self.port, ACQ_START, 1);
                    retried_start = true;
                    continue;
                }
                return Err("timeout waiting for a frame after start retry".into());
            }
            if r != GC_ERR_SUCCESS {
                return Err(format!("EventGetData: {}", last_error()));
            }
            let nd = unsafe { (raw.as_ptr() as *const EventNewBufferData).read_unaligned() };
            let Some(pos) = self.handles.iter().position(|h| *h == nd.buffer_handle) else {
                return Err("event buffer handle not announced".into());
            };
            let incomplete = self.buffer_u8(nd.buffer_handle, BUFFER_INFO_IS_INCOMPLETE)?;
            let new_data = self.buffer_u8(nd.buffer_handle, BUFFER_INFO_NEW_DATA)?;
            let filled = self.buffer_usize(nd.buffer_handle, BUFFER_INFO_SIZE_FILLED)?;
            let width = self.buffer_usize(nd.buffer_handle, BUFFER_INFO_WIDTH)?;
            let height = self.buffer_usize(nd.buffer_handle, BUFFER_INFO_HEIGHT)?;
            let pf = self.buffer_u64(nd.buffer_handle, BUFFER_INFO_PIXELFORMAT)?;
            let frame_id = self.buffer_u64(nd.buffer_handle, BUFFER_INFO_FRAMEID)?;
            // Copy before requeueing: after DSQueueBuffer the receiver may
            // hand this buffer to the next camera frame immediately.
            let copy_len = filled.min(self.buffers[pos].len());
            let data = self.buffers[pos][..copy_len].to_vec();
            let nonzero = data.iter().any(|b| *b != 0);
            let _ = DSQueueBuffer(self.ds, nd.buffer_handle);
            if incomplete == 0 && new_data != 0 && nonzero {
                return Ok(Frame {
                    width,
                    height,
                    pixel_format: pf as u32,
                    filled,
                    frame_id,
                    data,
                });
            }
        }
    }

    fn buffer_usize(&self, bh: BufferHandle, cmd: i32) -> Result<usize, String> {
        let mut dtype: InfoDataType = 0;
        let mut value = 0usize;
        let mut size = std::mem::size_of::<usize>();
        let r = DSGetBufferInfo(
            self.ds,
            bh,
            cmd,
            &mut dtype,
            (&mut value as *mut usize).cast::<c_void>(),
            &mut size,
        );
        if r != GC_ERR_SUCCESS {
            return Err(format!("DSGetBufferInfo({cmd}): {}", last_error()));
        }
        Ok(value)
    }

    fn buffer_u64(&self, bh: BufferHandle, cmd: i32) -> Result<u64, String> {
        let mut dtype: InfoDataType = 0;
        let mut value = 0u64;
        let mut size = std::mem::size_of::<u64>();
        let r = DSGetBufferInfo(
            self.ds,
            bh,
            cmd,
            &mut dtype,
            (&mut value as *mut u64).cast::<c_void>(),
            &mut size,
        );
        if r != GC_ERR_SUCCESS {
            return Err(format!("DSGetBufferInfo({cmd}): {}", last_error()));
        }
        Ok(value)
    }

    fn buffer_u8(&self, bh: BufferHandle, cmd: i32) -> Result<u8, String> {
        let mut dtype: InfoDataType = 0;
        let mut value = 0u8;
        let mut size = 1usize;
        let r = DSGetBufferInfo(
            self.ds,
            bh,
            cmd,
            &mut dtype,
            (&mut value as *mut u8).cast::<c_void>(),
            &mut size,
        );
        if r != GC_ERR_SUCCESS {
            return Err(format!("DSGetBufferInfo({cmd}): {}", last_error()));
        }
        Ok(value)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

struct Frame {
    width: usize,
    height: usize,
    pixel_format: u32,
    filled: usize,
    frame_id: u64,
    data: Vec<u8>,
}

fn get_string(mut f: impl FnMut(*mut c_char, *mut usize) -> GCError) -> String {
    let mut buf = vec![0u8; 4096];
    let mut size = buf.len();
    let r = f(buf.as_mut_ptr() as *mut c_char, &mut size);
    if r != GC_ERR_SUCCESS {
        return String::new();
    }
    String::from_utf8_lossy(&buf[..size.saturating_sub(1)]).into_owned()
}

fn last_error() -> String {
    let mut code = 0i32;
    let mut txt = vec![0u8; 1024];
    let mut n = txt.len();
    GCGetLastError(&mut code, txt.as_mut_ptr() as *mut c_char, &mut n);
    format!("{code}: {}", String::from_utf8_lossy(&txt[..n.saturating_sub(1)]))
}

fn rd32(port: PortHandle, addr: u64) -> Result<u32, String> {
    let mut buf = [0u8; 4];
    let mut size = buf.len();
    let r = GCReadPort(port, addr, buf.as_mut_ptr().cast(), &mut size);
    if r != GC_ERR_SUCCESS {
        return Err(format!("read 0x{addr:08x}: {}", last_error()));
    }
    Ok(u32::from_be_bytes(buf))
}

fn wr32(port: PortHandle, addr: u64, value: u32) -> Result<(), String> {
    let buf = value.to_be_bytes();
    let mut size = buf.len();
    let r = GCWritePort(port, addr, buf.as_ptr().cast(), &mut size);
    if r != GC_ERR_SUCCESS {
        return Err(format!("write 0x{addr:08x}={value}: {}", last_error()));
    }
    Ok(())
}

#[derive(Clone)]
struct TestSpec {
    name: &'static str,
    cfg: Config,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let wanted = args.next();
    let out_dir = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(default_out_dir);
    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!("create {}: {e}", out_dir.display());
        std::process::exit(2);
    }
    // Let the producer use its normal discovery/channel count and its
    // automatic packet-size path (including the XML fallback). Do not pin
    // GENTLITE_GEV_STREAM_CHANNELS or GENTLITE_GEV_PACKET_SIZE here: this
    // tool is also the acceptance test for those SDK defaults.
    if std::env::var_os("GENTLITE_GEV_RESEND").is_none() {
        std::env::set_var("GENTLITE_GEV_RESEND", "0");
    }
    if std::env::var_os("GENTLITE_GEV_RCVBUF_KB").is_none() {
        std::env::set_var("GENTLITE_GEV_RCVBUF_KB", "16384");
    }
    println!("output: {}", out_dir.display());

    let result = run(wanted, &out_dir);
    match result {
        Ok(()) => println!("ACCEPTANCE OK; TIFFs in {}", out_dir.display()),
        Err(e) => {
            eprintln!("ACCEPTANCE FAILED: {e}");
            std::process::exit(1);
        }
    }
}

fn default_out_dir() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Path::new("target")
        .join("camera-acceptance")
        .join(format!("run-{stamp}"))
}

fn run(wanted: Option<String>, out_dir: &Path) -> Result<(), String> {
    let mut s = Session::open(wanted)?;
    println!("device: {}", s.dev_string(DEVICE_INFO_MODEL));
    println!("serial: {}", s.dev_string(DEVICE_INFO_SERIAL_NUMBER));
    println!("version: {}", s.dev_string(DEVICE_INFO_VERSION));
    println!(
        "bootstrap: VERSION=0x{:08x} DEVICE_MODE=0x{:08x}",
        rd32(s.port, VERSION)?,
        rd32(s.port, DEVICE_MODE)?
    );

    let sensor_w = rd32(s.port, SENSOR_WIDTH)?;
    let sensor_h = rd32(s.port, SENSOR_HEIGHT)?;
    let min_w = rd32(s.port, MIN_WIDTH).unwrap_or(1);
    let min_h = rd32(s.port, MIN_HEIGHT).unwrap_or(1);
    let max_w = rd32(s.port, MAX_WIDTH).unwrap_or(sensor_w);
    let max_h = rd32(s.port, MAX_HEIGHT).unwrap_or(sensor_h);
    let inc_w = rd32(s.port, WIDTH_INC).unwrap_or(1).max(1);
    let inc_h = rd32(s.port, HEIGHT_INC).unwrap_or(1).max(1);
    let exp_min = rd32(s.port, EXPOSURE_MIN).unwrap_or(100);
    let exp_max = rd32(s.port, EXPOSURE_MAX).unwrap_or(100_000);
    let gain_min = rd32(s.port, GAIN_MIN).unwrap_or(0);
    let gain_max = rd32(s.port, GAIN_MAX).unwrap_or(24);
    let fps_min = rd32(s.port, FRAME_RATE_MIN).unwrap_or(1);
    let fps_max = rd32(s.port, FRAME_RATE_MAX).unwrap_or(50);
    let original_config = s.read_config()?;
    let mut baseline = original_config;
    // Use the known NI baseline (1024x768 Mono8, 1ms, gain 0, 50fps,
    // ReverseX/Y on) for the test matrix. The pre-test configuration is
    // kept separately and restored at the end.
    baseline.width = 1024;
    baseline.height = 768;
    baseline.offset_x = 0;
    baseline.offset_y = 0;
    baseline.pixel_format = PFNC_MONO8;
    baseline.exposure = 1000;
    baseline.gain = 0;
    baseline.frame_rate = 50;
    baseline.reverse_x = 1;
    baseline.reverse_y = 1;
    println!("sensor: {sensor_w}x{sensor_h}; valid: {min_w}..{max_w} x {min_h}..{max_h}, inc {inc_w}x{inc_h}");
    println!("ranges: exposure {exp_min}..{exp_max} us, gain {gain_min}..{gain_max}, fps {fps_min}..{fps_max}");
    println!("baseline: {}", baseline.describe());

    let full_w = round_down(max_w.max(min_w), inc_w, min_w);
    let full_h = round_down(max_h.max(min_h), inc_h, min_h);
    let roi_w = round_down(640.min(full_w), inc_w, min_w);
    let roi_h = round_down(480.min(full_h), inc_h, min_h);
    let roi_x = round_down(full_w.saturating_sub(roi_w) / 2, inc_w, 0);
    let roi_y = round_down(full_h.saturating_sub(roi_h) / 2, inc_h, 0);
    let exp_short = exp_min.max(1);
    let exp_long = exp_max.max(exp_min.max(1));
    let working_exposure = 10_000.clamp(exp_min.max(1), exp_max.max(exp_min.max(1)));
    let gain_low = gain_min;
    let gain_high = (gain_min + 100).min(gain_max).max(gain_min);
    let fps_10 = 10.clamp(fps_min.max(1), fps_max.max(fps_min.max(1)));

    let mut tests = Vec::new();
    tests.push(TestSpec {
        name: "01_baseline",
        cfg: baseline,
    });
    let mut full = baseline;
    full.width = full_w;
    full.height = full_h;
    full.offset_x = 0;
    full.offset_y = 0;
    full.pixel_format = PFNC_MONO8;
    full.exposure = working_exposure;
    // Full-sensor Mono8 at 50 fps is 65 MB/s; keep it at 10 fps for a
    // complete reference frame. Write frame rate last so it survives the
    // mutually-invalidating exposure write.
    full.frame_rate = fps_10;
    full.frame_rate_last = true;
    tests.push(TestSpec {
        name: "02_full_sensor_mono8",
        cfg: full,
    });
    let mut roi = baseline;
    roi.width = roi_w;
    roi.height = roi_h;
    roi.offset_x = roi_x;
    roi.offset_y = roi_y;
    roi.pixel_format = PFNC_MONO8;
    roi.exposure = working_exposure;
    tests.push(TestSpec {
        name: "03_roi_center_mono8",
        cfg: roi,
    });
    let mut exp_short_cfg = baseline;
    exp_short_cfg.exposure = exp_short;
    tests.push(TestSpec {
        name: "04_exposure_short",
        cfg: exp_short_cfg,
    });
    let mut exp_long_cfg = baseline;
    exp_long_cfg.exposure = exp_long;
    tests.push(TestSpec {
        name: "05_exposure_long",
        cfg: exp_long_cfg,
    });
    let mut gain_low_cfg = baseline;
    gain_low_cfg.gain = gain_low;
    gain_low_cfg.exposure = working_exposure;
    tests.push(TestSpec {
        name: "06_gain_low",
        cfg: gain_low_cfg,
    });
    let mut gain_high_cfg = baseline;
    gain_high_cfg.gain = gain_high;
    gain_high_cfg.exposure = working_exposure;
    tests.push(TestSpec {
        name: "07_gain_high",
        cfg: gain_high_cfg,
    });
    let mut fps_cfg = baseline;
    fps_cfg.frame_rate = fps_10;
    fps_cfg.exposure = working_exposure;
    fps_cfg.frame_rate_last = true;
    tests.push(TestSpec {
        name: "08_frame_rate_10",
        cfg: fps_cfg,
    });
    let mut mono12 = baseline;
    mono12.pixel_format = PFNC_MONO12;
    mono12.exposure = working_exposure;
    mono12.frame_rate = fps_10;
    mono12.frame_rate_last = true;
    tests.push(TestSpec {
        name: "09_mono12",
        cfg: mono12,
    });
    // The user's NI baseline already has both flips on (1,1); this test
    // turns them off so the saved image actually differs from baseline.
    let mut reverse = baseline;
    reverse.reverse_x = 0;
    reverse.reverse_y = 0;
    reverse.exposure = working_exposure;
    tests.push(TestSpec {
        name: "10_reverse_off",
        cfg: reverse,
    });
    let mut restart = baseline;
    restart.reverse_x = baseline.reverse_x;
    restart.reverse_y = baseline.reverse_y;
    restart.exposure = working_exposure;
    tests.push(TestSpec {
        name: "11_restart",
        cfg: restart,
    });

    let mut summary = String::new();
    summary.push_str(&format!(
        "camera model={} serial={} version={}\n",
        s.dev_string(DEVICE_INFO_MODEL),
        s.dev_string(DEVICE_INFO_SERIAL_NUMBER),
        s.dev_string(DEVICE_INFO_VERSION)
    ));
    summary.push_str(&format!(
        "sensor={}x{} original={}\n",
        sensor_w,
        sensor_h,
        original_config.describe()
    ));
    summary.push_str(&format!("test baseline={}\n", baseline.describe()));
    summary.push_str(&format!(
        "ranges exposure={}..{} us gain={}..{} fps={}..{}\n",
        exp_min, exp_max, gain_min, gain_max, fps_min, fps_max
    ));

    let only = std::env::var("CAMERA_ACCEPTANCE_ONLY").ok();
    let test_result = (|| -> Result<(), String> {
    for test in tests
        .iter()
        .filter(|t| only.as_deref().map_or(true, |name| name == t.name))
    {
        println!("\n=== {} ===", test.name);
        s.stop_stream();
        s.apply(&test.cfg)?;
        std::thread::sleep(Duration::from_millis(500));
        let actual = s.read_config()?;
        let payload = rd32(s.port, PAYLOAD_SIZE)?;
        println!("config: {}", actual.describe());
        println!("payload: {payload}");
        if test.name == "11_restart" {
            // Exercise a full start/stop cycle without requiring a frame
            // from the first cycle, then restart and capture.
            s.start_stream()?;
            std::thread::sleep(Duration::from_millis(1000));
            s.stop_stream();
            std::thread::sleep(Duration::from_millis(1500));
        }
        let mut frame = None;
        let mut last_err = String::new();
        for attempt in 0..2 {
            if attempt > 0 {
                // The camera occasionally misses the first start after a
                // close; retry one full stop/start cycle before failing.
                s.stop_stream();
                std::thread::sleep(Duration::from_millis(1200));
            }
            s.start_stream()?;
            for _ in 0..FRAMES_PER_TEST {
                match s.capture_frame() {
                    Ok(f) => {
                        // Keep the last complete frame: the first frame
                        // after a start can contain partially initialized
                        // rows on this camera.
                        frame = Some(f);
                    }
                    Err(e) => {
                        last_err = e;
                        break;
                    }
                }
            }
            if frame.is_some() {
                break;
            }
        }
        s.stop_stream();
        let frame = frame.ok_or_else(|| format!("{}: no frame ({last_err})", test.name))?;
        if frame.pixel_format != actual.pixel_format {
            return Err(format!(
                "{}: buffer pixel format 0x{:08x} != configured 0x{:08x}",
                test.name, frame.pixel_format, actual.pixel_format
            ));
        }
        let (bits, expected) = match actual.pixel_format {
            PFNC_MONO8 => (8u16, frame.width * frame.height),
            PFNC_MONO12 => (16u16, frame.width * frame.height * 2),
            other => {
                return Err(format!(
                    "{}: unsupported pixel format 0x{other:08x}",
                    test.name
                ))
            }
        };
        if frame.filled < expected {
            return Err(format!(
                "{}: short frame {} < {} bytes",
                test.name, frame.filled, expected
            ));
        }
        let path = out_dir.join(format!("{}.tiff", test.name));
        let desc = format!(
            "GenTLite camera acceptance | test={} | {} | frame_id={} | payload={}",
            test.name,
            actual.describe(),
            frame.frame_id,
            frame.filled
        );
        write_tiff(
            &path,
            frame.width,
            frame.height,
            bits,
            &frame.data[..expected],
            &desc,
        )
        .map_err(|e| format!("{}: TIFF {}: {e}", test.name, path.display()))?;
        // Keep the raw Mono12 bytes alongside the TIFF so byte order can
        // be checked without guessing from a rendered viewer.
        if actual.pixel_format == PFNC_MONO12 {
            let raw = out_dir.join(format!("{}.raw", test.name));
            fs::write(&raw, &frame.data[..expected])
                .map_err(|e| format!("{}: raw {}: {e}", test.name, raw.display()))?;
        }
        println!(
            "saved {} ({}x{}, {} bit, frame_id={})",
            path.display(),
            frame.width,
            frame.height,
            bits,
            frame.frame_id
        );
        summary.push_str(&format!(
            "{} OK {}x{} bits={} payload={} frame_id={} exposure={} gain={} fps={} tiff={}\n",
            test.name,
            frame.width,
            frame.height,
            bits,
            frame.filled,
            frame.frame_id,
            actual.exposure,
            actual.gain,
            actual.frame_rate,
            path.display()
        ));
    }
    Ok(())
    })();

    // Restore the pre-test configuration and leave the camera stopped, even
    // if one of the tests failed (otherwise the next run starts from the
    // failed test's settings).
    s.stop_stream();
    let restore_result = restore_config(&s, &original_config);
    s.stop_stream();
    let restored_cfg = s.read_config();
    let restored = restored_cfg
        .as_ref()
        .map(|c| c.describe())
        .unwrap_or_else(|e| format!("readback failed: {e}"));
    let locked_after = restored_cfg
        .as_ref()
        .map(|c| c.locked.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    summary.push_str(&format!(
        "restored original={restored}\n\
         tlparams_locked original={} after={locked_after}\n",
        original_config.locked
    ));
    let summary_name = match only.as_deref() {
        Some(name) => format!("summary-{name}.txt"),
        None => "summary.txt".to_string(),
    };
    fs::write(out_dir.join(summary_name), summary)
        .map_err(|e| format!("summary: {e}"))?;
    test_result?;
    restore_result?;
    Ok(())
}

/// Restore the user's pre-test configuration. ExposureTime and
/// AcquisitionFrameRate invalidate each other on this camera, so retry
/// once with the opposite write order if the first readback differs.
///
/// TLParamsLocked is intentionally not restored: it is a transient
/// stream-state lock, not persistent configuration. The run clears it and
/// must leave it clear, otherwise a stale pre-test 1 would lock the camera
/// for the next consumer. A non-zero readback after restore is an error;
/// the original value is recorded in the summary instead.
fn restore_config(s: &Session, cfg: &Config) -> Result<(), String> {
    s.apply(cfg)?;
    let mut actual = s.read_config()?;
    if actual.exposure != cfg.exposure || actual.frame_rate != cfg.frame_rate {
        let mut alternate = *cfg;
        alternate.frame_rate_last = true;
        s.apply(&alternate)?;
        actual = s.read_config()?;
    }
    if actual.exposure != cfg.exposure || actual.frame_rate != cfg.frame_rate {
        return Err(format!(
            "restore mismatch: wanted exp={} fps={}, got exp={} fps={}",
            cfg.exposure, cfg.frame_rate, actual.exposure, actual.frame_rate
        ));
    }
    if actual.locked != 0 {
        return Err(format!(
            "TLParamsLocked still {} after restore (must be 0; original was {})",
            actual.locked, cfg.locked
        ));
    }
    Ok(())
}

fn round_down(value: u32, inc: u32, min: u32) -> u32 {
    if inc == 0 {
        return value.max(min);
    }
    let v = value / inc * inc;
    v.max(min)
}

fn write_tiff(
    path: &Path,
    width: usize,
    height: usize,
    bits: u16,
    data: &[u8],
    description: &str,
) -> io::Result<()> {
    let bytes_per_px = (bits / 8) as usize;
    let expected = width
        .checked_mul(height)
        .and_then(|v| v.checked_mul(bytes_per_px))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "image size overflow"))?;
    if data.len() < expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("short data: {} < {expected}", data.len()),
        ));
    }
    let dt = format!("2026:11:17 12:00:00\0");
    let software = b"GenTLite camera_acceptance\0";
    let mut desc = description.as_bytes().to_vec();
    desc.push(0);

    let entries = 17u16;
    let ifd_size = 2 + 12 * entries as u32 + 4;
    let extra_start = 8 + ifd_size;
    let xres_off = extra_start;
    let yres_off = xres_off + 8;
    let software_off = yres_off + 8;
    let datetime_off = software_off + software.len() as u32;
    let desc_off = datetime_off + dt.len() as u32;
    let data_off = desc_off + desc.len() as u32;

    let mut out = Vec::with_capacity(data_off as usize + expected);
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&entries.to_le_bytes());

    let mut entry = |tag: u16, typ: u16, count: u32, value: u32| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    };

    entry(256, 4, 1, width as u32);
    entry(257, 4, 1, height as u32);
    entry(258, 3, 1, u32::from(bits));
    entry(259, 3, 1, 1); // no compression
    entry(262, 3, 1, 1); // BlackIsZero
    entry(270, 2, desc.len() as u32, desc_off);
    entry(273, 4, 1, data_off);
    entry(277, 3, 1, 1); // samples per pixel
    entry(278, 4, 1, height as u32);
    entry(279, 4, 1, expected as u32);
    entry(282, 5, 1, xres_off); // XResolution
    entry(283, 5, 1, yres_off); // YResolution
    entry(284, 3, 1, 1); // planar chunky
    entry(296, 3, 1, 2); // inches
    entry(305, 2, software.len() as u32, software_off);
    entry(306, 2, dt.len() as u32, datetime_off);
    entry(339, 3, 1, 1); // unsigned integer
    out.extend_from_slice(&0u32.to_le_bytes()); // next IFD

    // Extra fields, then pixel data.
    for _ in 0..2 {
        out.extend_from_slice(&72u32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
    }
    out.extend_from_slice(software);
    out.extend_from_slice(dt.as_bytes());
    out.extend_from_slice(&desc);
    out.extend_from_slice(&data[..expected]);
    debug_assert_eq!(out.len() as u32, data_off + expected as u32);
    fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiff_header_is_uncompressed_and_has_expected_size() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/camera-acceptance-test");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.tiff");
        write_tiff(&path, 2, 2, 8, &[0, 1, 2, 255], "unit").unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[0..2], b"II");
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 42);
        let _ = fs::remove_file(path);
    }
}
