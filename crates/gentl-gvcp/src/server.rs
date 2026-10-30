//! GVCP server: answers DISCOVERY / READREG / WRITEREG / READMEM / WRITEMEM
//! for a simulated (or test) device. One thread per server instance.

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use gentl_core::{GentlError, GentlResult};

use crate::wire::*;

/// Device behavior behind the wire: register/memory access plus the static
/// identity reported in DISCOVERY_ACK.
pub trait DeviceHandler: Send + 'static {
    /// Identity for DISCOVERY_ACK.
    fn discovery_info(&self) -> DiscoveryInfo;

    /// Read one 32-bit register. Err(status) is a GEV status code.
    fn read_reg(&self, addr: u32) -> Result<u32, u16>;

    /// Write one 32-bit register. Err(status) is a GEV status code.
    fn write_reg(&self, addr: u32, value: u32) -> Result<(), u16>;

    /// Read `count` bytes at `addr` from device memory. Err(status) is a
    /// GEV status code. Must succeed atomically (all bytes or error).
    fn read_mem(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16>;

    /// Write bytes at `addr` to device memory. Devices without WRITEMEM
    /// support keep the default NOT_IMPLEMENTED response.
    fn write_mem(&self, _addr: u32, _data: &[u8]) -> Result<(), u16> {
        Err(GEV_STATUS_NOT_IMPLEMENTED)
    }

    /// PACKETRESEND_CMD (§16.7): retransmit the given packet-id range of
    /// the cached block. Fire-and-forget — no ack exists (CR-201cd), so
    /// the default ignores the request (devices without resend support
    /// simply never answer; the requester times out, §16.7.3).
    fn packet_resend(&self, _channel: u16, _block_id: u64, _first: u32, _last: u32) {}
}

