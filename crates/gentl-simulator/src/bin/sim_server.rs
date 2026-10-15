//! sim_server: serve one `SimCamera` over GVCP/UDP until killed.
//!
//! Test harness for the C++ GenApi consumer acceptance (consumer-test/):
//! a C++ process cannot start Rust's SimCamera itself, so this binary runs
//! as a child process next to the GenApi test. The harness points the
//! producer at the loopback camera through its documented test hooks
//! (`GENTLITE_GVCP_PORT`, `GENTLITE_DISCOVERY_TARGETS`; notes Q11) with the
//! port printed below.
//!
//! Usage: `sim_server --port <udp>` (`0` = system-assigned). The bound port
//! is printed as `SIM_PORT <n>` on stdout.

use std::io::Write as _;
use std::net::Ipv4Addr;

use gentl_simulator::{SimCamera, SimConfig};

/// Fixed identity; the C++ consumer locates the sim by its device ID
/// (the MAC string, notes/OPEN-QUESTIONS Q4).
const MAC: [u8; 6] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];

fn parse_port() -> u16 {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => {
                let v = args.next().unwrap_or_else(|| {
                    eprintln!("--port requires a value");
                    std::process::exit(2);
                });
                return v.parse().unwrap_or_else(|_| {
                    eprintln!("--port: `{v}` is not a u16");
                    std::process::exit(2);
                });
            }
            "--help" | "-h" => {
                println!("usage: sim_server --port <udp port, 0 = auto>");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument `{other}` (try --help)");
                std::process::exit(2);
            }
        }
    }
    0
}

fn main() {
    let port = parse_port();

    let cam = SimCamera::new(SimConfig {
        mac: MAC,
        ip: Ipv4Addr::LOCALHOST,
        subnet_mask: Ipv4Addr::new(255, 0, 0, 0),
        gateway: Ipv4Addr::UNSPECIFIED,
        manufacturer_name: "GenTLite".into(),
        model_name: "SimCam".into(),
        device_version: "1.0.0".into(),
        serial_number: "SIM-0001".into(),
    });
    let server = cam.start(port).expect("start sim camera");
    let bound = server.local_port().expect("query server port");

    // One line the harness greps for; flush so it is visible immediately.
    println!("SIM_PORT {bound}");
    std::io::stdout().flush().expect("flush stdout");
    eprintln!("sim_server: SimCamera {bound} (device id 00:11:22:33:44:55)");

    // The GvcpServer serves from its own thread; park until the harness
    // kills us.
    loop {
        std::thread::park();
    }
}
