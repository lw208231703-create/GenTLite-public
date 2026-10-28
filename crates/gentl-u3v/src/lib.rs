//! USB3 Vision GenTL producer — the loadable `.cti`.
//!
//! GenTL 1.6 loading: the consumer resolves every function below by name
//! and starts with `GCInitLib`. TL → Interface (USB root hub) → Device →
//! Stream follows the gentl-core module/handle machinery; wire behavior
//! lives in packets/discovery/control/stream. Same discipline as
//! gentl-gige: strict spec compliance, no non-compliant-device
//! workarounds (project decision 2026-10-12); panics must never cross
//! this boundary.
//!
//! The built artifact is the cdylib (`gentl_u3v.dll` on Windows,
//! `libgentl_u3v.so` on Linux); the `.cti` rename happens at packaging
//! time (M5 release flow). The USB backend is split per OS (Phase B,
//! notes/PENDING.md "Linux 可移植"): SetupAPI + WinUSB on Windows,
//! rusb/libusb on Unix — behind the shared `usbio::UsbPipe` endpoint
//! surface, with control/cti/stream/descriptors platform-independent.

#![allow(non_snake_case)]

use std::os::raw::c_char;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gentl_core::types::*;
use gentl_core::{
    channel::ControlChannel, ds as ds_core, event_queue::WaitResult, free_handle, from_handle,
    last_error, set_error, set_last_error, to_handle, GentlError, GentlResult,
};

// Module wiring after the Phase B USB-backend split (notes/PENDING.md
// "Linux 可移植"): control/cti/stream/packets/descriptors/types are
// platform-independent — the endpoint surface behind them is the
// `usbio::UsbPipe` trait. discovery and the usbio backend internals are
// per-OS: SetupAPI + WinUSB on Windows (discovery.rs, usbio/winusb.rs),
// rusb/libusb on Unix (linux/discovery.rs, usbio/libusb.rs).
mod control;
mod cti;
pub mod descriptors;
pub mod packets;
mod stream;
mod types;
mod usbio;

/// Library-shaped U3V client facade (no GenTL handles): enumerate → open →
/// GenCP register/memory access → device XML → SIRM-programmed block pulls.
/// Added for the vgige smart-camera gateway's U3V camera backend; see the
/// module docs.
pub mod client;

#[cfg(windows)]
#[path = "discovery.rs"]
mod discovery;
#[cfg(not(windows))]
#[path = "linux/discovery.rs"]
mod discovery;

use cti::{DeviceAccess, DevicePort, TlSystem, U3vDevice, U3vInterface};
use stream::{stream_id, EventModule, U3vBuffer, U3vDataStream};

/// Diagnostic helper for tools/examples: the device description XML
/// behind a DEV_HANDLE (manifest first Device XML entry, unzipped when
/// zipped). Returns GC_ERR_INVALID_HANDLE for foreign handles.
pub fn fetch_device_xml(h_device: DevHandle) -> Result<Vec<u8>, GCError> {
    let device = unsafe { from_handle::<U3vDevice>(h_device) }.map_err(fail)?;
    device.device_xml().map_err(fail)
}

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

// EVENT_INFO_CMD_LIST (GenTL.h)
const EVENT_EVENT_TYPE: EventInfoCmd = 0;
const EVENT_NUM_IN_QUEUE: EventInfoCmd = 1;
const EVENT_NUM_FIRED: EventInfoCmd = 2;
const EVENT_SIZE_MAX: EventInfoCmd = 3;
const EVENT_INFO_DATA_SIZE_MAX: EventInfoCmd = 4;

// GenTL 1.6 PAYLOADTYPE_INFO_IDS / PIXELFORMAT_NAMESPACE_IDS.
const PAYLOAD_TYPE_IMAGE: usize = 1;
const PIXELFORMAT_NAMESPACE_PFNC_32BIT: u64 = 4; // GenTL v1.4: 32-bit PFNC values
const PIXELENDIANNESS_LITTLE: i32 = 1;

// URL_INFO_CMD_LIST (GenTL.h)
const URL_INFO_URL: UrlInfoCmd = 0;
const URL_INFO_SCHEME: UrlInfoCmd = 9;
const URL_SCHEME_LOCAL: i32 = 0;

// DEVICE_ACCESS_STATUS_LIST (GenTL.h).
const DEVICE_ACCESS_STATUS_READWRITE: i32 = 1;
const DEVICE_ACCESS_STATUS_NOACCESS: i32 = 3;
const DEVICE_ACCESS_STATUS_OPEN_READWRITE: i32 = 5;
const DEVICE_ACCESS_STATUS_OPEN_READONLY: i32 = 6;

/// GENTL_INFINITE (GenTL.h V1.4).
const GENTL_INFINITE: u64 = 0xFFFFFFFFFFFFFFFF;

const TL_TYPE_U3V: &str = "U3V"; // TLTypeU3VName (GenTL.h)