pub struct GvcpServer {
    socket: UdpSocket,
    thread: Option<JoinHandle<()>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl GvcpServer {
    /// Bind `0.0.0.0:port` and serve in a background thread.
    pub fn start<H: DeviceHandler>(port: u16, handler: H) -> GentlResult<Self> {
        Self::start_bound(Ipv4Addr::UNSPECIFIED, port, handler)
    }

    /// Bind `bind_ip:port` and serve in a background thread. A gateway
    /// serving a virtual device on one specific NIC (e.g. linux-vgige's
    /// up-facing port) binds that address instead of the unspecified one so
    /// that the device is not also reachable on the down-facing network.
    pub fn start_bound<H: DeviceHandler>(bind_ip: Ipv4Addr, port: u16, handler: H) -> GentlResult<Self> {
        let socket = UdpSocket::bind((bind_ip, port))
            .map_err(|e| GentlError::Io(format!("bind {bind_ip}:{port}: {e}")))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(GentlError::from)?;
        let socket2 = socket
            .try_clone()
            .map_err(|e| GentlError::Io(format!("try_clone: {e}")))?;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::spawn(move || serve_loop(socket2, stop2, handler));
        Ok(Self {
            socket,
            thread: Some(thread),
            stop,
        })
    }

    pub fn local_port(&self) -> GentlResult<u16> {
        self.socket
            .local_addr()
            .map(|a| a.port())
            .map_err(GentlError::from)
    }

    pub fn stop(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for GvcpServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn serve_loop<H: DeviceHandler>(
    socket: UdpSocket,
    stop: Arc<std::sync::atomic::AtomicBool>,
    handler: H,
) {
    let mut buf = [0u8; 65536];
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        let (n, peer) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                continue
            }
            Err(_) => continue,
        };
        let datagram = &buf[..n];
        let Some(cmd) = CmdHeader::decode(datagram) else { continue };

        let payload_in = &datagram[HEADER_LEN..];
        // Defensive: declared length vs actual datagram size.
        let payload_in = &payload_in[..payload_in.len().min(cmd.length as usize)];

        let mut out: Vec<u8> = Vec::with_capacity(HEADER_LEN + 512);
        let mut ack = [0u8; HEADER_LEN];

        match cmd.command {
            DISCOVERY_CMD => {
                let info = handler.discovery_info();
                let p = info.encode_payload();
                AckHeader::new(GEV_STATUS_SUCCESS, DISCOVERY_ACK, p.len() as u16, cmd.req_id)
                    .encode(&mut ack);
                out.extend_from_slice(&ack);
                out.extend_from_slice(&p);
            }
            READREG_CMD => {
                let mut values = Vec::new();
                let mut status = GEV_STATUS_SUCCESS;
                if payload_in.len() % 4 != 0 {
                    status = GEV_STATUS_INVALID_PARAMETER;
                } else {
                    for chunk in payload_in.chunks_exact(4) {
                        let addr = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                        match handler.read_reg(addr) {
                            Ok(v) => values.extend_from_slice(&v.to_be_bytes()),
                            Err(s) => {
                                status = s;
                                values.clear();
                                break;
                            }
                        }
                    }
                }
                let len = if status == GEV_STATUS_SUCCESS {
                    values.len()
                } else {
                    0
                };
                AckHeader::new(status, READREG_ACK, len as u16, cmd.req_id).encode(&mut ack);
                out.extend_from_slice(&ack);
                out.extend_from_slice(&values);
            }
            WRITEREG_CMD => {
                let mut status = GEV_STATUS_SUCCESS;
                let mut written = 0u32;
                if payload_in.is_empty() || payload_in.len() % 8 != 0 {
                    status = GEV_STATUS_INVALID_PARAMETER;
                } else {
                    for chunk in payload_in.chunks_exact(8) {
                        let addr = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                        let value = u32::from_be_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
                        if let Err(s) = handler.write_reg(addr, value) {
                            status = s;
                            break;
                        }
                        written += 1;
                    }
                }
                AckHeader::new(status, WRITEREG_ACK, 4, cmd.req_id).encode(&mut ack);
                out.extend_from_slice(&ack);
                out.extend_from_slice(&written.to_be_bytes());
            }
            READMEM_CMD => {
                let mut status = GEV_STATUS_SUCCESS;
                let mut data: Vec<u8> = Vec::new();
                if payload_in.len() != 8 {
                    status = GEV_STATUS_INVALID_PARAMETER;
                } else {
                    let addr = u32::from_be_bytes([payload_in[0], payload_in[1], payload_in[2], payload_in[3]]);
                    let count = u16::from_be_bytes([payload_in[6], payload_in[7]]);
                    if count == 0 || count % 4 != 0 {
                        status = GEV_STATUS_BAD_ALIGNMENT;
                    } else {
                        match handler.read_mem(addr, count) {
                            Ok(d) => {
                                data.extend_from_slice(&addr.to_be_bytes());
                                data.extend_from_slice(&d);
                            }
                            Err(s) => status = s,
                        }
                    }
                }
                let len = if status == GEV_STATUS_SUCCESS { data.len() } else { 0 };
                AckHeader::new(status, READMEM_ACK, len as u16, cmd.req_id).encode(&mut ack);
                out.extend_from_slice(&ack);
                out.extend_from_slice(&data);
            }
            WRITEMEM_CMD => {
                let mut status = GEV_STATUS_SUCCESS;
                let mut addr = 0u32;
                if payload_in.len() < 8 || (payload_in.len() - 4) % 4 != 0 {
                    status = GEV_STATUS_INVALID_PARAMETER;
                } else {
                    addr = u32::from_be_bytes([payload_in[0], payload_in[1], payload_in[2], payload_in[3]]);
                    if let Err(s) = handler.write_mem(addr, &payload_in[4..]) {
                        status = s;
                    }
                }
                let len = if status == GEV_STATUS_SUCCESS { 4 } else { 0 };
                AckHeader::new(status, WRITEMEM_ACK, len, cmd.req_id).encode(&mut ack);
                out.extend_from_slice(&ack);
                if status == GEV_STATUS_SUCCESS {
                    out.extend_from_slice(&addr.to_be_bytes());
                }
            }
            // PACKETRESEND (§16.7): fire-and-forget — CR-198ca forbids the
            // ACKNOWLEDGE flag and CR-201cd defines no PACKETRESEND_ACK,
            // so nothing is sent back. CR-199cd: any application may send
            // it; malformed payloads are ignored like any other
            // undeliverable request.
            PACKETRESEND_CMD => {
                if let Some((channel, block_id, first, last)) =
                    decode_packet_resend_payload(cmd.flag, payload_in)
                {
                    handler.packet_resend(channel, block_id, first, last);
                }
                continue;
            }
            // FORCEIP / EVENT* / ACTION: not part of M1 scope.
            _ => {
                AckHeader::new(
                    GEV_STATUS_NOT_IMPLEMENTED,
                    cmd.command.wrapping_add(1),
                    0,
                    cmd.req_id,
                )
                .encode(&mut ack);
                out.extend_from_slice(&ack);
            }
        }

        let _ = socket.send_to(&out, peer);
    }
}
