//! High-rate GVSP load generator for receive-path benchmarking (the
//! "100GigE simulator" role): sprays spec-shaped GVSP image blocks at a
//! target as fast as the stack allows, replacing a real camera when
//! testing 25/50/100G-class receive performance.
//!
//! Wire format is identical to the simulated camera's emitter (GVSP 3.0,
//! extended 64-bit block ids, spec §24–25): leader (packet_id 0), generic
//! payload packets (packet_id from 1, all full except the last, R-263st/
//! R-573st), trailer. Payload bytes are a fixed pre-filled pattern — the
//! hot loop only rewrites the 20-byte GVSP header per datagram.
//!
//! Usage:
//!   cargo run --release --example gvsp_flood -- <ip:port> [options]
//!     --width N        image width for the leader   (default 4096)
//!     --height N       image height                 (default 3072)
//!     --pf HEX         PFNC pixel format            (default 01080001 = Mono8)
//!     --payload N      GVSP payload bytes/packet    (default 9000, jumbo)
//!     --streams N      parallel streams; dest port increments per stream
//!                      (default 1; each stream is one sender thread with
//!                      its own block-id sequence on its own port, like a
//!                      multi-stream camera — the receiver must listen on
//!                      every port; never interleave streams on one port,
//!                      a single reassembler would drop every block)
//!     --seconds F      run time                     (default 10)
//!     --pps N          per-stream packet pacing     (default 0 = flat out)
//!     --block-gap-us N pause between blocks/stream  (default 0)
//!
//! Examples:
//!   # current single-thread receive path, jumbo frames, flat out:
//!   cargo run --release --example gvsp_perf_rx -- --port 50012 &
//!   cargo run --release --example gvsp_flood -- 127.0.0.1:50012
//!
//!   # four independent streams (future RSS fan-out shape):
//!   cargo run --release --example gvsp_perf_rx -- --port 50012 --streams 4 &
//!   cargo run --release --example gvsp_flood -- 127.0.0.1:50012 --streams 4
//!
//! Note: 127.0.0.1 loopback does not exercise the NIC/RSS path — it
//! measures the host UDP stack and the receiving process. Pointing the
//! sender at a real NIC address (second machine, or a cross-cabled pair)
//! is needed to measure driver/interrupt behavior.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gentl_gvcp::gvsp::*;

const PFNC_MONO8: u32 = 0x0108_0001;
const PFNC_MONO16: u32 = 0x0110_0007;

#[derive(Clone, Copy)]
struct Opts {
    target: SocketAddr,
    width: u32,
    height: u32,
    pf: u32,
    payload: usize,
    streams: u16,
    seconds: f64,
    pps: u64,
    block_gap: Duration,
}

fn next_val(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next().unwrap_or_else(|| panic!("{flag} needs a value"))
}

fn parse_args() -> Opts {
    let mut o = Opts {
        target: "127.0.0.1:50012".parse().unwrap(),
        width: 4096,
        height: 3072,
        pf: PFNC_MONO8,
        payload: 9000,
        streams: 1,
        seconds: 10.0,
        pps: 0,
        block_gap: Duration::ZERO,
    };
    let mut args = std::env::args().skip(1);
    if let Some(first) = args.next() {
        o.target = first.parse().expect("target must be <ip:port>");
    }
    while let Some(a) = args.next() {
        match a.as_str() {
            "--width" => o.width = next_val(&mut args, &a).parse().unwrap(),
            "--height" => o.height = next_val(&mut args, &a).parse().unwrap(),
            "--pf" => o.pf = u32::from_str_radix(&next_val(&mut args, &a), 16).unwrap(),
            "--payload" => o.payload = next_val(&mut args, &a).parse().unwrap(),
            "--streams" => o.streams = next_val(&mut args, &a).parse().unwrap(),
            "--seconds" => o.seconds = next_val(&mut args, &a).parse().unwrap(),
            "--pps" => o.pps = next_val(&mut args, &a).parse().unwrap(),
            "--block-gap-us" => {
                o.block_gap = Duration::from_micros(next_val(&mut args, &a).parse().unwrap())
            }
            other => panic!("unknown flag {other}"),
        }
    }
    assert!(o.streams >= 1, "at least one stream");
    o
}

fn bytes_per_pixel(pf: u32) -> usize {
    if pf == PFNC_MONO16 { 2 } else { 1 }
}

fn main() {
    let opts = parse_args();
    let image_len = opts.width as usize * opts.height as usize * bytes_per_pixel(opts.pf);
    let max_payload = 65_507 - GVSP_HEADER_LEN;
    assert!(
        opts.payload >= 1 && opts.payload <= max_payload,
        "payload must be 1..={max_payload}"
    );
    let packets_per_block = image_len.div_ceil(opts.payload);
    assert!(
        packets_per_block + 2 <= 65_536,
        "block needs {} packets (16-bit packet_id space, 2 for leader/trailer)",
        packets_per_block + 2
    );
    println!(
        "flood: {} stream(s) -> {} ({}x{}, pf={:#010x})",
        opts.streams, opts.target, opts.width, opts.height, opts.pf
    );
    println!(
        "flood: {} payload bytes/packet, {} packets/block, {} payload bytes/block",
        opts.payload, packets_per_block, image_len
    );

    let stop = Arc::new(AtomicBool::new(false));
    let packets = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for s in 0..opts.streams {
        let o = Opts {
            target: SocketAddr::new(
                opts.target.ip(),
                opts.target.port().checked_add(s).expect("port overflow"),
            ),
            ..opts
        };
        let stop = stop.clone();
        let packets = packets.clone();
        let bytes = bytes.clone();
        handles.push(std::thread::spawn(move || {
            flood_thread(&o, &stop, &packets, &bytes)
        }));
    }

    std::thread::sleep(Duration::from_secs_f64(opts.seconds));
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
    let p = packets.load(Ordering::Relaxed);
    let b = bytes.load(Ordering::Relaxed);
    println!(
        "flood: sent {} packets, {} payload bytes in {:.1}s = {:.2} Mpps, {:.2} Gbit/s (GVSP payload only)",
        p,
        b,
        opts.seconds,
        p as f64 / opts.seconds / 1e6,
        b as f64 * 8.0 / opts.seconds / 1e9
    );
}

