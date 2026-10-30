//! vgige: smart-camera gateway.
//!
//!   vgige --up <up-IP> --down <camera IP | U3V instance id>
//!         [--back <gige|u3v>] [--front <gige|u3v>]
//!         [--down-port <n>] [--port <n>] [--u3v-payload <bytes>]
//!         [--ext-nodes <path.xml>] [--no-stream]
//!
//! Two independent seams (the gateway matrix):
//! - back: how we talk to the real camera (GigE Vision UDP / USB3 Vision).
//! - front: what we present to the client (a GigE device on the up NIC /
//!   a U3V camera on the USB gadget port).
//! Exactly one backend and one front are compiled in (cargo features —
//! see the crate's [features] for the one forbidden link combination);
//! `--back`/`--front` select among the compiled ones at runtime.
//!
//! Graceful shutdown on SIGINT/SIGTERM (unix): stop serving, cascade the
//! stream close, release the camera.

mod signal;
#[cfg(feature = "front-gige")]
mod netid;

use std::io::Write as _;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(feature = "back-gige")]
use gentl_gvcp::registers as reg;
#[cfg(feature = "front-gige")]
use gentl_gvcp::GvcpServer;
use gentl_gvcp::GVCP_PORT;
#[cfg(feature = "back-gige")]
use vgige_back_gige::GigeBack;
#[cfg(feature = "back-u3v")]
use vgige_back_u3v::U3vBack;
#[cfg(feature = "back-gige")]
use vgige_core::CameraBack;
use vgige_core::{Relay, SharedBack, XmlMerger};
#[cfg(feature = "front-gige")]
use vgige_front_gige::{assemble_identity, GigEFront, GvspSession, UpIdentity};

#[cfg(all(not(feature = "back-gige"), not(feature = "back-null"), not(feature = "back-u3v")))]
compile_error!(
    "vgige-cli needs a camera backend: default (back-gige), back-null, or --no-default-features --features back-u3v"
);
#[cfg(all(not(feature = "front-gige"), not(feature = "front-u3v")))]
compile_error!("vgige-cli needs a front: default (front-gige) or --no-default-features --features front-u3v");
// All back/front combinations link. The producer crates only export their
// GenTL entry points with their `cti-exports` feature; vgige disables it.

const DEFAULT_GVCP_PORT: u16 = GVCP_PORT;
/// Gadget identity for `--front u3v` (QoI, Q39: a pid.codes private
/// testing allocation; production needs its own USB-IF ids).
#[cfg(feature = "front-u3v")]
const GADGET_VID: u16 = 0x1209;
#[cfg(feature = "front-u3v")]
const GADGET_PID: u16 = 0x7D01;
#[cfg(feature = "front-u3v")]
const GADGET_NAME: &str = "vgige";

/// Which camera family the down-facing side speaks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BackKind {
    /// `--down` is the camera's IPv4 address.
    #[cfg(feature = "back-gige")]
    Gige,
    /// No camera: the built-in synthetic camera (demo mode).
    #[cfg(feature = "back-null")]
    Null,
    /// `--down` is the U3V backend instance ID (Windows: CM device
    /// instance path; Linux: `<bus>:<address>`).
    #[cfg(feature = "back-u3v")]
    U3v,
}

impl BackKind {
    fn compiled(name: &str) -> Option<Self> {
        match name {
            #[cfg(feature = "back-gige")]
            "gige" => Some(Self::Gige),
            #[cfg(feature = "back-null")]
            "null" => Some(Self::Null),
            #[cfg(feature = "back-u3v")]
            "u3v" => Some(Self::U3v),
            _ => None,
        }
    }
}

/// What the gateway presents to the client.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FrontKind {
    /// A virtual GigE device on the up-facing NIC (GVCP server).
    #[cfg(feature = "front-gige")]
    Gige,
    /// A U3V camera on the USB gadget port (Linux FunctionFS; 整机待验).
    #[cfg(feature = "front-u3v")]
    U3v,
}

