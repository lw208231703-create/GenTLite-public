//! Module objects behind the GenTL handles and the ownership graph
//! between them. Same discipline as gentl-gige's cti.rs: TL → Interface
//! → Device → DataStream, children holding Weak parents so closed parents
//! invalidate children for good.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, Weak};

use gentl_core::{Module, ModuleType};

use crate::control::{Bootstrap, GenCpClient};
use crate::device_events::DeviceEvents;
use crate::stream::{EventModule, U3vBuffer, U3vDataStream};
use crate::types::{DeviceIdentity, FoundDevice};
use crate::usbio::InterfaceSet;

/// System (TL) module: one per process, created by `TLOpen`.
pub struct TlSystem {
    /// One GenTL Interface per USB root hub (LDVision-style, see
    /// notes/OPEN-QUESTIONS.md UQ3).
    pub interfaces: Mutex<Vec<Arc<U3vInterface>>>,
    /// IDs of interfaces with a live `IF_HANDLE`.
    pub opened_interfaces: Mutex<HashSet<String>>,
}

impl TlSystem {
    pub fn new(interfaces: Vec<Arc<U3vInterface>>) -> Arc<Self> {
        Arc::new(Self {
            interfaces: Mutex::new(interfaces),
            opened_interfaces: Mutex::new(HashSet::new()),
        })
    }
}

/// Interface module behind `IF_HANDLE`: one USB root hub.
pub struct U3vInterface {
    pub root_id: String,
    pub display: String,
    /// Devices found by the last `IFUpdateDeviceList`.
    pub devices: Mutex<Vec<FoundDevice>>,
    /// Device IDs with a live `DEV_HANDLE`.
    pub opened_devices: Mutex<HashSet<String>>,
    pub parent: Weak<TlSystem>,
}

impl U3vInterface {
    pub fn new_template(root_id: &str, display: &str, parent: &Arc<TlSystem>) -> Arc<Self> {
        Arc::new(Self {
            root_id: root_id.to_owned(),
            display: display.to_owned(),
            devices: Mutex::new(Vec::new()),
            opened_devices: Mutex::new(HashSet::new()),
            parent: Arc::downgrade(parent),
        })
    }

    /// Fresh instance for `TLOpenInterface` (never revives stale children).
    pub fn instantiate(self: &Arc<Self>) -> Arc<Self> {
        Arc::new(Self {
            root_id: self.root_id.clone(),
            display: self.display.clone(),
            devices: Mutex::new(Vec::new()),
            opened_devices: Mutex::new(HashSet::new()),
            parent: self.parent.clone(),
        })
    }
}

/// Device access level requested through `IFOpenDevice`. U3V 1.2 has no
/// exclusive-access mechanism (ABRM Access Privilege is "not used by this
/// standard"), so Exclusive currently behaves like Control — see
/// notes/OPEN-QUESTIONS.md UQ5.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeviceAccess {
    Monitor,
    Control,
    Exclusive,
}

/// An opened remote device: USB interface set (owning every backend
/// resource — file/WinUSB handles on Windows, the libusb handle plus
/// interface claims on Linux), GenCP control channel (holding the
/// control endpoint pipes), bootstrap register snapshot.
///
/// Field drop order is load-bearing: `ifaces` must go before `client` so
/// the endpoint pipes are dropped (and receiver threads joined, via the
/// stream modules) before the handles they reference are freed.
pub struct U3vDevice {
    pub id: String,
    pub identity: DeviceIdentity,
    pub access: DeviceAccess,
    pub ifaces: InterfaceSet,
    pub client: Arc<Mutex<GenCpClient>>,
    pub bootstrap: Bootstrap,
    /// Stream IDs with a live `DS_HANDLE`.
    pub opened_streams: Mutex<HashSet<String>>,
    pub events: DeviceEvents,
    pub parent: Weak<U3vInterface>,
}

impl U3vDevice {
    pub fn close(&self) {
        self.events.close(self);
        // U3V has no channel-release register to write back; closing the
        // handles is the whole teardown.
    }
}

/// The remote device port handed out by `DevGetPort`.
pub struct DevicePort {
    pub device: Weak<U3vDevice>,
}

macro_rules! impl_module {
    ($ty:ty, $kind:expr) => {
        impl Module for $ty {
            fn module_type(&self) -> ModuleType {
                $kind
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_any_arc(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> {
                self
            }
        }
    };
}

impl_module!(TlSystem, ModuleType::System);
impl_module!(U3vInterface, ModuleType::Interface);
impl_module!(U3vDevice, ModuleType::Device);
impl_module!(DevicePort, ModuleType::Port);
impl_module!(U3vDataStream, ModuleType::DataStream);
impl_module!(U3vBuffer, ModuleType::Buffer);
impl_module!(EventModule, ModuleType::Event);
