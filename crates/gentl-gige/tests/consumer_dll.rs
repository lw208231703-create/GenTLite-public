//! Real-consumer acceptance: load the built `gentl_gige.dll` with libloading
//! (no rlib on the call path) and drive the full GenTL 1.6 control-plane
//! flow through resolved function pointers, exactly like an external
//! consumer would.
//!
//! Resolving all 64 exports doubles as the export-completeness regression
//! test: any missing symbol fails the test naming it.
//!
//! The device under test is `gentl_simulator::SimCamera` on loopback,
//! reached through the producer's documented test hooks
//! (`GENTLITE_GVCP_PORT`, `GENTLITE_DISCOVERY_TARGETS`; see
//! `tests/control_plane.rs` and notes/OPEN-QUESTIONS.md Q11).

use std::ffi::{c_char, c_void};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::ptr;

use gentl_core::types::*;
use gentl_simulator::{SimCamera, SimConfig, StreamingProtocol, REG_EXPOSURE_TIME};
use libloading::Library;

const DLL_NAME: &str = "gentl_gige.dll";
const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
const DEVICE_ID: &str = "00:11:22:33:44:55";

const TL_INFO_ID: i32 = 0;
const TL_INFO_VENDOR: i32 = 1;
const TL_INFO_TLTYPE: i32 = 4;
const DEVICE_INFO_VENDOR: i32 = 1;
const DEVICE_ACCESS_EXCLUSIVE: u32 = 4;

