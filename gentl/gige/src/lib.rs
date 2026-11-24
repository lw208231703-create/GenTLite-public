//! GigE Vision GenTL producer — the loadable `.cti`.
//!
//! GenTL 1.6 loading: the consumer resolves every function below by name
//! (GetProcAddress / dlsym) and starts with `GCInitLib`.
//!
//! The implementation covers TL/IF/Dev modules, GVCP port access, GVSP
//! streams, buffer events and packet resend. Optional GenTL capabilities
//! outside the implemented image-acquisition surface return NOT_IMPLEMENTED.
//! Panics must never cross this boundary: real implementations convert
//! errors at the edge.
//!
//! The built artifact is `gentl_gige.dll`; the `.cti` rename happens at
//! packaging time (M5 release flow).

#![allow(non_snake_case)]

use std::os::raw::c_char;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gentl_core::types::*;
use gentl_core::{
    channel::ControlChannel, ds as ds_core, event_queue::WaitResult, free_handle, from_handle,
    last_error, set_error, set_last_error, to_handle, GentlError,
};
use gentl_gvcp::registers;

mod cti;
mod device;
mod discovery;
mod events;
mod gvsp_receiver;
mod payload;
mod stream;
mod sys;

use cti::{DeviceAccess, DeviceEntry, DevicePort, GevDevice, GevInterface, TlSystem};
use events::DeviceEventModule;
use stream::{EventModule, GevBuffer, GevDataStream, STREAM_ID};

// Re-exported for the `gvsp_perf_rx` benchmarking example and other
// wire-level tooling; not part of the GenTL surface.
pub use gvsp_receiver::{GvspReassembler, ReceiverStats};

// GenTL 1.6 enum values (include/GenTL.h is authoritative).
const INFO_DATATYPE_STRING: InfoDataType = 1;
const INFO_DATATYPE_INT32: InfoDataType = 5;
const INFO_DATATYPE_UINT32: InfoDataType = 6;
const INFO_DATATYPE_UINT64: InfoDataType = 8;
const INFO_DATATYPE_PTR: InfoDataType = 10;
const INFO_DATATYPE_BOOL8: InfoDataType = 11;
const INFO_DATATYPE_SIZET: InfoDataType = 12;

const TL_INFO_ID: TlInfoCmd = 0;
const TL_INFO_VENDOR: TlInfoCmd = 1;
const TL_INFO_MODEL: TlInfoCmd = 2;
const TL_INFO_VERSION: TlInfoCmd = 3;
const TL_INFO_TLTYPE: TlInfoCmd = 4;
const TL_INFO_NAME: TlInfoCmd = 5;
const TL_INFO_PATHNAME: TlInfoCmd = 6;
const TL_INFO_DISPLAYNAME: TlInfoCmd = 7;
const TL_INFO_CHAR_ENCODING: TlInfoCmd = 8;
const TL_INFO_GENTL_VER_MAJOR: TlInfoCmd = 9;
const TL_INFO_GENTL_VER_MINOR: TlInfoCmd = 10;

const INTERFACE_INFO_ID: InterfaceInfoCmd = 0;
const INTERFACE_INFO_DISPLAYNAME: InterfaceInfoCmd = 1;
const INTERFACE_INFO_TLTYPE: InterfaceInfoCmd = 2;

const DEVICE_INFO_ID: DeviceInfoCmd = 0;
const DEVICE_INFO_VENDOR: DeviceInfoCmd = 1;
const DEVICE_INFO_MODEL: DeviceInfoCmd = 2;
const DEVICE_INFO_TLTYPE: DeviceInfoCmd = 3;
const DEVICE_INFO_DISPLAYNAME: DeviceInfoCmd = 4;
const DEVICE_INFO_ACCESS_STATUS: DeviceInfoCmd = 5;
const DEVICE_INFO_USER_DEFINED_NAME: DeviceInfoCmd = 6;
const DEVICE_INFO_SERIAL_NUMBER: DeviceInfoCmd = 7;
const DEVICE_INFO_VERSION: DeviceInfoCmd = 8;
const DEVICE_INFO_TIMESTAMP_FREQUENCY: DeviceInfoCmd = 9;

const PORT_INFO_ID: PortInfoCmd = 0;
const PORT_INFO_VENDOR: PortInfoCmd = 1;
const PORT_INFO_MODEL: PortInfoCmd = 2;
const PORT_INFO_TLTYPE: PortInfoCmd = 3;
const PORT_INFO_MODULE: PortInfoCmd = 4;
const PORT_INFO_LITTLE_ENDIAN: PortInfoCmd = 5;
const PORT_INFO_BIG_ENDIAN: PortInfoCmd = 6;
const PORT_INFO_ACCESS_READ: PortInfoCmd = 7;
const PORT_INFO_ACCESS_WRITE: PortInfoCmd = 8;
const PORT_INFO_ACCESS_NA: PortInfoCmd = 9;
const PORT_INFO_ACCESS_NI: PortInfoCmd = 10;
const PORT_INFO_VERSION: PortInfoCmd = 11;
const PORT_INFO_PORTNAME: PortInfoCmd = 12;

// STREAM_INFO_CMD_LIST (GenTL.h)
const STREAM_INFO_ID: StreamInfoCmd = 0;
const STREAM_INFO_NUM_DELIVERED: StreamInfoCmd = 1;
const STREAM_INFO_NUM_UNDERRUN: StreamInfoCmd = 2;
const STREAM_INFO_NUM_ANNOUNCED: StreamInfoCmd = 3;
const STREAM_INFO_NUM_QUEUED: StreamInfoCmd = 4;
const STREAM_INFO_NUM_AWAIT_DELIVERY: StreamInfoCmd = 5;
const STREAM_INFO_NUM_STARTED: StreamInfoCmd = 6;
const STREAM_INFO_PAYLOAD_SIZE: StreamInfoCmd = 7;
const STREAM_INFO_IS_GRABBING: StreamInfoCmd = 8;
const STREAM_INFO_DEFINES_PAYLOADSIZE: StreamInfoCmd = 9;
const STREAM_INFO_TLTYPE: StreamInfoCmd = 10;
const STREAM_INFO_NUM_CHUNKS_MAX: StreamInfoCmd = 11;
const STREAM_INFO_BUF_ANNOUNCE_MIN: StreamInfoCmd = 12;
const STREAM_INFO_BUF_ALIGNMENT: StreamInfoCmd = 13;

// BUFFER_INFO_CMD_LIST (GenTL.h)
const BUFFER_INFO_BASE: BufferInfoCmd = 0;
const BUFFER_INFO_SIZE: BufferInfoCmd = 1;
const BUFFER_INFO_USER_PTR: BufferInfoCmd = 2;
const BUFFER_INFO_TIMESTAMP: BufferInfoCmd = 3;
const BUFFER_INFO_NEW_DATA: BufferInfoCmd = 4;
const BUFFER_INFO_IS_QUEUED: BufferInfoCmd = 5;
const BUFFER_INFO_IS_ACQUIRING: BufferInfoCmd = 6;
const BUFFER_INFO_IS_INCOMPLETE: BufferInfoCmd = 7;
const BUFFER_INFO_TLTYPE: BufferInfoCmd = 8;
const BUFFER_INFO_SIZE_FILLED: BufferInfoCmd = 9;
const BUFFER_INFO_WIDTH: BufferInfoCmd = 10;
const BUFFER_INFO_HEIGHT: BufferInfoCmd = 11;
const BUFFER_INFO_XOFFSET: BufferInfoCmd = 12;
const BUFFER_INFO_YOFFSET: BufferInfoCmd = 13;
const BUFFER_INFO_XPADDING: BufferInfoCmd = 14;
const BUFFER_INFO_YPADDING: BufferInfoCmd = 15;
const BUFFER_INFO_FRAMEID: BufferInfoCmd = 16;
const BUFFER_INFO_IMAGEPRESENT: BufferInfoCmd = 17;
const BUFFER_INFO_IMAGEOFFSET: BufferInfoCmd = 18;
const BUFFER_INFO_PAYLOADTYPE: BufferInfoCmd = 19;
const BUFFER_INFO_PIXELFORMAT: BufferInfoCmd = 20;
const BUFFER_INFO_PIXELFORMAT_NAMESPACE: BufferInfoCmd = 21;
const BUFFER_INFO_DELIVERED_IMAGEHEIGHT: BufferInfoCmd = 22;
const BUFFER_INFO_DELIVERED_CHUNKPAYLOADSIZE: BufferInfoCmd = 23;
const BUFFER_INFO_CHUNKLAYOUTID: BufferInfoCmd = 24;
const BUFFER_INFO_FILENAME: BufferInfoCmd = 25;
const BUFFER_INFO_PIXEL_ENDIANNESS: BufferInfoCmd = 26;
const BUFFER_INFO_DATA_SIZE: BufferInfoCmd = 27;
const BUFFER_INFO_TIMESTAMP_NS: BufferInfoCmd = 28;
const BUFFER_INFO_DATA_LARGER_THAN_BUFFER: BufferInfoCmd = 29;
const BUFFER_INFO_CONTAINS_CHUNKDATA: BufferInfoCmd = 30;

// EVENT_DATA_INFO_CMD_LIST (GenTL.h): per-event data handed to the consumer
// in the wire record layout (§17.1.1).
const EVENT_DATA_ID: EventDataInfoCmd = 0;
const EVENT_DATA_VALUE: EventDataInfoCmd = 1;
const EVENT_DATA_NUMID: EventDataInfoCmd = 2;

/// INFO_DATATYPE_BUFFER (GenTL.h).
const INFO_DATATYPE_BUFFER: InfoDataType = 13;

// EVENT_INFO_CMD_LIST (GenTL.h)
const EVENT_EVENT_TYPE: EventInfoCmd = 0;
const EVENT_NUM_IN_QUEUE: EventInfoCmd = 1;
const EVENT_NUM_FIRED: EventInfoCmd = 2;
const EVENT_SIZE_MAX: EventInfoCmd = 3;
const EVENT_INFO_DATA_SIZE_MAX: EventInfoCmd = 4;

// GenTL 1.6 PAYLOADTYPE_INFO_IDS / PIXELFORMAT_NAMESPACE_IDS.
const PAYLOAD_TYPE_IMAGE: usize = 1;
const PIXELFORMAT_NAMESPACE_GEV: u64 = 1;
const PIXELENDIANNESS_LITTLE: i32 = 1;

// DEVICE_ACCESS_STATUS_LIST (GenTL.h).
const DEVICE_ACCESS_STATUS_READWRITE: i32 = 1;
const DEVICE_ACCESS_STATUS_OPEN_READWRITE: i32 = 5;
const DEVICE_ACCESS_STATUS_OPEN_READONLY: i32 = 6;

/// GENTL_INFINITE (GenTL.h V1.4).
const GENTL_INFINITE: u64 = 0xFFFFFFFFFFFFFFFF;

// URL_INFO_CMD_LIST / URL_SCHEME_IDS / SEGMENT_INFO_CMD_LIST (GenTL.h).
const URL_INFO_URL: UrlInfoCmd = 0;
const URL_INFO_FILE_REGISTER_ADDRESS: UrlInfoCmd = 7;
const URL_INFO_FILE_SIZE: UrlInfoCmd = 8;
const URL_INFO_SCHEME: UrlInfoCmd = 9;
/// The GenTLite XML is read from the device's register map.
const URL_SCHEME_LOCAL: i32 = 0;
const SEGMENT_INFO_BASE: SegmentInfoCmd = 0;
const SEGMENT_INFO_SIZE: SegmentInfoCmd = 1;
const SEGMENT_INFO_IS_INCOMPLETE: SegmentInfoCmd = 2;

const TL_TYPE_GEV: &str = "GEV"; // TLTypeGEVName (GenTL.h)

// Basler-style identity (observed on ProducerGEV.cti, 2026-10-12): vendor
// name, tech as model, filename as ID, "<vendor> GEV GenTL Producer" as
// display name. MVS uses the same aggregate structure with terser strings.
const TL_ID: &str = TL_FILENAME;
const TL_VENDOR: &str = "GenTLite";
const TL_MODEL: &str = "GEV";
const TL_DISPLAYNAME: &str = "GenTLite GEV GenTL Producer";
const TL_FILENAME: &str = "gentl_gige.cti";
const PORT_NAME_DEVICE: &str = "Device"; // OPEN-QUESTION Q9

static LIB_INITIALIZED: AtomicBool = AtomicBool::new(false);
/// The System module can be opened only once per process (§6.3.2.6).
static TL_OPENED: AtomicBool = AtomicBool::new(false);

fn not_implemented(name: &str) -> GCError {
    set_error(GC_ERR_NOT_IMPLEMENTED, name);
    GC_ERR_NOT_IMPLEMENTED
}

fn fail(e: GentlError) -> GCError {
    set_last_error(&e);
    e.code()
}

/// GC_ERR_INVALID_INDEX: `GentlError` has no variant for it (core is
/// frozen for M1), so set it directly.
fn invalid_index() -> GCError {
    set_error(GC_ERR_INVALID_INDEX, "index out of range");
    GC_ERR_INVALID_INDEX
}

// Raw output helpers ----------------------------------------------------------

