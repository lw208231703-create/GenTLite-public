//! Dump the embedded GenICam XML to stdout (or a file) for inspection.
//! Usage: cargo run -p gentl-simulator --example dump_xml [out.xml]

use std::io::Write;
use std::net::Ipv4Addr;

use gentl_simulator::{SimCamera, SimConfig};

fn main() {
    let cam = SimCamera::new(SimConfig {
        mac: [0x02, 0x00, 0x11, 0x22, 0x33, 0x44],
        ip: Ipv4Addr::new(192, 168, 1, 10),
        subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
        gateway: Ipv4Addr::new(192, 168, 1, 1),
        manufacturer_name: "GenTLite".into(),
        model_name: "SimCam".into(),
        device_version: "1.0.0".into(),
        serial_number: "SIM-0001".into(),
    });
    let xml = cam.xml();
    match std::env::args().nth(1) {
        Some(path) => std::fs::File::create(path)
            .unwrap()
            .write_all(xml.as_bytes())
            .unwrap(),
        None => print!("{xml}"),
    }
}
