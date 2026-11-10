//! Q27 real-machine probe: does this camera answer GVCP commands with the
//! spec-figure flag (0x01) or the lsb-numbered variant (0x80)?
//!
//! Sends the same transactions twice — once per flag encoding — and reports
//! which elicits an acknowledge. CCP is acquired with 0x80 (pre-Q27 behavior
//! known to work on this camera family) and ALWAYS released, even on panic.
//!
//! Usage:
//!   cargo run --example flag_probe -- <camera-ip>

#![allow(non_snake_case)]

use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

use gentl_gvcp::wire::*;

const ACQ_START: u32 = 0x1000_0024; // AcquisitionStart (this camera's XML)

fn send_cmd(sock: &UdpSocket, target: (Ipv4Addr, u16), flag: u8, command: u16, payload: &[u8], req_id: u16) {
    let mut pkt = Vec::with_capacity(HEADER_LEN + payload.len());
    let mut hdr = [0u8; HEADER_LEN];
    CmdHeader { flag, command, length: payload.len() as u16, req_id }.encode(&mut hdr);
    pkt.extend_from_slice(&hdr);
    pkt.extend_from_slice(payload);
    sock.send_to(&pkt, target).expect("send_to");
}

fn probe(
    sock: &UdpSocket,
    target: (Ipv4Addr, u16),
    label: &str,
    flag: u8,
    command: u16,
    payload: &[u8],
    req_id: u16,
) -> String {
    send_cmd(sock, target, flag, command, payload, req_id);
    let mut buf = [0u8; 2048];
    let deadline = Instant::now() + Duration::from_millis(700);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return format!("{label}: flag=0x{flag:02x} -> TIMEOUT (no ACK)");
        }
        sock.set_read_timeout(Some(remaining.min(Duration::from_millis(100)))).unwrap();
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let Some(ack) = AckHeader::decode(&buf[..n]) else { continue };
                if ack.ack_id != req_id {
                    continue; // stale
                }
                if command == READREG_CMD && ack.status == 0 && n >= 12 {
                    let v = u32::from_be_bytes(buf[8..12].try_into().unwrap());
                    return format!(
                        "{label}: flag=0x{flag:02x} -> ACK value=0x{v:08x} ({})",
                        Ipv4Addr::from(v)
                    );
                }
                return format!(
                    "{label}: flag=0x{flag:02x} -> ACK status=0x{:04x}",
                    ack.status
                );
            }
            Err(_) => continue,
        }
    }
}

