//! GVSP fake-image transmitter of the simulated camera (spec Part 3 §22–26;
//! notes/gvcp-wirefacts.md "GVSP" section). One `StreamEmitter` per open
//! stream channel: a UDP socket plus one thread emitting image blocks
//! back-to-back to (SCDA0, SCP0).
//!
//! Simplifications vs a real camera (documented):
//! - Blocks are emitted continuously while the channel is open; there is no
//!   AcquisitionStart gating (notes/OPEN-QUESTIONS.md Q15).
//! - The inter-packet delay (SCPD0, spec §36.59) is a plain thread sleep in
//!   timestamp-counter units (the sim clock ticks at 1 MHz, so µs).
//! - Packet resend (PACKETRESEND_CMD) is not implemented (M3 scope).

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gentl_gvcp::gvsp::*;
use gentl_gvcp::registers as reg;

use crate::StreamConfig;

/// Payload bytes per full GVSP generic payload packet when SCPS0 reads as
/// 0 (unwritten): 1400 data bytes, so the IPv4 total (1400 + IP/UDP/GVSP
/// headers) stays at 1448, under the 1500-byte MTU.
pub(crate) const DEFAULT_PAYLOAD: usize = 1400;

/// Largest payload data chunk put into one datagram (maximum IPv4 UDP
/// payload minus the GVSP header).
const MAX_PAYLOAD: usize = 65_507 - GVSP_HEADER_LEN;

/// Pause between consecutive blocks while streaming.
const FRAME_INTERVAL: Duration = Duration::from_millis(1);

// PFNC pixel format value (include/PFNC.h).
const PFNC_MONO16: u32 = 0x0110_0007;

/// Emitter settings frozen at channel open. A real device forbids SCPSx
/// writes while streaming (spec §36.58), so the sim snapshots the registers
/// instead of re-reading them per packet.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StreamParams {
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub payload_size: usize,
    pub packet_delay: Duration,
}

impl StreamParams {
    /// Snapshot from the SCPS0/SCPD0 register values (spec layout,
    /// §36.58/§36.59) and the camera's stream configuration.
    ///
    /// SCPS0 bits 16-31 hold the *total* packet size including the
    /// IP/UDP/GVSP headers; the payload data length per packet is
    /// packet_size - GVSP_HEADERS_OVERHEAD. An unwritten (0) or absurdly
    /// small (< headers) packet_size falls back to [`DEFAULT_PAYLOAD`]
    /// (a real device rounds to the nearest supported size instead,
    /// R-562st — see notes/OPEN-QUESTIONS.md Q21).
    pub fn new(stream: &StreamConfig, scps0: u32, scpd0: u32) -> Self {
        let requested = match reg::scps_packet_size(scps0)
            .checked_sub(reg::GVSP_HEADERS_OVERHEAD)
        {
            Some(p) if p > 0 => p as usize,
            _ => DEFAULT_PAYLOAD,
        };
        // Multi-byte pixels must not be split across packets, so the payload
        // size is rounded down to a pixel multiple.
        let align = if stream.pixel_format == PFNC_MONO16 { 2 } else { 1 };
        StreamParams {
            width: stream.width,
            height: stream.height,
            pixel_format: stream.pixel_format,
            payload_size: requested.clamp(1, MAX_PAYLOAD) / align * align,
            packet_delay: Duration::from_micros(u64::from(scpd0)),
        }
    }

    fn bytes_per_pixel(&self) -> usize {
        if self.pixel_format == PFNC_MONO16 { 2 } else { 1 }
    }

    fn image_len(&self) -> usize {
        let pixels = u64::from(self.width) * u64::from(self.height);
        pixels.saturating_mul(self.bytes_per_pixel() as u64).min(1 << 31) as usize
    }
}

