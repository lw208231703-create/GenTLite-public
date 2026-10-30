//! The device-side protocol state machine — pure memory over byte
//! streams, never USB. `gadget.rs` binds these streams to FunctionFS;
//! tests bind them to in-memory queues and drive the device with the
//! real host-side client (`gentl_u3v::client::U3vCamera`).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use vgige_core::{CanonicalFrame, FrameSession, SharedBack};

pub mod desc;
pub mod emit;
pub mod regs;
pub mod sha1;
pub mod server;

pub use regs::RegisterFile;

/// Command poll cadence (stop-flag responsiveness).
pub(crate) const POLL: Duration = Duration::from_millis(100);

/// Byte-stream errors (transport-agnostic; `gadget.rs` maps I/O errors).
#[derive(Debug)]
pub enum StreamError {
    /// The peer is gone / the endpoint stalled unrecoverably.
    Closed,
    /// Anything else, with detail.
    Other(String),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::Closed => f.write_str("stream closed"),
            StreamError::Other(m) => f.write_str(m),
        }
    }
}

/// Host→device bulk byte stream: GenCP command transfers arrive here
/// (the control endpoint's OUT side; §4.1).
pub trait HostCommands: Send + Sync {
    /// One command transfer; `None` = nothing within the timeout.
    fn recv(&self, timeout: Duration) -> Option<Vec<u8>>;
}

/// Device→host bulk byte stream: GenCP acknowledges (control IN) or data
/// blocks (streaming IN), depending on the endpoint it binds.
pub trait DeviceTransfers: Send + Sync {
    fn send(&self, bytes: &[u8]) -> Result<(), StreamError>;
    /// Endpoint wMaxPacketSize — the memory transport splits writes into
    /// MPS-sized chunks to model real endpoint behavior (and to exercise
    /// the host's UQ16 piece accumulation).
    fn max_packet(&self) -> u16;
}

/// Everything the device-side register file and descriptors need at
/// construction. Identity strings come from the downstream camera
/// (`CameraBack::fetch_identity`); `payload_size` feeds SI Required
/// Payload Size (§5.4.1.3) — the operator-provided frame size the host
/// plans its transfer geometry against (Q39: the device cannot know the
/// per-frame size before the host asks).
#[derive(Clone)]
pub struct DeviceConfig {
    pub manufacturer: String,
    pub model: String,
    pub device_version: String,
    pub serial_number: String,
    /// Injected GenICam XML blob (manifest entry points at it).
    pub xml: Vec<u8>,
    /// SI Required Payload Size: full padded frame payload in bytes.
    pub payload_size: u64,
    /// Descriptor GUID (see `crate::synthesize_guid`).
    pub guid: [u8; 16],
}

/// The assembled virtual U3V camera: register file + GenCP command
/// server thread + stream emitter thread. Cloneable (Arc inside); the
/// gateway keeps one handle for the stream watcher/session and moves
/// another into the relay's session factory.
pub struct U3vDevice {
    pub(crate) inner: Arc<DeviceInner>,
}