// Basler-style identity (mirrors the gentl-gige producer).
const TL_ID: &str = TL_FILENAME;
const TL_VENDOR: &str = "GenTLite";
const TL_MODEL: &str = "U3V";
const TL_DISPLAYNAME: &str = "GenTLite U3V GenTL Producer";
const TL_FILENAME: &str = "gentl_u3v.cti";
const PORT_NAME_DEVICE: &str = "Device";

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
/// frozen), so set it directly.
fn invalid_index() -> GCError {
    set_error(GC_ERR_INVALID_INDEX, "index out of range");
    GC_ERR_INVALID_INDEX
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
/// consumer dies without ever seeing a GenTL error code. Every
/// `#[no_mangle] pub extern "C"` function therefore runs its body here —
/// `every_export_body_is_panic_guarded` enforces that mechanically, and
/// the default panic hook still reports the panic and backtrace on
/// stderr, so containment does not hide the bug.
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

fn lock<T>(m: &std::sync::Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>, GCError> {
    m.lock().map_err(|e| fail(GentlError::Io(e.to_string())))
}

// Library functions -----------------------------------------------------------

#[no_mangle]
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

#[no_mangle]
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

#[no_mangle]
pub extern "C" fn GCCloseLib() -> GCError {
    guard(|| {
        LIB_INITIALIZED.store(false, Ordering::SeqCst);
        GC_ERR_SUCCESS
    })
}

// System (TL) functions -------------------------------------------------------

#[no_mangle]
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
        let system = TlSystem::new(Vec::new());
        let templates: Vec<Arc<U3vInterface>> = discovery::list_root_hubs()
            .iter()
            .map(|r| {
                let display = format!("GenTLite USB3 Vision Interface [{}]", r.display);
                U3vInterface::new_template(&r.instance_id, &display, &system)
            })
            .collect();
        match system.interfaces.lock() {
            Ok(mut list) => *list = templates,
            Err(e) => {
                TL_OPENED.store(false, Ordering::SeqCst);
                return fail(GentlError::Io(e.to_string()));
            }
        }
        unsafe { *ph_tl = to_handle(system) };
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
pub extern "C" fn TLClose(h_tl: TLHandle) -> GCError {
    guard(|| {
        let system = module!(h_tl, TlSystem);
        TL_OPENED.store(false, Ordering::SeqCst);
        drop(system);
        unsafe { free_handle(h_tl) };
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
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
        let version = env!("CARGO_PKG_VERSION");
        unsafe {
            match i_info_cmd {
                TL_INFO_ID => info_string(pi_type, p_buffer, pi_size, TL_ID),
                TL_INFO_VENDOR => info_string(pi_type, p_buffer, pi_size, TL_VENDOR),
                TL_INFO_MODEL => info_string(pi_type, p_buffer, pi_size, TL_MODEL),
                TL_INFO_VERSION => info_string(pi_type, p_buffer, pi_size, version),
                TL_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_U3V),
                TL_INFO_NAME => info_string(pi_type, p_buffer, pi_size, TL_FILENAME),
                TL_INFO_PATHNAME => fail(GentlError::NotAvailable), // OPEN-QUESTION Q10 (gige)
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
    })
}

#[no_mangle]
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

#[no_mangle]
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
        unsafe { write_string(s_id, pi_size, &iface.root_id) }
    })
}

#[no_mangle]
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
        let Some(iface) = list.iter().find(|i| i.root_id == id) else {
            return fail(GentlError::InvalidId);
        };
        unsafe {
            match i_info_cmd {
                INTERFACE_INFO_ID => info_string(pi_type, p_buffer, pi_size, &iface.root_id),
                INTERFACE_INFO_DISPLAYNAME => {
                    info_string(pi_type, p_buffer, pi_size, &iface.display)
                }
                INTERFACE_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_U3V),
                _ => not_implemented("TLGetInterfaceInfo cmd"),
            }
        }
    })
}

#[no_mangle]
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
            list.iter().find(|i| i.root_id == id).cloned()
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