/// Locate the cdylib artifact next to the current test executable. Works
/// for the default layout (`target/<profile>/gentl_gige.dll`, with the test
/// exe in `target/<profile>/deps/`), a custom `CARGO_TARGET_DIR`, and
/// out-of-tree target dirs reached by walking ancestors of the exe.
fn find_producer_dll() -> PathBuf {
    // PROFILE is a runtime env var for tests (compile-time env! is only for
    // build scripts); fall back to debug if unset.
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".into());
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(target_dir) = option_env!("CARGO_TARGET_DIR") {
        candidates.push(PathBuf::from(target_dir).join(&profile).join(DLL_NAME));
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in exe.ancestors().skip(1) {
            candidates.push(dir.join(DLL_NAME));
            candidates.push(dir.join(&profile).join(DLL_NAME));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("target").join(&profile).join(DLL_NAME));
    }

    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "cannot find {DLL_NAME}; checked: {}",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// The complete GenTL 1.6 entry-point table (include/GenTL.h GC_API list,
/// 64 functions). Field names are the C names; signatures mirror the
/// header on x64 (GC_CALLTYPE is empty there). Unused fields are still
/// resolved — that resolution is the export-completeness gate.
#[allow(non_snake_case, dead_code)]
struct GentlApi {
    GCGetInfo: unsafe extern "C" fn(TlInfoCmd, *mut InfoDataType, *mut c_void, *mut usize) -> GCError,
    GCGetLastError: unsafe extern "C" fn(*mut GCError, *mut c_char, *mut usize) -> GCError,
    GCInitLib: unsafe extern "C" fn() -> GCError,
    GCCloseLib: unsafe extern "C" fn() -> GCError,
    GCReadPort: unsafe extern "C" fn(PortHandle, u64, *mut c_void, *mut usize) -> GCError,
    GCWritePort: unsafe extern "C" fn(PortHandle, u64, *const c_void, *mut usize) -> GCError,
    GCGetPortURL: unsafe extern "C" fn(PortHandle, *mut c_char, *mut usize) -> GCError,
    GCGetPortInfo:
        unsafe extern "C" fn(PortHandle, PortInfoCmd, *mut InfoDataType, *mut c_void, *mut usize) -> GCError,
    GCRegisterEvent: unsafe extern "C" fn(EventSrcHandle, EventType, *mut EventHandle) -> GCError,
    GCUnregisterEvent: unsafe extern "C" fn(EventSrcHandle, EventType) -> GCError,
    EventGetData: unsafe extern "C" fn(EventHandle, *mut c_void, *mut usize, u64) -> GCError,
    EventGetDataInfo: unsafe extern "C" fn(
        EventHandle,
        *const c_void,
        usize,
        EventDataInfoCmd,
        *mut InfoDataType,
        *mut c_void,
        *mut usize,
    ) -> GCError,
    EventGetInfo:
        unsafe extern "C" fn(EventHandle, EventInfoCmd, *mut InfoDataType, *mut c_void, *mut usize) -> GCError,
    EventFlush: unsafe extern "C" fn(EventHandle) -> GCError,
    EventKill: unsafe extern "C" fn(EventHandle) -> GCError,
    TLOpen: unsafe extern "C" fn(*mut TLHandle) -> GCError,
    TLClose: unsafe extern "C" fn(TLHandle) -> GCError,
    TLGetInfo:
        unsafe extern "C" fn(TLHandle, TlInfoCmd, *mut InfoDataType, *mut c_void, *mut usize) -> GCError,
    TLGetNumInterfaces: unsafe extern "C" fn(TLHandle, *mut u32) -> GCError,
    TLGetInterfaceID: unsafe extern "C" fn(TLHandle, u32, *mut c_char, *mut usize) -> GCError,
    TLGetInterfaceInfo: unsafe extern "C" fn(
        TLHandle,
        *const c_char,
        InterfaceInfoCmd,
        *mut InfoDataType,
        *mut c_void,
        *mut usize,
    ) -> GCError,
    TLOpenInterface: unsafe extern "C" fn(TLHandle, *const c_char, *mut IFHandle) -> GCError,
    TLUpdateInterfaceList: unsafe extern "C" fn(TLHandle, *mut u8, u64) -> GCError,
    IFClose: unsafe extern "C" fn(IFHandle) -> GCError,
    IFGetInfo:
        unsafe extern "C" fn(IFHandle, InterfaceInfoCmd, *mut InfoDataType, *mut c_void, *mut usize) -> GCError,
    IFGetNumDevices: unsafe extern "C" fn(IFHandle, *mut u32) -> GCError,
    IFGetDeviceID: unsafe extern "C" fn(IFHandle, u32, *mut c_char, *mut usize) -> GCError,
    IFUpdateDeviceList: unsafe extern "C" fn(IFHandle, *mut u8, u64) -> GCError,
    IFGetDeviceInfo: unsafe extern "C" fn(
        IFHandle,
        *const c_char,
        DeviceInfoCmd,
        *mut InfoDataType,
        *mut c_void,
        *mut usize,
    ) -> GCError,
    IFOpenDevice: unsafe extern "C" fn(IFHandle, *const c_char, DeviceAccessFlags, *mut DevHandle) -> GCError,
    DevGetPort: unsafe extern "C" fn(DevHandle, *mut PortHandle) -> GCError,
    DevGetNumDataStreams: unsafe extern "C" fn(DevHandle, *mut u32) -> GCError,
    DevGetDataStreamID: unsafe extern "C" fn(DevHandle, u32, *mut c_char, *mut usize) -> GCError,
    DevOpenDataStream: unsafe extern "C" fn(DevHandle, *const c_char, *mut DSHandle) -> GCError,
    DevGetInfo:
        unsafe extern "C" fn(DevHandle, DeviceInfoCmd, *mut InfoDataType, *mut c_void, *mut usize) -> GCError,
    DevClose: unsafe extern "C" fn(DevHandle) -> GCError,
    DSAnnounceBuffer:
        unsafe extern "C" fn(DSHandle, *mut c_void, usize, *mut c_void, *mut BufferHandle) -> GCError,
    DSAllocAndAnnounceBuffer: unsafe extern "C" fn(DSHandle, usize, *mut c_void, *mut BufferHandle) -> GCError,
    DSFlushQueue: unsafe extern "C" fn(DSHandle, AcqQueueType) -> GCError,
    DSStartAcquisition: unsafe extern "C" fn(DSHandle, AcqStartFlags, u64) -> GCError,
    DSStopAcquisition: unsafe extern "C" fn(DSHandle, AcqStopFlags) -> GCError,
    DSGetInfo:
        unsafe extern "C" fn(DSHandle, StreamInfoCmd, *mut InfoDataType, *mut c_void, *mut usize) -> GCError,
    DSGetBufferID: unsafe extern "C" fn(DSHandle, u32, *mut BufferHandle) -> GCError,
    DSClose: unsafe extern "C" fn(DSHandle) -> GCError,
    DSRevokeBuffer: unsafe extern "C" fn(DSHandle, BufferHandle, *mut *mut c_void, *mut *mut c_void) -> GCError,
    DSQueueBuffer: unsafe extern "C" fn(DSHandle, BufferHandle) -> GCError,
    DSGetBufferInfo: unsafe extern "C" fn(
        DSHandle,
        BufferHandle,
        BufferInfoCmd,
        *mut InfoDataType,
        *mut c_void,
        *mut usize,
    ) -> GCError,
    GCGetNumPortURLs: unsafe extern "C" fn(PortHandle, *mut u32) -> GCError,
    GCGetPortURLInfo:
        unsafe extern "C" fn(PortHandle, u32, UrlInfoCmd, *mut InfoDataType, *mut c_void, *mut usize) -> GCError,
    GCReadPortStacked: unsafe extern "C" fn(PortHandle, *mut PortRegisterStackEntry, *mut usize) -> GCError,
    GCWritePortStacked: unsafe extern "C" fn(PortHandle, *mut PortRegisterStackEntry, *mut usize) -> GCError,
    DSGetBufferChunkData:
        unsafe extern "C" fn(DSHandle, BufferHandle, *mut SingleChunkData, *mut usize) -> GCError,
    IFGetParentTL: unsafe extern "C" fn(IFHandle, *mut TLHandle) -> GCError,
    DevGetParentIF: unsafe extern "C" fn(DevHandle, *mut IFHandle) -> GCError,
    DSGetParentDev: unsafe extern "C" fn(DSHandle, *mut DevHandle) -> GCError,
    DSGetNumBufferParts: unsafe extern "C" fn(DSHandle, BufferHandle, *mut u32) -> GCError,
    DSGetBufferPartInfo: unsafe extern "C" fn(
        DSHandle,
        BufferHandle,
        u32,
        BufferPartInfoCmd,
        *mut InfoDataType,
        *mut c_void,
        *mut usize,
    ) -> GCError,
    DSAnnounceCompositeBuffer: unsafe extern "C" fn(
        DSHandle,
        usize,
        *mut *mut c_void,
        *mut usize,
        *mut c_void,
        *mut BufferHandle,
    ) -> GCError,
    DSGetBufferInfoStacked:
        unsafe extern "C" fn(DSHandle, BufferHandle, *mut DSBufferInfoStacked, usize) -> GCError,
    DSGetBufferPartInfoStacked: unsafe extern "C" fn(
        DSHandle,
        BufferHandle,
        *mut DSBufferPartInfoStacked,
        usize,
    ) -> GCError,
    DSGetNumFlows: unsafe extern "C" fn(DSHandle, *mut u32) -> GCError,
    DSGetFlowInfo:
        unsafe extern "C" fn(DSHandle, u32, FlowInfoCmd, *mut InfoDataType, *mut c_void, *mut usize) -> GCError,
    DSGetNumBufferSegments: unsafe extern "C" fn(DSHandle, BufferHandle, *mut u32) -> GCError,
    DSGetBufferSegmentInfo: unsafe extern "C" fn(
        DSHandle,
        BufferHandle,
        u32,
        SegmentInfoCmd,
        *mut InfoDataType,
        *mut c_void,
        *mut usize,
    ) -> GCError,
}

impl GentlApi {
    /// Resolve every export by name. Any missing symbol panics naming it —
    /// this is the export-completeness regression gate.
    unsafe fn load(lib: &Library) -> GentlApi {
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let s: libloading::Symbol<$ty> = lib
                    .get($name.as_bytes())
                    .unwrap_or_else(|e| panic!("producer DLL is missing export `{}`: {e}", $name));
                *s
            }};
        }

        GentlApi {
            GCGetInfo: sym!("GCGetInfo", unsafe extern "C" fn(
                TlInfoCmd, *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            GCGetLastError: sym!("GCGetLastError", unsafe extern "C" fn(
                *mut GCError, *mut c_char, *mut usize,
            ) -> GCError),
            GCInitLib: sym!("GCInitLib", unsafe extern "C" fn() -> GCError),
            GCCloseLib: sym!("GCCloseLib", unsafe extern "C" fn() -> GCError),
            GCReadPort: sym!("GCReadPort", unsafe extern "C" fn(
                PortHandle, u64, *mut c_void, *mut usize,
            ) -> GCError),
            GCWritePort: sym!("GCWritePort", unsafe extern "C" fn(
                PortHandle, u64, *const c_void, *mut usize,
            ) -> GCError),
            GCGetPortURL: sym!("GCGetPortURL", unsafe extern "C" fn(
                PortHandle, *mut c_char, *mut usize,
            ) -> GCError),
            GCGetPortInfo: sym!("GCGetPortInfo", unsafe extern "C" fn(
                PortHandle, PortInfoCmd, *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            GCRegisterEvent: sym!("GCRegisterEvent", unsafe extern "C" fn(
                EventSrcHandle, EventType, *mut EventHandle,
            ) -> GCError),
            GCUnregisterEvent: sym!("GCUnregisterEvent", unsafe extern "C" fn(
                EventSrcHandle, EventType,
            ) -> GCError),
            EventGetData: sym!("EventGetData", unsafe extern "C" fn(
                EventHandle, *mut c_void, *mut usize, u64,
            ) -> GCError),
            EventGetDataInfo: sym!("EventGetDataInfo", unsafe extern "C" fn(
                EventHandle, *const c_void, usize, EventDataInfoCmd,
                *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            EventGetInfo: sym!("EventGetInfo", unsafe extern "C" fn(
                EventHandle, EventInfoCmd, *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            EventFlush: sym!("EventFlush", unsafe extern "C" fn(EventHandle) -> GCError),
            EventKill: sym!("EventKill", unsafe extern "C" fn(EventHandle) -> GCError),
            TLOpen: sym!("TLOpen", unsafe extern "C" fn(*mut TLHandle) -> GCError),
            TLClose: sym!("TLClose", unsafe extern "C" fn(TLHandle) -> GCError),
            TLGetInfo: sym!("TLGetInfo", unsafe extern "C" fn(
                TLHandle, TlInfoCmd, *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            TLGetNumInterfaces: sym!("TLGetNumInterfaces", unsafe extern "C" fn(
                TLHandle, *mut u32,
            ) -> GCError),
            TLGetInterfaceID: sym!("TLGetInterfaceID", unsafe extern "C" fn(
                TLHandle, u32, *mut c_char, *mut usize,
            ) -> GCError),
            TLGetInterfaceInfo: sym!("TLGetInterfaceInfo", unsafe extern "C" fn(
                TLHandle, *const c_char, InterfaceInfoCmd,
                *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            TLOpenInterface: sym!("TLOpenInterface", unsafe extern "C" fn(
                TLHandle, *const c_char, *mut IFHandle,
            ) -> GCError),
            TLUpdateInterfaceList: sym!("TLUpdateInterfaceList", unsafe extern "C" fn(
                TLHandle, *mut u8, u64,
            ) -> GCError),
            IFClose: sym!("IFClose", unsafe extern "C" fn(IFHandle) -> GCError),
            IFGetInfo: sym!("IFGetInfo", unsafe extern "C" fn(
                IFHandle, InterfaceInfoCmd, *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            IFGetNumDevices: sym!("IFGetNumDevices", unsafe extern "C" fn(
                IFHandle, *mut u32,
            ) -> GCError),
            IFGetDeviceID: sym!("IFGetDeviceID", unsafe extern "C" fn(
                IFHandle, u32, *mut c_char, *mut usize,
            ) -> GCError),
            IFUpdateDeviceList: sym!("IFUpdateDeviceList", unsafe extern "C" fn(
                IFHandle, *mut u8, u64,
            ) -> GCError),
            IFGetDeviceInfo: sym!("IFGetDeviceInfo", unsafe extern "C" fn(
                IFHandle, *const c_char, DeviceInfoCmd,
                *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            IFOpenDevice: sym!("IFOpenDevice", unsafe extern "C" fn(
                IFHandle, *const c_char, DeviceAccessFlags, *mut DevHandle,
            ) -> GCError),
            DevGetPort: sym!("DevGetPort", unsafe extern "C" fn(DevHandle, *mut PortHandle) -> GCError),
            DevGetNumDataStreams: sym!("DevGetNumDataStreams", unsafe extern "C" fn(
                DevHandle, *mut u32,
            ) -> GCError),
            DevGetDataStreamID: sym!("DevGetDataStreamID", unsafe extern "C" fn(
                DevHandle, u32, *mut c_char, *mut usize,
            ) -> GCError),
            DevOpenDataStream: sym!("DevOpenDataStream", unsafe extern "C" fn(
                DevHandle, *const c_char, *mut DSHandle,
            ) -> GCError),
            DevGetInfo: sym!("DevGetInfo", unsafe extern "C" fn(
                DevHandle, DeviceInfoCmd, *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            DevClose: sym!("DevClose", unsafe extern "C" fn(DevHandle) -> GCError),
            DSAnnounceBuffer: sym!("DSAnnounceBuffer", unsafe extern "C" fn(
                DSHandle, *mut c_void, usize, *mut c_void, *mut BufferHandle,
            ) -> GCError),
            DSAllocAndAnnounceBuffer: sym!("DSAllocAndAnnounceBuffer", unsafe extern "C" fn(
                DSHandle, usize, *mut c_void, *mut BufferHandle,
            ) -> GCError),
            DSFlushQueue: sym!("DSFlushQueue", unsafe extern "C" fn(DSHandle, AcqQueueType) -> GCError),
            DSStartAcquisition: sym!("DSStartAcquisition", unsafe extern "C" fn(
                DSHandle, AcqStartFlags, u64,
            ) -> GCError),
            DSStopAcquisition: sym!("DSStopAcquisition", unsafe extern "C" fn(
                DSHandle, AcqStopFlags,
            ) -> GCError),
            DSGetInfo: sym!("DSGetInfo", unsafe extern "C" fn(
                DSHandle, StreamInfoCmd, *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            DSGetBufferID: sym!("DSGetBufferID", unsafe extern "C" fn(
                DSHandle, u32, *mut BufferHandle,
            ) -> GCError),
            DSClose: sym!("DSClose", unsafe extern "C" fn(DSHandle) -> GCError),
            DSRevokeBuffer: sym!("DSRevokeBuffer", unsafe extern "C" fn(
                DSHandle, BufferHandle, *mut *mut c_void, *mut *mut c_void,
            ) -> GCError),
            DSQueueBuffer: sym!("DSQueueBuffer", unsafe extern "C" fn(DSHandle, BufferHandle) -> GCError),
            DSGetBufferInfo: sym!("DSGetBufferInfo", unsafe extern "C" fn(
                DSHandle, BufferHandle, BufferInfoCmd,
                *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            GCGetNumPortURLs: sym!("GCGetNumPortURLs", unsafe extern "C" fn(
                PortHandle, *mut u32,
            ) -> GCError),
            GCGetPortURLInfo: sym!("GCGetPortURLInfo", unsafe extern "C" fn(
                PortHandle, u32, UrlInfoCmd, *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            GCReadPortStacked: sym!("GCReadPortStacked", unsafe extern "C" fn(
                PortHandle, *mut PortRegisterStackEntry, *mut usize,
            ) -> GCError),
            GCWritePortStacked: sym!("GCWritePortStacked", unsafe extern "C" fn(
                PortHandle, *mut PortRegisterStackEntry, *mut usize,
            ) -> GCError),
            DSGetBufferChunkData: sym!("DSGetBufferChunkData", unsafe extern "C" fn(
                DSHandle, BufferHandle, *mut SingleChunkData, *mut usize,
            ) -> GCError),
            IFGetParentTL: sym!("IFGetParentTL", unsafe extern "C" fn(
                IFHandle, *mut TLHandle,
            ) -> GCError),
            DevGetParentIF: sym!("DevGetParentIF", unsafe extern "C" fn(
                DevHandle, *mut IFHandle,
            ) -> GCError),
            DSGetParentDev: sym!("DSGetParentDev", unsafe extern "C" fn(
                DSHandle, *mut DevHandle,
            ) -> GCError),
            DSGetNumBufferParts: sym!("DSGetNumBufferParts", unsafe extern "C" fn(
                DSHandle, BufferHandle, *mut u32,
            ) -> GCError),
            DSGetBufferPartInfo: sym!("DSGetBufferPartInfo", unsafe extern "C" fn(
                DSHandle, BufferHandle, u32, BufferPartInfoCmd,
                *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            DSAnnounceCompositeBuffer: sym!("DSAnnounceCompositeBuffer", unsafe extern "C" fn(
                DSHandle, usize, *mut *mut c_void, *mut usize, *mut c_void, *mut BufferHandle,
            ) -> GCError),
            DSGetBufferInfoStacked: sym!("DSGetBufferInfoStacked", unsafe extern "C" fn(
                DSHandle, BufferHandle, *mut DSBufferInfoStacked, usize,
            ) -> GCError),
            DSGetBufferPartInfoStacked: sym!("DSGetBufferPartInfoStacked", unsafe extern "C" fn(
                DSHandle, BufferHandle, *mut DSBufferPartInfoStacked, usize,
            ) -> GCError),
            DSGetNumFlows: sym!("DSGetNumFlows", unsafe extern "C" fn(DSHandle, *mut u32) -> GCError),
            DSGetFlowInfo: sym!("DSGetFlowInfo", unsafe extern "C" fn(
                DSHandle, u32, FlowInfoCmd, *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
            DSGetNumBufferSegments: sym!("DSGetNumBufferSegments", unsafe extern "C" fn(
                DSHandle, BufferHandle, *mut u32,
            ) -> GCError),
            DSGetBufferSegmentInfo: sym!("DSGetBufferSegmentInfo", unsafe extern "C" fn(
                DSHandle, BufferHandle, u32, SegmentInfoCmd,
                *mut InfoDataType, *mut c_void, *mut usize,
            ) -> GCError),
        }
    }
}

/// Call `f` expecting GC_ERR_SUCCESS; on failure pull GCGetLastError text
/// from the DLL and panic.
fn check(api: &GentlApi, what: &str, r: GCError) {
    if r == GC_ERR_SUCCESS {
        return;
    }
    let mut code: GCError = 0;
    let mut text = [0u8; 1024];
    let mut size = text.len();
    let gr = unsafe { (api.GCGetLastError)(&mut code, text.as_mut_ptr().cast(), &mut size) };
    let msg = if gr == GC_ERR_SUCCESS {
        String::from_utf8_lossy(&text[..size.saturating_sub(1)]).into_owned()
    } else {
        String::new()
    };
    panic!("{what} failed: GC_ERROR {r} ({code}): {msg}");
}

fn read_port(api: &GentlApi, h: PortHandle, addr: u64, buf: &mut [u8]) {
    let mut size = buf.len();
    check(api, "GCReadPort", unsafe {
        (api.GCReadPort)(h, addr, buf.as_mut_ptr().cast(), &mut size)
    });
    assert_eq!(size, buf.len(), "GCReadPort size out");
}

fn write_port(api: &GentlApi, h: PortHandle, addr: u64, data: &[u8]) {
    let mut size = data.len();
    check(api, "GCWritePort", unsafe {
        (api.GCWritePort)(h, addr, data.as_ptr().cast(), &mut size)
    });
}

/// GenTL string output helper: size query then read into a fixed buffer.
fn get_string(
    api: &GentlApi,
    what: &str,
    f: impl FnOnce(*mut c_char, *mut usize) -> GCError,
) -> String {
    let mut buf = [0u8; 1024];
    let mut size = buf.len();
    check(api, what, f(buf.as_mut_ptr() as *mut c_char, &mut size));
    String::from_utf8_lossy(&buf[..size.saturating_sub(1)]).into_owned()
}

#[test]
fn consumer_dll_end_to_end() {
    let dll_path = find_producer_dll();
    eprintln!("loading producer DLL: {}", dll_path.display());

    let cam = SimCamera::new(SimConfig {
        mac: MAC,
        ip: Ipv4Addr::LOCALHOST,
        subnet_mask: Ipv4Addr::new(255, 0, 0, 0),
        gateway: Ipv4Addr::UNSPECIFIED,
        manufacturer_name: "GenTLite".into(),
        model_name: "SimCam".into(),
        device_version: "1.0.0".into(),
        serial_number: "SIM-0001".into(),
        streaming: StreamingProtocol::GvspOnly,
    });
    let server = cam.start(0).expect("start sim camera");
    let port = server.local_port().expect("server port");

    // Producer-side hooks for the loopback camera (see tests/control_plane.rs).
    std::env::set_var("GENTLITE_GVCP_PORT", port.to_string());
    std::env::set_var("GENTLITE_DISCOVERY_TARGETS", format!("127.0.0.1:{port}"));

    // Safety: the DLL is a freshly built producer artifact; nothing else is
    // loaded from this directory.
    let lib = unsafe {
        Library::new(&dll_path).unwrap_or_else(|e| panic!("LoadLibrary {}: {e}", dll_path.display()))
    };
    // Safety: signatures mirror include/GenTL.h on x64; the DLL stays loaded
    // for the whole test (drop order: `lib` outlives all calls below).
    let api = unsafe { GentlApi::load(&lib) };

    unsafe {
        check(&api, "GCInitLib", (api.GCInitLib)());

        let mut tl: TLHandle = ptr::null_mut();
        check(&api, "TLOpen", (api.TLOpen)(&mut tl));
        assert!(!tl.is_null());

        // TL info: ID / VENDOR / TLTYPE == "GEV".
        let id = get_string(&api, "TLGetInfo ID", |s, n| {
            let mut dtype: InfoDataType = 0;
            (api.TLGetInfo)(tl, TL_INFO_ID, &mut dtype, s.cast(), n)
        });
        assert_eq!(id, "gentl_gige.cti"); // Basler-style: filename as ID
        let vendor = get_string(&api, "TLGetInfo VENDOR", |s, n| {
            let mut dtype: InfoDataType = 0;
            (api.TLGetInfo)(tl, TL_INFO_VENDOR, &mut dtype, s.cast(), n)
        });
        assert_eq!(vendor, "GenTLite");
        let tltype = get_string(&api, "TLGetInfo TLTYPE", |s, n| {
            let mut dtype: InfoDataType = 0;
            (api.TLGetInfo)(tl, TL_INFO_TLTYPE, &mut dtype, s.cast(), n)
        });
        assert_eq!(tltype, "GEV");

        let mut num_ifaces = 0u32;
        check(&api, "TLGetNumInterfaces", (api.TLGetNumInterfaces)(tl, &mut num_ifaces));
        assert!(num_ifaces >= 1, "no network interfaces enumerated");

        let iface_id = get_string(&api, "TLGetInterfaceID", |s, n| {
            (api.TLGetInterfaceID)(tl, 0, s, n)
        });
        assert_eq!(
            iface_id,
            "GenTLite GEV GenTL Interface Module",
            "Basler-style aggregate interface"
        );

        let mut iface: IFHandle = ptr::null_mut();
        let c_id = std::ffi::CString::new(iface_id.clone()).unwrap();
        check(&api, "TLOpenInterface", (api.TLOpenInterface)(tl, c_id.as_ptr(), &mut iface));
        assert!(!iface.is_null());

        let mut changed = 0u8;
        check(&api, "IFUpdateDeviceList", (api.IFUpdateDeviceList)(iface, &mut changed, 200));

        let mut num_devs = 0u32;
        check(&api, "IFGetNumDevices", (api.IFGetNumDevices)(iface, &mut num_devs));
        assert!(num_devs >= 1, "discovery via GENTLITE_DISCOVERY_TARGETS found no device");

        let dev_id = get_string(&api, "IFGetDeviceID", |s, n| {
            (api.IFGetDeviceID)(iface, 0, s, n)
        });
        assert_eq!(dev_id, DEVICE_ID);

        let c_dev = std::ffi::CString::new(DEVICE_ID).unwrap();
        let mut dev: DevHandle = ptr::null_mut();
        check(&api, "IFOpenDevice", (api.IFOpenDevice)(
            iface,
            c_dev.as_ptr(),
            DEVICE_ACCESS_EXCLUSIVE,
            &mut dev,
        ));
        assert!(!dev.is_null());

        // Device info through the DLL: vendor from the discovery data.
        let dev_vendor = get_string(&api, "DevGetInfo VENDOR", |s, n| {
            let mut dtype: InfoDataType = 0;
            (api.DevGetInfo)(dev, DEVICE_INFO_VENDOR, &mut dtype, s.cast(), n)
        });
        assert_eq!(dev_vendor, "GenTLite");

        let mut port_h: PortHandle = ptr::null_mut();
        check(&api, "DevGetPort", (api.DevGetPort)(dev, &mut port_h));
        assert!(!port_h.is_null());

        // Version register: 0x00020002 in big-endian register order.
        let mut v = [0u8; 4];
        read_port(&api, port_h, 0x0000, &mut v);
        assert_eq!(v, [0x00, 0x02, 0x00, 0x02], "version 0x00020002 BE");

        // Exposure register (f32 BE per the sim's GenApi map) round-trip.
        let exposure = 33_000.0f32;
        write_port(&api, port_h, u64::from(REG_EXPOSURE_TIME), &exposure.to_bits().to_be_bytes());
        let mut v = [0u8; 4];
        read_port(&api, port_h, u64::from(REG_EXPOSURE_TIME), &mut v);
        assert_eq!(f32::from_bits(u32::from_be_bytes(v)), exposure);

        // XML URL from the bootstrap register, then read the XML header
        // straight from the register map at the URL address.
        let url = get_string(&api, "GCGetPortURL", |s, n| (api.GCGetPortURL)(port_h, s, n));
        assert!(url.to_lowercase().starts_with("local:"), "local URL: {url}");
        let xml_addr = u32::from_str_radix(url.split(';').nth(1).unwrap(), 16).unwrap();

        let mut head = [0u8; 5];
        read_port(&api, port_h, u64::from(xml_addr), &mut head);
        assert_eq!(&head, b"<?xml");

        check(&api, "DevClose", (api.DevClose)(dev));
        check(&api, "IFClose", (api.IFClose)(iface));
        check(&api, "TLClose", (api.TLClose)(tl));
        check(&api, "GCCloseLib", (api.GCCloseLib)());
    }

    drop(lib);
    server.stop();
}