pub(crate) struct DeviceInner {
    pub regs: Arc<RegisterFile>,
    control_in: Arc<dyn DeviceTransfers>,
    pub stream_in: Arc<dyn DeviceTransfers>,
    stop: AtomicBool,
    session_tx: SyncSender<SessionFrame>,
    session_rx: Mutex<Option<Receiver<SessionFrame>>>,
    session_generation: Arc<AtomicU64>,
    server: Mutex<Option<std::thread::JoinHandle<()>>>,
    emitter: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl U3vDevice {
    /// Build the device and spawn its server/emitter threads over the
    /// given byte streams. `control_out` feeds the GenCP command loop;
    /// the two `DeviceTransfers` are the control-IN and streaming-IN
    /// endpoints.
    pub fn new(
        config: DeviceConfig,
        back: SharedBack,
        control_out: Arc<dyn HostCommands>,
        control_in: Arc<dyn DeviceTransfers>,
        stream_in: Arc<dyn DeviceTransfers>,
        event_in: Arc<dyn DeviceTransfers>,
    ) -> Self {
        let regs = Arc::new(RegisterFile::new(config, back));
        regs.set_event_in(event_in);
        let (session_tx, session_rx) = sync_channel(4);
        let device = Self {
            inner: Arc::new(DeviceInner {
                regs: Arc::clone(&regs),
                control_in,
                stream_in,
                stop: AtomicBool::new(false),
                session_tx,
                session_rx: Mutex::new(Some(session_rx)),
                session_generation: Arc::new(AtomicU64::new(0)),
                server: Mutex::new(None),
                emitter: Mutex::new(None),
            }),
        };
        {
            let inner = Arc::clone(&device.inner);
            let server_regs = Arc::clone(&regs);
            *device.inner.server.lock().expect("server lock") = Some(
                std::thread::Builder::new()
                    .name("vgige-u3v-gencp".into())
                    .spawn(move || server::serve_loop(control_out, &inner, &server_regs))
                    .expect("spawn u3v gencp server"),
            );
            let rx = device
                .inner
                .session_rx
                .lock()
                .expect("session rx lock")
                .take()
                .expect("session channel once");
            let inner = Arc::clone(&device.inner);
            *device.inner.emitter.lock().expect("emitter lock") = Some(
                std::thread::Builder::new()
                    .name("vgige-u3v-emitter".into())
                    .spawn(move || emit::emitter_loop(rx, &inner))
                    .expect("spawn u3v emitter"),
            );
        }
        device
    }

    /// The front [`FrameSession`]: pushes canonical frames into the
    /// emitter queue. Cheap to take — one per relay.
    pub fn frame_session(&self) -> U3vStreamSession {
        U3vStreamSession {
            tx: self.inner.session_tx.clone(),
            frames_relayed: Arc::clone(&self.inner.regs.frames),
            generation: Arc::clone(&self.inner.session_generation),
            current: 0,
        }
    }

    /// Watch the host's Stream Enable (§5.4.1.2): invoked on every
    /// SI_CONTROL transition with the new enable bit. The gateway wires
    /// this to the relay's programmatic open/close — the U3V host is the
    /// stream-open trigger, unlike the GigE front's SCPx write.
    pub fn set_stream_watcher(&self, watcher: impl Fn(bool) + Send + 'static) {
        self.inner.regs.set_stream_watcher(watcher);
    }

    /// Business hook (custom-window slot 1).
    pub fn set_ai_result(&self, v: u32) {
        self.inner.regs.set_ai_result(v);
    }

    /// Business hook (custom-window slot 2).
    pub fn set_ai_score(&self, v: u32) {
        self.inner.regs.set_ai_score(v);
    }

    /// The descriptor set for the gadget layer (§3.2 layout).
    pub fn descriptor_bytes(&self, vid: u16, pid: u16) -> desc::DescriptorSet {
        desc::DescriptorSet::build(&self.inner.regs.config(), vid, pid)
    }

    /// Poll the stop flag (the gadget layer's shutdown path).
    pub fn stop_requested(&self) -> bool {
        self.inner.stop.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::Relaxed);
    }
}

impl Clone for U3vDevice {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for U3vDevice {
    fn drop(&mut self) {
        // Last handle gone: stop the threads and release the streams.
        if Arc::strong_count(&self.inner) == 1 {
            self.inner.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.inner.server.lock().expect("server lock").take() {
                let _ = t.join();
            }
            if let Some(t) = self.inner.emitter.lock().expect("emitter lock").take() {
                let _ = t.join();
            }
        }
    }
}

pub(crate) struct SessionFrame {
    generation: u64,
    frame: CanonicalFrame,
}

/// The emitter's intake: each open gets a new generation, so frames from
/// an older acquisition can be discarded without draining the new one.
pub struct U3vStreamSession {
    tx: SyncSender<SessionFrame>,
    frames_relayed: Arc<AtomicU32>,
    generation: Arc<AtomicU64>,
    current: u64,
}

impl FrameSession for U3vStreamSession {
    fn open(&mut self, _payload_hint: usize) -> Result<(), String> {
        self.current = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(())
    }

    fn send_frame(&mut self, frame: &CanonicalFrame) {
        self.frames_relayed.fetch_add(1, Ordering::Relaxed);
        // Bounded queue: a slow/absent U3V host must not backpressure
        // the relay; drop-newest at this boundary (the relay's own ring
        // already absorbed upstream loss accounting).
        let _ = self.tx.try_send(SessionFrame {
            generation: self.current,
            frame: CanonicalFrame {
                timestamp: frame.timestamp,
                pixel_format: frame.pixel_format,
                width: frame.width,
                height: frame.height,
                offset_x: frame.offset_x,
                offset_y: frame.offset_y,
                padding_x: frame.padding_x,
                padding_y: frame.padding_y,
                payload: frame.payload.clone(),
            },
        });
    }

    fn close(&mut self) {}
}

/// Shared read-only view of the device config for the descriptor layer.
impl RegisterFile {
    pub(crate) fn config(&self) -> &DeviceConfig {
        self.config_ref()
    }
}
