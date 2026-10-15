//! Shared WinUSB plumbing: overlapped pipe I/O with timeouts, interface
//! collection walking, and error text. Used by the control channel
//! (control.rs) and the streaming receiver (stream.rs).

use std::time::Duration;

use gentl_core::{GentlError, GentlResult};
use windows_sys::Win32::Devices::Usb::{
    WinUsb_AbortPipe, WinUsb_GetAssociatedInterface, WinUsb_GetOverlappedResult,
    WinUsb_QueryInterfaceSettings, WinUsb_QueryPipe, WinUsb_Free, WinUsb_ReadPipe, WinUsb_WritePipe,
    WinUsb_ResetPipe,
    WINUSB_INTERFACE_HANDLE, WINUSB_PIPE_INFORMATION, USB_INTERFACE_DESCRIPTOR,
    UsbdPipeTypeBulk,
};
use windows_sys::Win32::Foundation::{GetLastError, HANDLE, WAIT_OBJECT_0, WIN32_ERROR};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::discovery::{
    USB_CLASS_MISC, USB_INTERFACE_SUBCLASS_U3V, USB_PROTOCOL_CONTROL, USB_PROTOCOL_EVENT,
    USB_PROTOCOL_STREAMING,
};

/// USB pipe type for bulk endpoints.


pub fn last_error_text() -> String {
    let code = unsafe { GetLastError() };
    format!("win32 error {code}")
}

/// Owns a manual-reset event used for one overlapped operation at a time.
pub struct Overlapped {
    inner: OVERLAPPED,
    event: HANDLE,
}

impl Overlapped {
    pub fn new() -> GentlResult<Self> {
        // NULL name: unnamed event object (an empty string would create
        // a *named* object shared across calls).
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(GentlError::Io("CreateEvent".into()));
        }
        Ok(Self {
            inner: unsafe { std::mem::zeroed() },
            event,
        })
    }

    pub fn as_mut_ptr(&mut self) -> *mut OVERLAPPED {
        self.inner.hEvent = self.event;
        &mut self.inner
    }

    pub fn event(&self) -> HANDLE {
        self.event
    }

    /// Wait for the pending operation; false on timeout (operation still
    /// pending — cancel with `cancel_pending` before reusing).
    pub fn wait(&mut self, timeout: Duration) -> bool {
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let r = unsafe { WaitForSingleObject(self.event, ms) };
        r == WAIT_OBJECT_0
    }

    /// Bytes transferred of the completed operation. WinUSB documents
    /// `WinUsb_GetOverlappedResult` (the interface handle is an opaque
    /// context, not a kernel HANDLE).
    pub fn result(&mut self, winusb: WINUSB_INTERFACE_HANDLE) -> std::io::Result<u32> {
        let mut transferred: u32 = 0;
        let ok = unsafe {
            WinUsb_GetOverlappedResult(winusb, &self.inner, &mut transferred, 1)
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(transferred)
        }
    }
}

impl Drop for Overlapped {
    fn drop(&mut self) {
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(self.event);
        }
    }
}

/// Synchronous-style overlapped write of the whole buffer. WinUSB
/// returns ERROR_IO_PENDING for the overlapped path; we wait up to
/// `timeout`.
pub fn write_pipe(
    winusb: WINUSB_INTERFACE_HANDLE,
    pipe: u8,
    buf: &[u8],
    timeout: Duration,
) -> GentlResult<()> {
    let mut ov = Overlapped::new()?;
    let mut written: u32 = 0;
    let ok = unsafe {
        WinUsb_WritePipe(
            winusb,
            pipe,
            buf.as_ptr() as *mut u8,
            buf.len() as u32,
            &mut written,
            ov.as_mut_ptr(),
        )
    };
    let last: WIN32_ERROR = unsafe { GetLastError() };
    if ok == 0 {
        // ERROR_IO_PENDING (997) is the normal overlapped path.
        if last != 997 {
            return Err(GentlError::Io(format!("WinUsb_WritePipe: {last}")));
        }
        if !ov.wait(timeout) {
            cancel_pending(winusb, pipe, ov.event());
            return Err(GentlError::Timeout);
        }
        let n = ov
            .result(winusb)
            .map_err(|e| GentlError::Io(format!("WritePipe overlapped: {e}")))?;
        if n as usize != buf.len() {
            return Err(GentlError::Io(format!("WritePipe short: {n}/{}", buf.len())));
        }
    }
    Ok(())
}

