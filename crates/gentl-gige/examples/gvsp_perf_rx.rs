//! GVSP receive-path benchmark: the production reassembly pipeline
//! (`receiver_loop`'s exact shape — one UDP socket and one
//! `GvspReassembler` per stream, feeding a shared `DsCore`) driven by a
//! configurable number of parallel streams, so the single-thread ceiling
//! of the current architecture (and later the RSS fan-out work) can be
//! measured without a real camera. Pair with the simulator's
//! `gvsp_flood` sender.
//!
//! Buffers are recycled by a background thread that periodically moves
//! delivered (Filled) buffers back to the input pool — the perf analog of
//! a consumer calling DSQueueBuffer on every frame.
//!
//! Usage:
//!   cargo run --release --example gvsp_perf_rx -- [options]
//!     --port N         first stream port, +1 per stream (default 50012)
//!     --streams N      parallel receive sockets/threads (default 1 —
//!                      the current production shape; >1 previews the
//!                      M3-4 RSS fan-out architecture)
//!     --seconds F      run time                (default 10)
//!     --buf-size N     bytes per buffer        (default 16 MiB)
//!     --buf-count N    announced buffers       (default 16)
//!     --recycle-ms N   buffer recycle period   (default 50)
//!
//! Example (start the receiver first, then the sender):
//!   cargo run --release --example gvsp_perf_rx -- --port 50012 --streams 4
//!   cargo run --release --example gvsp_flood -- 127.0.0.1:50012 --streams 4

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gentl_core::ds::{AcqQueueOp, DsCore, GENTL_INFINITE};
use gentl_gige::{GvspReassembler, ReceiverStats};

struct Opts {
    port: u16,
    streams: u16,
    seconds: f64,
    buf_size: usize,
    buf_count: usize,
    recycle: Duration,
}

fn next_val(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next().unwrap_or_else(|| panic!("{flag} needs a value"))
}

fn parse_args() -> Opts {
    let mut o = Opts {
        port: 50012,
        streams: 1,
        seconds: 10.0,
        buf_size: 16 * 1024 * 1024,
        buf_count: 16,
        recycle: Duration::from_millis(50),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => o.port = next_val(&mut args, &a).parse().unwrap(),
            "--streams" => o.streams = next_val(&mut args, &a).parse().unwrap(),
            "--seconds" => o.seconds = next_val(&mut args, &a).parse().unwrap(),
            "--buf-size" => o.buf_size = next_val(&mut args, &a).parse().unwrap(),
            "--buf-count" => o.buf_count = next_val(&mut args, &a).parse().unwrap(),
            "--recycle-ms" => o.recycle = Duration::from_millis(next_val(&mut args, &a).parse().unwrap()),
            other => panic!("unknown flag {other}"),
        }
    }
    assert!(o.streams >= 1, "at least one stream");
    o
}

fn main() {
    let opts = parse_args();
    println!(
        "perf_rx: {} stream(s) on port {}..{}, {}x {} MiB buffers for {:.0}s",
        opts.streams,
        opts.port,
        opts.port + opts.streams - 1,
        opts.buf_count,
        opts.buf_size / 1024 / 1024,
        opts.seconds
    );

    let core = Arc::new(DsCore::new());
    for _ in 0..opts.buf_count {
        let id = core.announce_owned(opts.buf_size, std::ptr::null_mut()).expect("announce");
        core.queue_buffer(id).expect("queue");
    }
    core.start_acquisition(GENTL_INFINITE).expect("start");

    let stop = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + Duration::from_secs_f64(opts.seconds);

    // Buffer recycler: stand-in for a consumer re-queueing delivered
    // buffers (DSQueueBuffer per frame). Filled -> input pool.
    {
        let core = core.clone();
        let stop = stop.clone();
        let recycle = opts.recycle;
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if core.await_delivery_count() > 0 {
                    let _ = core.flush(AcqQueueOp::AllToInput);
                }
                std::thread::sleep(recycle);
            }
        });
    }

    let mut handles = Vec::new();
    for s in 0..opts.streams {
        let core = core.clone();
        let stop = stop.clone();
        let port = opts.port + s;
        handles.push(std::thread::spawn(move || {
            recv_thread(port, core, deadline, stop)
        }));
    }
    let mut all = ReceiverStats::default();
    for h in handles {
        let s = h.join().unwrap();
        all.merge(&s);
    }
    stop.store(true, Ordering::Relaxed);

    let secs = opts.seconds;
    let udp_bytes = all.payload_bytes + all.datagrams * 48; // IP+UDP+GVSP headers
    println!("perf_rx: {:.1}s", secs);
    println!(
        "perf_rx: {} datagrams, {:.2} Mpps, {:.2} Gbit/s on the wire (incl. 48B headers)",
        all.datagrams,
        all.datagrams as f64 / secs / 1e6,
        udp_bytes as f64 * 8.0 / secs / 1e9
    );
    println!(
        "perf_rx: {} payload bytes written, {:.2} GByte/s into buffers",
        all.payload_bytes,
        all.payload_bytes as f64 / secs / 1e9
    );
    println!(
        "perf_rx: blocks: {} delivered, {} completed, dropped: {} incomplete / {} no-buffer / {} too-small / {} not-acquiring / {} unsupported",
        all.frames_delivered,
        all.frames_completed,
        all.frames_dropped_incomplete,
        all.frames_dropped_no_buffer,
        all.frames_dropped_too_small,
        all.frames_dropped_not_acquiring,
        all.frames_dropped_unsupported
    );
    println!(
        "perf_rx: engine: {} delivered, {} underruns",
        core.num_delivered(),
        core.num_underrun()
    );
}

fn recv_thread(
    port: u16,
    core: Arc<DsCore>,
    deadline: Instant,
    stop: Arc<AtomicBool>,
) -> ReceiverStats {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).expect("bind");
    let _ = socket.set_read_timeout(Some(Duration::from_millis(100)));
    let mut reasm = GvspReassembler::new();
    let mut buf = [0u8; 65536];
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => reasm.on_datagram(&core, &buf[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                eprintln!("perf_rx: stream {port}: recv error: {e}");
                break;
            }
        }
    }
    reasm.stats().clone()
}
