//! USB device discovery: SetupAPI enumeration of USB root hubs (GenTL
//! Interfaces) and USB3 Vision device matching per U3V 1.2 §3.
//!
//! Matching rule, taken from the spec text (NOT the legacy 0xEF/0x02
//! interface codes — those are the *device-level* class codes, U3V 1.2
//! Table 3-1; interfaces use subclass 0x05, Tables 3-2..3-8):
//! - Device Descriptor: class EF / subclass 02 / protocol 01 (IAD present),
//!   visible in SetupAPI as compatible ID `USB\Class_EF&SubClass_02`.
//! - IAD: EF/05/00; Control interface: EF/05/00; Event: EF/05/01;
//!   Streaming: EF/05/02.
//! Devices we cannot open for descriptor reads still appear in the list
//! (access status NO_ACCESS); they are counted, never fatal.

use std::ffi::OsString;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;

use gentl_core::{GentlError, GentlResult};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_IDW, CM_Get_Parent, CM_Locate_DevNodeW, CR_SUCCESS,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceInterfaceDetailW,
    SetupDiGetDevicePropertyW, SetupDiGetDeviceRegistryPropertyW,
    HDEVINFO, SP_DEVICE_INTERFACE_DATA, SP_DEVINFO_DATA, SPDRP_COMPATIBLEIDS, SPDRP_HARDWAREID,
    DIGCF_ALLCLASSES, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
};
use windows_sys::Win32::Devices::Properties::{DEVPROPKEY, DEVPKEY_Device_BusReportedDeviceDesc, DEVPROPTYPE};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
};
use windows_sys::Win32::Devices::Usb::{
    WinUsb_Free, WinUsb_GetDescriptor, WinUsb_Initialize, WINUSB_INTERFACE_HANDLE,
};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};

/// SetupAPI "no device set" value (HDEVINFO is an isize in windows-sys).
const INVALID_HDEVINFO: HDEVINFO = -1isize;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};

use crate::descriptors::{
    device_info_descriptor, DESC_CONFIGURATION, DESC_STRING, USB_CLASS_MISC,
    USB_DEVICE_SUBCLASS_U3V, USB_INTERFACE_SUBCLASS_U3V,
};
use crate::types::{DeviceIdentity, Enumeration, FoundDevice, RootHubInfo};
use crate::usbio;

/// SetupAPI compatible-ID prefix identifying the U3V device-level class
/// codes (Table 3-1: EF/02 because an IAD is present).
fn class_compat_prefix() -> String {
    format!(
        "USB\\Class_{:02X}&SubClass_{:02X}",
        USB_CLASS_MISC, USB_DEVICE_SUBCLASS_U3V
    )
}
/// Hardware-ID prefix of USB root hubs.
const ROOT_HUB_PREFIX: &str = "USB\\ROOT_HUB";