/// Overlapped read into `buf`; returns the number of bytes transferred.
/// A timeout cancels the transfer before returning GentlError::Timeout.
pub fn read_pipe(
    winusb: WINUSB_INTERFACE_HANDLE,
    pipe: u8,
    buf: &mut [u8],
    timeout: Duration,
) -> GentlResult<u32> {
    let mut ov = Overlapped::new()?;
    let mut read: u32 = 0;
    let ok = unsafe {
        WinUsb_ReadPipe(
            winusb,
            pipe,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut read,
            ov.as_mut_ptr(),
        )
    };
    let last: WIN32_ERROR = unsafe { GetLastError() };
    if ok != 0 {
        return Ok(read); // completed inline (possible for 0-length)
    }
    if last != 997 {
        return Err(GentlError::Io(format!("WinUsb_ReadPipe: {last}")));
    }
    if !ov.wait(timeout) {
        cancel_pending(winusb, pipe, ov.event());
        return Err(GentlError::Timeout);
    }
    let n = ov
        .result(winusb)
        .map_err(|e| GentlError::Io(format!("ReadPipe overlapped: {e}")))?;
    Ok(n)
}

/// Cancel a stuck transfer so the pipe stays usable after a timeout.
fn cancel_pending(winusb: WINUSB_INTERFACE_HANDLE, pipe: u8, event: HANDLE) {
    unsafe {
        let _ = WinUsb_AbortPipe(winusb, pipe);
    }
    // The aborted overlapped op completes with ERROR_OPERATION_ABORTED;
    // drain the event so the next user of the pipe starts clean.
    let _ = unsafe { WaitForSingleObject(event, 1000) };
    // Audit B11: AbortPipe leaves the pipe in an indeterminate state;
    // WinUSB requires a reset before the pipe is reused.
    unsafe {
        let _ = WinUsb_ResetPipe(winusb, pipe);
    }
}