impl FrontKind {
    fn compiled(name: &str) -> Option<Self> {
        match name {
            #[cfg(feature = "front-gige")]
            "gige" => Some(Self::Gige),
            #[cfg(feature = "front-u3v")]
            "u3v" => Some(Self::U3v),
            _ => None,
        }
    }
}

struct Args {
    /// GigE front only: the up-facing NIC address the virtual device
    /// binds and claims.
    #[cfg_attr(not(feature = "front-gige"), allow(dead_code))]
    up: Ipv4Addr,
    back: BackKind,
    front: FrontKind,
    down: String,
    /// GigE back only; parsed in every build so the CLI surface stays
    /// identical across feature matrices.
    #[cfg_attr(not(feature = "back-gige"), allow(dead_code))]
    down_port: u16,
    /// GigE front only (same reasoning).
    #[cfg_attr(not(feature = "front-gige"), allow(dead_code))]
    port: u16,
    /// U3V front only: SI Required Payload Size (frame size in bytes).
    #[cfg_attr(not(feature = "front-u3v"), allow(dead_code))]
    u3v_payload: Option<u64>,
    ext_nodes: Option<String>,
    no_stream: bool,
}

fn usage() -> String {
    "usage: vgige --up <up-facing IP> --down <camera IP | U3V instance id> \
     [--back <gige|u3v>] [--front <gige|u3v>] [--down-port <GVCP port, default 3956>] \
     [--port <up-facing GVCP port, default 3956>] [--u3v-payload <frame bytes, front u3v only>] \
     [--ext-nodes <XML fragment path>] [--no-stream]"
        .to_owned()
}

fn parse_ip(flag: &str, value: &str) -> Ipv4Addr {
    value.parse().unwrap_or_else(|_| {
        eprintln!("{flag}: `{value}` is not an IPv4 address");
        std::process::exit(2);
    })
}

fn parse_port(flag: &str, value: &str) -> u16 {
    value.parse().unwrap_or_else(|_| {
        eprintln!("{flag}: `{value}` is not a u16 port");
        std::process::exit(2);
    })
}

fn parse_args() -> Args {
    let mut up = None;
    let mut back = BackKind::compiled("gige")
        .or_else(|| BackKind::compiled("null"))
        .or_else(|| BackKind::compiled("u3v"))
        .expect("at least one backend is compiled");
    let mut front = FrontKind::compiled("gige")
        .or_else(|| FrontKind::compiled("u3v"))
        .expect("at least one front is compiled");
    let mut down = None;
    let mut down_port = DEFAULT_GVCP_PORT;
    let mut port = DEFAULT_GVCP_PORT;
    let mut u3v_payload = None;
    let mut ext_nodes = None;
    let mut no_stream = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |flag: &str| -> String {
            args.next().unwrap_or_else(|| {
                eprintln!("{flag} requires a value");
                std::process::exit(2);
            })
        };
        match a.as_str() {
            "--up" => {
                let v = value(&a);
                up = Some(parse_ip(&a, &v));
            }
            "--down" => down = Some(value(&a)),
            "--back" => {
                let v = value(&a);
                back = BackKind::compiled(&v).unwrap_or_else(|| {
                    eprintln!("--back: `{v}` is not available in this build");
                    std::process::exit(2);
                });
            }
            "--front" => {
                let v = value(&a);
                front = FrontKind::compiled(&v).unwrap_or_else(|| {
                    eprintln!("--front: `{v}` is not available in this build");
                    std::process::exit(2);
                });
            }
            "--down-port" => {
                let v = value(&a);
                down_port = parse_port(&a, &v);
            }
            "--port" => {
                let v = value(&a);
                port = parse_port(&a, &v);
            }
            "--u3v-payload" => {
                let v = value(&a);
                u3v_payload = Some(v.parse().unwrap_or_else(|_| {
                    eprintln!("--u3v-payload: `{v}` is not a u64");
                    std::process::exit(2);
                }));
            }
            "--ext-nodes" => {
                ext_nodes = Some(value(&a));
            }
            "--no-stream" => no_stream = true,
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument `{other}`\n{}", usage());
                std::process::exit(2);
            }
        }
    }
    // `--up` (up-facing IP) only matters for the GigE front; the U3V
    // gadget front's upstream is the USB cable, no IP involved.
    #[cfg(feature = "front-gige")]
    if front == FrontKind::Gige && up.is_none() {
        eprintln!("--up is required with --front gige\n{}", usage());
        std::process::exit(2);
    }
    let up = up.unwrap_or(Ipv4Addr::LOCALHOST);
    let down = down.unwrap_or_else(|| require_down(back));
    #[cfg(feature = "back-gige")]
    if back == BackKind::Gige {
        // Validate eagerly so a typo fails before any connection attempt.
        parse_ip("--down", &down);
    }
    Args {
        up,
        back,
        front,
        down,
        down_port,
        port,
        u3v_payload,
        ext_nodes,
        no_stream,
    }
}