fn flood_thread(o: &Opts, stop: &AtomicBool, packets: &AtomicU64, bytes: &AtomicU64) {
    let image_len = o.width as usize * o.height as usize * bytes_per_pixel(o.pf);
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("bind");
    socket.connect(o.target).expect("connect");

    // One reusable datagram: GVSP header + payload, payload pre-filled once.
    let mut packet = vec![0u8; GVSP_HEADER_LEN + o.payload];
    for (i, b) in packet[GVSP_HEADER_LEN..].iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let mut hdr_buf = [0u8; GVSP_HEADER_LEN];
    let leader_body = encode_image_leader_body(&ImageLeader {
        field_id: 0,
        field_count: 0,
        timestamp: 0,
        pixel_format: o.pf,
        size_x: o.width,
        size_y: o.height,
        offset_x: 0,
        offset_y: 0,
        padding_x: 0,
        padding_y: 0,
    });
    let trailer_body = encode_image_trailer_body(o.height);
    let leader_dgram_len = GVSP_HEADER_LEN + leader_body.len();
    let trailer_dgram_len = GVSP_HEADER_LEN + trailer_body.len();

    // R-259st: block_id restarts at 1 on channel (re)open.
    let mut block_id = 1u64;
    let epoch = Instant::now();
    let pace = (o.pps > 0).then(|| Duration::from_secs_f64(1.0 / o.pps as f64));
    while !stop.load(Ordering::Relaxed) {
        // Leader (packet_id 0); refresh the embedded timestamp (body 4..12).
        let timestamp = epoch.elapsed().as_micros() as u64;
        write_header(&mut hdr_buf, PACKET_FORMAT_DATA_LEADER, block_id, 0);
        packet[..GVSP_HEADER_LEN].copy_from_slice(&hdr_buf);
        let mut body = leader_body;
        body[4..12].copy_from_slice(&timestamp.to_be_bytes());
        packet[GVSP_HEADER_LEN..leader_dgram_len].copy_from_slice(&body);
        send(&socket, &packet[..leader_dgram_len], packets, bytes, pace, stop);

        // Payloads (packet_id from 1, R-264st).
        let mut packet_id = 1u32;
        let mut offset = 0usize;
        while offset < image_len {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let n = o.payload.min(image_len - offset);
            write_header(&mut hdr_buf, PACKET_FORMAT_DATA_PAYLOAD, block_id, packet_id);
            packet[..GVSP_HEADER_LEN].copy_from_slice(&hdr_buf);
            send(
                &socket,
                &packet[..GVSP_HEADER_LEN + n],
                packets,
                bytes,
                pace,
                stop,
            );
            packet_id += 1;
            offset += n;
        }

        // Trailer closes the block.
        write_header(&mut hdr_buf, PACKET_FORMAT_DATA_TRAILER, block_id, packet_id);
        packet[..GVSP_HEADER_LEN].copy_from_slice(&hdr_buf);
        packet[GVSP_HEADER_LEN..trailer_dgram_len].copy_from_slice(&trailer_body);
        send(
            &socket,
            &packet[..trailer_dgram_len],
            packets,
            bytes,
            pace,
            stop,
        );

        block_id += 1;
        if o.block_gap > Duration::ZERO {
            spin_sleep(o.block_gap, stop);
        }
    }
}

fn write_header(buf: &mut [u8; GVSP_HEADER_LEN], packet_format: u8, block_id: u64, packet_id: u32) {
    GvspHeader {
        status: 0,
        flag: 0,
        extended_id: true,
        packet_format,
        block_id,
        packet_id,
    }
    .encode(buf);
}

fn send(
    socket: &UdpSocket,
    dgram: &[u8],
    packets: &AtomicU64,
    bytes: &AtomicU64,
    pace: Option<Duration>,
    stop: &AtomicBool,
) {
    // Blocking send: loopback backpressure throttles us naturally, like a
    // camera whose link is saturated. Send errors are ignored as on a real
    // camera (the receiver may have gone away).
    let _ = socket.send(dgram);
    packets.fetch_add(1, Ordering::Relaxed);
    bytes.fetch_add(dgram.len() as u64 - GVSP_HEADER_LEN as u64, Ordering::Relaxed);
    if let Some(p) = pace {
        spin_sleep(p, stop);
    }
}

/// Burn the CPU for `d` (pacing accuracy at high pps), bailing on stop.
fn spin_sleep(d: Duration, stop: &AtomicBool) {
    let start = Instant::now();
    while start.elapsed() < d {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::hint::spin_loop();
    }
}
