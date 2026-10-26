//! Discovery result types shared by both USB backends (notes/PENDING.md
//! "Linux 可移植" Phase B): the Windows SetupAPI discovery and the Linux
//! rusb discovery produce the same shapes, consumed by the (shared) cti
//! module and the GenTL surface in lib.rs.

/// One USB root hub = one GenTL Interface (LDVision-style presentation,
/// notes/OPEN-QUESTIONS.md UQ3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootHubInfo {
    /// Root hub identifier. Windows: the CM device instance ID
    /// (`USB\ROOT_HUB30\4&...`). Linux: synthesized as
    /// `USB\ROOT_HUB30\BUS<n>` from the libusb bus number (QoI — libusb
    /// does not model root hubs; see notes/OPEN-QUESTIONS.md Q37).
    pub instance_id: String,
    /// Bus-reported description, e.g. "USB 根集线器(USB 3.0)".
    pub display: String,
}

/// Identity from the Class-Specific Device Info Descriptor (§3.2.5
/// Table 3-5). Strings come from the descriptor's string indexes, never
/// from the standard device descriptor (R-5ch).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub guid: Option<String>,
    pub gen_cp_version: u32,
    pub u3v_version: u32,
    pub manufacturer: String,
    pub model: String,
    pub family: String,
    pub device_version: String,
    pub manufacturer_info: String,
    pub serial_number: String,
    pub user_defined_name: String,
}

/// A U3V device found on the bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundDevice {
    /// GenTL device ID: the U3V GUID when readable, else
    /// "VID:PID:Serial" from the instance path (notes/OPEN-QUESTIONS UQ2).
    /// Linux fallback appends the session-scoped bus:address because the
    /// serial string is unreadable without opening (QoI, Q37).
    pub id: String,
    /// Backend instance ID (used to open the device). Windows: CM device
    /// instance ID. Linux: `<bus>:<address>` decimal.
    pub instance_id: String,
    /// Instance ID of the root hub this device hangs off.
    pub root_id: String,
    pub vid: u16,
    pub pid: u16,
    pub openable: bool,
    pub identity: DeviceIdentity,
}

/// Result of one enumeration sweep.
#[derive(Debug, Default)]
pub struct Enumeration {
    pub devices: Vec<FoundDevice>,
    /// Candidates/entries dropped along the way (foreign devices with the
    /// same class codes that failed U3V confirmation, property read
    /// failures, ...). Counted, never fatal.
    pub skipped: usize,
}