/// `--down` default when the backend needs no camera target.
fn require_down(back: BackKind) -> String {
    #[cfg(feature = "back-null")]
    if back == BackKind::Null {
        return "null".to_owned();
    }
    let _ = back;
    eprintln!("--down is required with a real camera backend (--back null needs none)");
    std::process::exit(2);
}

fn main() {
    let args = parse_args();
    signal::install();

    // Downstream control channel. GigE takes Exclusive Access for our
    // lifetime (the camera is ours alone; remote clients talk to the
    // virtual device); U3V has no channel-release register (UQ5) — the
    // facade owns the USB handles.
    #[allow(unused_mut)]
    let mut back: SharedBack = match args.back {
        #[cfg(feature = "back-gige")]
        BackKind::Gige => {
            let down_ip = parse_ip("--down", &args.down);
            let mut back = GigeBack::connect(down_ip, args.down_port).unwrap_or_else(|e| {
                eprintln!("vgige: connect {}:{}: {e}", args.down, args.down_port);
                std::process::exit(1);
            });
            if let Err(e) = CameraBack::write_reg(&mut back, reg::CCP, reg::CCP_EXCLUSIVE_ACCESS) {
                eprintln!("vgige: taking CCP on the camera failed: {e:#06X}");
                std::process::exit(1);
            }
            Arc::new(Mutex::new(back))
        }
        #[cfg(feature = "back-null")]
        BackKind::Null => Arc::new(Mutex::new(vgige_back_null::NullBack::new())),
        #[cfg(feature = "back-u3v")]
        BackKind::U3v => Arc::new(Mutex::new(U3vBack::open(&args.down).unwrap_or_else(|e| {
            eprintln!("vgige: opening the U3V camera {}: {e}", args.down);
            std::process::exit(1);
        }))),
    };
    // Every later GigE startup failure path must release the CCP: an
    // orphaned exclusive lock wedges these cameras until power-cycle
    // (real-machine finding 2026-10-19/22, notes/OPEN-QUESTIONS.md Q35
    // family). The end-of-main release is kept as defense in depth; a
    // second CCP=0 is a harmless ignored error. U3V: no equivalent exists.
    #[cfg(feature = "back-gige")]
    struct DownGuard(SharedBack);
    #[cfg(feature = "back-gige")]
    impl Drop for DownGuard {
        fn drop(&mut self) {
            if let Ok(mut back) = self.0.lock() {
                let _ = back.write_reg(reg::CCP, 0);
            }
        }
    }
    #[cfg(feature = "back-gige")]
    let _down_guard: Option<DownGuard> = match args.back {
        BackKind::Gige => {
            // Diagnostic (2026-10-22, LeadingOE .89): an immediate READMEM
            // after the EA grant times out deterministically while the same
            // transaction via the producer (which reads XML ~1 s later)
            // succeeds — the camera firmware appears to need a settling
            // window after privilege grant before servicing further control
            // transactions. QoI, pending vendor clarification (Q35 family).
            std::thread::sleep(std::time::Duration::from_millis(1000));
            Some(DownGuard(Arc::clone(&back)))
        }
        #[cfg(feature = "back-null")]
        BackKind::Null => None,
        #[cfg(feature = "back-u3v")]
        BackKind::U3v => None,
    };

    // Camera XML (fetched via the back; zipped descriptions are
    // transparently unzipped) + injected nodes.
    let raw_xml = {
        let mut back = back.lock().expect("back lock poisoned");
        back.fetch_xml().unwrap_or_else(|s| {
            eprintln!("vgige: fetching the camera XML: GEV status 0x{s:04X}");
            std::process::exit(1);
        })
    };
    let ext = args.ext_nodes.as_deref().map(|path| {
        std::fs::read(path).unwrap_or_else(|e| {
            eprintln!("vgige: reading ext nodes {path}: {e}");
            std::process::exit(1);
        })
    });
    let xml = Arc::new(XmlMerger::new(&raw_xml, ext.as_deref()).unwrap_or_else(|e| {
        eprintln!("vgige: merging the camera XML: {e}");
        std::process::exit(1);
    }));

    match args.front {
        #[cfg(feature = "front-gige")]
        FrontKind::Gige => run_gige_front(&args, back, Arc::clone(&xml)),
        #[cfg(feature = "front-u3v")]
        FrontKind::U3v => run_u3v_front(&args, back, Arc::clone(&xml)),
    }
}