/// Read a NUL-terminated C string from the consumer. None if NULL.
unsafe fn c_str<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    std::str::from_utf8(std::slice::from_raw_parts(p as *const u8, len)).ok()
}

/// Standard GenTL string output: a size query (NULL buffer) returns the
/// needed size with GC_ERR_SUCCESS; a too-small buffer reports the needed
/// size with GC_ERR_BUFFER_TOO_SMALL.
///
/// # Safety
/// `pi_size` must be a valid pointer; `s_out` must have `*pi_size` bytes
/// of capacity or be NULL.
unsafe fn write_string(s_out: *mut c_char, pi_size: *mut usize, s: &str) -> GCError {
    let needed = s.len() + 1;
    if s_out.is_null() {
        *pi_size = needed;
        return GC_ERR_SUCCESS;
    }
    if *pi_size < needed {
        *pi_size = needed;
        return GC_ERR_BUFFER_TOO_SMALL;
    }
    ptr::copy_nonoverlapping(s.as_ptr(), s_out as *mut u8, s.len());
    *s_out.add(s.len()) = 0;
    *pi_size = needed;
    GC_ERR_SUCCESS
}

/// Fixed-size value output with the same size-query/buffer-too-small
/// semantics as [`write_string`].
///
/// # Safety
/// Same contract as `write_string` for `p_buffer`/`pi_size`.
unsafe fn write_value<T: Copy>(
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
    v: &T,
) -> GCError {
    let needed = std::mem::size_of::<T>();
    if p_buffer.is_null() {
        *pi_size = needed;
        return GC_ERR_SUCCESS;
    }
    if *pi_size < needed {
        *pi_size = needed;
        return GC_ERR_BUFFER_TOO_SMALL;
    }
    ptr::copy_nonoverlapping((v as *const T).cast::<u8>(), p_buffer as *mut u8, needed);
    *pi_size = needed;
    GC_ERR_SUCCESS
}

/// Info-function preamble: both pointers are mandatory (§6.3 error lists:
/// "Parameters piSize and/or piType are invalid pointers").
fn check_info_args(pi_type: *mut InfoDataType, pi_size: *mut usize) -> GCError {
    if pi_type.is_null() || pi_size.is_null() {
        return fail(GentlError::InvalidParameter("piType/piSize"));
    }
    GC_ERR_SUCCESS
}

/// # Safety
/// Callers guarantee the GenTL buffer contract (see `write_string`).
unsafe fn info_string(
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
    s: &str,
) -> GCError {
    *pi_type = INFO_DATATYPE_STRING;
    write_string(p_buffer as *mut c_char, pi_size, s)
}

/// # Safety
/// Callers guarantee the GenTL buffer contract (see `write_string`).
unsafe fn info_value<T: Copy>(
    pi_type: *mut InfoDataType,
    datatype: InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
    v: &T,
) -> GCError {
    *pi_type = datatype;
    write_value(p_buffer, pi_size, v)
}

/// Handle → typed module, mapping failures to the GenTL error code.
macro_rules! module {
    ($h:expr, $ty:ty) => {
        match unsafe { from_handle::<$ty>($h) } {
            Ok(m) => m,
            Err(e) => return fail(e),
        }
    };
}

/// A child handle becomes implicitly invalid when its parent module is
/// closed (GenTL 1.6 §6.2); we enforce that through parent `Weak`s.
fn parent_alive<T>(parent: &std::sync::Weak<T>) -> Result<(), GCError> {
    if parent.strong_count() == 0 {
        return Err(fail(GentlError::InvalidHandle));
    }
    Ok(())
}

fn display_name(vendor: &str, model: &str, id: &str) -> String {
    format!("{vendor} {model} ({id})")
}

fn require_initialized() -> Result<(), GCError> {
    if !LIB_INITIALIZED.load(Ordering::SeqCst) {
        return Err(fail(GentlError::NotInitialized));
    }
    Ok(())
}

// Panic containment -----------------------------------------------------------

/// Text carried by a caught panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
}

/// Run an entry-point body, turning a panic into GC_ERR_ERROR.
///
/// Module contract: "panics must never cross this boundary". A panic that
/// reaches an `extern "C"` frame cannot unwind, so the process aborts
/// ("thread caused non-unwinding panic. aborting.", SIGABRT) and the
/// consumer (NI MAX, pylon, HALCON, …) dies without ever seeing a GenTL
/// error code. Every exported entry point therefore runs its body here —
/// `every_export_body_is_panic_guarded` enforces that mechanically — and the
/// default panic hook still reports the panic and backtrace on stderr, so
/// containment does not hide the bug.
fn guard(body: impl FnOnce() -> GCError) -> GCError {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(code) => code,
        Err(payload) => {
            let text = format!("internal panic: {}", panic_message(&*payload));
            set_error(GC_ERR_ERROR, &text);
            GC_ERR_ERROR
        }
    }
}