/// Enable RAW_IO on a streaming (bulk IN) pipe.
///
/// RAW_IO bypasses WinUSB's normal request-queuing layer and submits reads
/// directly to the USB core stack, eliminating the inter-request gaps that
/// some host controllers (laptops in particular) exhibit and that throttle
/// high-bandwidth streams (field-proven on a bulk UVC device; see
/// notes/OPEN-QUESTIONS.md UQ16 and notes/PENDING.md R1 — including the
/// request-length alignment contract exercised here).
///
/// Contract the receiver relies on (MSDN): with RAW_IO every request
/// length must be a multiple of the endpoint `wMaxPacketSize`; a request
/// still completes on buffer-full or short packet, so U3V piece
/// boundaries (each block piece is its own transfer, §5.2.1.2/§5.2.1.4)
/// keep their meaning via `actual_length`.
pub fn set_raw_io(handle: WINUSB_INTERFACE_HANDLE, pipe: u8) -> GentlResult<()> {
    use windows_sys::Win32::Devices::Usb::{RAW_IO, WinUsb_SetPipePolicy};
    let value: u8 = 1; // TRUE
    let ok = unsafe { WinUsb_SetPipePolicy(handle, pipe, RAW_IO, 1, &value as *const u8 as *const _) };
    if ok == 0 {
        return Err(GentlError::Io(format!(
            "WinUsb_SetPipePolicy(RAW_IO) pipe 0x{pipe:02x}: {}",
            last_error_text()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::align_request;

    #[test]
    fn align_request_rounds_to_mps() {
        assert_eq!(align_request(52, 1024), 1024); // leader
        assert_eq!(align_request(32, 1024), 1024); // trailer
        assert_eq!(align_request(1024, 1024), 1024);
        assert_eq!(align_request(1025, 1024), 2048); // odd final piece
        assert_eq!(align_request(1_048_576, 1024), 1_048_576); // full TS piece
        assert_eq!(align_request(100, 512), 512);
        assert_eq!(align_request(100, 1), 100); // degenerate mps
    }
}

/// One endpoint of a U3V interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeInfo {
    pub pipe_id: u8,
    pub is_in: bool,
    /// `wMaxPacketSize` of the endpoint (RAW_IO needs it for request
    /// alignment).
    pub max_packet: u16,
}

/// All interfaces of an opened U3V device with their bulk pipes.
///
/// The raw handles are owned here; every use goes through WinUSB calls
/// taking the pointer by value, and `Drop` frees them. The device
/// module holding this set must be `Send + Sync` (GenTL handles cross
/// threads; gige's `GevDevice` wraps the same kind of resource).
pub struct InterfaceSet {
    /// Default interface handle (owned by the caller's OpenedDevice).
    default: WINUSB_INTERFACE_HANDLE,
    /// Associated interfaces obtained via WinUsb_GetAssociatedInterface
    /// (must be freed with WinUsb_Free).
    associated: Vec<WINUSB_INTERFACE_HANDLE>,
    pub control_out: Option<(usize, u8, u16)>,
    pub control_in: Option<(usize, u8, u16)>,
    pub event_in: Option<(usize, u8, u16)>,
    pub stream_in: Vec<(usize, u8, u16)>,
}

impl InterfaceSet {
    /// Walk interface 0 and its associated interfaces, classifying bulk
    /// pipes by U3V interface protocol (§3.2.4/3.2.6/3.2.7). Stream
    /// interfaces keep descriptor order (CR-123st: stream index = SIRM
    /// order).
    pub fn collect(default: WINUSB_INTERFACE_HANDLE) -> GentlResult<Self> {
        let mut set = Self {
            default,
            associated: Vec::new(),
            control_out: None,
            control_in: None,
            event_in: None,
            stream_in: Vec::new(),
        };
        let mut handles = vec![default];
        for i in 0..16u8 {
            let mut h: WINUSB_INTERFACE_HANDLE = std::ptr::null_mut();
            let ok = unsafe { WinUsb_GetAssociatedInterface(default, i, &mut h) };
            if ok == 0 || h.is_null() {
                break;
            }
            set.associated.push(h);
            handles.push(h);
        }
        for (idx, h) in handles.iter().enumerate() {
            let Some((iface_desc, pipes)) = interface_pipes(*h) else {
                continue;
            };
            if iface_desc.bInterfaceClass != USB_CLASS_MISC
                || iface_desc.bInterfaceSubClass != USB_INTERFACE_SUBCLASS_U3V
            {
                continue;
            }
            match iface_desc.bInterfaceProtocol {
                USB_PROTOCOL_CONTROL => {
                    for p in pipes {
                        if p.is_in {
                            set.control_in.get_or_insert((idx, p.pipe_id, p.max_packet));
                        } else {
                            set.control_out.get_or_insert((idx, p.pipe_id, p.max_packet));
                        }
                    }
                }
                USB_PROTOCOL_EVENT => {
                    for p in pipes {
                        if p.is_in {
                            set.event_in.get_or_insert((idx, p.pipe_id, p.max_packet));
                        }
                    }
                }
                USB_PROTOCOL_STREAMING => {
                    for p in pipes {
                        if p.is_in {
                            set.stream_in.push((idx, p.pipe_id, p.max_packet));
                        }
                    }
                }
                _ => {}
            }
        }
        if set.control_out.is_none() || set.control_in.is_none() {
            return Err(GentlError::Io(
                "device lacks the mandatory Device Control Interface bulk endpoints (R-11cd)"
                    .into(),
            ));
        }
        Ok(set)
    }

    /// WinUSB handle for an interface index from this set.
    pub fn handle(&self, idx: usize) -> Option<WINUSB_INTERFACE_HANDLE> {
        if idx == 0 {
            Some(self.default)
        } else {
            self.associated.get(idx - 1).copied()
        }
    }
}

impl Drop for InterfaceSet {
    fn drop(&mut self) {
        for h in self.associated.drain(..) {
            unsafe {
                let _ = WinUsb_Free(h);
            }
        }
    }
}

unsafe impl Send for InterfaceSet {}
unsafe impl Sync for InterfaceSet {}

/// WinUSB interface handle + pipe id, moved into the receiver thread.
/// The stream joins the thread before the device module (which owns the
/// handle set) can be dropped.
#[derive(Clone, Copy)]
pub struct PipeHandle(pub WINUSB_INTERFACE_HANDLE, pub u8, pub u16);

/// Round a piece size up to the endpoint's maximum packet size, as RAW_IO
/// requires every request length to be a multiple of it.
pub fn align_request(size: u32, mps: u16) -> u32 {
    let mps = u32::from(mps.max(1));
    size.div_ceil(mps) * mps
}
unsafe impl Send for PipeHandle {}

fn interface_pipes(
    h: WINUSB_INTERFACE_HANDLE,
) -> Option<(USB_INTERFACE_DESCRIPTOR, Vec<PipeInfo>)> {
    let mut iface_desc: USB_INTERFACE_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let ok = unsafe { WinUsb_QueryInterfaceSettings(h, 0, &mut iface_desc) };
    if ok == 0 {
        return None;
    }
    let mut pipes = Vec::new();
    for i in 0..iface_desc.bNumEndpoints {
        let mut info: WINUSB_PIPE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { WinUsb_QueryPipe(h, 0, i, &mut info) };
        if ok == 0 {
            continue;
        }
        if info.PipeType != UsbdPipeTypeBulk {
            continue; // §3.2.4/3.2.6/3.2.7: U3V default interfaces use bulk only
        }
        pipes.push(PipeInfo {
            pipe_id: info.PipeId,
            is_in: info.PipeId & 0x80 != 0,
            max_packet: info.MaximumPacketSize,
        });
    }
    Some((iface_desc, pipes))
}
