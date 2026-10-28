//! USB device discovery on Linux/Unix via rusb/libusb (Phase B,
//! notes/PENDING.md "Linux 可移植"): U3V matching per spec §3 at the
//! descriptor level — Linux sees the real descriptors, so the Windows
//! two-level registry dance (Q12: device-level compatible ID, then
//! interface-child confirmation) collapses into one check. Device IDs:
//! the Device Info GUID when the device opens (R-1cd), else a
//! session-scoped `VID:PID:bus:address` fallback with ACCESS_STATUS =
//! NOACCESS (QoI — the serial string is unreadable without opening;
//! notes/OPEN-QUESTIONS.md Q37).

use std::sync::Arc;

use gentl_core::{GentlError, GentlResult};
use rusb::{Device, UsbContext as _};

use crate::descriptors::{
    device_info_descriptor, DESC_CONFIGURATION, DESC_INTERFACE, USB_CLASS_MISC,
    USB_DEVICE_SUBCLASS_U3V, USB_INTERFACE_SUBCLASS_U3V,
};
use crate::types::{DeviceIdentity, Enumeration, FoundDevice, RootHubInfo};
use crate::usbio::libusb::{usb_context, LibUsbDevice, UsbHandle};

/// One GenTL Interface per USB bus that currently has devices on it.
///
/// QoI (Q37): libusb enumerates devices, not hubs, and there is no
/// host-side root-hub devnode registry like Windows' CM — presenting one
/// interface per present bus is the closest mapping of the UQ3
/// "one interface per root hub" model. Interface IDs stay in the
/// `USB\ROOT_HUB30\...` family so consumers see the same shape.
pub fn list_root_hubs() -> Vec<RootHubInfo> {
    let Some(context) = usb_context() else {
        // No libusb context: no USB stack on this host (container, CI
        // runner, WSL without usbipd). An empty list is the honest answer
        // and keeps every caller on the "no devices" path.
        return Vec::new();
    };
    let mut buses: Vec<u8> = match context.devices() {
        Ok(list) => list.iter().map(|d| d.bus_number()).collect(),
        Err(_) => return Vec::new(),
    };
    buses.sort_unstable();
    buses.dedup();
    buses
        .into_iter()
        .map(|b| RootHubInfo {
            instance_id: format!("USB\\ROOT_HUB30\\BUS{b}"),
            display: format!("USB Bus {b}"),
        })
        .collect()
}

fn root_hub_id(bus: u8) -> String {
    format!("USB\\ROOT_HUB30\\BUS{bus}")
}

/// Enumerate U3V devices on the whole bus. Per-device failures are
/// counted in `skipped`; enumeration itself never fails.
pub fn enumerate_devices() -> Enumeration {
    let mut out = Enumeration::default();
    let Some(context) = usb_context() else {
        return out;
    };
    let Ok(list) = context.devices() else {
        return out;
    };
    for dev in list.iter() {
        let Ok(dd) = dev.device_descriptor() else {
            out.skipped += 1;
            continue;
        };
        if !is_u3v_candidate(&dev, &dd) {
            continue; // not a U3V device: skip silently
        }
        let (vid, pid) = (dd.vendor_id(), dd.product_id());
        let (bus, addr) = (dev.bus_number(), dev.address());
        let instance_id = format!("{bus}:{addr}");
        let root_id = root_hub_id(bus);

        // Best effort: open and read the Device Info descriptor. A device
        // we cannot open (permissions, busy) still appears in the list
        // with NO_ACCESS, same policy as the Windows backend.
        match read_identity(&instance_id) {
            Ok(identity) => {
                let id = identity
                    .guid
                    .clone()
                    .unwrap_or_else(|| fallback_id(vid, pid, &instance_id));
                out.devices.push(FoundDevice {
                    id,
                    instance_id,
                    root_id,
                    vid,
                    pid,
                    openable: true,
                    identity,
                });
            }
            Err(_) => {
                out.devices.push(FoundDevice {
                    id: fallback_id(vid, pid, &instance_id),
                    instance_id,
                    root_id,
                    vid,
                    pid,
                    openable: false,
                    identity: DeviceIdentity::default(),
                });
            }
        }
    }
    out
}