/// Handle to the running transmitter. `Drop` signals the thread and joins
/// it; the emitter never touches the register map, so this cannot deadlock.
pub(crate) struct StreamEmitter {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StreamEmitter {
    pub fn open(target: SocketAddr, params: StreamParams) -> io::Result<Self> {
        // Dynamic source port per [O-086st].
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let stop = stop.clone();
            move || emit_loop(socket, target, params, stop)
        });
        Ok(StreamEmitter {
            stop,
            thread: Some(thread),
        })
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for StreamEmitter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn emit_loop(socket: UdpSocket, target: SocketAddr, params: StreamParams, stop: Arc<AtomicBool>) {
    // R-259st: block_id64 restarts at 1 every time the channel opens.
    let mut block_id = 1u64;
    let epoch = Instant::now();
    // One reusable datagram buffer per block; the GVSP header carries no
    // length, receivers derive sizes from the UDP datagram length.
    let mut packet = [0u8; GVSP_HEADER_LEN + MAX_PAYLOAD];
    while !stop.load(Ordering::Relaxed) {
        // 1 MHz sim clock, same tick as the Timestamp Value registers.
        let timestamp = epoch.elapsed().as_micros() as u64;
        send_block(&socket, target, &params, block_id, timestamp, &mut packet, &stop);
        block_id += 1;
        sleep_interruptible(FRAME_INTERVAL, &stop);
    }
}

/// Send one block: data leader (packet_id 0), data payloads (packet_id from
/// 1, R-264st), data trailer. All packets but the last payload carry exactly
/// `payload_size` bytes (R-263st); none ever exceeds it (R-573st).
fn send_block(
    socket: &UdpSocket,
    target: SocketAddr,
    params: &StreamParams,
    block_id: u64,
    timestamp: u64,
    packet: &mut [u8],
    stop: &AtomicBool,
) {
    let image_len = params.image_len();

    write_header(packet, PACKET_FORMAT_DATA_LEADER, block_id, 0);
    let leader = ImageLeader {
        field_id: 0,
        field_count: 0,
        timestamp,
        pixel_format: params.pixel_format,
        size_x: params.width,
        size_y: params.height,
        offset_x: 0,
        offset_y: 0,
        padding_x: 0,
        padding_y: 0,
    };
    let body = encode_image_leader_body(&leader);
    packet[GVSP_HEADER_LEN..GVSP_HEADER_LEN + body.len()].copy_from_slice(&body);
    if !send_until_stopped(socket, target, &packet[..GVSP_HEADER_LEN + body.len()], stop) {
        return;
    }

    let mut packet_id = 1u32;
    let mut offset = 0usize;
    while offset < image_len {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let n = params.payload_size.min(image_len - offset);
        fill_pattern(
            &mut packet[GVSP_HEADER_LEN..GVSP_HEADER_LEN + n],
            offset,
            block_id,
            params.bytes_per_pixel(),
        );
        write_header(packet, PACKET_FORMAT_DATA_PAYLOAD, block_id, packet_id);
        if !send_until_stopped(socket, target, &packet[..GVSP_HEADER_LEN + n], stop) {
            return;
        }
        packet_id += 1;
        offset += n;
        if params.packet_delay > Duration::ZERO {
            sleep_interruptible(params.packet_delay, stop);
        }
    }

    write_header(packet, PACKET_FORMAT_DATA_TRAILER, block_id, packet_id);
    let body = encode_image_trailer_body(params.height);
    packet[GVSP_HEADER_LEN..GVSP_HEADER_LEN + body.len()].copy_from_slice(&body);
    let _ = send_until_stopped(socket, target, &packet[..GVSP_HEADER_LEN + body.len()], stop);
}

fn write_header(packet: &mut [u8], packet_format: u8, block_id: u64, packet_id: u32) {
    let buf: &mut [u8; GVSP_HEADER_LEN] = (&mut packet[..GVSP_HEADER_LEN])
        .try_into()
        .expect("gvsp header buffer");
    GvspHeader {
        status: 0,
        flag: 0,
        extended_id: true, // 64-bit block id (spec §24.2)
        packet_format,
        block_id,
        packet_id,
    }
    .encode(buf);
}

/// Deterministic gradient: pixel `i` derives from `(i + block_id)`, letting
/// a receiver verify both the payload split and the block numbering
/// (mod 256 for Mono8, mod 65536 for Mono16).
fn fill_pattern(buf: &mut [u8], byte_offset: usize, block_id: u64, bpp: usize) {
    if bpp == 1 {
        for (k, b) in buf.iter_mut().enumerate() {
            *b = (byte_offset + k + block_id as usize) as u8;
        }
    } else {
        let first_pixel = byte_offset / 2;
        for (p, chunk) in buf.chunks_exact_mut(2).enumerate() {
            // Multi-byte pixel data is little-endian on the wire (R-344st).
            let v = (first_pixel + p + block_id as usize) as u16;
            chunk.copy_from_slice(&v.to_le_bytes());
        }
    }
}

/// Send one datagram, waiting out send-buffer backpressure. Returns false
/// when asked to stop while waiting. Hard send errors (e.g. the receiver
/// went away) are ignored like on a real camera: streaming continues.
fn send_until_stopped(socket: &UdpSocket, target: SocketAddr, buf: &[u8], stop: &AtomicBool) -> bool {
    loop {
        match socket.send_to(buf, target) {
            Ok(_) => return true,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Relaxed) {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return true,
        }
    }
}

/// Sleep up to `d`, returning early when `stop` is set.
fn sleep_interruptible(d: Duration, stop: &AtomicBool) {
    let start = Instant::now();
    while start.elapsed() < d {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regmap::SimRegMap;
    use crate::{SimConfig, StreamConfig};
    use gentl_gvcp::registers as reg;
    use gentl_gvcp::wire::GEV_STATUS_BUSY;
    use gentl_gvcp::DeviceHandler;
    use std::net::UdpSocket;

    const MONO8: u32 = 0x0108_0001;

    fn map_with_stream(stream: StreamConfig) -> SimRegMap {
        SimRegMap::new(
            SimConfig {
                mac: [0x02, 0x00, 0x11, 0x22, 0x33, 0x44],
                ip: Ipv4Addr::new(192, 168, 1, 10),
                subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
                gateway: Ipv4Addr::new(192, 168, 1, 1),
                manufacturer_name: "GenTLite".into(),
                model_name: "SimCam".into(),
                device_version: "1.0.0".into(),
                serial_number: "SIM-0001".into(),
            },
            b"<?xml version=\"1.0\"?><fake/>",
            stream,
        )
    }

    /// Local receiver socket with the channel opened towards it.
    fn open_channel(m: &SimRegMap, scps0: Option<u32>) -> UdpSocket {
        let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let port = rx.local_addr().unwrap().port();
        m.write_reg(reg::SCDA0, u32::from(Ipv4Addr::LOCALHOST))
            .unwrap();
        if let Some(v) = scps0 {
            m.write_reg(reg::SCPS0, v).unwrap();
        }
        m.write_reg(reg::SCP0, reg::scp_make(port)).unwrap();
        rx
    }

    struct Packet {
        header: GvspHeader,
        data: Vec<u8>,
    }

    fn recv_packet(rx: &UdpSocket) -> Packet {
        let mut buf = [0u8; 65536];
        let (n, _) = rx.recv_from(&mut buf).expect("gvsp datagram");
        let header = GvspHeader::decode(&buf[..n]).expect("gvsp header");
        Packet {
            header,
            data: buf[GVSP_HEADER_LEN..n].to_vec(),
        }
    }

    /// Receive one full block: leader, `n_payloads` payloads, trailer.
    fn recv_block(rx: &UdpSocket, n_payloads: usize) -> Vec<Packet> {
        (0..n_payloads + 2).map(|_| recv_packet(rx)).collect()
    }

    /// Assert the socket sees no further datagrams (~`window` of silence).
    fn assert_silent(rx: &UdpSocket, window: Duration) {
        let mut buf = [0u8; 2048];
        rx.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        // Flush anything still queued from before the channel closed.
        while rx.recv_from(&mut buf).is_ok() {}
        rx.set_read_timeout(Some(window)).unwrap();
        assert!(
            rx.recv_from(&mut buf).is_err(),
            "datagrams arriving on a closed channel"
        );
    }

    #[test]
    fn emits_mono8_blocks_matching_scps_and_pattern() {
        let m = map_with_stream(StreamConfig {
            width: 64,
            height: 48,
            pixel_format: MONO8,
        });
        // SCPS0 in the spec layout: total packet size 1000+48 -> a 1000-byte
        // payload per full packet.
        let rx = open_channel(&m, Some(reg::scps_make(1000 + reg::GVSP_HEADERS_OVERHEAD)));
        // 64 * 48 = 3072 bytes -> 1000 + 1000 + 1000 + 72.
        for block_idx in 0..10 {
            let block_id = (block_idx + 1) as u64;
            let pkts = recv_block(&rx, 4);

            let leader = &pkts[0];
            assert_eq!(leader.header.packet_format, PACKET_FORMAT_DATA_LEADER);
            assert_eq!(leader.header.packet_id, 0);
            assert_eq!(leader.header.block_id, block_id);
            assert!(leader.header.extended_id);
            let l = ImageLeader::decode(&leader.data).expect("leader body");
            assert_eq!(l.pixel_format, MONO8);
            assert_eq!(l.size_x, 64);
            assert_eq!(l.size_y, 48);

            for (i, p) in pkts[1..5].iter().enumerate() {
                assert_eq!(p.header.packet_format, PACKET_FORMAT_DATA_PAYLOAD);
                assert_eq!(p.header.packet_id, (i + 1) as u32);
                assert_eq!(p.header.block_id, block_id);
                let want = if i < 3 { 1000 } else { 72 };
                assert_eq!(p.data.len(), want, "payload {i} size");
                let base = i * 1000;
                for (k, &b) in p.data.iter().enumerate() {
                    assert_eq!(
                        b,
                        (base + k + block_id as usize) as u8,
                        "payload {i} byte {k}"
                    );
                }
            }

            let trailer = &pkts[5];
            assert_eq!(trailer.header.packet_format, PACKET_FORMAT_DATA_TRAILER);
            assert_eq!(trailer.header.packet_id, 5);
            assert_eq!(decode_image_trailer_body(&trailer.data).unwrap(), 48);
        }

        m.write_reg(reg::SCP0, 0).unwrap();
        assert_eq!(m.read_reg(reg::SCP0).unwrap(), 0);
        assert_silent(&rx, Duration::from_millis(300));
    }

    #[test]
    fn scps_zero_or_unwritten_defaults_to_1400() {
        let m = map_with_stream(StreamConfig {
            width: 128,
            height: 20,
            pixel_format: MONO8,
        });
        // Factory default: total packet size 1400+48 (spec layout), no
        // write at all.
        assert_eq!(
            m.read_reg(reg::SCPS0).unwrap(),
            reg::scps_make(DEFAULT_PAYLOAD as u32 + reg::GVSP_HEADERS_OVERHEAD)
        );
        let rx = open_channel(&m, None);
        let pkts = recv_block(&rx, 2);
        assert_eq!(pkts[1].data.len(), 1400);
        assert_eq!(pkts[2].data.len(), 2560 - 1400);
        m.write_reg(reg::SCP0, 0).unwrap();
        drop(rx);

        // An explicit 0 selects the same default after a re-open.
        let rx = open_channel(&m, Some(0));
        let pkts = recv_block(&rx, 2);
        assert_eq!(pkts[1].data.len(), 1400);
        assert_eq!(pkts[2].data.len(), 2560 - 1400);
        m.write_reg(reg::SCP0, 0).unwrap();
    }

    #[test]
    fn stream_params_interpret_scps0_in_spec_layout() {
        let stream = StreamConfig {
            width: 64,
            height: 48,
            pixel_format: MONO8,
        };
        // Total packet size 1448 -> 1400 payload bytes (the sim default).
        let p = StreamParams::new(&stream, reg::scps_make(1448), 0);
        assert_eq!(p.payload_size, 1400);
        // Flag bits (fire_test_packet / do_not_fragment) are not size bits.
        let p = StreamParams::new(
            &stream,
            reg::scps_make(1000) | reg::SCPS_FIRE_TEST_PACKET | reg::SCPS_DO_NOT_FRAGMENT,
            0,
        );
        assert_eq!(p.payload_size, 1000 - reg::GVSP_HEADERS_OVERHEAD as usize);
        // Unwritten (0) or smaller-than-headers values fall back to 1400.
        assert_eq!(StreamParams::new(&stream, 0, 0).payload_size, DEFAULT_PAYLOAD);
        assert_eq!(
            StreamParams::new(&stream, reg::scps_make(10), 0).payload_size,
            DEFAULT_PAYLOAD
        );
        // Multi-byte pixels round the payload down to a pixel multiple.
        let s16 = StreamConfig {
            width: 64,
            height: 48,
            pixel_format: PFNC_MONO16,
        };
        assert_eq!(
            StreamParams::new(&s16, reg::scps_make(999), 0).payload_size,
            (999 - reg::GVSP_HEADERS_OVERHEAD as usize) / 2 * 2
        );
    }

    #[test]
    fn mono16_pattern_is_little_endian() {
        let m = map_with_stream(StreamConfig {
            width: 4,
            height: 2,
            pixel_format: PFNC_MONO16,
        });
        // 4 * 2 pixels * 2 bytes = 16 bytes -> a single payload packet.
        let rx = open_channel(&m, None);
        let pkts = recv_block(&rx, 1);
        let l = ImageLeader::decode(&pkts[0].data).expect("leader body");
        assert_eq!(l.pixel_format, PFNC_MONO16);
        assert_eq!(l.size_x, 4);
        assert_eq!(l.size_y, 2);
        let data = &pkts[1].data;
        assert_eq!(data.len(), 16);
        for p in 0..8 {
            let v = u16::from_le_bytes([data[2 * p], data[2 * p + 1]]);
            assert_eq!(v, (p + 1) as u16, "pixel {p}");
        }
        assert_eq!(decode_image_trailer_body(&pkts[2].data).unwrap(), 2);
        m.write_reg(reg::SCP0, 0).unwrap();
    }

    #[test]
    fn block_id_resets_on_reopen_and_scp_is_busy_while_open() {
        let m = map_with_stream(StreamConfig::default());
        let rx = open_channel(&m, None);
        assert_eq!(recv_packet(&rx).header.block_id, 1);
        // R-586cd: a non-zero SCPx write while streaming is rejected.
        assert_eq!(
            m.write_reg(reg::SCP0, reg::scp_make(1234)).unwrap_err(),
            GEV_STATUS_BUSY
        );
        m.write_reg(reg::SCP0, 0).unwrap();
        drop(rx);

        // R-259st: the block id restarts at 1 on the new channel.
        let rx = open_channel(&m, None);
        assert_eq!(recv_packet(&rx).header.block_id, 1);
        m.write_reg(reg::SCP0, 0).unwrap();
    }
}