#[no_mangle]
pub extern "C" fn TLUpdateInterfaceList(
    h_tl: TLHandle,
    pb_changed: *mut u8,
    _i_timeout: u64,
) -> GCError {
    guard(|| {
        let system = module!(h_tl, TlSystem);
        // Re-enumerate root hubs; opened interface instances keep their own
        // device lists (a re-open picks up the new template list).
        let new_roots = discovery::list_root_hubs();
        let new_ids: Vec<String> = new_roots.iter().map(|r| r.instance_id.clone()).collect();
        let old_ids: Vec<String> = {
            let list = match system.interfaces.lock() {
                Ok(l) => l,
                Err(e) => return fail(GentlError::Io(e.to_string())),
            };
            list.iter().map(|i| i.root_id.clone()).collect()
        };
        let changed = new_ids != old_ids;
        if changed {
            let templates: Vec<Arc<U3vInterface>> = new_roots
                .iter()
                .map(|r| {
                    let display = format!("GenTLite USB3 Vision Interface [{}]", r.display);
                    U3vInterface::new_template(&r.instance_id, &display, &system)
                })
                .collect();
            match system.interfaces.lock() {
                Ok(mut list) => *list = templates,
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

fn iface_from(h_iface: IFHandle) -> Result<Arc<U3vInterface>, GCError> {
    let iface = unsafe { from_handle::<U3vInterface>(h_iface) }.map_err(fail)?;
    parent_alive(&iface.parent)?;
    Ok(iface)
}

#[no_mangle]
pub extern "C" fn IFClose(h_iface: IFHandle) -> GCError {
    guard(|| {
        let iface = module!(h_iface, U3vInterface);
        if let Some(tl) = iface.parent.upgrade() {
            if let Ok(mut opened) = tl.opened_interfaces.lock() {
                opened.remove(&iface.root_id);
            }
        }
        drop(iface);
        unsafe { free_handle(h_iface) };
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
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
        let iface = match iface_from(h_iface) {
            Ok(i) => i,
            Err(e) => return e,
        };
        unsafe {
            match i_info_cmd {
                INTERFACE_INFO_ID => info_string(pi_type, p_buffer, pi_size, &iface.root_id),
                INTERFACE_INFO_DISPLAYNAME => {
                    info_string(pi_type, p_buffer, pi_size, &iface.display)
                }
                INTERFACE_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_U3V),
                _ => not_implemented("IFGetInfo cmd"),
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn IFUpdateDeviceList(
    h_iface: IFHandle,
    pb_changed: *mut u8,
    _i_timeout: u64,
) -> GCError {
    guard(|| {
        let iface = match iface_from(h_iface) {
            Ok(i) => i,
            Err(e) => return e,
        };
        // USB enumeration is host-local and instant; the timeout has no
        // meaning here (nothing to wait for on the bus).
        let found = discovery::enumerate_devices();
        let new_entries: Vec<types::FoundDevice> = found
            .devices
            .into_iter()
            .filter(|d| d.root_id == iface.root_id)
            .collect();
        let old_ids: Vec<String> = {
            let devices = match lock(&iface.devices) {
                Ok(d) => d,
                Err(e) => return e,
            };
            devices.iter().map(|d| d.id.clone()).collect()
        };
        let changed = old_ids != new_entries.iter().map(|d| d.id.clone()).collect::<Vec<_>>();
        {
            let mut devices = match lock(&iface.devices) {
                Ok(d) => d,
                Err(e) => return e,
            };
            *devices = new_entries;
        }
        if !pb_changed.is_null() {
            unsafe { *pb_changed = u8::from(changed) };
        }
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
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

#[no_mangle]
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

fn find_device(iface: &U3vInterface, id: &str) -> Result<Option<types::FoundDevice>, GCError> {
    let devices = lock(&iface.devices)?;
    Ok(devices.iter().find(|d| d.id == id).cloned())
}

#[no_mangle]
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
        let ident = &entry.identity;
        unsafe {
            match i_info_cmd {
                DEVICE_INFO_ID => info_string(pi_type, p_buffer, pi_size, &entry.id),
                DEVICE_INFO_VENDOR => info_string(pi_type, p_buffer, pi_size, &ident.manufacturer),
                DEVICE_INFO_MODEL => info_string(pi_type, p_buffer, pi_size, &ident.model),
                DEVICE_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_U3V),
                DEVICE_INFO_DISPLAYNAME => info_string(
                    pi_type,
                    p_buffer,
                    pi_size,
                    &display_name(&ident.manufacturer, &ident.model, &entry.id),
                ),
                DEVICE_INFO_ACCESS_STATUS => {
                    let opened = match lock(&iface.opened_devices) {
                        Ok(o) => o.contains(id),
                        Err(e) => return e,
                    };
                    let v = if !entry.openable {
                        DEVICE_ACCESS_STATUS_NOACCESS
                    } else if opened {
                        DEVICE_ACCESS_STATUS_OPEN_READWRITE
                    } else {
                        DEVICE_ACCESS_STATUS_READWRITE
                    };
                    info_value(pi_type, INFO_DATATYPE_INT32, p_buffer, pi_size, &v)
                }
                DEVICE_INFO_USER_DEFINED_NAME => {
                    info_string(pi_type, p_buffer, pi_size, &ident.user_defined_name)
                }
                DEVICE_INFO_SERIAL_NUMBER => {
                    info_string(pi_type, p_buffer, pi_size, &ident.serial_number)
                }
                DEVICE_INFO_VERSION => {
                    info_string(pi_type, p_buffer, pi_size, &ident.device_version)
                }
                DEVICE_INFO_TIMESTAMP_FREQUENCY => {
                    // U3V timestamps are nanoseconds (§5.5.5.1.1 timestamp).
                    let v: u64 = 1_000_000_000;
                    info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
                }
                _ => not_implemented("IFGetDeviceInfo cmd"),
            }
        }
    })
}

#[no_mangle]
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
            4 => DeviceAccess::Exclusive, // no U3V exclusive mechanism (UQ5)
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
            // prior IFUpdateDeviceList; run the enumeration ourselves.
            let found = discovery::enumerate_devices();
            let entries: Vec<types::FoundDevice> = found
                .devices
                .into_iter()
                .filter(|d| d.root_id == iface.root_id)
                .collect();
            {
                let mut devices = match lock(&iface.devices) {
                    Ok(d) => d,
                    Err(e) => return e,
                };
                *devices = entries;
            }
            entry = match find_device(&iface, id) {
                Ok(e) => e,
                Err(e) => return e,
            };
        }
        let Some(entry) = entry else {
            return fail(GentlError::InvalidId);
        };
        if !entry.openable {
            // Listed but not accessible (no driver bound / foreign owner).
            return fail(GentlError::AccessDenied);
        }

        {
            let opened = match lock(&iface.opened_devices) {
                Ok(o) => o,
                Err(e) => return e,
            };
            if opened.contains(id) {
                return fail(GentlError::ResourceInUse);
            }
        }
        let device = match open_u3v_device(&iface, &entry, access) {
            Ok(d) => d,
            Err(e) => return fail(e),
        };
        match lock(&iface.opened_devices) {
            Ok(mut o) => {
                o.insert(id.to_owned());
            }
            Err(e) => return e,
        };
        unsafe { *ph_device = to_handle(device) };
        GC_ERR_SUCCESS
    })
}

/// Open the USB device, collect interfaces, bring up the GenCP control
/// channel and read the bootstrap registers. Backend-agnostic: opens via
/// the platform discovery module, classifies endpoints via
/// `usbio::InterfaceSet`, and transacts through `UsbPipe`s — the same
/// control-plane flow on WinUSB and libusb.
fn open_u3v_device(
    iface: &Arc<U3vInterface>,
    entry: &types::FoundDevice,
    access: DeviceAccess,
) -> GentlResult<Arc<U3vDevice>> {
    let opened = discovery::open_device_all(&entry.instance_id);
    let ifaces = usbio::InterfaceSet::collect(opened)?;
    let out_pipe = ifaces.control_out.clone().ok_or(GentlError::Io(
        "no Device Control Interface bulk OUT endpoint".into(),
    ))?;
    let in_pipe = ifaces.control_in.clone().ok_or(GentlError::Io(
        "no Device Control Interface bulk IN endpoint".into(),
    ))?;
    let mut client = control::GenCpClient::new(out_pipe, in_pipe);
    let bootstrap = control::read_bootstrap(&mut client)?;
    Ok(Arc::new(U3vDevice {
        id: entry.id.clone(),
        identity: entry.identity.clone(),
        access,
        // Drop order matters: the interface set owns every backend
        // handle (see cti.rs), the client only holds endpoint pipes.
        ifaces,
        client: Arc::new(std::sync::Mutex::new(client)),
        bootstrap,
        opened_streams: std::sync::Mutex::new(std::collections::HashSet::new()),
        parent: Arc::downgrade(iface),
    }))
}

// Device functions ------------------------------------------------------------

fn device_from(h_device: DevHandle) -> Result<Arc<U3vDevice>, GCError> {
    let device = unsafe { from_handle::<U3vDevice>(h_device) }.map_err(fail)?;
    parent_alive(&device.parent)?;
    Ok(device)
}

#[no_mangle]
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

fn device_info(
    device: &U3vDevice,
    i_info_cmd: DeviceInfoCmd,
    pi_type: *mut InfoDataType,
    p_buffer: *mut core::ffi::c_void,
    pi_size: *mut usize,
) -> GCError {
    let ident = &device.identity;
    unsafe {
        match i_info_cmd {
            DEVICE_INFO_ID => info_string(pi_type, p_buffer, pi_size, &device.id),
            DEVICE_INFO_VENDOR => info_string(pi_type, p_buffer, pi_size, &ident.manufacturer),
            DEVICE_INFO_MODEL => info_string(pi_type, p_buffer, pi_size, &ident.model),
            DEVICE_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_U3V),
            DEVICE_INFO_DISPLAYNAME => info_string(
                pi_type,
                p_buffer,
                pi_size,
                &display_name(&ident.manufacturer, &ident.model, &device.id),
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
                info_string(pi_type, p_buffer, pi_size, &ident.user_defined_name)
            }
            DEVICE_INFO_SERIAL_NUMBER => {
                info_string(pi_type, p_buffer, pi_size, &ident.serial_number)
            }
            DEVICE_INFO_VERSION => info_string(pi_type, p_buffer, pi_size, &ident.device_version),
            DEVICE_INFO_TIMESTAMP_FREQUENCY => {
                let v: u64 = 1_000_000_000; // U3V timestamps are ns
                info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
            }
            _ => not_implemented("DevGetInfo cmd"),
        }
    }
}

#[no_mangle]
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

#[no_mangle]
pub extern "C" fn DevClose(h_device: DevHandle) -> GCError {
    guard(|| {
        let device = module!(h_device, U3vDevice);
        device.close();
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

fn ds_from(h_ds: DSHandle) -> Result<Arc<U3vDataStream>, GCError> {
    let ds = unsafe { from_handle::<U3vDataStream>(h_ds) }.map_err(fail)?;
    parent_alive(&ds.parent)?;
    Ok(ds)
}

/// A buffer handle is only valid on the stream it was announced to.
fn ds_buffer_from(
    ds: &Arc<U3vDataStream>,
    h_buffer: BufferHandle,
) -> Result<Arc<U3vBuffer>, GCError> {
    let buf = unsafe { from_handle::<U3vBuffer>(h_buffer) }.map_err(fail)?;
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

// Data stream functions (GenTL 1.6 §6.3.4.5, §6.3.5) --------------------------

#[no_mangle]
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
        // Number of Stream Channels from the SBRM (§4.2.2.6).
        unsafe { *pi_num_data_streams = device.bootstrap.number_of_stream_channels };
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
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
        if i_index >= device.bootstrap.number_of_stream_channels {
            return invalid_index();
        }
        let id = stream_id(i_index);
        unsafe { write_string(s_data_stream_id, pi_size, &id) }
    })
}

#[no_mangle]
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
        let Some(channel) =
            (0..device.bootstrap.number_of_stream_channels).find(|i| stream_id(*i) == id)
        else {
            return fail(GentlError::InvalidId);
        };
        {
            let opened = match lock(&device.opened_streams) {
                Ok(o) => o,
                Err(e) => return e,
            };
            if opened.contains(id) {
                return fail(GentlError::ResourceInUse);
            }
        }
        let stream = match U3vDataStream::open(&device, channel) {
            Ok(s) => s,
            Err(e) => return fail(e),
        };
        if let Ok(mut opened) = device.opened_streams.lock() {
            opened.insert(id.to_owned());
        }
        unsafe { *ph_data_stream = to_handle(stream) };
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
pub extern "C" fn DSClose(h_ds: DSHandle) -> GCError {
    guard(|| {
        let ds = module!(h_ds, U3vDataStream);
        ds.close();
        if let Some(device) = ds.parent.upgrade() {
            if let Ok(mut opened) = device.opened_streams.lock() {
                opened.remove(&stream_id(ds.channel_index));
            }
        }
        drop(ds);
        unsafe { free_handle(h_ds) };
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
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
                STREAM_INFO_ID => {
                    info_string(pi_type, p_buffer, pi_size, &stream_id(ds.channel_index))
                }
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
                    // SI Required Payload Size snapshot (§5.4.1.3).
                    let v = ds.core.expected_payload_size();
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                STREAM_INFO_IS_GRABBING => {
                    let v: u8 = ds.core.is_grabbing().into();
                    info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
                }
                STREAM_INFO_DEFINES_PAYLOADSIZE => {
                    // The U3V data stream reports the payload size the device
                    // requires (SIRM), so it "defines" it for the consumer.
                    let v: u8 = 1;
                    info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
                }
                STREAM_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_U3V),
                STREAM_INFO_NUM_CHUNKS_MAX => {
                    let v: usize = 0;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                STREAM_INFO_BUF_ANNOUNCE_MIN => {
                    let v: usize = 1;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                STREAM_INFO_BUF_ALIGNMENT => {
                    // U3V payload alignment comes from the SI Info register
                    // (§5.4.1.1); announced buffers must hold aligned image
                    // rows, 1 is always safe for the buffer base itself.
                    let v: usize = 1;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                _ => not_implemented("DSGetInfo cmd"),
            }
        }
    })
}

#[no_mangle]
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
        let module = Arc::new(U3vBuffer {
            stream: Arc::downgrade(&ds),
            id,
        });
        let handle = to_handle(module);
        ds.set_buffer_handle(id, handle);
        unsafe { *ph_buffer = handle };
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
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
        let module = Arc::new(U3vBuffer {
            stream: Arc::downgrade(&ds),
            id,
        });
        let handle = to_handle(module);
        ds.set_buffer_handle(id, handle);
        unsafe { *ph_buffer = handle };
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
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

#[no_mangle]
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

#[no_mangle]
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

#[no_mangle]
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
            // Roll the engine back; transport start cleans up partial SIRM state.
            let _ = ds.core.stop_acquisition();
            return fail(e);
        }
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
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

#[no_mangle]
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

#[no_mangle]
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
            incomplete: b.incomplete,
            meta: b.meta,
            block_id: b.block_id,
        }) {
            Some(s) => s,
            None => return fail(GentlError::InvalidHandle),
        };
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
                    // U3V leader timestamps are ns; the legacy tick field
                    // reports them as-is (tick frequency = 1 GHz).
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
                BUFFER_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_U3V),
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
                    // U3V leaders carry 32-bit PFNC values (§5.5.7).
                    let v = PIXELFORMAT_NAMESPACE_PFNC_32BIT;
                    info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
                }
                BUFFER_INFO_DELIVERED_IMAGEHEIGHT => match need_meta(&snap) {
                    Ok(m) => {
                        // The trailer's actual size_y was folded into meta.height.
                        let v = m.height as usize;
                        info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                    }
                    Err(e) => fail(e),
                },
                BUFFER_INFO_DELIVERED_CHUNKPAYLOADSIZE => {
                    let v: usize = 0;
                    info_value(pi_type, INFO_DATATYPE_SIZET, p_buffer, pi_size, &v)
                }
                BUFFER_INFO_CHUNKLAYOUTID => {
                    let v: u64 = 0;
                    info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
                }
                BUFFER_INFO_FILENAME => info_string(pi_type, p_buffer, pi_size, ""),
                BUFFER_INFO_PIXEL_ENDIANNESS => {
                    // U3V data blocks are little-endian end to end (R-75st).
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
                    &snap.filled_size,
                ),
                BUFFER_INFO_TIMESTAMP_NS => match need_meta(&snap) {
                    Ok(m) => {
                        let v = m.timestamp;
                        info_value(pi_type, INFO_DATATYPE_UINT64, p_buffer, pi_size, &v)
                    }
                    Err(e) => fail(e),
                },
                BUFFER_INFO_DATA_LARGER_THAN_BUFFER => {
                    let v: u8 = 0;
                    info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
                }
                BUFFER_INFO_CONTAINS_CHUNKDATA => {
                    let v: u8 = 0;
                    info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &v)
                }
                _ => not_implemented("DSGetBufferInfo cmd"),
            }
        }
    })
}