/// Session-scoped fallback device ID (Q37): Windows falls back to the
/// serial embedded in the CM instance ID; the Linux serial string is
/// unreadable without opening, so bus:address distinguishes siblings
/// until the GUID can be read.
fn fallback_id(vid: u16, pid: u16, instance_id: &str) -> String {
    format!("{vid:04X}:{pid:04X}:{instance_id}")
}

/// U3V matching per §3, descriptor level:
/// - non-composite device declaring class EF/02 (Table 3-1), or
/// - composite (device class 0) with an interface declaring EF/05
///   (Tables 3-3/3-6/3-8).
///
/// Unlike the Windows registry path (Q12), no open is needed for
/// matching: libusb reads the configuration descriptor directly.
fn is_u3v_candidate(dev: &Device<rusb::Context>, dd: &rusb::DeviceDescriptor) -> bool {
    if dd.class_code() == USB_CLASS_MISC && dd.sub_class_code() == USB_DEVICE_SUBCLASS_U3V {
        return true;
    }
    if dd.class_code() != 0 {
        return false;
    }
    dev.active_config_descriptor()
        .map(|cfg| {
            cfg.interfaces().any(|i| {
                i.descriptors().next().is_some_and(|d| {
                    d.class_code() == USB_CLASS_MISC
                        && d.sub_class_code() == USB_INTERFACE_SUBCLASS_U3V
                })
            })
        })
        .unwrap_or(false)
}

/// Owned libusb handle for one opened device. Interface claims and the
/// kernel-driver handoff live in the shared `LibUsbDevice` owner.
pub struct OpenedDevice {
    inner: Arc<LibUsbDevice>,
}

impl OpenedDevice {
    pub(crate) fn into_libusb(self) -> Arc<LibUsbDevice> {
        self.inner
    }

    fn handle(&self) -> &UsbHandle {
        self.inner.handle()
    }
}

/// Open every interface path of the instance. libusb opens the whole
/// device in one handle (interfaces are claimed per-interface inside
/// [`crate::usbio::InterfaceSet::collect`]), so this is a single-element
/// vec kept for API symmetry with the Windows backend (per-MI handles).
pub fn open_device_all(instance_id: &str) -> Vec<OpenedDevice> {
    open_device(instance_id).into_iter().collect()
}

/// Open the device identified by the `<bus>:<address>` instance ID.
pub fn open_device(instance_id: &str) -> GentlResult<OpenedDevice> {
    let (bus, addr) = parse_bus_addr(instance_id).ok_or(GentlError::InvalidId)?;
    let context = usb_context()
        .ok_or_else(|| GentlError::Io("libusb context unavailable on this host".to_string()))?;
    let list = context
        .devices()
        .map_err(|e| GentlError::Io(format!("libusb device list: {e}")))?;
    let dev = list
        .iter()
        .find(|d| d.bus_number() == bus && d.address() == addr)
        .ok_or(GentlError::InvalidId)?;
    let handle = dev
        .open()
        .map_err(|e| GentlError::Io(format!("open {instance_id}: {e}")))?;
    // usbfs has no WinUSB-style driver exclusivity: a kernel driver may
    // own an interface of a foreign/reset-bound device. Best-effort
    // auto-detach re-attaches it when the interface is released (QoI).
    let _ = handle.set_auto_detach_kernel_driver(true);
    Ok(OpenedDevice {
        inner: Arc::new(LibUsbDevice::new(handle)),
    })
}

/// `<bus>:<address>` (both decimal u8) — this backend's instance ID.
fn parse_bus_addr(instance_id: &str) -> Option<(u8, u8)> {
    let (b, a) = instance_id.split_once(':')?;
    Some((b.parse().ok()?, a.parse().ok()?))
}

