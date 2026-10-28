//! libusb (rusb) backend (Linux/Unix): synchronous bulk transfers on
//! claimed interfaces. Piece framing correctness comes from the
//! OS-independent UQ16 accumulation layer in mod.rs — usbfs/libusb sync
//! bulk completes a request on buffer-full or short packet, the same
//! completion contract RAW_IO documents on Windows, so the receiver loop
//! runs unchanged.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gentl_core::{GentlError, GentlResult};
use rusb::{ConfigDescriptor, DeviceHandle, GlobalContext, TransferType};

use crate::descriptors::{
    USB_CLASS_MISC, USB_INTERFACE_SUBCLASS_U3V, USB_PROTOCOL_CONTROL, USB_PROTOCOL_EVENT,
    USB_PROTOCOL_STREAMING,
};
use crate::discovery::OpenedDevice;
use crate::usbio::UsbPipe;

/// Shared device-handle owner: keeps the libusb handle alive for every
/// pipe, and releases the claimed interfaces (reverse order) on drop.
/// `auto_detach_kernel_driver` was enabled at open, so releasing also
/// re-attaches whatever kernel driver owned an interface.
pub(crate) struct LibUsbDevice {
    handle: DeviceHandle<GlobalContext>,
    claimed: Mutex<Vec<u8>>,
}

impl LibUsbDevice {
    pub(crate) fn new(handle: DeviceHandle<GlobalContext>) -> Self {
        Self {
            handle,
            claimed: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn handle(&self) -> &DeviceHandle<GlobalContext> {
        &self.handle
    }

    fn claim(&self, interface: u8) -> GentlResult<()> {
        self.handle
            .claim_interface(interface)
            .map_err(|e| GentlError::Io(format!("claim interface {interface}: {e}")))?;
        self.claimed
            .lock()
            .map_err(|_| GentlError::Io("claimed list lock poisoned".into()))?
            .push(interface);
        Ok(())
    }
}

impl Drop for LibUsbDevice {
    fn drop(&mut self) {
        if let Ok(mut v) = self.claimed.lock() {
            for i in v.drain(..).rev() {
                let _ = self.handle.release_interface(i);
            }
        }
    }
}

/// All U3V interfaces of an opened device with their bulk pipes.
///
/// Interfaces are claimed at collect time (descriptor order kept for the
/// stream endpoints, CR-123st: stream index = SIRM order). The event
/// interface is claimed too — GenTL event delivery over the DEI is a
/// follow-up (notes/OPEN-QUESTIONS.md UQ7), but claiming keeps the
/// endpoint owned by this producer instead of racing a later claim.
pub struct InterfaceSet {
    device: Arc<LibUsbDevice>,
    pub control_out: Option<Arc<dyn UsbPipe>>,
    pub control_in: Option<Arc<dyn UsbPipe>>,
    pub event_in: Option<Arc<dyn UsbPipe>>,
    pub stream_in: Vec<Arc<dyn UsbPipe>>,
}

impl InterfaceSet {
    pub fn collect(owners: Vec<OpenedDevice>) -> GentlResult<Self> {
        // libusb: one handle owns all interfaces; extra entries would be
        // duplicate opens of the same device (Windows opens one handle
        // per MI interface instead).
        let opened = owners.into_iter().next().ok_or_else(|| {
            GentlError::Io("device has no openable interface".into())
        })?;
        let device = opened.into_libusb();
        let cfg = active_config(&device)?;
        let mut set = Self {
            device,
            control_out: None,
            control_in: None,
            event_in: None,
            stream_in: Vec::new(),
        };
        for iface in cfg.interfaces() {
            let Some(desc) = iface.descriptors().next() else {
                continue; // no alt-0 descriptor: cannot happen on real devices
            };
            if desc.class_code() != USB_CLASS_MISC || desc.sub_class_code() != USB_INTERFACE_SUBCLASS_U3V {
                continue;
            }
            set.device.claim(desc.interface_number())?;
            for ep in desc
                .endpoint_descriptors()
                .filter(|ep| ep.transfer_type() == TransferType::Bulk)
            {
                let (address, mps) = (ep.address(), ep.max_packet_size());
                let pipe: Arc<dyn UsbPipe> = Arc::new(LibUsbPipe {
                    device: set.device.clone(),
                    endpoint: address,
                    mps,
                });
                match (desc.protocol_code(), address & 0x80 != 0) {
                    (USB_PROTOCOL_CONTROL, true) => {
                        set.control_in.get_or_insert(pipe);
                    }
                    (USB_PROTOCOL_CONTROL, false) => {
                        set.control_out.get_or_insert(pipe);
                    }
                    (USB_PROTOCOL_EVENT, true) => {
                        set.event_in.get_or_insert(pipe);
                    }
                    (USB_PROTOCOL_STREAMING, true) => set.stream_in.push(pipe),
                    // e.g. a bulk OUT on the event/streaming interface:
                    // not a U3V endpoint (§3.2.6/§3.2.7), ignore.
                    _ => {}
                }
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
}

/// Active configuration descriptor of the opened device (libusb needs no
/// open handle for this, but having one makes the read reliable).
fn active_config(device: &Arc<LibUsbDevice>) -> GentlResult<ConfigDescriptor> {
    device
        .handle
        .device()
        .active_config_descriptor()
        .map_err(|e| GentlError::Io(format!("active config descriptor: {e}")))
}

/// libusb bulk endpoint implementing [`UsbPipe`].
struct LibUsbPipe {
    device: Arc<LibUsbDevice>,
    endpoint: u8,
    mps: u16,
}

impl UsbPipe for LibUsbPipe {
    fn read_bulk(&self, buf: &mut [u8], timeout: Duration) -> GentlResult<u32> {
        match self
            .device
            .handle
            .read_bulk(self.endpoint, buf, timeout)
        {
            Ok(n) => Ok(n as u32),
            Err(rusb::Error::Timeout) => Err(GentlError::Timeout),
            Err(e) => Err(GentlError::Io(format!(
                "libusb bulk IN 0x{:02x}: {e}",
                self.endpoint
            ))),
        }
    }

    fn write_bulk(&self, buf: &[u8], timeout: Duration) -> GentlResult<()> {
        match self
            .device
            .handle
            .write_bulk(self.endpoint, buf, timeout)
        {
            Ok(n) if n == buf.len() => Ok(()),
            Ok(n) => Err(GentlError::Io(format!(
                "libusb bulk OUT 0x{:02x} short: {n}/{}",
                self.endpoint,
                buf.len()
            ))),
            Err(rusb::Error::Timeout) => Err(GentlError::Timeout),
            Err(e) => Err(GentlError::Io(format!(
                "libusb bulk OUT 0x{:02x}: {e}",
                self.endpoint
            ))),
        }
    }

    fn flush(&self) {
        // UQ6 mapping: the WinUSB backend flushes a possibly stale IN
        // acknowledge here; usbfs cancels synchronous transfers on
        // timeout, so there is nothing stale to flush — the remaining
        // failure mode on Linux is an endpoint halt, whose recovery is
        // clear_halt (libusb's equivalent of the ENDPOINT_HALT
        // re-synchronization, §4.1.4.1.2).
        let _ = self.device.handle.clear_halt(self.endpoint);
    }

    fn set_raw_io(&self) -> GentlResult<()> {
        // No-op (see the trait doc): usbfs has no request-queuing bypass
        // policy; the UQ16 accumulation layer covers framing.
        Ok(())
    }

    fn max_packet(&self) -> u16 {
        self.mps
    }
}