fn main() {
    let target: Ipv4Addr = std::env::args()
        .nth(1)
        .expect("usage: flag_probe <camera-ip>")
        .parse()
        .expect("bad ip");

    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    sock.connect((target, GVCP_PORT)).unwrap();
    let tgt = (target, GVCP_PORT);

    // Acquire exclusive access with the historically-working 0x80 flag.
    let mut ccp = Vec::new();
    ccp.extend_from_slice(&CCP.to_be_bytes());
    ccp.extend_from_slice(&1u32.to_be_bytes()); // exclusive_access (Q31 masks)
    let mut req = 100u16;

    let acquire = probe(&sock, tgt, "CCP=EA     ", 0x80, WRITEREG_CMD, &ccp, req);
    println!("{acquire}");
    req += 1;

    // Same register, both flag encodings. Write 0 to AcquisitionStart is a
    // no-op (the command value is 1), safe to repeat.
    let mut wr = Vec::new();
    wr.extend_from_slice(&ACQ_START.to_be_bytes());
    wr.extend_from_slice(&0u32.to_be_bytes());
    println!("{}", probe(&sock, tgt, "WRITEREG   ", 0x01, WRITEREG_CMD, &wr, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "WRITEREG   ", 0x80, WRITEREG_CMD, &wr, req));
    req += 1;

    let rd = ACQ_START.to_be_bytes();
    println!("{}", probe(&sock, tgt, "READREG    ", 0x01, READREG_CMD, &rd, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "READREG    ", 0x80, READREG_CMD, &rd, req));
    req += 1;

    // Always release CCP (0 write is always legal; use 0x80).
    let mut rel = Vec::new();
    rel.extend_from_slice(&CCP.to_be_bytes());
    rel.extend_from_slice(&0u32.to_be_bytes());
    println!("{}", probe(&sock, tgt, "CCP=0 rel  ", 0x80, WRITEREG_CMD, &rel, req));
    req += 1;

    // Phase 2: replicate the producer's stream-channel open, then probe the
    // control channel again. If WRITEREG starts timing out only after SCP0
    // is opened, the camera firmware starves control ACKs while streaming.
    let local: Ipv4Addr = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "192.168.1.1".into())
        .parse()
        .expect("bad local ip");

    println!("{}", probe(&sock, tgt, "reacquire  ", 0x80, WRITEREG_CMD, &ccp, req));
    req += 1;

    let mut scda = Vec::new();
    scda.extend_from_slice(&SCDA0.to_be_bytes());
    scda.extend_from_slice(&u32::from(local).to_be_bytes());
    println!("{}", probe(&sock, tgt, "SCDA0      ", 0x01, WRITEREG_CMD, &scda, req));
    req += 1;

    let mut scp = Vec::new();
    scp.extend_from_slice(&SCP0.to_be_bytes());
    scp.extend_from_slice(&40012u32.to_be_bytes()); // bare port = spec layout (Q31)
    println!("{}", probe(&sock, tgt, "SCP0 open  ", 0x01, WRITEREG_CMD, &scp, req));
    req += 1;

    std::thread::sleep(Duration::from_millis(300));
    println!("{}", probe(&sock, tgt, "WRITEREG s1", 0x01, WRITEREG_CMD, &wr, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "WRITEREG s2", 0x01, WRITEREG_CMD, &wr, req));
    req += 1;

    // Close the channel and release.
    let mut scp0 = Vec::new();
    scp0.extend_from_slice(&SCP0.to_be_bytes());
    scp0.extend_from_slice(&0u32.to_be_bytes());
    println!("{}", probe(&sock, tgt, "SCP0 close ", 0x01, WRITEREG_CMD, &scp0, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "CCP=0 fin  ", 0x80, WRITEREG_CMD, &rel, req));
    req += 1;

    // Phase 3: reopen the channel with a DRAINING receiver bound (camera
    // blasts at full rate), then probe the control channel. If WRITEREG
    // times out only while frames actually flow, the firmware starves
    // control ACKs during streaming.
    println!("{}", probe(&sock, tgt, "reacquire2 ", 0x80, WRITEREG_CMD, &ccp, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "SCDA0 #2   ", 0x01, WRITEREG_CMD, &scda, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "SCP0 open2 ", 0x01, WRITEREG_CMD, &scp, req));
    req += 1;

    let drain = UdpSocket::bind((local, 40012u16)).unwrap();
    drain.set_nonblocking(true).unwrap();
    let mut drain_buf = [0u8; 65536];
    let mut received = 0usize;
    let mut first_bytes = [0u8; 64];
    let mut first_len = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(500) {
        while let Ok((n, _)) = drain.recv_from(&mut drain_buf) {
            if received == 0 && n >= 8 {
                first_len = n.min(64);
                first_bytes[..first_len].copy_from_slice(&drain_buf[..first_len]);
            }
            received += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("drain: {received} datagrams in 500ms, first len {first_len}, head {:02x?}", &first_bytes[..first_len.min(24)]);

    println!("{}", probe(&sock, tgt, "WRITEREG d1", 0x01, WRITEREG_CMD, &wr, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "WRITEREG d2", 0x01, WRITEREG_CMD, &wr, req));
    req += 1;

    println!("{}", probe(&sock, tgt, "SCP0 close2", 0x01, WRITEREG_CMD, &scp0, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "CCP=0 fin2 ", 0x80, WRITEREG_CMD, &rel, req));
    req += 1;
    drop(drain);

    // Phase 4: open channel, ensure AcquisitionMode=Continuous(2), start
    // acquisition, count streamed datagrams on the drain socket, stop.
    const ACQ_MODE: u32 = 0x1000_0020;
    println!("{}", probe(&sock, tgt, "reacquire3 ", 0x80, WRITEREG_CMD, &ccp, req));
    req += 1;
    // Force-close any channel a crashed consumer left open (R-091ca: zero
    // write is always legal) before reopening.
    let mut scp0_pre = Vec::new();
    scp0_pre.extend_from_slice(&SCP0.to_be_bytes());
    scp0_pre.extend_from_slice(&0u32.to_be_bytes());
    println!("{}", probe(&sock, tgt, "SCP0 fclose", 0x01, WRITEREG_CMD, &scp0_pre, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "SCDA0 #3   ", 0x01, WRITEREG_CMD, &scda, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "SCP0 open3 ", 0x01, WRITEREG_CMD, &scp, req));
    req += 1;

    let am = ACQ_MODE.to_be_bytes();
    println!("{}", probe(&sock, tgt, "AcqMode rd ", 0x01, READREG_CMD, &am, req));
    req += 1;
    let mut amw = Vec::new();
    amw.extend_from_slice(&ACQ_MODE.to_be_bytes());
    amw.extend_from_slice(&2u32.to_be_bytes());
    println!("{}", probe(&sock, tgt, "AcqMode=2  ", 0x01, WRITEREG_CMD, &amw, req));
    req += 1;

    let drain = if std::env::var("PROBE_NO_DRAIN").is_ok() {
        None // handoff: an external listener (e.g. gvsp_perf_rx) owns the port
    } else {
        Some(UdpSocket::bind((local, 40012u16)).unwrap())
    };
    if let Some(d) = &drain {
        d.set_nonblocking(true).unwrap();
    }

    let mut acq1 = Vec::new();
    acq1.extend_from_slice(&ACQ_START.to_be_bytes());
    acq1.extend_from_slice(&1u32.to_be_bytes());
    println!("{}", probe(&sock, tgt, "AcqStart=1 ", 0x01, WRITEREG_CMD, &acq1, req));
    req += 1;

    let mut drain_buf = [0u8; 65536];
    let mut received = 0usize;
    let mut first = [0u8; 32];
    let mut first_len = 0usize;
    let mut leader = [0u8; 48];
    let mut leader_len = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1500) {
        if let Some(d) = &drain {
            while let Ok((n, _)) = d.recv_from(&mut drain_buf) {
                if received == 0 {
                    first_len = n.min(32);
                    first[..first_len].copy_from_slice(&drain_buf[..first_len]);
                }
                if drain_buf.len() > 8 && drain_buf[4] == 1 && leader_len == 0 {
                    leader_len = n.min(48);
                    leader[..leader_len].copy_from_slice(&drain_buf[..leader_len]);
                    let _ = std::fs::write("D:/tmp/real_leader.bin", &drain_buf[..n]);
                    let _ = std::fs::write("D:/tmp/real_leader.len", n.to_string());
                }
                received += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!(
        "stream: {received} datagrams in 1.5s, first len {first_len}, head {:02x?}",
        &first[..first_len.min(24)]
    );
    println!("legacy leader ({} bytes): {:02x?}", leader_len, &leader[..leader_len]);

    let mut acq0 = Vec::new();
    acq0.extend_from_slice(&ACQ_START.to_be_bytes());
    acq0.extend_from_slice(&0u32.to_be_bytes());
    println!("{}", probe(&sock, tgt, "AcqStop=0  ", 0x01, WRITEREG_CMD, &acq0, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "SCP0 close3", 0x01, WRITEREG_CMD, &scp0, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "CCP=0 fin3 ", 0x80, WRITEREG_CMD, &rel, req));
    req += 1;

    // Phase 5: how many network interfaces does the camera report, and what
    // is interface #1's current IP? (Two MVS entries on this PC suggest a
    // dual-identity camera; Table 36-1: NUM_NETWORK_INTERFACES @ 0x0600,
    // interface #1 Current IP @ 0x069C.)
    println!("{}", probe(&sock, tgt, "reacquire4 ", 0x80, WRITEREG_CMD, &ccp, req));
    req += 1;
    for (label, addr) in [
        ("NumIfs 0x600", 0x0600u32),
        ("If0 IP  0x024", 0x0024),
        ("If1 IP  0x69C", 0x069C),
        ("If1 Cfg 0x68C", 0x068C),
    ] {
        let rd = addr.to_be_bytes();
        println!("{}", probe(&sock, tgt, label, 0x01, READREG_CMD, &rd, req));
        req += 1;
    }
    println!("{}", probe(&sock, tgt, "CCP=0 fin4 ", 0x80, WRITEREG_CMD, &rel, req));
    req += 1;

    // Phase 6: soft-reset the camera's network stack with FORCEIP
    // (static_IP=0 -> device closes channels and restarts IP configuration,
    // no ACK, R-151cd). Recovery attempt for the camera-side hang observed
    // after killed MVS sessions.
    println!("-- FORCEIP soft reset --");
    let mut fi = [0u8; 56];
    fi[2..8].copy_from_slice(&[0x00, 0x30, 0x53, 0x9b, 0xdb, 0x6f]);
    // static IP / mask / gateway left zero -> restart IP config cycle.
    // R-148ca: FORCEIP MUST be broadcast, so a dedicated unconnected
    // socket with SO_BROADCAST (the control socket is connected).
    {
        let bcast = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        bcast.set_broadcast(true).unwrap();
        send_cmd(&bcast, (Ipv4Addr::new(255, 255, 255, 255), GVCP_PORT), 0x11, FORCEIP_CMD, &fi, req);
    }
    req += 1;
    std::thread::sleep(Duration::from_millis(3000));

    println!("{}", probe(&sock, tgt, "reacquire5 ", 0x80, WRITEREG_CMD, &ccp, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "SCDA0 #5   ", 0x01, WRITEREG_CMD, &scda, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "SCP0 open5 ", 0x01, WRITEREG_CMD, &scp, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "AcqMode=2b ", 0x01, WRITEREG_CMD, &amw, req));
    req += 1;

    let drain = UdpSocket::bind((local, 40012u16)).unwrap();
    drain.set_nonblocking(true).unwrap();
    println!("{}", probe(&sock, tgt, "AcqStart=1b", 0x01, WRITEREG_CMD, &acq1, req));
    req += 1;
    let mut drain_buf = [0u8; 65536];
    let mut received = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1500) {
        while let Ok((_, _)) = drain.recv_from(&mut drain_buf) {
            received += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("stream after FORCEIP: {received} datagrams in 1.5s");
    println!("{}", probe(&sock, tgt, "AcqStop=0b ", 0x01, WRITEREG_CMD, &acq0, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "SCP0 close5", 0x01, WRITEREG_CMD, &scp0, req));
    req += 1;
    println!("{}", probe(&sock, tgt, "CCP=0 fin5 ", 0x80, WRITEREG_CMD, &rel, req));
}

const CCP: u32 = 0x0A00;
const SCDA0: u32 = 0x0D18;
const SCP0: u32 = 0x0D00;
