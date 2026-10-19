//! Software-simulated GigE Vision camera: answers GVCP discovery/register
//! commands over UDP and emits a configurable fake GVSP image stream.
//! Key to CI regression and development without real hardware.

mod emitter;
mod regmap;
mod xml;

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};

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
    /// Which streaming protocol(s) the device advertises via the Streaming
    /// Capability register (spec §36.27) and accepts via the Streaming
    /// Configuration register (spec §36.38, Table 36-2).
    pub streaming: StreamingProtocol,
}

/// Streaming protocol mode of a simulated device (spec §36.27/§36.38).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamingProtocol {
    /// GVSP only: Streaming Capability (0x092C) reads 0 (the default).
    GvspOnly,
    /// GVSP and GVRSP: capability bit 5 (gvrsp_mode_supported) plus bit 3
    /// (SCEBAx_supported, mandatory when GVRSP is supported — CR-665cd);
    /// the active mode still factory-defaults to GVSP.
    Both,
    /// GVRSP only: capability bits 4|5; GVSP stream channel registers
    /// answer GEV_STATUS_ACCESS_DENIED (CR-671cd) and mode 0 writes are
    /// rejected per Table 36-2.
    GvrspOnly,
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
    /// Number of GVSP stream channels (SCPx blocks, spec §36.57). Each
    /// channel is an independent block sequence to its own (SCDAx,
    /// host_port) target; channels 1..511 are optional in the spec, so a
    /// device may answer GEV_STATUS_INVALID_ADDRESS past its count — the
    /// sim answers it past `channels`. Must be >= 1; smaller values are
    /// clamped (a transmitter device always has channel 0).
    pub channels: u16,
    /// Transmit with the 1.2 legacy GVSP header form (1.2 §24 Fig 24-1:
    /// 8-byte headers, EI=0, 16-bit block_id) and the 1.2 leader body
    /// (36 bytes, u16 paddings). false (default) = the 3.0 form (20-byte
    /// headers, EI=1, §24.2/§25.2.1). SCPSx packet_size counts the mode's
    /// header budget (36 vs 48 bytes, 1.2 §27.46 / 3.0 §36.58).
    pub legacy_headers: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        StreamConfig {
            width: 1024,
            height: 768,
            pixel_format: 0x0108_0001, // Mono8 (include/PFNC.h)
            channels: 1,
            legacy_headers: false,
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

    /// Test hook (§16.7 regression): start dropping the given payload
    /// packets on send (leader/trailer are never dropped). Share a set
    /// with the test and insert (block_id, packet_id) pairs before
    /// starting acquisition.
    pub fn set_packet_drop(&self, drops: Arc<Mutex<std::collections::BTreeSet<(u64, u32)>>>) {
        self.device.set_packet_drop(drops);
    }

    /// The embedded GenICam XML (tests, consumer-side validation).
    pub fn xml(&self) -> &str {
        &self.xml
    }
}
