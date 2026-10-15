//! Software-simulated GigE Vision camera: answers GVCP discovery/register
//! commands over UDP and emits a configurable fake GVSP image stream.
//! Key to CI regression and development without real hardware.

mod emitter;
mod regmap;
mod xml;

use std::net::Ipv4Addr;
use std::sync::Arc;

use gentl_core::GentlResult;
use gentl_gvcp::GvcpServer;
use regmap::{SimDevice, SimRegMap};

/// User register area and per-register addresses inside it.
pub use regmap::{REG_EXPOSURE_TIME, REG_HEIGHT, REG_PIXEL_FORMAT, REG_WIDTH, USER_BASE};
/// Base address of the embedded GenICam XML blob.
pub use regmap::XML_BASE;

/// Identity and network configuration of a simulated camera. Strings longer
/// than a bootstrap slot are truncated to the slot size (32/32/32/16).
pub struct SimConfig {
    pub mac: [u8; 6],
    pub ip: Ipv4Addr,
    pub subnet_mask: Ipv4Addr,
    pub gateway: Ipv4Addr,
    /// Bootstrap Manufacturer Name slot, ≤ 32 bytes.
    pub manufacturer_name: String,
    /// Bootstrap Model Name slot, ≤ 32 bytes.
    pub model_name: String,
    /// Bootstrap Device Version slot, ≤ 32 bytes.
    pub device_version: String,
    /// Bootstrap Serial Number slot, ≤ 16 bytes.
    pub serial_number: String,
}

impl SimConfig {
    fn normalized(mut self) -> Self {
        self.manufacturer_name.truncate(32);
        self.model_name.truncate(32);
        self.device_version.truncate(32);
        self.serial_number.truncate(16);
        self
    }
}

/// Fake image stream configuration of a simulated camera (M2).
///
/// `pixel_format` is a PFNC value; the emitter supports Mono8 (0x01080001)
/// and Mono16 (0x01100007). The stream dimensions also seed the
/// Width/Height/PixelFormat user registers and the GenApi XML so every
/// view of the device agrees with what the stream carries.
///
/// Kept as a separate `SimCamera` parameter rather than a `SimConfig`
/// field: existing construction sites (including gentl-gige's integration
/// tests) build `SimConfig` with struct literals, and a new field would
/// break them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamConfig {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// PFNC pixel format (Mono8/Mono16 supported).
    pub pixel_format: u32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        StreamConfig {
            width: 1024,
            height: 768,
            pixel_format: 0x0108_0001, // Mono8 (include/PFNC.h)
        }
    }
}

/// One simulated camera: a register/memory map plus the embedded GenICam
/// XML. `start` serves it over GVCP/UDP.
pub struct SimCamera {
    device: SimDevice,
    xml: String,
}

impl SimCamera {
    /// Build the register map and embed the GenICam XML at `XML_BASE`,
    /// publishing `local:simcam.xml;<addr hex>;<len hex>` in the First URL
    /// bootstrap register (spec §14.1.1, R-119cd). The camera emits the
    /// default 1024x768 Mono8 fake stream when a stream channel is opened.
    pub fn new(config: SimConfig) -> Self {
        Self::build(config, StreamConfig::default())
    }

    /// Like [`new`](Self::new), with a custom fake-image stream
    /// configuration (dimensions and PFNC pixel format).
    pub fn with_stream(config: SimConfig, stream: StreamConfig) -> Self {
        Self::build(config, stream)
    }

    fn build(config: SimConfig, stream: StreamConfig) -> Self {
        let config = config.normalized();
        let xml = xml::build_xml(stream.width, stream.height);
        let map = Arc::new(SimRegMap::new(config, xml.as_bytes(), stream));
        SimCamera {
            device: SimDevice::new(map),
            xml,
        }
    }

    /// Serve GVCP on UDP `port` (0 = system-assigned). The returned server
    /// keeps serving until dropped or stopped.
    pub fn start(&self, port: u16) -> GentlResult<GvcpServer> {
        GvcpServer::start(port, self.device.clone())
    }

    /// The embedded GenICam XML (tests, consumer-side validation).
    pub fn xml(&self) -> &str {
        &self.xml
    }
}