/// Open the device and read the Class-Specific Device Info Descriptor
/// plus its strings (same policy as the Windows backend: the device
/// stays listed even when the descriptor read fails — the caller marks
/// it NO_ACCESS).
fn read_identity(instance_id: &str) -> GentlResult<DeviceIdentity> {
    let opened = open_device(instance_id)?;
    let dev = opened.handle().device();
    let cfg = dev
        .active_config_descriptor()
        .map_err(|e| GentlError::Io(format!("config descriptor: {e}")))?;
    let info = device_info_descriptor(&config_blob(&cfg)).ok_or(GentlError::InvalidId)?;
    let h = opened.handle();
    let mut id = DeviceIdentity {
        guid: string_descriptor(h, info.i_device_guid),
        gen_cp_version: info.gen_cp_version,
        u3v_version: info.u3v_version,
        ..DeviceIdentity::default()
    };
    // The GUID is the device identity (R-1cd); without it the device
    // cannot be told apart from siblings reliably.
    if id.guid.is_none() {
        return Err(GentlError::InvalidId);
    }
    id.manufacturer = string_descriptor(h, info.i_vendor_name).unwrap_or_default();
    id.model = string_descriptor(h, info.i_model_name).unwrap_or_default();
    id.family = string_descriptor(h, info.i_family_name).unwrap_or_default();
    id.device_version = string_descriptor(h, info.i_device_version).unwrap_or_default();
    id.manufacturer_info = string_descriptor(h, info.i_manufacturer_info).unwrap_or_default();
    id.serial_number = string_descriptor(h, info.i_serial_number).unwrap_or_default();
    id.user_defined_name = string_descriptor(h, info.i_user_defined_name).unwrap_or_default();
    Ok(id)
}

/// Serialize rusb's parsed configuration descriptor into the raw blob
/// shape the shared parser consumes. QoI: libusb exposes parsed
/// descriptors (IADs are not surfaced at all), so the blob is
/// synthesized — config header + each alt-0 interface descriptor + its
/// class-specific tail (`extra()`). That is exactly what
/// `parse_config_descriptor` needs; the U3V Device Info descriptor sits
/// right after the control interface descriptor (R-15cd), inside
/// `extra()`.
fn config_blob(cfg: &rusb::ConfigDescriptor) -> Vec<u8> {
    let mut b = vec![0u8; 9];
    b[0] = 9;
    b[1] = DESC_CONFIGURATION;
    for iface in cfg.interfaces() {
        let Some(d) = iface.descriptors().next() else {
            continue;
        };
        b.extend_from_slice(&[
            9,
            DESC_INTERFACE,
            d.interface_number(),
            d.setting_number(),
            d.num_endpoints(),
            d.class_code(),
            d.sub_class_code(),
            d.protocol_code(),
            0,
        ]);
        b.extend_from_slice(d.extra());
    }
    let total = b.len() as u16;
    b[2] = total as u8;
    b[3] = (total >> 8) as u8;
    b
}

/// Read USB string descriptor `index` (ASCII view, first language).
fn string_descriptor(h: &UsbHandle, index: u8) -> Option<String> {
    if index == 0 {
        return None;
    }
    h.read_string_descriptor_ascii(index)
        .ok()
        .map(|s| s.trim_end_matches('\u{0}').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_addr_roundtrip() {
        assert_eq!(parse_bus_addr("1:4"), Some((1, 4)));
        assert_eq!(parse_bus_addr("255:255"), Some((255, 255)));
        assert_eq!(parse_bus_addr("1"), None);
        assert_eq!(parse_bus_addr("1:x"), None);
        assert_eq!(parse_bus_addr("300:1"), None, "u8 range");
        assert_eq!(format!("{:?}", parse_bus_addr("3:12").unwrap()), "(3, 12)");
    }

    /// The synthesized root-hub ID and the per-device root_id must agree
    /// (IFOpenDevice filters devices by interface root_id).
    #[test]
    fn root_hub_id_matches_listing_shape() {
        assert_eq!(root_hub_id(2), "USB\\ROOT_HUB30\\BUS2");
        assert!(list_root_hubs()
            .iter()
            .all(|r| r.instance_id.starts_with("USB\\ROOT_HUB30\\BUS")));
    }
}
