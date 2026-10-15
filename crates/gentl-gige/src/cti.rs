//! Module objects behind the GenTL handles and the ownership graph between
//! them.
//!
//! Handle ownership follows the GenTL module hierarchy:
//! - `TlSystem` is owned by the `TL_HANDLE` box; it holds the enumerated
//!   interface list and the set of currently opened interface IDs.
//! - `GevInterface` instances in the TL list are enumeration *templates*.
//!   `TLOpenInterface` clones one into a fresh instance owned by the
//!   `IF_HANDLE` box, so stale child handles can never revive: a
//!   `GevDevice`/`DevicePort` holds a `Weak` reference to its parent
//!   instance, which dies for good when its handle is closed.
//! - `GevDevice` is owned by the `DEV_HANDLE` box; `DevicePort` holds a
//!   `Weak<GevDevice>` and becomes `GC_ERR_INVALID_HANDLE` after `DevClose`.

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, Weak};

use gentl_core::{Module, ModuleType};
use gentl_gvcp::{DiscoveryInfo, GvcpClient};

use crate::discovery::NicInfo;
use crate::stream::{EventModule, GevBuffer, GevDataStream};

/// System (TL) module: one per process, created by `TLOpen`.
pub struct TlSystem {
    /// The Basler-style single aggregate interface template.
    pub interfaces: Mutex<Vec<Arc<GevInterface>>>,
    /// NICs used as discovery inputs for the aggregate interface.
    pub nics: Mutex<Vec<NicInfo>>,
    /// IDs of interfaces with a live `IF_HANDLE`; `TLOpenInterface` refuses
    /// a second instantiation with `GC_ERR_RESOURCE_IN_USE` (GenTL 1.6
    /// §6.3.2.7).
    pub opened_interfaces: Mutex<HashSet<String>>,
}

impl TlSystem {
    pub fn new(interfaces: Vec<Arc<GevInterface>>, nics: Vec<NicInfo>) -> Arc<Self> {
        Arc::new(Self {
            interfaces: Mutex::new(interfaces),
            nics: Mutex::new(nics),
            opened_interfaces: Mutex::new(HashSet::new()),
        })
    }
}

/// The single Basler-style aggregate interface: one interface
/// spanning all NICs (observed on MvProducerGEV.cti).
pub struct GevInterface {
    /// Devices found by the last `IFUpdateDeviceList`.
    pub devices: Mutex<Vec<DeviceEntry>>,
    /// Device IDs with a live `DEV_HANDLE`; guards `IFOpenDevice`.
    pub opened_devices: Mutex<HashSet<String>>,
    pub parent: Weak<TlSystem>,
}

impl GevInterface {
    pub const ID: &'static str = crate::discovery::AGGREGATE_IFACE_ID;
    pub const DISPLAY_NAME: &'static str = crate::discovery::AGGREGATE_IFACE_DISPLAY_NAME;

    pub fn new_template(parent: &Arc<TlSystem>) -> Arc<Self> {
        Arc::new(Self {
            devices: Mutex::new(Vec::new()),
            opened_devices: Mutex::new(HashSet::new()),
            parent: Arc::downgrade(parent),
        })
    }

    /// Instantiation handed out by `TLOpenInterface`: a fresh object so a
    /// later re-open never resurrects children of a closed instance.
    pub fn instantiate(self: &Arc<Self>) -> Arc<Self> {
        Arc::new(Self {
            devices: Mutex::new(Vec::new()),
            opened_devices: Mutex::new(HashSet::new()),
            parent: self.parent.clone(),
        })
    }
}

/// One device entry in an interface's device list.
pub struct DeviceEntry {
    /// GenTL device ID (MAC string, see notes/OPEN-QUESTIONS Q4).
    pub id: String,
    pub discovery: DiscoveryInfo,
}

/// Device access level the producer acquired on the control channel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeviceAccess {
    /// No CCP write: read-only (monitor) control channel.
    Monitor,
    /// CCP control access (other hosts may read).
    Control,
    /// CCP exclusive access.
    Exclusive,
}

/// An opened remote device: GVCP control channel + heartbeat worker.
pub struct GevDevice {
    pub id: String,
    pub discovery: DiscoveryInfo,
    pub access: DeviceAccess,
    /// Shared with the heartbeat thread; `GvcpClient` documents that
    /// sharing needs external synchronization.
    pub client: Arc<Mutex<GvcpClient>>,
    pub stop_heartbeat: Arc<AtomicBool>,
    pub heartbeat_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Stream IDs with a live `DS_HANDLE`; guards `DevOpenDataStream`
    /// (GenTL 1.6 §6.3.4.6: a second open of the same ID is
    /// GC_ERR_RESOURCE_IN_USE).
    pub opened_streams: Mutex<HashSet<String>>,
    pub parent: Weak<GevInterface>,
}

/// The remote device port handed out by `DevGetPort`. Wraps the device
/// weakly so the port dies together with the device handle.
pub struct DevicePort {
    pub device: Weak<GevDevice>,
}

// Module trait --------------------------------------------------------------

macro_rules! impl_module {
    ($ty:ty, $kind:expr) => {
        impl Module for $ty {
            fn module_type(&self) -> ModuleType {
                $kind
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_any_arc(
                self: Arc<Self>,
            ) -> Arc<dyn std::any::Any + Send + Sync> {
                self
            }
        }
    };
}

impl_module!(TlSystem, ModuleType::System);
impl_module!(GevInterface, ModuleType::Interface);
impl_module!(GevDevice, ModuleType::Device);
impl_module!(DevicePort, ModuleType::Port);
impl_module!(GevDataStream, ModuleType::DataStream);
impl_module!(GevBuffer, ModuleType::Buffer);
impl_module!(EventModule, ModuleType::Event);

#[cfg(test)]
mod tests {
    use super::*;
    use gentl_core::{from_handle, to_handle};

    #[test]
    fn handle_roundtrip_and_wrong_type() {
        let tl = TlSystem::new(Vec::new(), Vec::new());
        let h = to_handle(tl);
        // Same type: fine.
        let back = unsafe { from_handle::<TlSystem>(h) };
        assert!(back.is_ok());
        // Wrong type: GC_ERR_INVALID_HANDLE path, no crash.
        let wrong = unsafe { from_handle::<GevInterface>(h) };
        assert!(wrong.is_err());
        unsafe { gentl_core::free_handle(h) };
        // Null: error, no crash.
        assert!(unsafe { from_handle::<TlSystem>(std::ptr::null_mut()) }.is_err());
    }
}