fn wide(s: &str) -> Vec<u16> {
    OsString::from(s).as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn wide_to_string(p: &[u16]) -> String {
    let end = p.iter().position(|&c| c == 0).unwrap_or(p.len());
    String::from_utf16_lossy(&p[..end])
}

/// Read a REG_SZ (or REG_MULTI_SZ) SetupAPI device property as a String.
fn dev_reg_string(info: HDEVINFO, data: &SP_DEVINFO_DATA, prop: u32) -> Option<String> {
    let mut buf = vec![0u16; 512];
    let mut reg_type: u32 = 0;
    let mut needed: u32 = 0;
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            info,
            data,
            prop,
            &mut reg_type,
            buf.as_mut_ptr() as *mut u8,
            (buf.len() * 2) as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        return None;
    }
    // REG_MULTI_SZ: join every segment until the terminating empty string
    // (a device class can hide in any position — the U3V composite marker
    // USB\COMPOSITE is typically LAST in the list).
    let s = wide_to_string_multi(&buf);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Decode a REG_MULTI_SZ buffer into one `|`-joined uppercase string.
fn wide_to_string_multi(buf: &[u16]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for seg in buf.split(|&c| c == 0) {
        if seg.is_empty() {
            break; // double NUL terminates the list
        }
        parts.push(String::from_utf16_lossy(seg));
    }
    parts.join("|")
}

fn dev_instance_id(info: HDEVINFO, data: &SP_DEVINFO_DATA) -> Option<String> {
    let mut buf = vec![0u16; 512];
    let mut needed: u32 = 0;
    let ok = unsafe {
        SetupDiGetDeviceInstanceIdW(info, data, buf.as_mut_ptr(), buf.len() as u32, &mut needed)
    };
    (ok != 0).then(|| wide_to_string(&buf))
}

/// DEVPKEY_Device_BusReportedDeviceDesc (friendly bus name like
/// "USB 根集线器(USB 3.0)").
fn bus_reported_desc(info: HDEVINFO, data: &SP_DEVINFO_DATA) -> Option<String> {
    let mut prop_type: DEVPROPTYPE = 0;
    let mut buf = vec![0u16; 512];
    let ok = unsafe {
        SetupDiGetDevicePropertyW(
            info,
            data,
            &DEVPKEY_Device_BusReportedDeviceDesc as *const DEVPROPKEY,
            &mut prop_type,
            buf.as_mut_ptr() as *mut u8,
            (buf.len() * 2) as u32,
            std::ptr::null_mut(),
            0,
        )
    };
    (ok != 0 && prop_type == 1 /* DEVPROP_TYPE_STRING */).then(|| wide_to_string(&buf))
}

/// Enumerate all present USB root hubs.
pub fn list_root_hubs() -> Vec<RootHubInfo> {
    let mut out = Vec::new();
    let usb = wide("USB");
    unsafe {
        let info = SetupDiGetClassDevsW(
            std::ptr::null(),
            usb.as_ptr(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_ALLCLASSES,
        );
        if info == INVALID_HDEVINFO {
            return out;
        }
        let mut idx = 0u32;
        loop {
            let mut data: SP_DEVINFO_DATA = std::mem::zeroed();
            data.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiEnumDeviceInfo(info, idx, &mut data) == 0 {
                break;
            }
            idx += 1;
            let Some(hw) = dev_reg_string(info, &data, SPDRP_HARDWAREID) else {
                continue;
            };
            if !hw.to_ascii_uppercase().starts_with(ROOT_HUB_PREFIX) {
                continue;
            }
            let Some(id) = dev_instance_id(info, &data) else {
                continue;
            };
            let display = bus_reported_desc(info, &data).unwrap_or_else(|| "USB Root Hub".into());
            out.push(RootHubInfo {
                instance_id: id,
                display,
            });
        }
        SetupDiDestroyDeviceInfoList(info);
    }
    out
}

/// Walk the CM devnode parents of `instance_id` until a USB root hub is
/// found; None when the chain cannot be walked (device vanished, access
/// denied deep in the tree).
pub fn root_of(instance_id: &str) -> Option<String> {
    let mut id = wide(instance_id);
    let mut node: u32 = 0;
    unsafe {
        if CM_Locate_DevNodeW(&mut node, id.as_mut_ptr(), 0) != CR_SUCCESS {
            return None;
        }
        for _ in 0..32 {
            let mut parent: u32 = 0;
            if CM_Get_Parent(&mut parent, node, 0) != CR_SUCCESS {
                return None;
            }
            node = parent;
            let mut buf = vec![0u16; 512];
            if CM_Get_Device_IDW(node, buf.as_mut_ptr(), buf.len() as u32, 0) != CR_SUCCESS {
                return None;
            }
            let cur = wide_to_string(&buf);
            if cur.to_ascii_uppercase().starts_with(ROOT_HUB_PREFIX) {
                return Some(cur);
            }
        }
    }
    None
}

fn parse_vid_pid(instance_id: &str) -> Option<(u16, u16)> {
    // Instance IDs look like "USB\VID_1234&PID_5678\serial".
    let mut parts = instance_id.split(['\\', '&']);
    let _bus = parts.next()?;
    let vid = parts.next()?.strip_prefix("VID_")?;
    let pid = parts.next()?.strip_prefix("PID_")?;
    Some((
        u16::from_str_radix(vid, 16).ok()?,
        u16::from_str_radix(pid, 16).ok()?,
    ))
}

fn instance_serial(instance_id: &str) -> Option<String> {
    let mut parts = instance_id.split('\\');
    let _ = parts.next()?;
    let _ = parts.next()?;
    let serial = parts.next()?;
    (!serial.is_empty()).then(|| serial.to_owned())
}

/// Every present USB device with its (uppercased) compatible IDs, as seen
/// through SPDRP_COMPATIBLEIDS. Used by enumeration and by the
/// device-interface path fallback (composite MI_* children).
pub(crate) fn all_usb_devices() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let usb = wide("USB");
    unsafe {
        let info = SetupDiGetClassDevsW(
            std::ptr::null(),
            usb.as_ptr(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_ALLCLASSES,
        );
        if info == INVALID_HDEVINFO {
            return out;
        }
        let mut idx = 0u32;
        loop {
            let mut data: SP_DEVINFO_DATA = std::mem::zeroed();
            data.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiEnumDeviceInfo(info, idx, &mut data) == 0 {
                break;
            }
            idx += 1;
            if let Some(id) = dev_instance_id(info, &data) {
                let compat = dev_reg_string(info, &data, SPDRP_COMPATIBLEIDS).unwrap_or_default();
                out.push((id, compat.to_ascii_uppercase()));
            }
        }
        SetupDiDestroyDeviceInfoList(info);
    }
    out
}

/// Enumerate U3V devices on the whole bus. Per-device failures are
/// counted in `skipped`; enumeration itself never fails.
pub fn enumerate_devices() -> Enumeration {
    let mut out = Enumeration::default();
    let usb = wide("USB");
    unsafe {
        let info = SetupDiGetClassDevsW(
            std::ptr::null(),
            usb.as_ptr(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_ALLCLASSES,
        );
        if info == INVALID_HDEVINFO {
            return out;
        }
        // Pass 1: collect every present USB device so composite parents can
        // be matched through their interface children (see below).
        let mut all: Vec<(String, String)> = Vec::new();
        let mut idx = 0u32;
        loop {
            let mut data: SP_DEVINFO_DATA = std::mem::zeroed();
            data.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiEnumDeviceInfo(info, idx, &mut data) == 0 {
                break;
            }
            idx += 1;
            let Some(instance_id) = dev_instance_id(info, &data) else {
                out.skipped += 1;
                continue;
            };
            if instance_id.to_ascii_uppercase().starts_with(ROOT_HUB_PREFIX) {
                continue;
            }
            let compat = dev_reg_string(info, &data, SPDRP_COMPATIBLEIDS).unwrap_or_default();
            all.push((instance_id, compat.to_ascii_uppercase()));
        }
        // U3V identification (spec §3.1/§3.2): the device-level class
        // EF/02 lives in the IAD, which Windows does NOT turn into a
        // registry compatible ID — composite devices keep DevClass_00
        // (observed on a Hikvision U3V camera, 2026-10-13). Match:
        //   a) devices directly declaring Class_EF&SubClass_02, or
        //   b) composite devices having an interface child that declares
        //      the U3V control interface class EF/05 (Table 3-3).
        // Final confirmation stays descriptor-based after open (§3.2.5).
        let has_u3v_iface_child = |parent: &str| {
            let parent = parent.to_ascii_uppercase();
            all.iter().any(|(child_id, child_compat)| {
                child_id.contains("&MI_")
                    && direct_parent(child_id).map(|p| p.to_ascii_uppercase()) == Some(parent.clone())
                    && child_compat.contains("CLASS_EF&SUBCLASS_05")
            })
        };
        for (instance_id, compat) in &all {
            let is_candidate = compat.contains(&class_compat_prefix())
                || (compat.contains("USB\\COMPOSITE") && has_u3v_iface_child(&instance_id));
            if !is_candidate {
                continue; // not a U3V device: skip silently
            }
            let Some((vid, pid)) = parse_vid_pid(&instance_id) else {
                out.skipped += 1;
                continue;
            };
            let Some(root_id) = root_of(&instance_id) else {
                out.skipped += 1;
                continue;
            };

            // Best effort: open and read the Device Info descriptor. A
            // device bound to a foreign/no driver cannot be opened; it
            // still appears in the list with NO_ACCESS.
            match read_identity(&instance_id) {
                Ok(identity) => {
                    let id = identity.guid.clone().unwrap_or_else(|| {
                        format!("{vid:04X}:{pid:04X}:{}", instance_serial(&instance_id).unwrap_or_default())
                    });
                    out.devices.push(FoundDevice {
                        id,
                        instance_id: instance_id.clone(),
                        root_id,
                        vid,
                        pid,
                        openable: true,
                        identity,
                    });
                }
                Err(_) => {
                    out.devices.push(FoundDevice {
                        id: format!(
                            "{vid:04X}:{pid:04X}:{}",
                            instance_serial(instance_id).unwrap_or_default()
                        ),
                        instance_id: instance_id.clone(),
                        root_id,
                        vid,
                        pid,
                        openable: false,
                        identity: DeviceIdentity::default(),
                    });
                }
            }
        }
        SetupDiDestroyDeviceInfoList(info);
    }
    out
}

/// One-hop CM parent of a device instance ID (e.g. the composite device
/// owning an interface child). Instance suffixes differ between parent and
/// child in the ID strings, so string surgery cannot derive this — only
/// the Configuration Manager knows the real topology.
fn direct_parent(instance_id: &str) -> Option<String> {
    let mut id = wide(instance_id);
    let mut node: u32 = 0;
    unsafe {
        if CM_Locate_DevNodeW(&mut node, id.as_mut_ptr(), 0) != CR_SUCCESS {
            return None;
        }
        let mut parent: u32 = 0;
        if CM_Get_Parent(&mut parent, node, 0) != CR_SUCCESS {
            return None;
        }
        let mut buf = vec![0u16; 512];
        if CM_Get_Device_IDW(parent, buf.as_mut_ptr(), buf.len() as u32, 0) != CR_SUCCESS {
            return None;
        }
        Some(wide_to_string(&buf))
    }
}




/// Open the device and read the Class-Specific Device Info Descriptor
/// plus its strings. Fails when the device cannot be opened or does not
/// confirm as U3V (spec-confirmed matching, §3).
fn read_identity(instance_id: &str) -> GentlResult<DeviceIdentity> {
    let dev = open_device(instance_id)?;
    let result = read_identity_from_handle(dev.winusb);
    // Whether or not the descriptor read worked, the device answered: it
    // stays listed. A non-U3V foreign device with the same class codes is
    // dropped here (counted as skipped by the caller).
    result
}

/// Owned WinUSB handle pair for one opened device.
pub struct OpenedDevice {
    pub file: HANDLE,
    pub winusb: WINUSB_INTERFACE_HANDLE,
}

impl OpenedDevice {
    /// Wrap the raw pair; the caller must not free either handle.
    pub unsafe fn from_raw(file: HANDLE, winusb: WINUSB_INTERFACE_HANDLE) -> Self {
        Self { file, winusb }
    }
}

impl Drop for OpenedDevice {
    fn drop(&mut self) {
        unsafe {
            if !self.winusb.is_null() {
                let _ = WinUsb_Free(self.winusb);
            }
            if !self.file.is_null() {
                let _ = CloseHandle(self.file);
            }
        }
    }
}

// The handles are owned solely by this struct; every use goes through
// WinUSB/CloseHandle calls that take the raw pointer by value. The
// device module behind the GenTL handle must be Send + Sync (gige's
// GevDevice holds the same kind of raw resources via GvcpClient).
unsafe impl Send for OpenedDevice {}
unsafe impl Sync for OpenedDevice {}

/// Open EVERY device interface path of the instance (one per MI_xN child
/// of a composite device). Zadig-style per-interface WinUSB bindings are
/// independent device nodes: WinUsb_GetAssociatedInterface sees none of
/// them, so each interface needs its own CreateFile+WinUSB handle.
pub fn open_device_all(instance_id: &str) -> Vec<OpenedDevice> {
    device_interface_paths(instance_id)
        .iter()
        .filter_map(|path| open_path(path).ok())
        .collect()
}

/// Open a USB device instance for overlapped I/O and initialize WinUSB
/// (interface 0 = default control interface, R-10cd).
pub fn open_device(instance_id: &str) -> GentlResult<OpenedDevice> {
    let paths = device_interface_paths(instance_id);
    if paths.is_empty() {
        return Err(GentlError::Io("no device interface".into()));
    }
    let mut last_err = String::from("no openable device interface");
    for path in &paths {
        match open_path(path) {
            Ok(dev) => return Ok(dev),
            Err(e) => last_err = format!("{path}: {e}"),
        }
    }
    Err(GentlError::Io(last_err))
}

fn open_path(path: &str) -> GentlResult<OpenedDevice> {
    let path_w = wide(path);
    let file = unsafe {
        CreateFileW(
            path_w.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(GentlError::Io(format!("open {path}: {}", usbio::last_error_text())));
    }
    let mut winusb: WINUSB_INTERFACE_HANDLE = std::ptr::null_mut();
    let ok = unsafe { WinUsb_Initialize(file, &mut winusb) };
    if ok == 0 {
        let e = usbio::last_error_text();
        unsafe {
            let _ = CloseHandle(file);
        }
        return Err(GentlError::Io(format!("WinUsb_Initialize: {e}")));
    }
    Ok(unsafe { OpenedDevice::from_raw(file, winusb) })
}

/// Collect every device-interface path of one devnode, across all
/// interface classes (`InterfaceClassGuid = NULL` enumerates every class).
/// Binding-dependent: WinUSB INFs typically register the interface on the
/// MI_00 child node only, while composite parents expose
/// `GUID_DEVINTERFACE_USB_DEVICE` — callers try them all.
/// GUID_DEVINTERFACE_WINUSB: registered by winusb.inf's WINUSB.NT.HW
/// (DeviceInterfaceGUIDs={F72FE0D4-...}) for every Needs=WINUSB.NT
/// binding. Verified against C:\Windows\INF\winusb.inf (2026-10-13).
const GUID_DEVINTERFACE_WINUSB: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0xf72fe0d4_cbcb_407d_8814_9ed673d0dd6b);

const GUID_DEVINTERFACE_WINUSB_REF: *const windows_sys::core::GUID =
    &GUID_DEVINTERFACE_WINUSB;


/// GUID_DEVINTERFACE_USB_DEVICE ({A5DCBF10-6530-11D2-901F-00C04FB951ED}).
const GUID_DEVINTERFACE_USB_DEVICE: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0xa5dcbf10_6530_11d2_901f_00c04fb951ed);

const GUID_DEVINTERFACE_USB_DEVICE_REF: *const windows_sys::core::GUID =
    &GUID_DEVINTERFACE_USB_DEVICE;

/// Enumerate device interfaces of `class_guid` GLOBALLY and keep those
/// owned by any of `instance_ids` (compared case-insensitively). Global
/// enumeration is required: per-devnode SetupDi filtering by instance ID
/// is not supported by SetupDiGetClassDevs (its second parameter matches
/// enumerator names, not instance IDs — a silent no-op).
fn interface_paths(instance_ids: &[String], class_guid: *const windows_sys::core::GUID) -> Vec<String> {
    let mut paths = Vec::new();
    let targets: Vec<String> = instance_ids.iter().map(|s| s.to_ascii_uppercase()).collect();
    unsafe {
        let info = SetupDiGetClassDevsW(
            class_guid,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if info == INVALID_HDEVINFO {
            return paths;
        }
        let mut idx = 0u32;
        loop {
            let mut ifdata: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            ifdata.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(
                info,
                std::ptr::null_mut(),
                class_guid,
                idx,
                &mut ifdata,
            ) == 0
            {
                break;
            }
            idx += 1;
            let mut needed: u32 = 0;
            SetupDiGetDeviceInterfaceDetailW(info, &mut ifdata, std::ptr::null_mut(), 0, &mut needed, std::ptr::null_mut());
            if needed == 0 {
                continue;
            }
            let mut buf = vec![0u8; needed as usize];
            let mut devinfo: SP_DEVINFO_DATA = std::mem::zeroed();
            devinfo.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            let ok = SetupDiGetDeviceInterfaceDetailW(
                info,
                &mut ifdata,
                buf.as_mut_ptr() as *mut _,
                needed,
                std::ptr::null_mut(),
                &mut devinfo,
            );
            if ok == 0 {
                continue;
            }
            let Some(owner) = dev_instance_id(info, &devinfo) else {
                continue;
            };
            if !targets.contains(&owner.to_ascii_uppercase()) {
                continue;
            }
            // SP_DEVICE_INTERFACE_DETAIL_DATA_W starts with a cbSize DWORD.
            let p = buf.as_ptr().add(std::mem::size_of::<u32>()) as *const u16;
            paths.push(wide_to_string(std::slice::from_raw_parts(p, wide_len(p))));
        }
        SetupDiDestroyDeviceInfoList(info);
    }
    paths
}fn wide_len(mut p: *const u16) -> usize {
    unsafe {
        let mut n = 0;
        while *p != 0 {
            n += 1;
            p = p.add(1);
        }
        n
    }
}

/// Candidate device-interface paths to open, most specific first: the
/// devnode itself, then (for composite devices) every MI_* child — a
/// WinUSB binding may register the interface on the control-interface
/// child only (observed 2026-10-13 after rebinding a Hikvision camera).
/// Universal fallback: scan `HKLM\SYSTEM\CurrentControlSet\Control\DeviceClasses`
/// for every device interface registered on the candidate devnodes. This is
/// the binding-agnostic source of truth — a Zadig/libwdi-style WinUSB INF
/// registers a custom DeviceInterfaceGUID (observed 2026-10-13:
/// {2F3C3EC2-...} for this camera) that no fixed GUID list can cover.
fn device_classes_paths(instance_ids: &[String]) -> Vec<String> {
    let mut paths = Vec::new();
    // Match key form: USB#VID_2BDF&PID_0001&MI_00#7&... (backslashes -> '#').
    let needles: Vec<String> = instance_ids
        .iter()
        .map(|id| id.to_ascii_uppercase().replace(char::from(92), "#"))
        .collect();
    let base_wide: Vec<u16> = OsString::from("SYSTEM").as_os_str().encode_wide().chain(once(92u16)).chain(OsString::from("CurrentControlSet").as_os_str().encode_wide()).chain(once(92u16)).chain(OsString::from("Control").as_os_str().encode_wide()).chain(once(92u16)).chain(OsString::from("DeviceClasses").as_os_str().encode_wide()).chain(once(0u16)).collect();
    unsafe {
        let mut classes: *mut core::ffi::c_void = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, base_wide.as_ptr(), 0, KEY_READ | KEY_ENUMERATE_SUB_KEYS, &mut classes) != 0 {
            return paths;
        }
        let mut gidx = 0u32;
        loop {
            let mut gname = vec![0u16; 128];
            let mut glen = gname.len() as u32;
            if RegEnumKeyExW(classes, gidx, gname.as_mut_ptr(), &mut glen, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) != 0 {
                break;
            }
            gidx += 1;
            gname.truncate(glen as usize);
            let mut cls_key: *mut core::ffi::c_void = std::ptr::null_mut();
            if RegOpenKeyExW(classes, gname.as_ptr(), 0, KEY_READ | KEY_ENUMERATE_SUB_KEYS, &mut cls_key) != 0 {
                continue;
            }
            let mut sidx = 0u32;
            loop {
                let mut sname = vec![0u16; 512];
                let mut slen = sname.len() as u32;
                if RegEnumKeyExW(cls_key, sidx, sname.as_mut_ptr(), &mut slen, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) != 0 {
                    break;
                }
                sidx += 1;
                sname.truncate(slen as usize);
                let sub = String::from_utf16_lossy(&sname);
                let up = sub.to_ascii_uppercase();
                if needles.iter().any(|n| up.contains(n)) {
                    // "##?#USB#..." -> "\?\USB#..."
                    if let Some(rest) = sub.strip_prefix("##?#") {
                        paths.push(format!("\\\\?\\{rest}"));
                    }
                }
            }
            RegCloseKey(cls_key);
        }
        RegCloseKey(classes);
    }
    paths
}

fn device_interface_paths(instance_id: &str) -> Vec<String> {
    // Candidate devnodes: the device itself plus its interface children
    // (a WinUSB binding typically registers the interface on the MI_00
    // control-interface child, not the composite parent).
    let mut targets = vec![instance_id.to_string()];
    let upper = instance_id.to_ascii_uppercase();
    for (child, _) in all_usb_devices() {
        if child.contains("&MI_")
            && direct_parent(&child).map(|p| p.to_ascii_uppercase()) == Some(upper.clone())
        {
            targets.push(child);
        }
    }

    // Interface classes, in binding-likelihood order:
    // 1) GUID_DEVINTERFACE_WINUSB — registered by any Needs=WINUSB.NT
    //    binding through winusb.inf's WINUSB.NT.HW (DeviceInterfaceGUIDs
    //    {F72FE0D4-...}, verified in C:\Windows\INF\winusb.inf).
    // 2) GUID_DEVINTERFACE_USB_DEVICE — registered by the USB stack for
    //    composite devices and their function children.
    let mut paths = interface_paths(&targets, GUID_DEVINTERFACE_WINUSB_REF);
    if paths.is_empty() {
        paths = interface_paths(&targets, GUID_DEVINTERFACE_USB_DEVICE_REF);
    }
    if paths.is_empty() {
        paths = device_classes_paths(&targets);
    }
    if paths.is_empty() {
        // Last resort: synthesize canonical device-object paths
        // (\?\USB#...#{GUID}) and let CreateFile decide.
        for t in &targets {
            paths.push(canonical_usb_path(t));
        }
    }
    paths
}
/// `USB\VID_x&PID_y\serial` -> `\?\USB#VID_x&PID_y#serial#{a5dcbf10-6530-11d2-901f-00c04fb951ed}`
fn canonical_usb_path(instance_id: &str) -> String {
    format!(
        "\\\\?\\{}#{{a5dcbf10-6530-11d2-901f-00c04fb951ed}}",
        instance_id.replace('\\', "#")
    )
}

/// Read USB string descriptor `index` (LANGID 0 = first language).
fn string_descriptor(winusb: WINUSB_INTERFACE_HANDLE, index: u8) -> Option<String> {
    if index == 0 {
        return None;
    }
    let mut buf = vec![0u8; 256];
    let mut n: u32 = 0;
    let ok = unsafe {
        WinUsb_GetDescriptor(
            winusb,
            DESC_STRING,
            index,
            0, // first language ID
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut n,
        )
    };
    if ok == 0 || n < 4 {
        return None;
    }
    // UNICODE string: byte 0 = length, byte 1 = type, then UTF-16LE.
    let len = buf[0] as u32;
    if len < 4 || len > n {
        return None;
    }
    let chars = ((len - 2) / 2) as usize;
    let mut wide = Vec::with_capacity(chars);
    for i in 0..chars {
        wide.push(u16::from_le_bytes([buf[2 + i * 2], buf[3 + i * 2]]));
    }
    let s = String::from_utf16_lossy(&wide);
    Some(s.trim_end_matches('\u{0}').to_owned())
}

fn read_identity_from_handle(winusb: WINUSB_INTERFACE_HANDLE) -> GentlResult<DeviceIdentity> {
    let cfg = configuration_descriptor(winusb)?;
    match device_info_descriptor(&cfg) {
        Some(info) => {
            let mut id = DeviceIdentity {
                guid: string_descriptor(winusb, info.i_device_guid),
                gen_cp_version: info.gen_cp_version,
                u3v_version: info.u3v_version,
                ..DeviceIdentity::default()
            };
            // The CS descriptor confirms U3V; a missing GUID string only
            // downgrades the device ID to the vid:pid:serial fallback
            // (filled by the caller), it must not unlist the device.
            let _ = id.guid.is_none();
            id.manufacturer = string_descriptor(winusb, info.i_vendor_name).unwrap_or_default();
            id.model = string_descriptor(winusb, info.i_model_name).unwrap_or_default();
            id.family = string_descriptor(winusb, info.i_family_name).unwrap_or_default();
            id.device_version = string_descriptor(winusb, info.i_device_version).unwrap_or_default();
            id.manufacturer_info = string_descriptor(winusb, info.i_manufacturer_info).unwrap_or_default();
            id.serial_number = string_descriptor(winusb, info.i_serial_number).unwrap_or_default();
            id.user_defined_name = string_descriptor(winusb, info.i_user_defined_name).unwrap_or_default();
            Ok(id)
        }
        None => {
            // Composite-gadget path (observed on the vgige FunctionFS
            // device): WinUsb_GetDescriptor serves the kernel-composed
            // per-interface configuration, which carries NO class-specific
            // descriptors (f_fs.c rejects CS_INTERFACE outside the
            // HID/CCID/DFU whitelist), so the Device Info CS is never in
            // this blob. Fall back to the §3.2 interface shape: a U3V
            // camera exposes control/event/streaming interfaces EF/05.
            let parsed = crate::descriptors::parse_config_descriptor(&cfg)
                .ok_or(GentlError::InvalidId)?;
            let is_u3v = parsed
                .interfaces
                .iter()
                .any(|i| i.class == USB_CLASS_MISC && i.sub_class == USB_INTERFACE_SUBCLASS_U3V);
            if !is_u3v {
                return Err(GentlError::InvalidId);
            }
            Ok(DeviceIdentity::default())
        }
    }
}

fn configuration_descriptor(winusb: WINUSB_INTERFACE_HANDLE) -> GentlResult<Vec<u8>> {
    // Read the header (9 bytes) for wTotalLength, then the whole blob.
    let mut hdr = [0u8; 9];
    let mut n: u32 = 0;
    let ok = unsafe {
        WinUsb_GetDescriptor(
            winusb,
            DESC_CONFIGURATION,
            0,
            0,
            hdr.as_mut_ptr(),
            hdr.len() as u32,
            &mut n,
        )
    };
    if ok == 0 || n < 9 {
        return Err(GentlError::Io("configuration descriptor header".into()));
    }
    let total = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
    if total < 9 || total > (1 << 20) {
        return Err(GentlError::Io("configuration descriptor length".into()));
    }
    let mut buf = vec![0u8; total];
    let mut n: u32 = 0;
    let ok = unsafe {
        WinUsb_GetDescriptor(winusb, DESC_CONFIGURATION, 0, 0, buf.as_mut_ptr(), total as u32, &mut n)
    };
    if ok == 0 || n < 9 {
        return Err(GentlError::Io("configuration descriptor".into()));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vid_pid_and_serial_parsing() {
        assert_eq!(
            parse_vid_pid(r"USB\VID_04B4&PID_1234\ABC123"),
            Some((0x04B4, 0x1234))
        );
        assert_eq!(instance_serial(r"USB\VID_04B4&PID_1234\ABC123"), Some("ABC123".into()));
        assert!(parse_vid_pid(r"USB\ROOT_HUB30\1&2").is_none());
        assert_eq!(instance_serial(r"USB\VID_04B4&PID_1234"), None);
    }
}