// Signaling functions (GenTL 1.6 §6.3.7) --------------------------------------

#[no_mangle]
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
        // Only data stream modules carry events in this producer.
        let ds = match unsafe { from_handle::<U3vDataStream>(h_event_src) } {
            Ok(d) => d,
            Err(_) => return not_implemented("GCRegisterEvent: event source module"),
        };
        if let Err(e) = parent_alive(&ds.parent) {
            return e;
        }
        if i_event_id != EVENT_NEW_BUFFER {
            // §6.3.7.6: NOT_IMPLEMENTED also covers event types not valid
            // for the module. Device-event delivery (EVENT_FEATURE_INVALIDATE
            // over the DEI) is a follow-up (notes/OPEN-QUESTIONS.md UQ7).
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

#[no_mangle]
pub extern "C" fn GCUnregisterEvent(h_event_src: EventSrcHandle, i_event_id: EventType) -> GCError {
    guard(|| {
        let ds = match unsafe { from_handle::<U3vDataStream>(h_event_src) } {
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

#[no_mangle]
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
            // delivered; for New Buffer the buffer leaves the output queue
            // and is not requeued (§6.3.7.2).
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

#[no_mangle]
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

#[no_mangle]
pub extern "C" fn EventFlush(h_event: EventHandle) -> GCError {
    guard(|| {
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

#[no_mangle]
pub extern "C" fn EventKill(h_event: EventHandle) -> GCError {
    guard(|| {
        let ev = module!(h_event, EventModule);
        ev.queue.kill_one();
        GC_ERR_SUCCESS
    })
}

// Port functions --------------------------------------------------------------

fn port_device(h_port: PortHandle) -> Result<Arc<U3vDevice>, GCError> {
    let port = unsafe { from_handle::<DevicePort>(h_port) }.map_err(fail)?;
    port.device
        .upgrade()
        .ok_or_else(|| fail(GentlError::InvalidHandle))
}

#[no_mangle]
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

#[no_mangle]
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
        let buf = unsafe { std::slice::from_raw_parts(p_buffer as *const u8, len) };
        if let Err(e) = device.write_mem(i_address, buf) {
            return fail(e);
        }
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
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

#[no_mangle]
pub extern "C" fn GCGetNumPortURLs(h_port: PortHandle, pi_num_urls: *mut u32) -> GCError {
    guard(|| {
        if pi_num_urls.is_null() {
            return fail(GentlError::InvalidParameter("piNumURLs"));
        }
        let device = match port_device(h_port) {
            Ok(d) => d,
            Err(e) => return e,
        };
        // One local URL when the manifest has a Device XML, else none.
        let ok = device.xml_url().is_ok();
        unsafe { *pi_num_urls = u32::from(ok) };
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
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
        let device = match port_device(h_port) {
            Ok(d) => d,
            Err(e) => return e,
        };
        if i_url_index != 0 || device.xml_url().is_err() {
            return invalid_index();
        }
        unsafe {
            match i_info_cmd {
                URL_INFO_URL => {
                    let url = match device.xml_url() {
                        Ok(u) => u,
                        Err(e) => return fail(e),
                    };
                    info_string(pi_type, p_buffer, pi_size, &url)
                }
                URL_INFO_SCHEME => {
                    let v: i32 = URL_SCHEME_LOCAL;
                    info_value(pi_type, INFO_DATATYPE_INT32, p_buffer, pi_size, &v)
                }
                _ => not_implemented("GCGetPortURLInfo cmd"),
            }
        }
    })
}

#[no_mangle]
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
        let ident = &device.identity;
        unsafe {
            match i_info_cmd {
                PORT_INFO_ID => info_string(pi_type, p_buffer, pi_size, &device.id),
                PORT_INFO_VENDOR => info_string(pi_type, p_buffer, pi_size, &ident.manufacturer),
                PORT_INFO_MODEL => info_string(pi_type, p_buffer, pi_size, &ident.model),
                PORT_INFO_TLTYPE => info_string(pi_type, p_buffer, pi_size, TL_TYPE_U3V),
                PORT_INFO_MODULE => info_string(pi_type, p_buffer, pi_size, "Device"),
                // U3V bootstrap registers and protocol fields are
                // little-endian (R-30cd).
                PORT_INFO_LITTLE_ENDIAN => {
                    info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &1u8)
                }
                PORT_INFO_BIG_ENDIAN => {
                    info_value(pi_type, INFO_DATATYPE_BOOL8, p_buffer, pi_size, &0u8)
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
                PORT_INFO_VERSION => info_string(pi_type, p_buffer, pi_size, &ident.device_version),
                PORT_INFO_PORTNAME => info_string(pi_type, p_buffer, pi_size, PORT_NAME_DEVICE),
                _ => not_implemented("GCGetPortInfo cmd"),
            }
        }
    })
}

// Parent queries (GenTL v1.4) -------------------------------------------------

#[no_mangle]
pub extern "C" fn IFGetParentTL(h_iface: IFHandle, ph_tl: *mut TLHandle) -> GCError {
    guard(|| {
        if ph_tl.is_null() {
            return fail(GentlError::InvalidParameter("phTL"));
        }
        unsafe { *ph_tl = ptr::null_mut() };
        let iface = module!(h_iface, U3vInterface);
        let Some(tl) = iface.parent.upgrade() else {
            return fail(GentlError::InvalidHandle);
        };
        unsafe { *ph_tl = to_handle(tl) };
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
pub extern "C" fn DevGetParentIF(h_device: DevHandle, ph_iface: *mut IFHandle) -> GCError {
    guard(|| {
        if ph_iface.is_null() {
            return fail(GentlError::InvalidParameter("phIface"));
        }
        unsafe { *ph_iface = ptr::null_mut() };
        let device = module!(h_device, U3vDevice);
        let Some(iface) = device.parent.upgrade() else {
            return fail(GentlError::InvalidHandle);
        };
        unsafe { *ph_iface = to_handle(iface) };
        GC_ERR_SUCCESS
    })
}

#[no_mangle]
pub extern "C" fn DSGetParentDev(h_ds: DSHandle, ph_device: *mut DevHandle) -> GCError {
    guard(|| {
        if ph_device.is_null() {
            return fail(GentlError::InvalidParameter("phDevice"));
        }
        unsafe { *ph_device = ptr::null_mut() };
        let ds = module!(h_ds, U3vDataStream);
        let Some(device) = ds.parent.upgrade() else {
            return fail(GentlError::InvalidHandle);
        };
        unsafe { *ph_device = to_handle(device) };
        GC_ERR_SUCCESS
    })
}

// Stubs (filled milestone by milestone) ---------------------------------------

macro_rules! stub {
    ($name:ident($($arg:ident: $ty:ty),* $(,)?)) => {
        #[no_mangle]
        pub extern "C" fn $name($($arg: $ty),*) -> GCError {
            guard(|| {
                $(let _ = $arg;)*
                not_implemented(stringify!($name))
            })
        }
    };
}

stub!(GCGetInfo(i_info_cmd: TlInfoCmd, pi_type: *mut InfoDataType, p_buffer: *mut core::ffi::c_void, pi_size: *mut usize));
stub!(EventGetDataInfo(h_event: EventHandle, p_in_buffer: *const core::ffi::c_void, i_in_size: usize, i_info_cmd: EventDataInfoCmd, pi_type: *mut InfoDataType, p_out_buffer: *mut core::ffi::c_void, pi_out_size: *mut usize));

/* GenTL v1.1 */
stub!(GCReadPortStacked(h_port: PortHandle, p_entries: *mut PortRegisterStackEntry, pi_num_entries: *mut usize));
stub!(GCWritePortStacked(h_port: PortHandle, p_entries: *mut PortRegisterStackEntry, pi_num_entries: *mut usize));

/* GenTL v1.3 */
stub!(DSGetBufferChunkData(h_ds: DSHandle, h_buffer: BufferHandle, p_chunk_data: *mut SingleChunkData, pi_num_chunks: *mut usize));

/* GenTL v1.4 */

/* GenTL v1.5 */
stub!(DSGetNumBufferParts(h_ds: DSHandle, h_buffer: BufferHandle, pi_num_parts: *mut u32));
stub!(DSGetBufferPartInfo(h_ds: DSHandle, h_buffer: BufferHandle, i_part_index: u32, i_info_cmd: BufferPartInfoCmd, pi_type: *mut InfoDataType, p_buffer: *mut core::ffi::c_void, pi_size: *mut usize));

/* GenTL v1.6 */
stub!(DSAnnounceCompositeBuffer(h_ds: DSHandle, i_num_segments: usize, pp_segments: *mut *mut core::ffi::c_void, pi_sizes: *mut usize, p_private: *mut core::ffi::c_void, ph_buffer: *mut BufferHandle));
stub!(DSGetBufferInfoStacked(h_ds: DSHandle, h_buffer: BufferHandle, p_info_stacked: *mut DSBufferInfoStacked, i_num_infos: usize));
stub!(DSGetBufferPartInfoStacked(h_ds: DSHandle, h_buffer: BufferHandle, p_info_stacked: *mut DSBufferPartInfoStacked, i_num_infos: usize));
stub!(DSGetNumFlows(h_ds: DSHandle, pi_num_flows: *mut u32));
stub!(DSGetFlowInfo(h_ds: DSHandle, i_flow_index: u32, i_info_cmd: FlowInfoCmd, pi_type: *mut InfoDataType, p_buffer: *mut core::ffi::c_void, pi_size: *mut usize));
stub!(DSGetNumBufferSegments(h_ds: DSHandle, h_buffer: BufferHandle, pi_num_segments: *mut u32));
stub!(DSGetBufferSegmentInfo(h_ds: DSHandle, h_buffer: BufferHandle, i_segment_index: u32, i_info_cmd: SegmentInfoCmd, pi_type: *mut InfoDataType, p_buffer: *mut core::ffi::c_void, pi_size: *mut usize));

// Tests -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_string_size_fit_too_small_and_query() {
        let mut buf = [0u8; 8];

        let mut size = buf.len();
        let r = unsafe { write_string(buf.as_mut_ptr() as *mut c_char, &mut size, "abc") };
        assert_eq!(r, GC_ERR_SUCCESS);
        assert_eq!(size, 4);
        assert_eq!(&buf[..4], b"abc\0");

        let mut size = 2;
        let r = unsafe { write_string(buf.as_mut_ptr() as *mut c_char, &mut size, "abcdef") };
        assert_eq!(r, GC_ERR_BUFFER_TOO_SMALL);
        assert_eq!(size, 7);

        let mut size = 0usize;
        let r = unsafe { write_string(ptr::null_mut(), &mut size, "hello") };
        assert_eq!(r, GC_ERR_SUCCESS);
        assert_eq!(size, 6);
    }

    /// The module contract ("panics must never cross this boundary"): an
    /// internal panic becomes GC_ERR_ERROR plus last-error text instead of
    /// unwinding into the consumer's C stack. A panic reaching an
    /// `extern "C"` frame aborts the whole process — "thread caused
    /// non-unwinding panic. aborting." / SIGABRT — which is how the
    /// 2026-10 CI run killed `cargo test -p gentl-u3v --lib`.
    #[test]
    fn guard_turns_an_entry_point_panic_into_gc_error() {
        assert_eq!(guard(|| GC_ERR_SUCCESS), GC_ERR_SUCCESS);
        assert_eq!(guard(|| panic!("boom in an entry point")), GC_ERR_ERROR);
        let (code, text) = last_error();
        assert_eq!(code, GC_ERR_ERROR);
        assert!(
            text.contains("boom in an entry point"),
            "last error text carries the panic payload, got {text:?}"
        );
    }

    /// Mechanically enforce that invariant for every export: a new
    /// `pub extern "C" fn` without `guard(|| ...)` only shows up as an
    /// abort on a machine that hits the panic, so the source is checked
    /// directly (this also covers the bodies the `stub!` macro generates).
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

    /// TL lifecycle, interface enumeration, and every invalid-handle/index/id
    /// path. One test on purpose: the TL module is process-wide. Machines
    /// without a USB host controller (a VM/CI runner) legitimately
    /// enumerate zero interfaces — only the hardware-independent checks
    /// run there.
    #[test]
    fn tl_lifecycle_and_error_paths() {
        GCCloseLib();
        let mut tl: TLHandle = ptr::null_mut();
        assert_eq!(TLOpen(&mut tl), GC_ERR_NOT_INITIALIZED);
        assert_eq!(GCInitLib(), GC_ERR_SUCCESS);

        assert_eq!(TLOpen(&mut tl), GC_ERR_SUCCESS);
        assert!(!tl.is_null());

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

        // TLTYPE "U3V".
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
        assert_eq!(unsafe { c_str(buf.as_ptr() as *const c_char) }, Some("U3V"));

        // At least one USB root hub => at least one interface. A machine
        // without a USB host controller (e.g. a CI runner VM) enumerates
        // zero interfaces — that is spec-correct; only the checks that
        // hold with no interface at all run in that case.
        let mut n = 0u32;
        assert_eq!(TLGetNumInterfaces(tl, &mut n), GC_ERR_SUCCESS);
        if n == 0 {
            eprintln!("no USB root hubs on this machine; skipping interface-dependent checks");
            let mut idbuf = [0u8; 256];
            let mut size = idbuf.len();
            assert_eq!(
                TLGetInterfaceID(tl, 0, idbuf.as_mut_ptr() as *mut c_char, &mut size),
                GC_ERR_INVALID_INDEX
            );
            let bad_id = b"USB\\ROOT_HUB99\\bogus\0".as_ptr() as *const c_char;
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
            let mut tl2: TLHandle = ptr::null_mut();
            assert_eq!(TLOpen(&mut tl2), GC_ERR_RESOURCE_IN_USE);
            assert_eq!(IFClose(tl), GC_ERR_INVALID_HANDLE);
            assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
            return;
        }

        // Interface ID round-trip and info.
        let mut idbuf = [0u8; 256];
        let mut size = idbuf.len();
        assert_eq!(
            TLGetInterfaceID(tl, 0, idbuf.as_mut_ptr() as *mut c_char, &mut size),
            GC_ERR_SUCCESS
        );
        let id = unsafe { c_str(idbuf.as_ptr() as *const c_char) }.expect("iface id");
        assert!(id.to_ascii_uppercase().starts_with("USB\\ROOT_HUB"));
        let mut size = 0usize;
        assert_eq!(
            TLGetInterfaceInfo(
                tl,
                idbuf.as_ptr() as *const c_char,
                INTERFACE_INFO_TLTYPE,
                &mut dtype,
                ptr::null_mut(),
                &mut size
            ),
            GC_ERR_SUCCESS
        );
        assert_eq!(size, 4, "\"U3V\" + NUL");
        let mut size = buf.len();
        assert_eq!(
            TLGetInterfaceInfo(
                tl,
                idbuf.as_ptr() as *const c_char,
                INTERFACE_INFO_TLTYPE,
                &mut dtype,
                buf.as_mut_ptr().cast(),
                &mut size
            ),
            GC_ERR_SUCCESS
        );
        assert_eq!(unsafe { c_str(buf.as_ptr() as *const c_char) }, Some("U3V"));

        // Out-of-range index, unknown ID, second open.
        let mut size = idbuf.len();
        assert_eq!(
            TLGetInterfaceID(tl, n, idbuf.as_mut_ptr() as *mut c_char, &mut size),
            GC_ERR_INVALID_INDEX
        );
        let bad_id = b"USB\\ROOT_HUB99\\bogus\0".as_ptr() as *const c_char;
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
        let mut tl2: TLHandle = ptr::null_mut();
        assert_eq!(TLOpen(&mut tl2), GC_ERR_RESOURCE_IN_USE);

        // Open/close the interface; device list update works with no
        // cameras attached (returns success, zero devices).
        let mut iface: IFHandle = ptr::null_mut();
        assert_eq!(
            TLOpenInterface(tl, idbuf.as_ptr() as *const c_char, &mut iface),
            GC_ERR_SUCCESS
        );
        assert!(!iface.is_null());
        let mut changed = 2u8;
        assert_eq!(IFUpdateDeviceList(iface, &mut changed, 100), GC_ERR_SUCCESS);
        assert!(changed <= 1);
        let mut ndev = 99u32;
        assert_eq!(IFGetNumDevices(iface, &mut ndev), GC_ERR_SUCCESS);
        // No assertion on the count: a U3V camera may or may not be
        // attached; both results are spec-correct.
        let _ = ndev;
        // Parent query: a fresh boxed handle to the same TL module.
        let mut parent: TLHandle = ptr::null_mut();
        assert_eq!(IFGetParentTL(iface, &mut parent), GC_ERR_SUCCESS);
        assert!(!parent.is_null());
        assert!(unsafe { from_handle::<TlSystem>(parent) }.is_ok());
        // Wrong-typed handle is rejected without closing the TL.
        assert_eq!(IFClose(tl), GC_ERR_INVALID_HANDLE);
        assert_eq!(IFClose(iface), GC_ERR_SUCCESS);

        assert_eq!(TLClose(tl), GC_ERR_SUCCESS);
        let mut tl3: TLHandle = ptr::null_mut();
        assert_eq!(TLOpen(&mut tl3), GC_ERR_SUCCESS);
        assert_eq!(TLClose(tl3), GC_ERR_SUCCESS);
    }
}