/// The GigE front: virtual device on the up-facing NIC (control proxy +
/// custom window + injected XML + GVSP 3.0 relay).
#[cfg(feature = "front-gige")]
fn run_gige_front(args: &Args, back: SharedBack, xml: Arc<XmlMerger>) {
    // Up-facing identity: the virtual device claims this NIC's MAC/subnet.
    let link = netid::resolve_up_link(args.up).unwrap_or_else(|e| {
        eprintln!("vgige: resolving the up-facing link: {e}");
        std::process::exit(1);
    });

    // Core: relay (virtual stream channel + driver) and the register
    // space; front: discovery identity + GVSP 3.0 encoding session.
    let relay = Relay::new(
        Arc::clone(&back),
        !args.no_stream,
        Box::new(|regs| Box::new(GvspSession::new(regs))),
    );
    let space = vgige_core::RegisterSpace::new(
        Arc::clone(&back),
        Arc::clone(&xml),
        args.up,
        link.mac,
        relay,
    );
    let identity = assemble_identity(
        &back,
        &UpIdentity {
            ip: args.up,
            mac: link.mac,
            mask: link.mask,
        },
    )
    .unwrap_or_else(|e| {
        eprintln!("vgige: assembling the virtual device: {e}");
        std::process::exit(1);
    });
    let front = GigEFront::new(identity, space);

    // Bind the up-facing address explicitly: the device must not answer
    // on the camera-facing network.
    let server = GvcpServer::start_bound(args.up, args.port, front.clone()).unwrap_or_else(|e| {
        eprintln!("vgige: starting the GVCP server: {e}");
        std::process::exit(1);
    });
    let bound = server.local_port().expect("server local port");

    let down_desc = match args.back {
        #[cfg(feature = "back-gige")]
        BackKind::Gige => format!("{}:{}", args.down, args.down_port),
        #[cfg(feature = "back-null")]
        BackKind::Null => args.down.clone(),
        #[cfg(feature = "back-u3v")]
        BackKind::U3v => args.down.clone(),
    };
    println!(
        "VGIGE_READY up={}:{} (mac {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}) down={} xml={}B nodes={} stream={}",
        args.up,
        bound,
        link.mac[0], link.mac[1], link.mac[2], link.mac[3], link.mac[4], link.mac[5],
        down_desc,
        xml.blob().len(),
        xml.node_count(),
        if args.no_stream { "off" } else { "on" },
    );
    std::io::stdout().flush().expect("flush stdout");
    eprintln!(
        "vgige: virtual GigE device on {}:{}, proxying {} ({} local XML bytes, {} injected nodes); Ctrl+C to stop",
        args.up,
        bound,
        down_desc,
        xml.blob().len(),
        xml.node_count(),
    );

    // Serve until SIGINT/SIGTERM (unix). Windows runs until killed —
    // the default Ctrl+C handler ends the process.
    while !signal::shutdown_requested() {
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("vgige: shutting down");

    // Order: stop answering clients, cascade the stream close (relay
    // Drop -> downstream close, R-091ca), release CCP (guard).
    drop(server);
    drop(front);
    println!("VGIGE_STOPPED");
}

/// The U3V device front: the gateway presents itself as a USB3 Vision
/// camera on the gadget port. The host's Stream Enable (§5.4.1.2) opens
/// the downstream channel via the relay watcher.
#[cfg(feature = "front-u3v")]
fn run_u3v_front(args: &Args, back: SharedBack, xml: Arc<XmlMerger>) {
    use vgige_front_u3v::{gadget, synthesize_guid, DeviceConfig, U3vDevice};

    let identity = back
        .lock()
        .expect("back lock poisoned")
        .fetch_identity()
        .unwrap_or_else(|s| {
            eprintln!("vgige: reading the camera identity: GEV status 0x{s:04X}");
            std::process::exit(1);
        });
    let payload = args.u3v_payload.unwrap_or_else(|| {
        eprintln!("--front u3v requires --u3v-payload <frame bytes> (the SI Required Payload Size the host plans its geometry against; Q39)");
        std::process::exit(2);
    });
    let guid = synthesize_guid(&format!("{}:{}", identity.serial_number, identity.model));
    let device_config = DeviceConfig::from_parts(identity, xml.blob(), payload, guid);
    let gadget_config =
        gadget::GadgetConfig::new(GADGET_NAME, GADGET_VID, GADGET_PID, device_config.clone());

    // The relay factory needs the device, and the device needs the
    // gadget streams; the slot breaks the cycle (filled before any host
    // can enable the stream — the gadget endpoints only exist after
    // `gadget::open`).
    let slot: Arc<Mutex<Option<U3vDevice>>> = Arc::new(Mutex::new(None));
    let relay = Relay::new(
        Arc::clone(&back),
        !args.no_stream,
        Box::new({
            let slot = Arc::clone(&slot);
            move |_regs| {
                let device = slot
                    .lock()
                    .expect("device slot poisoned")
                    .clone()
                    .expect("U3vDevice must exist before a stream opens");
                Box::new(device.frame_session())
            }
        }),
    );

    let gadget = gadget::open(&gadget_config).unwrap_or_else(|e| {
        eprintln!("vgige: setting up the USB gadget: {e}");
        std::process::exit(1);
    });
    let device = U3vDevice::new(
        device_config,
        Arc::clone(&back),
        gadget.streams.control_out.clone(),
        gadget.streams.control_in.clone(),
        gadget.streams.stream_in.clone(),
        gadget.streams.event_in.clone(),
    );
    *slot.lock().expect("device slot poisoned") = Some(device.clone());
    device.set_stream_watcher({
        let relay = relay.clone();
        move |on| {
            if on {
                let _ = relay.open();
            } else {
                relay.close();
            }
        }
    });

    println!(
        "VGIGE_READY front=u3v gadget={} (vid {:04X}:{:04X}) down={} xml={}B nodes={} stream={}",
        GADGET_NAME,
        GADGET_VID,
        GADGET_PID,
        args.down,
        xml.blob().len(),
        xml.node_count(),
        if args.no_stream { "off" } else { "on" },
    );
    std::io::stdout().flush().expect("flush stdout");
    eprintln!(
        "vgige: virtual U3V camera '{}' on the gadget port, proxying {} ({} local XML bytes, {} injected nodes); Ctrl+C to stop",
        GADGET_NAME,
        args.down,
        xml.blob().len(),
        xml.node_count(),
    );

    // Serve ep0 on its own thread; the signal loop stays in main.
    let serve_device = device.clone();
    let serve = std::thread::Builder::new()
        .name("vgige-u3v-ep0".into())
        .spawn(move || {
            let _ = gadget.serve(&serve_device);
        })
        .expect("spawn ep0 serve thread");
    while !signal::shutdown_requested() {
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("vgige: shutting down");
    device.stop();
    drop(device);
    drop(relay);
    // The ep0 thread is detached: it is blocked in a read on the
    // endpoint file, which unblocks when the process tears the gadget
    // down; the process exits right below (daemon shutdown semantics).
    drop(serve);
    println!("VGIGE_STOPPED");
}