// Library functions -----------------------------------------------------------

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCGetLastError(
    pi_error_code: *mut GCError,
    s_err_text: *mut c_char,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    // GenTL 1.6 §6.3.1.3: NULL piErrorCode or piSize -> INVALID_PARAMETER;
    // sErrText == NULL means "report the needed size via *piSize".
    if pi_error_code.is_null() || pi_size.is_null() {
        return GC_ERR_INVALID_PARAMETER;
    }
    let (code, text) = last_error();
    unsafe { *pi_error_code = code };
    if s_err_text.is_null() {
        unsafe { *pi_size = text.len() + 1 };
        return GC_ERR_SUCCESS;
    }
    let needed = text.len() + 1;
    let avail = unsafe { *pi_size };
    if avail < needed {
        unsafe { *pi_size = needed };
        return GC_ERR_BUFFER_TOO_SMALL;
    }
    unsafe {
        ptr::copy_nonoverlapping(text.as_ptr(), s_err_text as *mut u8, text.len());
        *s_err_text.add(text.len()) = 0;
        *pi_size = needed;
    }
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCInitLib() -> GCError {
    guard(|| {
    // GenTL 1.6 §6.3.1.4: double GCInitLib without an intervening
    // GCCloseLib returns GC_ERR_RESOURCE_IN_USE (no reference counting).
    if LIB_INITIALIZED.swap(true, Ordering::SeqCst) {
        return GC_ERR_RESOURCE_IN_USE;
    }
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCCloseLib() -> GCError {
    guard(|| {
    LIB_INITIALIZED.store(false, Ordering::SeqCst);
    GC_ERR_SUCCESS
    })
}

// System (TL) functions -------------------------------------------------------

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn TLOpen(ph_tl: *mut TLHandle) -> GCError {
    guard(|| {
    if ph_tl.is_null() {
        return fail(GentlError::InvalidParameter("phTL"));
    }
    unsafe { *ph_tl = ptr::null_mut() };
    if let Err(e) = require_initialized() {
        return e;
    }
    if TL_OPENED.swap(true, Ordering::SeqCst) {
        return fail(GentlError::ResourceInUse);
    }
    let system = TlSystem::new(Vec::new(), discovery::list_nics());
    let template = GevInterface::new_template(&system);
    match system.interfaces.lock() {
        Ok(mut list) => *list = vec![template],
        Err(e) => {
            TL_OPENED.store(false, Ordering::SeqCst);
            return fail(GentlError::Io(e.to_string()));
        }
    }
    unsafe { *ph_tl = to_handle(system) };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn TLClose(h_tl: TLHandle) -> GCError {
    guard(|| {
    let system = module!(h_tl, TlSystem);
    TL_OPENED.store(false, Ordering::SeqCst);
    drop(system);
    unsafe { free_handle(h_tl) };
    GC_ERR_SUCCESS
    })
}

fn module_path() -> Option<String> {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::System::LibraryLoader::{
            GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        };

        fn anchor() {}
        let mut module = std::ptr::null_mut();
        let ok = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                anchor as *const u16,
                &mut module,
            )
        };
        if ok == 0 {
            return None;
        }
        let mut buf = vec![0u16; 32768];
        let n = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) };
        if n == 0 {
            return None;
        }
        Some(OsString::from_wide(&buf[..n as usize]).to_string_lossy().into_owned())
    }
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CStr;
        use std::os::raw::c_void;

        fn anchor() {}
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::dladdr(anchor as *const c_void, &mut info) };
        if ok == 0 || info.dli_fname.is_null() {
            return None;
        }
        Some(unsafe { CStr::from_ptr(info.dli_fname) }.to_string_lossy().into_owned())
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/// `TL_INFO_CMD` dispatch shared by `TLGetInfo` (§6.4.1.3) and the legacy
/// `GCGetInfo` (§6.3.1.2). The legacy entry must report the same
/// information without an instantiated System module, so the dispatch must
/// not touch module state.
///
/// # Safety
/// `pi_type`/`pi_size` were validated by the caller (`check_info_args`);
/// `p_buffer` follows the GenTL size-query contract.
unsafe fn tl_info(
    i_info_cmd: TlInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    let version = env!("CARGO_PKG_VERSION");
    match i_info_cmd {
        TL_INFO_ID => info_string(pi_type, p_buffer, pi_size, TL_ID),
        TL_INFO_VENDOR => info_string(pi_type, p_buffer, pi_size, TL_VENDOR),
        TL_INFO_MODEL => info_string(pi_type, p_buffer, pi_size, TL_MODEL),
        TL_INFO_VERSION => info_string(pi_type, p_buffer, pi_size, version),
        TL_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_GEV),
        TL_INFO_NAME => info_string(pi_type, p_buffer, pi_size, TL_FILENAME),
        TL_INFO_PATHNAME => match module_path() {
            Some(path) => info_string(pi_type, p_buffer, pi_size, &path),
            None => fail(GentlError::NotAvailable),
        },
        TL_INFO_DISPLAYNAME => info_string(pi_type, p_buffer, pi_size, TL_DISPLAYNAME),
        TL_INFO_CHAR_ENCODING => {
            let v: i32 = 0; // TL_CHAR_ENCODING_ASCII
            info_value(pi_type, INFO_DATATYPE_INT32, p_buffer, pi_size, &v)
        }
        TL_INFO_GENTL_VER_MAJOR => {
            let v: u32 = 1;
            info_value(pi_type, INFO_DATATYPE_UINT32, p_buffer, pi_size, &v)
        }
        TL_INFO_GENTL_VER_MINOR => {
            let v: u32 = 6;
            info_value(pi_type, INFO_DATATYPE_UINT32, p_buffer, pi_size, &v)
        }
        _ => not_implemented("TLGetInfo cmd"),
    }
}

/// Legacy GenTL 1.0 entry point: inquire TL information without a System
/// instance (§6.3.1.2). `GCInitLib` must have been called first
/// (`GC_ERR_NOT_INITIALIZED` otherwise), and the reported values must match
/// `TLGetInfo` (§6.4.1.3 keeps the two synchronized).
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCGetInfo(
    i_info_cmd: TlInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    if let Err(e) = require_initialized() {
        return e;
    }
    let e = check_info_args(pi_type, pi_size);
    if e != GC_ERR_SUCCESS {
        return e;
    }
    unsafe { tl_info(i_info_cmd, pi_type, p_buffer, pi_size) }
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn TLGetInfo(
    h_tl: TLHandle,
    i_info_cmd: TlInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    let e = check_info_args(pi_type, pi_size);
    if e != GC_ERR_SUCCESS {
        return e;
    }
    let _system = module!(h_tl, TlSystem);
    unsafe { tl_info(i_info_cmd, pi_type, p_buffer, pi_size) }
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn TLGetNumInterfaces(h_tl: TLHandle, pi_num_ifaces: *mut u32) -> GCError {
    guard(|| {
    if pi_num_ifaces.is_null() {
        return fail(GentlError::InvalidParameter("piNumIfaces"));
    }
    let system = module!(h_tl, TlSystem);
    let n = match system.interfaces.lock() {
        Ok(list) => list.len() as u32,
        Err(e) => return fail(GentlError::Io(e.to_string())),
    };
    unsafe { *pi_num_ifaces = n };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn TLGetInterfaceID(
    h_tl: TLHandle,
    i_index: u32,
    s_id: *mut c_char,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    if pi_size.is_null() {
        return fail(GentlError::InvalidParameter("piSize"));
    }
    let system = module!(h_tl, TlSystem);
    let list = match system.interfaces.lock() {
        Ok(l) => l,
        Err(e) => return fail(GentlError::Io(e.to_string())),
    };
    let Some(iface) = list.get(i_index as usize) else {
        return invalid_index();
    };
    let _ = iface;
    unsafe { write_string(s_id, pi_size, GevInterface::ID) }
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn TLGetInterfaceInfo(
    h_tl: TLHandle,
    s_iface_id: *const c_char,
    i_info_cmd: InterfaceInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    let e = check_info_args(pi_type, pi_size);
    if e != GC_ERR_SUCCESS {
        return e;
    }
    let Some(id) = (unsafe { c_str(s_iface_id) }) else {
        return fail(GentlError::InvalidParameter("sIfaceID"));
    };
    let system = module!(h_tl, TlSystem);
    let list = match system.interfaces.lock() {
        Ok(l) => l,
        Err(e) => return fail(GentlError::Io(e.to_string())),
    };
    let Some(_iface) = list.iter().find(|_| GevInterface::ID == id) else {
        return fail(GentlError::InvalidId);
    };
    unsafe {
        match i_info_cmd {
            INTERFACE_INFO_ID => info_string(pi_type, p_buffer, pi_size, GevInterface::ID),
            INTERFACE_INFO_DISPLAYNAME => {
                info_string(pi_type, p_buffer, pi_size, GevInterface::DISPLAY_NAME)
            }
            INTERFACE_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_GEV),
            _ => not_implemented("TLGetInterfaceInfo cmd"),
        }
    }
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn TLOpenInterface(
    h_tl: TLHandle,
    s_iface_id: *const c_char,
    ph_iface: *mut IFHandle,
) -> GCError {
    guard(|| {
    if ph_iface.is_null() {
        return fail(GentlError::InvalidParameter("phIface"));
    }
    unsafe { *ph_iface = ptr::null_mut() };
    let Some(id) = (unsafe { c_str(s_iface_id) }) else {
        return fail(GentlError::InvalidParameter("sIfaceID"));
    };
    let system = module!(h_tl, TlSystem);
    let template = {
        let list = match system.interfaces.lock() {
            Ok(l) => l,
            Err(e) => return fail(GentlError::Io(e.to_string())),
        };
        list.iter().find(|_| GevInterface::ID == id).cloned()
    };
    let Some(template) = template else {
        return fail(GentlError::InvalidId);
    };
    {
        let mut opened = match system.opened_interfaces.lock() {
            Ok(o) => o,
            Err(e) => return fail(GentlError::Io(e.to_string())),
        };
        if opened.contains(id) {
            return fail(GentlError::ResourceInUse);
        }
        opened.insert(id.to_owned());
    }
    let instance = template.instantiate();
    unsafe { *ph_iface = to_handle(instance) };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn TLUpdateInterfaceList(
    h_tl: TLHandle,
    pb_changed: *mut u8,
    _i_timeout: u64,
) -> GCError {
    guard(|| {
    let system = module!(h_tl, TlSystem);
    // The aggregate interface itself never appears/disappears; what can
    // change is the underlying NIC set, which determines discovery reach.
    let new_nics = discovery::list_nics();
    let old_nics = match system.nics.lock() {
        Ok(n) => n.clone(),
        Err(e) => return fail(GentlError::Io(e.to_string())),
    };
    let changed = new_nics != old_nics;
    if changed {
        match system.nics.lock() {
            Ok(mut n) => *n = new_nics,
            Err(e) => return fail(GentlError::Io(e.to_string())),
        }
    }
    if !pb_changed.is_null() {
        unsafe { *pb_changed = u8::from(changed) };
    }
    GC_ERR_SUCCESS
    })
}

// Interface functions ---------------------------------------------------------

fn iface_from(h_iface: IFHandle) -> Result<Arc<GevInterface>, GCError> {
    let iface = unsafe { from_handle::<GevInterface>(h_iface) }.map_err(fail)?;
    parent_alive(&iface.parent)?;
    Ok(iface)
}

fn lock<T>(m: &std::sync::Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, GCError> {
    m.lock().map_err(|e| fail(GentlError::Io(e.to_string())))
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn IFClose(h_iface: IFHandle) -> GCError {
    guard(|| {
    let iface = module!(h_iface, GevInterface);
    if let Some(tl) = iface.parent.upgrade() {
        if let Ok(mut opened) = tl.opened_interfaces.lock() {
            opened.remove(GevInterface::ID);
        }
    }
    drop(iface);
    unsafe { free_handle(h_iface) };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn IFGetInfo(
    h_iface: IFHandle,
    i_info_cmd: InterfaceInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    let e = check_info_args(pi_type, pi_size);
    if e != GC_ERR_SUCCESS {
        return e;
    }
    // `iface_from` validates the handle; the aggregate answers are
    // constants, no per-instance data is read.
    let _iface = match iface_from(h_iface) {
        Ok(i) => i,
        Err(e) => return e,
    };
    unsafe {
        match i_info_cmd {
            INTERFACE_INFO_ID => info_string(pi_type, p_buffer, pi_size, GevInterface::ID),
            INTERFACE_INFO_DISPLAYNAME => {
                info_string(pi_type, p_buffer, pi_size, GevInterface::DISPLAY_NAME)
            }
            INTERFACE_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_GEV),
            _ => not_implemented("IFGetInfo cmd"),
        }
    }
    })
}

/// Discovery timeout honoring the caller's budget; GENTL_INFINITE is
/// capped because a producer must finish "in a reasonable amount of time".
fn update_timeout(i_timeout: u64) -> Duration {
    match i_timeout {
        GENTL_INFINITE => Duration::from_millis(200),
        ms => Duration::from_millis(ms.clamp(1, 10_000)),
    }
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn IFUpdateDeviceList(
    h_iface: IFHandle,
    pb_changed: *mut u8,
    i_timeout: u64,
) -> GCError {
    guard(|| {
    let iface = match iface_from(h_iface) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let nics = match iface.parent.upgrade() {
        Some(tl) => match tl.nics.lock() {
            Ok(n) => n.clone(),
            Err(e) => return fail(GentlError::Io(e.to_string())),
        },
        None => Vec::new(),
    };
    let found = match discovery::discover_all(&nics, update_timeout(i_timeout)) {
        Ok(f) => f,
        Err(e) => return fail(e),
    };
    let mut devices = match lock(&iface.devices) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let old_ids: Vec<String> = devices.iter().map(|d| d.id.clone()).collect();
    let new_entries: Vec<DeviceEntry> = found
        .into_iter()
        .map(|d| DeviceEntry {
            id: discovery::device_id(&d.mac),
            discovery: d,
        })
        .collect();
    let changed = old_ids != new_entries.iter().map(|d| d.id.clone()).collect::<Vec<_>>();
    *devices = new_entries;
    drop(devices);
    if !pb_changed.is_null() {
        unsafe { *pb_changed = u8::from(changed) };
    }
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn IFGetNumDevices(h_iface: IFHandle, pi_num_devices: *mut u32) -> GCError {
    guard(|| {
    if pi_num_devices.is_null() {
        return fail(GentlError::InvalidParameter("piNumDevices"));
    }
    let iface = match iface_from(h_iface) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let n = match lock(&iface.devices) {
        Ok(d) => d.len() as u32,
        Err(e) => return e,
    };
    unsafe { *pi_num_devices = n };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn IFGetDeviceID(
    h_iface: IFHandle,
    i_index: u32,
    s_device_id: *mut c_char,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    if pi_size.is_null() {
        return fail(GentlError::InvalidParameter("piSize"));
    }
    let iface = match iface_from(h_iface) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let devices = match lock(&iface.devices) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let Some(entry) = devices.get(i_index as usize) else {
        return invalid_index();
    };
    unsafe { write_string(s_device_id, pi_size, &entry.id) }
    })
}

fn find_device(iface: &GevInterface, id: &str) -> Result<Option<DeviceEntry>, GCError> {
    let devices = lock(&iface.devices)?;
    Ok(devices.iter().find(|d| d.id == id).map(|d| DeviceEntry {
        id: d.id.clone(),
        discovery: d.discovery.clone(),
    }))
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn IFGetDeviceInfo(
    h_iface: IFHandle,
    s_device_id: *const c_char,
    i_info_cmd: DeviceInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    let e = check_info_args(pi_type, pi_size);
    if e != GC_ERR_SUCCESS {
        return e;
    }
    let Some(id) = (unsafe { c_str(s_device_id) }) else {
        return fail(GentlError::InvalidParameter("sDeviceID"));
    };
    let iface = match iface_from(h_iface) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let entry = match find_device(&iface, id) {
        Ok(e) => e,
        Err(e) => return e,
    };
    let Some(entry) = entry else {
        return fail(GentlError::InvalidId);
    };
    let d = &entry.discovery;
    unsafe {
        match i_info_cmd {
            DEVICE_INFO_ID => info_string(pi_type, p_buffer, pi_size, &entry.id),
            DEVICE_INFO_VENDOR => info_string(pi_type, p_buffer, pi_size, &d.manufacturer_name),
            DEVICE_INFO_MODEL => info_string(pi_type, p_buffer, pi_size, &d.model_name),
            DEVICE_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_GEV),
            DEVICE_INFO_DISPLAYNAME => info_string(
                pi_type,
                p_buffer,
                pi_size,
                &display_name(&d.manufacturer_name, &d.model_name, &entry.id),
            ),
            DEVICE_INFO_ACCESS_STATUS => {
                let opened = match lock(&iface.opened_devices) {
                    Ok(o) => o.contains(id),
                    Err(e) => return e,
                };
                let v = if opened {
                    DEVICE_ACCESS_STATUS_OPEN_READWRITE
                } else {
                    DEVICE_ACCESS_STATUS_READWRITE
                };
                info_value(pi_type, INFO_DATATYPE_INT32, p_buffer, pi_size, &v)
            }
            DEVICE_INFO_USER_DEFINED_NAME => {
                info_string(pi_type, p_buffer, pi_size, &d.user_defined_name)
            }
            DEVICE_INFO_SERIAL_NUMBER => info_string(pi_type, p_buffer, pi_size, &d.serial_number),
            DEVICE_INFO_VERSION => info_string(pi_type, p_buffer, pi_size, &d.device_version),
            DEVICE_INFO_TIMESTAMP_FREQUENCY => fail(GentlError::NotAvailable),
            _ => not_implemented("IFGetDeviceInfo cmd"),
        }
    }
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn IFOpenDevice(
    h_iface: IFHandle,
    s_device_id: *const c_char,
    i_open_flag: DeviceAccessFlags,
    ph_device: *mut DevHandle,
) -> GCError {
    guard(|| {
    if ph_device.is_null() {
        return fail(GentlError::InvalidParameter("phDevice"));
    }
    unsafe { *ph_device = ptr::null_mut() };
    let Some(id) = (unsafe { c_str(s_device_id) }) else {
        return fail(GentlError::InvalidParameter("sDeviceID"));
    };
    let access = match i_open_flag {
        2 => DeviceAccess::Monitor,
        3 => DeviceAccess::Control,
        4 => DeviceAccess::Exclusive,
        _ => return fail(GentlError::InvalidParameter("iOpenFlag")),
    };
    let iface = match iface_from(h_iface) {
        Ok(i) => i,
        Err(e) => return e,
    };

    let mut entry = match find_device(&iface, id) {
        Ok(e) => e,
        Err(e) => return e,
    };
    if entry.is_none() {
        // §6.2/§6.3.3: opening by a known ID must be possible without a
        // prior IFUpdateDeviceList; run the discovery ourselves.
        let nics = match iface.parent.upgrade() {
            Some(tl) => tl.nics.lock().map(|n| n.clone()).unwrap_or_default(),
            None => Vec::new(),
        };
        if let Ok(found) = discovery::discover_all(&nics, Duration::from_millis(200)) {
            if let Ok(mut devices) = iface.devices.lock() {
                *devices = found
                    .into_iter()
                    .map(|d| DeviceEntry {
                        id: discovery::device_id(&d.mac),
                        discovery: d,
                    })
                    .collect();
            }
        }
        entry = match find_device(&iface, id) {
            Ok(e) => e,
            Err(e) => return e,
        };
    }
    let Some(entry) = entry else {
        return fail(GentlError::InvalidId);
    };

    {
        let opened = match lock(&iface.opened_devices) {
            Ok(o) => o,
            Err(e) => return e,
        };
        if opened.contains(id) {
            return fail(GentlError::ResourceInUse);
        }
    }
    let device = match GevDevice::open(&iface, &entry, access) {
        Ok(d) => d,
        Err(e) => return fail(e),
    };
    match lock(&iface.opened_devices) {
        Ok(mut o) => o.insert(id.to_owned()),
        Err(e) => return e,
    };
    unsafe { *ph_device = to_handle(device) };
    GC_ERR_SUCCESS
    })
}

// Device functions ------------------------------------------------------------

fn device_from(h_device: DevHandle) -> Result<Arc<GevDevice>, GCError> {
    let device = unsafe { from_handle::<GevDevice>(h_device) }.map_err(fail)?;
    parent_alive(&device.parent)?;
    Ok(device)
}

fn device_info(
    device: &GevDevice,
    i_info_cmd: DeviceInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    let d = &device.discovery;
    unsafe {
        match i_info_cmd {
            DEVICE_INFO_ID => info_string(pi_type, p_buffer, pi_size, &device.id),
            DEVICE_INFO_VENDOR => info_string(pi_type, p_buffer, pi_size, &d.manufacturer_name),
            DEVICE_INFO_MODEL => info_string(pi_type, p_buffer, pi_size, &d.model_name),
            DEVICE_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_GEV),
            DEVICE_INFO_DISPLAYNAME => info_string(
                pi_type,
                p_buffer,
                pi_size,
                &display_name(&d.manufacturer_name, &d.model_name, &device.id),
            ),
            DEVICE_INFO_ACCESS_STATUS => {
                let v = if device.access == DeviceAccess::Monitor {
                    DEVICE_ACCESS_STATUS_OPEN_READONLY
                } else {
                    DEVICE_ACCESS_STATUS_OPEN_READWRITE
                };
                info_value(pi_type, INFO_DATATYPE_INT32, p_buffer, pi_size, &v)
            }
            DEVICE_INFO_USER_DEFINED_NAME => {
                info_string(pi_type, p_buffer, pi_size, &d.user_defined_name)
            }
            DEVICE_INFO_SERIAL_NUMBER => info_string(pi_type, p_buffer, pi_size, &d.serial_number),
            DEVICE_INFO_VERSION => info_string(pi_type, p_buffer, pi_size, &d.device_version),
            DEVICE_INFO_TIMESTAMP_FREQUENCY => {
                let hi = match device.read_reg(u64::from(registers::TIMESTAMP_TICK_FREQ_HIGH)) {
                    Ok(v) => u64::from(v),
                    Err(e) => return fail(e),
                };
                let lo = match device.read_reg(u64::from(registers::TIMESTAMP_TICK_FREQ_LOW)) {
                    Ok(v) => u64::from(v),
                    Err(e) => return fail(e),
                };
                let v = (hi << 32) | lo;
                info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
            }
            _ => not_implemented("DevGetInfo cmd"),
        }
    }
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DevGetPort(h_device: DevHandle, ph_remote_device: *mut PortHandle) -> GCError {
    guard(|| {
    if ph_remote_device.is_null() {
        return fail(GentlError::InvalidParameter("phRemoteDevice"));
    }
    unsafe { *ph_remote_device = ptr::null_mut() };
    let device = match device_from(h_device) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let port = DevicePort {
        device: Arc::downgrade(&device),
    };
    unsafe { *ph_remote_device = to_handle(Arc::new(port)) };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DevGetInfo(
    h_device: DevHandle,
    i_info_cmd: DeviceInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    let e = check_info_args(pi_type, pi_size);
    if e != GC_ERR_SUCCESS {
        return e;
    }
    let device = match device_from(h_device) {
        Ok(d) => d,
        Err(e) => return e,
    };
    device_info(&device, i_info_cmd, pi_type, p_buffer, pi_size)
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DevClose(h_device: DevHandle) -> GCError {
    guard(|| {
    let device = module!(h_device, GevDevice);
    device.close();
    if let Ok(mut opened) = device.opened_streams.lock() {
        opened.clear();
    }
    if let Some(iface) = device.parent.upgrade() {
        if let Ok(mut opened) = iface.opened_devices.lock() {
            opened.remove(&device.id);
        }
    }
    drop(device);
    unsafe { free_handle(h_device) };
    GC_ERR_SUCCESS
    })
}

// Data stream helpers ---------------------------------------------------------

fn ds_from(h_ds: DSHandle) -> Result<Arc<GevDataStream>, GCError> {
    let ds = unsafe { from_handle::<GevDataStream>(h_ds) }.map_err(fail)?;
    parent_alive(&ds.parent)?;
    Ok(ds)
}

/// A buffer handle is only valid on the stream it was announced to.
fn ds_buffer_from(
    ds: &Arc<GevDataStream>,
    h_buffer: BufferHandle,
) -> Result<Arc<GevBuffer>, GCError> {
    let buf = unsafe { from_handle::<GevBuffer>(h_buffer) }.map_err(fail)?;
    match buf.stream.upgrade() {
        Some(owner) if Arc::ptr_eq(&owner, ds) => Ok(buf),
        _ => Err(fail(GentlError::InvalidHandle)),
    }
}

/// GC_ERR_ABORT has no GentlError variant; set it directly.
fn aborted() -> GCError {
    set_error(GC_ERR_ABORT, "event wait aborted");
    GC_ERR_ABORT
}

// Data stream functions (GenTL 1.6 §6.3.4.5, §6.3.5) -------------------------

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DevGetNumDataStreams(
    h_device: DevHandle,
    pi_num_data_streams: *mut u32,
) -> GCError {
    guard(|| {
    if pi_num_data_streams.is_null() {
        return fail(GentlError::InvalidParameter("piNumDataStreams"));
    }
    let device = match device_from(h_device) {
        Ok(d) => d,
        Err(e) => return e,
    };
    // The stream channel count lives in the device bootstrap (0x0904);
    // the read doubles as a liveness check — a device that cannot answer
    // it would fail on the first real transaction anyway.
    if let Err(e) = device.read_reg(u64::from(registers::NUM_STREAM_CHANNELS)) {
        return fail(e);
    }
    // The M3-4a design exposes a single GenTL DataStream ("Stream0") that
    // fans out to the device's GVSP channels internally, so the module
    // count stays 1 regardless of how many stream channels the device
    // implements.
    unsafe { *pi_num_data_streams = 1 };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DevGetDataStreamID(
    h_device: DevHandle,
    i_index: u32,
    s_data_stream_id: *mut c_char,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    if pi_size.is_null() {
        return fail(GentlError::InvalidParameter("piSize"));
    }
    let device = match device_from(h_device) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let _ = device;
    if i_index != 0 {
        return invalid_index();
    }
    unsafe { write_string(s_data_stream_id, pi_size, STREAM_ID) }
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DevOpenDataStream(
    h_device: DevHandle,
    s_data_stream_id: *const c_char,
    ph_data_stream: *mut DSHandle,
) -> GCError {
    guard(|| {
    if ph_data_stream.is_null() {
        return fail(GentlError::InvalidParameter("phDataStream"));
    }
    unsafe { *ph_data_stream = ptr::null_mut() };
    let Some(id) = (unsafe { c_str(s_data_stream_id) }) else {
        return fail(GentlError::InvalidParameter("sDataStreamID"));
    };
    let device = match device_from(h_device) {
        Ok(d) => d,
        Err(e) => return e,
    };
    if id != STREAM_ID {
        return fail(GentlError::InvalidId);
    }
    match device.opened_streams.lock() {
        Ok(opened) if opened.contains(STREAM_ID) => {
            return fail(GentlError::ResourceInUse);
        }
        Ok(_) => {}
        Err(e) => return fail(GentlError::Io(e.to_string())),
    }
    let stream = match GevDataStream::open(&device) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    if let Ok(mut opened) = device.opened_streams.lock() {
        opened.insert(STREAM_ID.to_owned());
    }
    unsafe { *ph_data_stream = to_handle(stream) };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSClose(h_ds: DSHandle) -> GCError {
    guard(|| {
    let ds = module!(h_ds, GevDataStream);
    ds.close();
    if let Some(device) = ds.parent.upgrade() {
        if let Ok(mut opened) = device.opened_streams.lock() {
            opened.remove(STREAM_ID);
        }
    }
    drop(ds);
    unsafe { free_handle(h_ds) };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSGetInfo(
    h_ds: DSHandle,
    i_info_cmd: StreamInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    let e = check_info_args(pi_type, pi_size);
    if e != GC_ERR_SUCCESS {
        return e;
    }
    let ds = match ds_from(h_ds) {
        Ok(d) => d,
        Err(e) => return e,
    };
    unsafe {
        match i_info_cmd {
            STREAM_INFO_ID => info_string(pi_type, p_buffer, pi_size, STREAM_ID),
            STREAM_INFO_NUM_DELIVERED => {
                let v = ds.core.num_delivered();
                info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
            }
            STREAM_INFO_NUM_UNDERRUN => {
                let v = ds.core.num_underrun();
                info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
            }
            STREAM_INFO_NUM_ANNOUNCED => {
                let v = ds.core.announced_count();
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            STREAM_INFO_NUM_QUEUED => {
                let v = ds.core.queued_count();
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            STREAM_INFO_NUM_AWAIT_DELIVERY => {
                let v = ds.core.await_delivery_count();
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            STREAM_INFO_NUM_STARTED => {
                let v = ds.core.num_started_buffers();
                info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
            }
            STREAM_INFO_PAYLOAD_SIZE => {
                // PayloadSize follows Width/Height/PixelFormat; before the
                // stream starts, refresh from the device register so a
                // changed ROI is visible without reopening the stream.
                if !ds.core.is_grabbing() {
                    if let Some(device) = ds.parent.upgrade() {
                        if let Some(size) = device.payload_size() {
                            ds.core.set_expected_payload_size(size);
                        }
                    }
                }
                let v = ds.core.expected_payload_size();
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            STREAM_INFO_IS_GRABBING => {
                let v: u8 = ds.core.is_grabbing().into();
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
            }
            STREAM_INFO_DEFINES_PAYLOADSIZE => {
                // OPEN-QUESTION Q17: for GEV the payload size is defined by
                // the remote device, not the data stream.
                let v: u8 = 0;
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
            }
            STREAM_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_GEV),
            STREAM_INFO_NUM_CHUNKS_MAX => {
                let v: usize = 0;
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            STREAM_INFO_BUF_ANNOUNCE_MIN => {
                let v: usize = 1;
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            STREAM_INFO_BUF_ALIGNMENT => {
                let v: usize = 1;
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            _ => not_implemented("DSGetInfo cmd"),
        }
    }
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSAnnounceBuffer(
    h_ds: DSHandle,
    p_buffer: *mut core::ffi::c_void,
    i_size: usize,
    p_private: *mut core::ffi::c_void,
    ph_buffer: *mut BufferHandle,
) -> GCError {
    guard(|| {
    if ph_buffer.is_null() {
        return fail(GentlError::InvalidParameter("phBuffer"));
    }
    unsafe { *ph_buffer = ptr::null_mut() };
    if p_buffer.is_null() {
        return fail(GentlError::InvalidParameter("pBuffer"));
    }
    let ds = match ds_from(h_ds) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let id = match ds.core.announce_external(p_buffer, i_size, p_private) {
        Ok(id) => id,
        Err(e) => return fail(e),
    };
    let module = Arc::new(GevBuffer {
        stream: Arc::downgrade(&ds),
        id,
    });
    let handle = to_handle(module);
    ds.set_buffer_handle(id, handle);
    unsafe { *ph_buffer = handle };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSAllocAndAnnounceBuffer(
    h_ds: DSHandle,
    i_size: usize,
    p_private: *mut core::ffi::c_void,
    ph_buffer: *mut BufferHandle,
) -> GCError {
    guard(|| {
    if ph_buffer.is_null() {
        return fail(GentlError::InvalidParameter("phBuffer"));
    }
    unsafe { *ph_buffer = ptr::null_mut() };
    let ds = match ds_from(h_ds) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let id = match ds.core.announce_owned(i_size, p_private) {
        Ok(id) => id,
        Err(e) => return fail(e),
    };
    let module = Arc::new(GevBuffer {
        stream: Arc::downgrade(&ds),
        id,
    });
    let handle = to_handle(module);
    ds.set_buffer_handle(id, handle);
    unsafe { *ph_buffer = handle };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSQueueBuffer(h_ds: DSHandle, h_buffer: BufferHandle) -> GCError {
    guard(|| {
    let ds = match ds_from(h_ds) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let buf = match ds_buffer_from(&ds, h_buffer) {
        Ok(b) => b,
        Err(e) => return e,
    };
    match ds.core.queue_buffer(buf.id) {
        Ok(()) => GC_ERR_SUCCESS,
        Err(e) => fail(e),
    }
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSRevokeBuffer(
    h_ds: DSHandle,
    h_buffer: BufferHandle,
    p_buffer: *mut *mut core::ffi::c_void,
    p_private: *mut *mut core::ffi::c_void,
) -> GCError {
    guard(|| {
    let ds = match ds_from(h_ds) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let buf = match ds_buffer_from(&ds, h_buffer) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let id = buf.id;
    drop(buf);
    let revoked = match ds.core.revoke(id) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    if let Some(handle) = ds.buffer_handle(id) {
        ds.remove_buffer_handle(id);
        unsafe { free_handle(handle) };
    }
    if !p_buffer.is_null() {
        unsafe {
            *p_buffer = revoked
                .base
                .map(|b| b as *mut core::ffi::c_void)
                .unwrap_or(ptr::null_mut());
        }
    }
    if !p_private.is_null() {
        unsafe { *p_private = revoked.user_ptr as *mut core::ffi::c_void };
    }
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSFlushQueue(h_ds: DSHandle, i_operation: AcqQueueType) -> GCError {
    guard(|| {
    let ds = match ds_from(h_ds) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let op = match i_operation {
        0 => ds_core::AcqQueueOp::InputToOutput,
        1 => ds_core::AcqQueueOp::OutputDiscard,
        2 => ds_core::AcqQueueOp::AllToInput,
        3 => ds_core::AcqQueueOp::UnqueuedToInput,
        4 => ds_core::AcqQueueOp::AllDiscard,
        _ => return not_implemented("DSFlushQueue operation"),
    };
    match ds.core.flush(op) {
        Ok(()) => GC_ERR_SUCCESS,
        Err(e) => fail(e),
    }
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSStartAcquisition(
    h_ds: DSHandle,
    i_start_flags: AcqStartFlags,
    i_num_to_acquire: u64,
) -> GCError {
    guard(|| {
    if i_start_flags != 0 {
        return not_implemented("DSStartAcquisition flags");
    }
    let ds = match ds_from(h_ds) {
        Ok(d) => d,
        Err(e) => return e,
    };
    if let Err(e) = ds.core.start_acquisition(i_num_to_acquire) {
        return fail(e);
    }
    if let Err(e) = ds.start() {
        // Roll the engine back; transport start cleans up partial channels.
        let _ = ds.core.stop_acquisition();
        return fail(e);
    }
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSStopAcquisition(h_ds: DSHandle, i_stop_flags: AcqStopFlags) -> GCError {
    guard(|| {
    let kill = match i_stop_flags {
        0 => false,
        1 => true, // ACQ_STOP_FLAGS_KILL
        _ => return not_implemented("DSStopAcquisition flags"),
    };
    let ds = match ds_from(h_ds) {
        Ok(d) => d,
        Err(e) => return e,
    };
    if let Err(e) = ds.core.stop_acquisition() {
        return fail(e);
    }
    ds.stop(kill);
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSGetBufferID(
    h_ds: DSHandle,
    i_index: u32,
    ph_buffer: *mut BufferHandle,
) -> GCError {
    guard(|| {
    if ph_buffer.is_null() {
        return fail(GentlError::InvalidParameter("phBuffer"));
    }
    unsafe { *ph_buffer = ptr::null_mut() };
    let ds = match ds_from(h_ds) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let Some(id) = ds.core.buffer_id_at(i_index as usize) else {
        return invalid_index();
    };
    let Some(handle) = ds.buffer_handle(id) else {
        return fail(GentlError::InvalidHandle);
    };
    unsafe { *ph_buffer = handle };
    GC_ERR_SUCCESS
    })
}

/// One buffer's engine state, copied out of the pool for the info match.
struct BufferSnapshot {
    data_size: usize,
    base: *mut core::ffi::c_void,
    user_ptr: *mut core::ffi::c_void,
    size: usize,
    state: ds_core::BufferState,
    new_data: bool,
    filled_size: usize,
    incomplete: bool,
    meta: Option<ds_core::FrameMeta>,
    block_id: u64,
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSGetBufferInfo(
    h_ds: DSHandle,
    h_buffer: BufferHandle,
    i_info_cmd: BufferInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    let e = check_info_args(pi_type, pi_size);
    if e != GC_ERR_SUCCESS {
        return e;
    }
    let ds = match ds_from(h_ds) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let buf = match ds_buffer_from(&ds, h_buffer) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let snap = match ds.core.with_buffer(buf.id, |b| BufferSnapshot {
        base: b.base_ptr(),
        user_ptr: b.user_ptr(),
        size: b.size(),
        state: b.state,
        new_data: b.new_data,
        filled_size: b.filled_size,
        data_size: b.data_size,
        incomplete: b.incomplete,
        meta: b.meta,
        block_id: b.block_id,
    }) {
        Some(s) => s,
        None => return fail(GentlError::InvalidHandle),
    };
    // Meta-dependent infos on a never-filled buffer.
    let need_meta = |snap: &BufferSnapshot| snap.meta.ok_or(GentlError::NotAvailable);
    unsafe {
        match i_info_cmd {
            BUFFER_INFO_BASE => {
                info_value(pi_type, INFO_DATATYPE_PTR, p_buffer, pi_size, &snap.base)
            }
            BUFFER_INFO_SIZE => {
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &snap.size)
            }
            BUFFER_INFO_USER_PTR => info_value(
                pi_type,
                INFO_DATATYPE_PTR,
                p_buffer,
                pi_size,
                &snap.user_ptr,
            ),
            BUFFER_INFO_TIMESTAMP => match need_meta(&snap) {
                Ok(m) => {
                    let v = m.timestamp;
                    info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
                }
                Err(e) => fail(e),
            },
            BUFFER_INFO_NEW_DATA => {
                let v: u8 = snap.new_data.into();
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_IS_QUEUED => {
                let v: u8 = snap.state.is_queued_info().into();
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_IS_ACQUIRING => {
                let v: u8 = (snap.state == ds_core::BufferState::Filling).into();
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_IS_INCOMPLETE => {
                let v: u8 = snap.incomplete.into();
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_GEV),
            BUFFER_INFO_SIZE_FILLED => info_value(
                pi_type,
                INFO_DATATYPE_SIZET,
                p_buffer,
                pi_size,
                &snap.filled_size,
            ),
            BUFFER_INFO_WIDTH => match need_meta(&snap) {
                Ok(m) => {
                    let v = m.width as usize;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                Err(e) => fail(e),
            },
            BUFFER_INFO_HEIGHT => match need_meta(&snap) {
                Ok(m) => {
                    let v = m.height as usize;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                Err(e) => fail(e),
            },
            BUFFER_INFO_XOFFSET => match need_meta(&snap) {
                Ok(m) => {
                    let v = m.offset_x as usize;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                Err(e) => fail(e),
            },
            BUFFER_INFO_YOFFSET => match need_meta(&snap) {
                Ok(m) => {
                    let v = m.offset_y as usize;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                Err(e) => fail(e),
            },
            BUFFER_INFO_XPADDING => match need_meta(&snap) {
                Ok(m) => {
                    let v = m.padding_x as usize;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                Err(e) => fail(e),
            },
            BUFFER_INFO_YPADDING => match need_meta(&snap) {
                Ok(m) => {
                    let v = m.padding_y as usize;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                Err(e) => fail(e),
            },
            BUFFER_INFO_FRAMEID => {
                let v = snap.block_id;
                info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_IMAGEPRESENT => {
                let v: u8 = snap.meta.is_some().into();
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_IMAGEOFFSET => {
                let v: usize = 0;
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_PAYLOADTYPE => {
                let v = PAYLOAD_TYPE_IMAGE;
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_PIXELFORMAT => match need_meta(&snap) {
                Ok(m) => {
                    let v = u64::from(m.pixel_format);
                    info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
                }
                Err(e) => fail(e),
            },
            BUFFER_INFO_PIXELFORMAT_NAMESPACE => {
                let v = PIXELFORMAT_NAMESPACE_GEV;
                info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_DELIVERED_IMAGEHEIGHT => match need_meta(&snap) {
                Ok(m) => {
                    // Trailer rows are validated against the leader.
                    let v = m.height as usize;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                Err(e) => fail(e),
            },
            BUFFER_INFO_DELIVERED_CHUNKPAYLOADSIZE => {
                // GenTL 1.2: total bytes of all chunks in the buffer.
                let v: usize = snap.meta.and_then(|m| m.chunk).map_or(0, |c| c.len);
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_CHUNKLAYOUTID => {
                let v: u64 = snap.meta.and_then(|m| m.chunk).map_or(0, |c| c.layout_id);
                info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_FILENAME => info_string(pi_type, p_buffer, pi_size, ""),
            BUFFER_INFO_PIXEL_ENDIANNESS => {
                // GVSP image payloads carry little-endian pixel data.
                info_value(
                    pi_type,
                    INFO_DATATYPE_INT32,
                    p_buffer,
                    pi_size,
                    &PIXELENDIANNESS_LITTLE,
                )
            }
            BUFFER_INFO_DATA_SIZE => info_value(
                pi_type,
                INFO_DATATYPE_SIZET,
                p_buffer,
                pi_size,
                &snap.data_size,
            ),
            BUFFER_INFO_TIMESTAMP_NS => match (need_meta(&snap), ds.tick_freq) {
                (Ok(m), Some(freq)) if freq > 0 => {
                    let v = m.timestamp.saturating_mul(1_000_000_000) / freq;
                    info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
                }
                _ => fail(GentlError::NotAvailable),
            },
            BUFFER_INFO_DATA_LARGER_THAN_BUFFER => {
                let v: u8 = 0;
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
            }
            BUFFER_INFO_CONTAINS_CHUNKDATA => {
                let v: u8 = u8::from(snap.meta.and_then(|m| m.chunk).is_some());
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
            }
            _ => not_implemented("DSGetBufferInfo cmd"),
        }
    }
    })
}

/// `DSGetBufferChunkData` (GenTL 1.6 §6.3.5.13): report the chunk layout of
/// one delivered buffer by parsing the transport chunk tags. The receiver
/// records the chunk region at the extended-chunk trailer (§25.1); GigE
/// chunk tags are big-endian (§24.4 / CR-276s) and decoded from the end of
/// the block.
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSGetBufferChunkData(
    h_ds: DSHandle,
    h_buffer: BufferHandle,
    p_chunk_data: *mut SingleChunkData,
    pi_num_chunks: *mut usize,
) -> GCError {
    guard(|| {
        if pi_num_chunks.is_null() {
            return fail(GentlError::InvalidParameter("piNumChunks"));
        }
        let ds = match ds_from(h_ds) {
            Ok(d) => d,
            Err(e) => return e,
        };
        let buf = match ds_buffer_from(&ds, h_buffer) {
            Ok(b) => b,
            Err(e) => return e,
        };
        let snap = match ds.core.with_buffer(buf.id, |b| BufferSnapshot {
            base: b.base_ptr(),
            user_ptr: b.user_ptr(),
            size: b.size(),
            state: b.state,
            new_data: b.new_data,
            filled_size: b.filled_size,
            data_size: b.data_size,
            incomplete: b.incomplete,
            meta: b.meta,
            block_id: b.block_id,
        }) {
            Some(s) => s,
            None => return fail(GentlError::InvalidHandle),
        };
        let Some(meta) = snap.meta else {
            return fail(GentlError::NotAvailable);
        };
        let Some(chunk) = meta.chunk else {
            // The buffer carries no chunk data (§6.3.5.13 Returns).
            unsafe { *pi_num_chunks = 0 };
            return GC_ERR_NO_DATA;
        };
        // A truncated frame keeps its recorded region, but only the bytes
        // inside the announced buffer are parseable.
        let resident = chunk.len.min(snap.size.saturating_sub(chunk.offset));
        let chunks = unsafe {
            let base = (snap.base as *const u8).add(chunk.offset);
            let block = std::slice::from_raw_parts(base, resident);
            gentl_core::chunk::parse_chunks(block, true)
        };
        let Ok(chunks) = chunks else {
            // 搂6.3.5.13 Returns: a chunk buffer that does not parse maps to
            // GC_ERR_PARSING_CHUNK_DATA (not the generic I/O error).
            return fail(GentlError::ParsingChunkData("malformed GigE chunk tags"));
        };
        if p_chunk_data.is_null() {
            unsafe { *pi_num_chunks = chunks.len() };
            return GC_ERR_SUCCESS;
        }
        let capacity = unsafe { *pi_num_chunks };
        if capacity < chunks.len() {
            // 搂6.3.5.13 Returns: too few entries is GC_ERR_BUFFER_TOO_SMALL,
            // and *piNumChunks reports the number the consumer must provide.
            unsafe { *pi_num_chunks = chunks.len() };
            return GC_ERR_BUFFER_TOO_SMALL;
        }
        for (i, c) in chunks.iter().enumerate() {
            unsafe {
                let entry = &mut *p_chunk_data.add(i);
                entry.chunk_id = c.id;
                entry.chunk_offset = (chunk.offset + c.offset) as isize;
                entry.chunk_length = c.len;
            }
        }
        unsafe { *pi_num_chunks = chunks.len() };
        GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSGetParentDev(h_ds: DSHandle, ph_device: *mut DevHandle) -> GCError {
    guard(|| {
    if ph_device.is_null() {
        return fail(GentlError::InvalidParameter("phDevice"));
    }
    unsafe { *ph_device = ptr::null_mut() };
    let ds = module!(h_ds, GevDataStream);
    let Some(device) = ds.parent.upgrade() else {
        return fail(GentlError::InvalidHandle);
    };
    unsafe { *ph_device = to_handle(device) };
    GC_ERR_SUCCESS
    })
}

// Signaling functions (GenTL 1.6 §6.3.7) --------------------------------------

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCRegisterEvent(
    h_event_src: EventSrcHandle,
    i_event_id: EventType,
    ph_event: *mut EventHandle,
) -> GCError {
    guard(|| {
    if ph_event.is_null() {
        return fail(GentlError::InvalidParameter("phEvent"));
    }
    unsafe { *ph_event = ptr::null_mut() };
    if i_event_id == EVENT_REMOTE_DEVICE {
        // GenTL 1.6 §6.3.7.1: device-level GenApi events arrive over the
        // GigE Vision message channel (§12/§17).
        let device = match unsafe { from_handle::<GevDevice>(h_event_src) } {
            Ok(d) => d,
            Err(_) => return not_implemented("GCRegisterEvent: event source module"),
        };
        if let Err(e) = parent_alive(&device.parent) {
            return e;
        }
        let camera_ip = std::net::Ipv4Addr::from(device.discovery.current_ip);
        let module = match device.events.register(&device.client, camera_ip, i_event_id) {
            Ok(m) => m,
            Err(e) => return fail(e),
        };
        unsafe { *ph_event = to_handle(module) };
        return GC_ERR_SUCCESS;
    }
    // Only data stream modules carry events in this producer.
    let ds = match unsafe { from_handle::<GevDataStream>(h_event_src) } {
        Ok(d) => d,
        Err(_) => return not_implemented("GCRegisterEvent: event source module"),
    };
    if let Err(e) = parent_alive(&ds.parent) {
        return e;
    }
    if i_event_id != EVENT_NEW_BUFFER {
        // No other event type is implemented (§6.3.7.6: NOT_IMPLEMENTED
        // also covers event types not valid for the module).
        return not_implemented("event type");
    }
    let module = match ds.register_event(i_event_id) {
        Ok(m) => m,
        Err(e) => return fail(e),
    };
    unsafe { *ph_event = to_handle(module) };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCUnregisterEvent(h_event_src: EventSrcHandle, i_event_id: EventType) -> GCError {
    guard(|| {
    if i_event_id == EVENT_REMOTE_DEVICE {
        let device = match unsafe { from_handle::<GevDevice>(h_event_src) } {
            Ok(d) => d,
            Err(_) => return fail(GentlError::InvalidHandle),
        };
        return match device.events.unregister(&device.client, i_event_id) {
            Ok(()) => GC_ERR_SUCCESS,
            Err(e) => fail(e),
        };
    }
    let ds = match unsafe { from_handle::<GevDataStream>(h_event_src) } {
        Ok(d) => d,
        Err(_) => return not_implemented("GCUnregisterEvent: event source module"),
    };
    if i_event_id != EVENT_NEW_BUFFER {
        return not_implemented("event type");
    }
    match ds.unregister_event(i_event_id) {
        Ok(()) => GC_ERR_SUCCESS,
        Err(e) => fail(e),
    }
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn EventGetData(
    h_event: EventHandle,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
    i_timeout: u64,
) -> GCError {
    guard(|| {
    if pi_size.is_null() {
        return fail(GentlError::InvalidParameter("piSize"));
    }
    if let Ok(ev) = unsafe { from_handle::<DeviceEventModule>(h_event) } {
        if ev.event_type != EVENT_REMOTE_DEVICE {
            return not_implemented("EventGetData: event type");
        }
        let timeout = if i_timeout == GENTL_INFINITE {
            Duration::from_millis(u64::MAX)
        } else {
            Duration::from_millis(i_timeout)
        };
        let entry = match ev.queue.wait(timeout) {
            WaitResult::Entry(e) => e,
            WaitResult::Killed | WaitResult::Dead => return aborted(),
            WaitResult::Timeout => return fail(GentlError::Timeout),
        };
        if p_buffer.is_null() {
            // Size query: the event payload is discarded (GenTL 1.6 §6.3.7.2).
            unsafe { *pi_size = 0 };
            return GC_ERR_SUCCESS;
        }
        let needed = entry.payload.len();
        if unsafe { *pi_size } < needed {
            ev.queue.restore_front(entry);
            unsafe { *pi_size = needed };
            return GC_ERR_BUFFER_TOO_SMALL;
        }
        unsafe {
            ptr::copy_nonoverlapping(entry.payload.as_ptr(), p_buffer as *mut u8, needed);
            *pi_size = needed;
        }
        return GC_ERR_SUCCESS;
    }
    let ev = module!(h_event, EventModule);
    let ds = match ev.source.upgrade() {
        Some(d) => d,
        None => return fail(GentlError::InvalidHandle),
    };
    if ev.event_type != EVENT_NEW_BUFFER {
        return not_implemented("EventGetData: event type");
    }
    let timeout = if i_timeout == GENTL_INFINITE {
        Duration::from_millis(u64::MAX)
    } else {
        Duration::from_millis(i_timeout)
    };
    let entry = match ev.queue.wait(timeout) {
        WaitResult::Entry(e) => e,
        WaitResult::Killed | WaitResult::Dead => return aborted(),
        WaitResult::Timeout => return fail(GentlError::Timeout),
    };
    // EVENT_NEW_BUFFER carries EVENT_NEW_BUFFER_DATA (GenTL.h).
    let handle = match ds.buffer_handle(entry.id) {
        Some(h) => h,
        None => {
            ev.queue.restore_front(entry);
            return fail(GentlError::InvalidHandle);
        }
    };
    let data = EventNewBufferData {
        buffer_handle: handle,
        p_user_pointer: ds
            .core
            .with_buffer(entry.id, |b| b.user_ptr())
            .unwrap_or(ptr::null_mut()),
    };
    let needed = std::mem::size_of::<EventNewBufferData>();
    if p_buffer.is_null() {
        // NULL buffer: the data is removed from the queue without being
        // delivered; for New Buffer the buffer leaves the output queue and
        // is not requeued (§6.3.7.2).
        let _ = ds.core.mark_delivered(entry.id);
        unsafe { *pi_size = 0 };
        return GC_ERR_SUCCESS;
    }
    let avail = unsafe { *pi_size };
    if avail < needed {
        ev.queue.restore_front(entry);
        unsafe { *pi_size = needed };
        return GC_ERR_BUFFER_TOO_SMALL;
    }
    unsafe {
        ptr::copy_nonoverlapping(
            (&data as *const EventNewBufferData).cast::<u8>(),
            p_buffer as *mut u8,
            needed,
        );
        *pi_size = needed;
    }
    // Retrieval removes the buffer from the output queue (§5.1.3).
    let _ = ds.core.mark_delivered(entry.id);
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn EventGetInfo(
    h_event: EventHandle,
    i_info_cmd: EventInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    let e = check_info_args(pi_type, pi_size);
    if e != GC_ERR_SUCCESS {
        return e;
    }
    if let Ok(ev) = unsafe { from_handle::<DeviceEventModule>(h_event) } {
        // Device events carry a 24-byte wire record plus EVENTDATA payload;
        // 576 bytes is the §17.1/§17.2 packet limit.
        unsafe {
            return match i_info_cmd {
                EVENT_EVENT_TYPE => {
                    let v: i32 = ev.event_type;
                    info_value(pi_type, INFO_DATATYPE_INT32, p_buffer, pi_size, &v)
                }
                EVENT_SIZE_MAX => {
                    let v: usize = 576;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                _ => fail(GentlError::NotAvailable),
            };
        }
    }
    let ev = module!(h_event, EventModule);
    let data_size = std::mem::size_of::<EventNewBufferData>();
    unsafe {
        match i_info_cmd {
            EVENT_EVENT_TYPE => {
                let v: i32 = ev.event_type;
                info_value(pi_type, INFO_DATATYPE_INT32, p_buffer, pi_size, &v)
            }
            EVENT_NUM_IN_QUEUE => {
                let v = ev.queue.len();
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            EVENT_NUM_FIRED => {
                let v = ev.queue.fired();
                info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
            }
            EVENT_SIZE_MAX => {
                let v = data_size;
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            EVENT_INFO_DATA_SIZE_MAX => {
                let v = data_size;
                info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
            }
            _ => not_implemented("EventGetInfo cmd"),
        }
    }
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn EventFlush(h_event: EventHandle) -> GCError {
    guard(|| {
    // Device events (EVENT_REMOTE_DEVICE) have their own module type; the
    // handle is still a valid event object for flush/kill (搂6.3.7.4/6.3.7.5).
    if let Ok(ev) = unsafe { from_handle::<DeviceEventModule>(h_event) } {
        ev.queue.flush();
        return GC_ERR_SUCCESS;
    }
    let ev = module!(h_event, EventModule);
    // Flushing the New Buffer event discards the output buffer queue, too.
    if let Some(ds) = ev.source.upgrade() {
        if ev.event_type == EVENT_NEW_BUFFER {
            ds.core.discard_output();
            return GC_ERR_SUCCESS;
        }
    }
    ev.queue.flush();
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn EventKill(h_event: EventHandle) -> GCError {
    guard(|| {
    if let Ok(ev) = unsafe { from_handle::<DeviceEventModule>(h_event) } {
        ev.queue.kill_one();
        return GC_ERR_SUCCESS;
    }
    let ev = module!(h_event, EventModule);
    ev.queue.kill_one();
    GC_ERR_SUCCESS
    })
}

// Port functions --------------------------------------------------------------

fn port_device(h_port: PortHandle) -> Result<Arc<GevDevice>, GCError> {
    let port = unsafe { from_handle::<DevicePort>(h_port) }.map_err(fail)?;
    port.device
        .upgrade()
        .ok_or_else(|| fail(GentlError::InvalidHandle))
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCReadPort(
    h_port: PortHandle,
    i_address: u64,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    if p_buffer.is_null() || pi_size.is_null() {
        return fail(GentlError::InvalidParameter("pBuffer/piSize"));
    }
    let len = unsafe { *pi_size };
    if len > (1 << 24) {
        return fail(GentlError::InvalidParameter("piSize"));
    }
    let device = match port_device(h_port) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let mut buf = vec![0u8; len];
    if let Err(e) = device.read_mem(i_address, &mut buf) {
        return fail(e);
    }
    unsafe {
        ptr::copy_nonoverlapping(buf.as_ptr(), p_buffer as *mut u8, len);
        *pi_size = len;
    }
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCWritePort(
    h_port: PortHandle,
    i_address: u64,
    p_buffer: *const core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    if p_buffer.is_null() || pi_size.is_null() {
        return fail(GentlError::InvalidParameter("pBuffer/piSize"));
    }
    let len = unsafe { *pi_size };
    if len > (1 << 24) {
        return fail(GentlError::InvalidParameter("piSize"));
    }
    let device = match port_device(h_port) {
        Ok(d) => d,
        Err(e) => return e,
    };
    if device.access == DeviceAccess::Monitor {
        return fail(GentlError::AccessDenied);
    }
    let buf = unsafe { std::slice::from_raw_parts(p_buffer as *const u8, len) };
    if let Err(e) = device.write_mem(i_address, buf) {
        return fail(e);
    }
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCGetPortURL(
    h_port: PortHandle,
    s_url: *mut c_char,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    if pi_size.is_null() {
        return fail(GentlError::InvalidParameter("piSize"));
    }
    let device = match port_device(h_port) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let url = match device.xml_url() {
        Ok(u) => u,
        Err(e) => return fail(e),
    };
    // URL string list: each entry NUL-terminated, list ends with an extra
    // NUL (§6.3.6.2).
    let needed = url.len() + 2;
    if s_url.is_null() {
        unsafe { *pi_size = needed };
        return GC_ERR_SUCCESS;
    }
    if unsafe { *pi_size } < needed {
        unsafe { *pi_size = needed };
        return GC_ERR_BUFFER_TOO_SMALL;
    }
    unsafe {
        ptr::copy_nonoverlapping(url.as_ptr(), s_url as *mut u8, url.len());
        *s_url.add(url.len()) = 0;
        *s_url.add(url.len() + 1) = 0;
        *pi_size = needed;
    }
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCGetPortInfo(
    h_port: PortHandle,
    i_info_cmd: PortInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
    let e = check_info_args(pi_type, pi_size);
    if e != GC_ERR_SUCCESS {
        return e;
    }
    let device = match port_device(h_port) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let d = &device.discovery;
    unsafe {
        match i_info_cmd {
            PORT_INFO_ID => info_string(pi_type, p_buffer, pi_size, &device.id),
            PORT_INFO_VENDOR => info_string(pi_type, p_buffer, pi_size, &d.manufacturer_name),
            PORT_INFO_MODEL => info_string(pi_type, p_buffer, pi_size, &d.model_name),
            PORT_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_GEV),
            PORT_INFO_MODULE => info_string(pi_type, p_buffer, pi_size, "Device"),
            PORT_INFO_LITTLE_ENDIAN => {
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &0u8)
            }
            PORT_INFO_BIG_ENDIAN => {
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &1u8)
            }
            PORT_INFO_ACCESS_READ => {
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &1u8)
            }
            PORT_INFO_ACCESS_WRITE => {
                let v: u8 = (device.access != DeviceAccess::Monitor).into();
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
            }
            PORT_INFO_ACCESS_NA => {
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &0u8)
            }
            PORT_INFO_ACCESS_NI => {
                info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &0u8)
            }
            PORT_INFO_VERSION => info_string(pi_type, p_buffer, pi_size, &d.device_version),
            PORT_INFO_PORTNAME => info_string(pi_type, p_buffer, pi_size, PORT_NAME_DEVICE),
            _ => not_implemented("GCGetPortInfo cmd"),
        }
    }
    })
}

// Parent queries (GenTL v1.4) -------------------------------------------------

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn IFGetParentTL(h_iface: IFHandle, ph_tl: *mut TLHandle) -> GCError {
    guard(|| {
    if ph_tl.is_null() {
        return fail(GentlError::InvalidParameter("phTL"));
    }
    unsafe { *ph_tl = ptr::null_mut() };
    let iface = module!(h_iface, GevInterface);
    let Some(tl) = iface.parent.upgrade() else {
        return fail(GentlError::InvalidHandle);
    };
    unsafe { *ph_tl = to_handle(tl) };
    GC_ERR_SUCCESS
    })
}

#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DevGetParentIF(h_device: DevHandle, ph_iface: *mut IFHandle) -> GCError {
    guard(|| {
    if ph_iface.is_null() {
        return fail(GentlError::InvalidParameter("phIface"));
    }
    unsafe { *ph_iface = ptr::null_mut() };
    let device = module!(h_device, GevDevice);
    let Some(iface) = device.parent.upgrade() else {
        return fail(GentlError::InvalidHandle);
    };
    unsafe { *ph_iface = to_handle(iface) };
    GC_ERR_SUCCESS
    })
}

// Stubs (filled milestone by milestone) ---------------------------------------

macro_rules! stub {
    ($name:ident($($arg:ident: $ty:ty),* $(,)?)) => {
        #[cfg_attr(feature = "cti-exports", no_mangle)]
        pub extern "C" fn $name($($arg: $ty),*) -> GCError {
            guard(|| {
                $(let _ = $arg;)*
                not_implemented(stringify!($name))
            })
        }
    };
}

/// GenTL 1.6 §6.3.7.3: parse one event payload delivered by `EventGetData`.
/// The payload is the GigE Vision wire record (§17.1.1), so the event id,
/// stream channel, block id and timestamp are read without re-encoding.
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn EventGetDataInfo(
    h_event: EventHandle,
    p_in_buffer: *const core::ffi::c_void,
    i_in_size: usize,
    i_info_cmd: EventDataInfoCmd,
    pi_type: *mut InfoDataType,
    p_out_buffer: *mut core::ffi::c_void,
    pi_out_size: *mut usize,
) -> GCError {
    guard(|| {
        let e = check_info_args(pi_type, pi_out_size);
        if e != GC_ERR_SUCCESS {
            return e;
        }
        // Only device events carry event data; EVENT_NEW_BUFFER is a fixed
        // GenTL structure and defines no EVENT_DATA_INFO commands.
        if unsafe { from_handle::<DeviceEventModule>(h_event) }.is_err() {
            return not_implemented("EventGetDataInfo: event type");
        }
        if p_in_buffer.is_null() || i_in_size == 0 {
            return fail(GentlError::InvalidParameter("pInBuffer/iInSize"));
        }
        let bytes = unsafe { std::slice::from_raw_parts(p_in_buffer as *const u8, i_in_size) };
        let Some(event) = gentl_gvcp::wire::GvcpEvent::decode(bytes) else {
            return fail(GentlError::InvalidParameter("event data"));
        };
        unsafe {
            match i_info_cmd {
                EVENT_DATA_ID => info_string(
                    pi_type,
                    p_out_buffer,
                    pi_out_size,
                    &gentl_core::event_id_hex(event.event_id),
                ),
                EVENT_DATA_NUMID => {
                    let v = u64::from(event.event_id);
                    info_value(pi_type, INFO_DATATYPE_UINT64, p_out_buffer, pi_out_size, &v)
                }
                EVENT_DATA_VALUE => {
                    // GenTL 1.6 搂4.2: the value buffer starts at address 0
                    // of the remote device's event port. For GigE Vision that
                    // is the entire EVENTDATA packet without the 8-byte GVCP
                    // header (搂17.3 Figure 17-1): the 24-byte standardized
                    // header (event_size/event_id/...) followed by the data,
                    // not just the payload.
                    *pi_type = INFO_DATATYPE_BUFFER;
                    let record = event.encode();
                    let needed = record.len();
                    if p_out_buffer.is_null() {
                        *pi_out_size = needed;
                        return GC_ERR_SUCCESS;
                    }
                    if *pi_out_size < needed {
                        *pi_out_size = needed;
                        return GC_ERR_BUFFER_TOO_SMALL;
                    }
                    ptr::copy_nonoverlapping(
                        record.as_ptr(),
                        p_out_buffer as *mut u8,
                        needed,
                    );
                    *pi_out_size = needed;
                    GC_ERR_SUCCESS
                }
                _ => not_implemented("EventGetDataInfo cmd"),
            }
        }
    })
}

/* GenTL v1.1 */

/// URL module: the GenTLite device description is exactly one URL — the
/// First URL bootstrap register (`local:` XML slot, §14.1.1).
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCGetNumPortURLs(h_port: PortHandle, pi_num_urls: *mut u32) -> GCError {
    guard(|| {
        if pi_num_urls.is_null() {
            return fail(GentlError::InvalidParameter("piNumURLs"));
        }
        match port_device(h_port) {
            Ok(_) => {
                unsafe { *pi_num_urls = 1 };
                GC_ERR_SUCCESS
            }
            Err(e) => e,
        }
    })
}

/// First URL metadata (§6.3.6.6-§6.3.6.7). The XML comes from the camera's
/// register map, so the scheme is URL_SCHEME_LOCAL and the file register
/// address / size come from the URL itself.
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCGetPortURLInfo(
    h_port: PortHandle,
    i_url_index: u32,
    i_info_cmd: UrlInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
        let e = check_info_args(pi_type, pi_size);
        if e != GC_ERR_SUCCESS {
            return e;
        }
        if let Err(e) = port_device(h_port) {
            return e;
        }
        if i_url_index != 0 {
            return fail(GentlError::InvalidId);
        }
        if i_info_cmd == URL_INFO_SCHEME {
            let scheme = URL_SCHEME_LOCAL;
            return unsafe {
                info_value(pi_type, INFO_DATATYPE_INT32, p_buffer, pi_size, &scheme)
            };
        }
        let mut raw = [0u8; 512];
        let mut size = raw.len();
        let r = GCReadPort(
            h_port,
            u64::from(registers::FIRST_URL),
            raw.as_mut_ptr().cast(),
            &mut size,
        );
        if r != GC_ERR_SUCCESS {
            return r;
        }
        let url = unsafe { c_str(raw.as_ptr() as *const c_char) }
            .map(str::to_owned)
            .unwrap_or_default();
        match i_info_cmd {
            URL_INFO_URL => unsafe { info_string(pi_type, p_buffer, pi_size, &url) },
            URL_INFO_FILE_REGISTER_ADDRESS | URL_INFO_FILE_SIZE => {
                let parsed = url.strip_prefix("local:").and_then(|rest| {
                    let mut parts = rest.split(';').skip(1);
                    Some((parts.next()?, parts.next()?))
                });
                let Some((addr_text, size_text)) = parsed else {
                    return fail(GentlError::NotAvailable);
                };
                let value = if i_info_cmd == URL_INFO_FILE_REGISTER_ADDRESS {
                    u64::from_str_radix(addr_text, 16)
                } else {
                    u64::from_str_radix(size_text, 16)
                };
                match value {
                    Ok(v) => unsafe {
                        info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
                    },
                    Err(_) => fail(GentlError::NotAvailable),
                }
            }
            _ => not_implemented("GCGetPortURLInfo cmd"),
        }
    })
}

/// Stacked register read (§6.3.6.8): on an error the completed count is
/// returned in `piNumEntries` (an explicit exception to the usual error
/// handling).
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCReadPortStacked(
    h_port: PortHandle,
    p_entries: *mut PortRegisterStackEntry,
    pi_num_entries: *mut usize,
) -> GCError {
    guard(|| {
        if p_entries.is_null() || pi_num_entries.is_null() {
            return fail(GentlError::InvalidParameter("pEntries/piNumEntries"));
        }
        let requested = unsafe { *pi_num_entries };
        let mut done = 0usize;
        while done < requested {
            let entry = unsafe { &mut *p_entries.add(done) };
            let mut size = entry.size;
            let r = GCReadPort(h_port, entry.address, entry.p_buffer, &mut size);
            if r != GC_ERR_SUCCESS {
                unsafe { *pi_num_entries = done };
                return r;
            }
            entry.size = size;
            done += 1;
        }
        unsafe { *pi_num_entries = done };
        GC_ERR_SUCCESS
    })
}

/// Stacked register write (§6.3.6.9): same partial-count contract as
/// [`GCReadPortStacked`].
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn GCWritePortStacked(
    h_port: PortHandle,
    p_entries: *mut PortRegisterStackEntry,
    pi_num_entries: *mut usize,
) -> GCError {
    guard(|| {
        if p_entries.is_null() || pi_num_entries.is_null() {
            return fail(GentlError::InvalidParameter("pEntries/piNumEntries"));
        }
        let requested = unsafe { *pi_num_entries };
        let mut done = 0usize;
        while done < requested {
            let entry = unsafe { &mut *p_entries.add(done) };
            let mut size = entry.size;
            let r = GCWritePort(h_port, entry.address, entry.p_buffer, &mut size);
            if r != GC_ERR_SUCCESS {
                unsafe { *pi_num_entries = done };
                return r;
            }
            entry.size = size;
            done += 1;
        }
        unsafe { *pi_num_entries = done };
        GC_ERR_SUCCESS
    })
}

/* GenTL v1.3 */

/* GenTL v1.4 */

/* GenTL v1.5 */

/// §6.3.5.15: the GigE producer delivers single-part image buffers, so the
/// payload is fully described by BUFFER_INFO — the producer MUST answer
/// GC_ERR_NO_DATA and the consumer then skips DSGetBufferPartInfo.
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSGetNumBufferParts(
    h_ds: DSHandle,
    h_buffer: BufferHandle,
    pi_num_parts: *mut u32,
) -> GCError {
    guard(|| {
        if pi_num_parts.is_null() {
            return fail(GentlError::InvalidParameter("piNumParts"));
        }
        let ds = match ds_from(h_ds) {
            Ok(d) => d,
            Err(e) => return e,
        };
        if let Err(e) = ds_buffer_from(&ds, h_buffer) {
            return e;
        }
        unsafe { *pi_num_parts = 0 };
        GC_ERR_NO_DATA
    })
}

// DSGetBufferPartInfo stays NOT_IMPLEMENTED: the producer reports no parts
// (§6.3.5.15), so a compliant consumer never calls it.
stub!(DSGetBufferPartInfo(h_ds: DSHandle, h_buffer: BufferHandle, i_part_index: u32, i_info_cmd: BufferPartInfoCmd, pi_type: *mut InfoDataType, p_buffer: *mut core::ffi::c_void, pi_size: *mut usize));

/* GenTL v1.6 */
stub!(DSAnnounceCompositeBuffer(h_ds: DSHandle, i_num_segments: usize, pp_segments: *mut *mut core::ffi::c_void, pi_sizes: *mut usize, p_private: *mut core::ffi::c_void, ph_buffer: *mut BufferHandle));
/// §6.3.5.17: run every requested BUFFER_INFO_CMD through DSGetBufferInfo.
/// Per-entry failures land in `iResult` and do not fail the call.
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSGetBufferInfoStacked(
    h_ds: DSHandle,
    h_buffer: BufferHandle,
    p_info_stacked: *mut DSBufferInfoStacked,
    i_num_infos: usize,
) -> GCError {
    guard(|| {
        if i_num_infos == 0 {
            return GC_ERR_SUCCESS;
        }
        if p_info_stacked.is_null() {
            return fail(GentlError::InvalidParameter("pInfoStacked"));
        }
        {
            let ds = match ds_from(h_ds) {
                Ok(d) => d,
                Err(e) => return e,
            };
            if let Err(e) = ds_buffer_from(&ds, h_buffer) {
                return e;
            }
        }
        for i in 0..i_num_infos {
            let entry = unsafe { &mut *p_info_stacked.add(i) };
            let mut size = entry.i_size;
            entry.i_result = DSGetBufferInfo(
                h_ds,
                h_buffer,
                entry.i_info_cmd,
                &mut entry.i_type,
                entry.p_buffer,
                &mut size,
            );
            entry.i_size = size;
        }
        // Individual failures must not leak the last error (§6.3.5.17).
        set_error(GC_ERR_SUCCESS, "no error");
        GC_ERR_SUCCESS
    })
}

/// §6.3.5.18: stacked buffer-part info. The producer reports no parts
/// (§6.3.5.15), so every entry answers NOT_IMPLEMENTED in `iResult` while
/// the call itself succeeds.
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSGetBufferPartInfoStacked(
    h_ds: DSHandle,
    h_buffer: BufferHandle,
    p_info_stacked: *mut DSBufferPartInfoStacked,
    i_num_infos: usize,
) -> GCError {
    guard(|| {
        if i_num_infos == 0 {
            return GC_ERR_SUCCESS;
        }
        if p_info_stacked.is_null() {
            return fail(GentlError::InvalidParameter("pInfoStacked"));
        }
        {
            let ds = match ds_from(h_ds) {
                Ok(d) => d,
                Err(e) => return e,
            };
            if let Err(e) = ds_buffer_from(&ds, h_buffer) {
                return e;
            }
        }
        for i in 0..i_num_infos {
            let entry = unsafe { &mut *p_info_stacked.add(i) };
            let mut size = entry.i_size;
            entry.i_result = DSGetBufferPartInfo(
                h_ds,
                h_buffer,
                entry.i_part_index,
                entry.i_info_cmd,
                &mut entry.i_type,
                entry.p_buffer,
                &mut size,
            );
            entry.i_size = size;
        }
        set_error(GC_ERR_SUCCESS, "no error");
        GC_ERR_SUCCESS
    })
}
stub!(DSGetNumFlows(h_ds: DSHandle, pi_num_flows: *mut u32));
stub!(DSGetFlowInfo(h_ds: DSHandle, i_flow_index: u32, i_info_cmd: FlowInfoCmd, pi_type: *mut InfoDataType, p_buffer: *mut core::ffi::c_void, pi_size: *mut usize));
/// §6.3.5.21: a contiguous announced buffer always has exactly one virtual
/// segment, so the number must be non-zero.
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSGetNumBufferSegments(
    h_ds: DSHandle,
    h_buffer: BufferHandle,
    pi_num_segments: *mut u32,
) -> GCError {
    guard(|| {
        if pi_num_segments.is_null() {
            return fail(GentlError::InvalidParameter("piNumSegments"));
        }
        let ds = match ds_from(h_ds) {
            Ok(d) => d,
            Err(e) => return e,
        };
        if let Err(e) = ds_buffer_from(&ds, h_buffer) {
            return e;
        }
        unsafe { *pi_num_segments = 1 };
        GC_ERR_SUCCESS
    })
}

/// §6.3.5.22: segment 0 information for a contiguous buffer.
#[cfg_attr(feature = "cti-exports", no_mangle)]
pub extern "C" fn DSGetBufferSegmentInfo(
    h_ds: DSHandle,
    h_buffer: BufferHandle,
    i_segment_index: u32,
    i_info_cmd: SegmentInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    guard(|| {
        let e = check_info_args(pi_type, pi_size);
        if e != GC_ERR_SUCCESS {
            return e;
        }
        let ds = match ds_from(h_ds) {
            Ok(d) => d,
            Err(e) => return e,
        };
        let buf = match ds_buffer_from(&ds, h_buffer) {
            Ok(b) => b,
            Err(e) => return e,
        };
        if i_segment_index != 0 {
            return invalid_index();
        }
        let snap = match ds.core.with_buffer(buf.id, |b| BufferSnapshot {
            base: b.base_ptr(),
            user_ptr: b.user_ptr(),
            size: b.size(),
            state: b.state,
            new_data: b.new_data,
            filled_size: b.filled_size,
            data_size: b.data_size,
            incomplete: b.incomplete,
            meta: b.meta,
            block_id: b.block_id,
        }) {
            Some(s) => s,
            None => return fail(GentlError::InvalidHandle),
        };
        unsafe {
            match i_info_cmd {
                SEGMENT_INFO_BASE => info_value(
                    pi_type,
                    INFO_DATATYPE_PTR,
                    p_buffer,
                    pi_size,
                    &snap.base,
                ),
                SEGMENT_INFO_SIZE => info_value(
                    pi_type,
                    INFO_DATATYPE_SIZET,
                    p_buffer,
                    pi_size,
                    &snap.size,
                ),
                SEGMENT_INFO_IS_INCOMPLETE => {
                    let v: u8 = u8::from(snap.incomplete);
                    info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
                }
                _ => not_implemented("DSGetBufferSegmentInfo cmd"),
            }
        }
    })
}

// Tests -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_string_size_fit_too_small_and_query() {
        let mut buf = [0u8; 8];

        // Exact fit.
        let mut size = buf.len();
        let r = unsafe { write_string(buf.as_mut_ptr() as *mut c_char, &mut size, "abc") };
        assert_eq!(r, GC_ERR_SUCCESS);
        assert_eq!(size, 4);
        assert_eq!(&buf[..4], b"abc\0");

        // Too small: needed size (incl. NUL) reported back.
        let mut size = 2;
        let r = unsafe { write_string(buf.as_mut_ptr() as *mut c_char, &mut size, "abcdef") };
        assert_eq!(r, GC_ERR_BUFFER_TOO_SMALL);
        assert_eq!(size, 7);

        // NULL buffer: pure size query, success.
        let mut size = 0usize;
        let r = unsafe { write_string(ptr::null_mut(), &mut size, "hello") };
        assert_eq!(r, GC_ERR_SUCCESS);
        assert_eq!(size, 6);
    }

    #[test]
    fn write_value_u32_semantics() {
        let mut buf = [0u8; 8];
        let v: u32 = 0xDEADBEEF;

        let mut size = 4;
        let r = unsafe { write_value(buf.as_mut_ptr() as *mut core::ffi::c_void, &mut size, &v) };
        assert_eq!(r, GC_ERR_SUCCESS);
        assert_eq!(buf[..4], v.to_ne_bytes());

        let mut size = 2;
        let r = unsafe { write_value(buf.as_mut_ptr() as *mut core::ffi::c_void, &mut size, &v) };
        assert_eq!(r, GC_ERR_BUFFER_TOO_SMALL);
        assert_eq!(size, 4);

        let mut size = 0usize;
        let r = unsafe { write_value(ptr::null_mut(), &mut size, &v) };
        assert_eq!(r, GC_ERR_SUCCESS);
        assert_eq!(size, 4);
    }

    /// TL lifecycle, info outputs, and every invalid-handle/index/id path.
    /// One test on purpose: the TL module is process-wide, so parallel
    /// tests would race the open flag.
    #[test]
    fn tl_lifecycle_and_error_paths() {
        // TLOpen before GCInitLib -> GC_ERR_NOT_INITIALIZED.
        GCCloseLib();
        let mut tl: TLHandle = ptr::null_mut();
        assert_eq!(TLOpen(&mut tl), GC_ERR_NOT_INITIALIZED);
        // The legacy GCGetInfo is gated on GCInitLib as well (§6.3.1.2).
        let mut gt: InfoDataType = -1;
        let mut gsize = 0usize;
        assert_eq!(
            GCGetInfo(TL_INFO_VENDOR, &mut gt, ptr::null_mut(), &mut gsize),
            GC_ERR_NOT_INITIALIZED
        );
        assert_eq!(GCInitLib(), GC_ERR_SUCCESS);

        // GCGetInfo works without a System instance and must report the same
        // information as TLGetInfo (§6.3.1.2, §6.4.1.3).
        let mut gbuf = [0u8; 64];
        let mut gt: InfoDataType = -1;
        let mut gsize = gbuf.len();
        assert_eq!(
            GCGetInfo(
                TL_INFO_VENDOR,
                &mut gt,
                gbuf.as_mut_ptr().cast(),
                &mut gsize
            ),
            GC_ERR_SUCCESS
        );
        assert_eq!(gt, INFO_DATATYPE_STRING);
        assert_eq!(
            unsafe { c_str(gbuf.as_ptr() as *const c_char) },
            Some("GenTLite")
        );
        let mut gsize = gbuf.len();
        assert_eq!(
            GCGetInfo(
                TL_INFO_TLTYPE,
                &mut gt,
                gbuf.as_mut_ptr().cast(),
                &mut gsize
            ),
            GC_ERR_SUCCESS
        );
        assert_eq!(unsafe { c_str(gbuf.as_ptr() as *const c_char) }, Some("GEV"));
        // Unknown info command -> NOT_IMPLEMENTED, no crash.
        let mut gsize = gbuf.len();
        assert_eq!(
            GCGetInfo(0x7FFF, &mut gt, gbuf.as_mut_ptr().cast(), &mut gsize),
            GC_ERR_NOT_IMPLEMENTED
        );

        assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
        assert!(!tl.is_null());

        // NULL handle -> INVALID_HANDLE, no crash.
        let mut dtype: InfoDataType = -1;
        let mut size = 0usize;
        assert_eq!(
            TLGetInfo(
                ptr::null_mut(),
                TL_INFO_VENDOR,
                &mut dtype,
                ptr::null_mut(),
                &mut size
            ),
            GC_ERR_INVALID_HANDLE
        );

        // Vendor string round-trip.
        let mut buf = [0u8; 64];
        let mut size = buf.len();
        assert_eq!(
            TLGetInfo(
                tl,
                TL_INFO_VENDOR,
                &mut dtype,
                buf.as_mut_ptr().cast(),
                &mut size
            ),
            GC_ERR_SUCCESS
        );
        assert_eq!(dtype, INFO_DATATYPE_STRING);
        assert_eq!(
            unsafe { c_str(buf.as_ptr() as *const c_char) },
            Some("GenTLite")
        );

        // TLTYPE "GEV".
        let mut size = buf.len();
        assert_eq!(
            TLGetInfo(
                tl,
                TL_INFO_TLTYPE,
                &mut dtype,
                buf.as_mut_ptr().cast(),
                &mut size
            ),
            GC_ERR_SUCCESS
        );
        assert_eq!(unsafe { c_str(buf.as_ptr() as *const c_char) }, Some("GEV"));

        // GenTL version major/minor = 1/6.
        let mut v: u32 = 0;
        let mut size = std::mem::size_of::<u32>();
        assert_eq!(
            TLGetInfo(
                tl,
                TL_INFO_GENTL_VER_MAJOR,
                &mut dtype,
                (&mut v as *mut u32).cast(),
                &mut size
            ),
            GC_ERR_SUCCESS
        );
        assert_eq!(dtype, INFO_DATATYPE_UINT32);
        assert_eq!(v, 1);

        // NULL piType -> INVALID_PARAMETER.
        assert_eq!(
            TLGetInfo(
                tl,
                TL_INFO_VENDOR,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut size
            ),
            GC_ERR_INVALID_PARAMETER
        );

        // Wrong-typed handle (TL handle passed as interface) -> INVALID_HANDLE.
        assert_eq!(IFClose(tl), GC_ERR_INVALID_HANDLE);
        // Last error is retrievable: text=NULL reports the needed size via
        // *piSize (GenTL 1.6 §6.3.1.3); piSize=NULL -> INVALID_PARAMETER.
        let mut err_code: GCError = 0;
        let mut needed = 0usize;
        assert_eq!(
            GCGetLastError(&mut err_code, ptr::null_mut(), &mut needed),
            GC_ERR_SUCCESS
        );
        assert_eq!(err_code, GC_ERR_INVALID_HANDLE);
        assert!(needed > 1, "needed size includes NUL");
        assert_eq!(
            GCGetLastError(&mut err_code, ptr::null_mut(), ptr::null_mut()),
            GC_ERR_INVALID_PARAMETER
        );

        // Out-of-range interface index -> GC_ERR_INVALID_INDEX.
        let mut n = 0u32;
        assert_eq!(TLGetNumInterfaces(tl, &mut n), GC_ERR_SUCCESS);
        let mut idbuf = [0u8; 64];
        let mut size = idbuf.len();
        assert_eq!(
            TLGetInterfaceID(tl, n, idbuf.as_mut_ptr() as *mut c_char, &mut size),
            GC_ERR_INVALID_INDEX
        );

        // Unknown interface ID -> INVALID_ID.
        let bad_id = b"GEV-FFFFFFFFFFFF\0".as_ptr() as *const c_char;
        let mut size = 0usize;
        assert_eq!(
            TLGetInterfaceInfo(
                tl,
                bad_id,
                INTERFACE_INFO_TLTYPE,
                &mut dtype,
                ptr::null_mut(),
                &mut size
            ),
            GC_ERR_INVALID_ID
        );

        // A second TLOpen while open -> RESOURCE_IN_USE.
        let mut tl2: TLHandle = ptr::null_mut();
        assert_eq!(TLOpen(&mut tl2), GC_ERR_RESOURCE_IN_USE);

        // List access with a NULL handle -> INVALID_HANDLE, no crash.
        let mut n2 = 0u32;
        assert_eq!(
            TLGetNumInterfaces(ptr::null_mut(), &mut n2),
            GC_ERR_INVALID_HANDLE
        );
        // NULL out-pointer -> INVALID_PARAMETER (checked before the handle).
        assert_eq!(
            TLGetNumInterfaces(tl, ptr::null_mut()),
            GC_ERR_INVALID_PARAMETER
        );

        assert_eq!(TLClose(tl), GC_ERR_SUCCESS);

        // After TLClose the module can be opened again.
        let mut tl3: TLHandle = ptr::null_mut();
        assert_eq!(TLOpen(&mut tl3), GC_ERR_SUCCESS);
        assert_eq!(TLClose(tl3), GC_ERR_SUCCESS);
    }

    /// A panic inside an entry-point body must surface as GC_ERR_ERROR plus
    /// last-error text, never as an unwind across the C ABI (which aborts
    /// the whole host process).
    #[test]
    fn guard_turns_an_entry_point_panic_into_gc_error() {
        assert_eq!(guard(|| panic!("boom in an entry point")), GC_ERR_ERROR);
        let mut code: GCError = 0;
        let mut size = 0usize;
        assert_eq!(
            GCGetLastError(&mut code, ptr::null_mut(), &mut size),
            GC_ERR_SUCCESS
        );
        assert_eq!(code, GC_ERR_ERROR);
        let mut text = [0u8; 128];
        let mut size = text.len();
        assert_eq!(
            GCGetLastError(&mut code, text.as_mut_ptr() as *mut c_char, &mut size),
            GC_ERR_SUCCESS
        );
        let message = unsafe { c_str(text.as_ptr() as *const c_char) }.unwrap();
        assert!(
            message.contains("internal panic: boom in an entry point"),
            "last-error text carries the panic payload, got {message:?}"
        );
    }

    /// Mechanical enforcement of the module contract: a panic that reaches
    /// an `extern "C"` frame aborts the process, so every exported entry
    /// point must run its body inside [`guard`]. Source-level check because
    /// a runtime abort test would kill the test harness.
    #[test]
    fn every_export_body_is_panic_guarded() {
        let src = include_str!("lib.rs");
        let marker = "pub extern \"C\" fn ";
        let mut checked = 0usize;
        let mut rest = src;
        while let Some(at) = rest.find(marker) {
            let after = &rest[at + marker.len()..];
            let brace = after.find('{').expect("export without a body brace");
            let name: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect();
            assert!(
                after[brace + 1..].trim_start().starts_with("guard(||"),
                "`{name}` does not wrap its body in guard(|| ...)"
            );
            checked += 1;
            rest = &after[brace + 1..];
        }
        assert!(
            checked > 40,
            "expected the GenTL entry points, found {checked}"
        );
    }
}
