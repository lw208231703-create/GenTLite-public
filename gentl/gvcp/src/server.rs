//! GVCP server: answers DISCOVERY / READREG / WRITEREG / READMEM / WRITEMEM
//! for a simulated (or test) device. One thread per server instance.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
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

    /// FORCEIP_CMD (§16.2): apply this device's force-IP policy. MAC
    /// mismatch and an occupied control channel MUST be discarded silently
    /// (R-149cd/R-150cd); static_IP == 0 restarts the IP configuration
    /// cycle without an ACK (R-151cd); a non-zero static_IP is applied and
    /// acknowledged (R-152cd). The default discards the request.
    fn force_ip(&self, _peer: SocketAddr, _request: ForceIpRequest) -> ForceIpOutcome {
        ForceIpOutcome::Ignore
    }

    /// Read one 32-bit register. Err(status) is a GEV status code.
    fn read_reg(&self, addr: u32) -> Result<u32, u16>;

    /// Peer-aware register read. Devices implementing primary/secondary
    /// access rules (GigE Vision 10.1) override this; the default keeps the
    /// peer-independent behavior.
    fn read_reg_from(&self, _peer: SocketAddr, addr: u32) -> Result<u32, u16> {
        self.read_reg(addr)
    }

    /// Write one 32-bit register. Err(status) is a GEV status code.
    fn write_reg(&self, addr: u32, value: u32) -> Result<(), u16>;

    /// Peer-aware register write. Devices with multi-host state (CCP,
    /// heartbeat ownership) can override this; the default preserves the
    /// peer-independent behavior.
    fn write_reg_from(
        &self,
        _peer: SocketAddr,
        addr: u32,
        value: u32,
    ) -> Result<(), u16> {
        self.write_reg(addr, value)
    }

    /// Read `count` bytes at `addr` from device memory. Err(status) is a
    /// GEV status code. Must succeed atomically (all bytes or error).
    fn read_mem(&self, addr: u32, count: u16) -> Result<Vec<u8>, u16>;

    /// Peer-aware memory read (access rules, GigE Vision 10.1).
    fn read_mem_from(&self, _peer: SocketAddr, addr: u32, count: u16) -> Result<Vec<u8>, u16> {
        self.read_mem(addr, count)
    }

    /// Write bytes at `addr` to device memory. Devices without WRITEMEM
    /// support keep the default NOT_IMPLEMENTED response.
    fn write_mem(&self, _addr: u32, _data: &[u8]) -> Result<(), u16> {
        Err(GEV_STATUS_NOT_IMPLEMENTED)
    }

    /// Peer-aware memory write (access rules, GigE Vision 10.1).
    fn write_mem_from(&self, _peer: SocketAddr, addr: u32, data: &[u8]) -> Result<(), u16> {
        self.write_mem(addr, data)
    }

    /// Peer-aware WRITEMEM with byte-progress information. Devices that
    /// commit words before a later word fails return the 0-based byte index
    /// of the failure (R-194cd); the default treats the write as atomic.
    fn write_mem_from_progress(
        &self,
        peer: SocketAddr,
        addr: u32,
        data: &[u8],
    ) -> Result<(), (u16, u32)> {
        self.write_mem_from(peer, addr, data)
            .map_err(|status| (status, 0))
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
                // GigE Vision 16.3.1: at least one register address must be
                // present; a zero-length READREG is not a valid command.
                if payload_in.is_empty() || payload_in.len() % 4 != 0 {
                    status = GEV_STATUS_INVALID_PARAMETER;
                } else {
                    for chunk in payload_in.chunks_exact(4) {
                        let addr = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                        // 16.3.1: the address list entries are 32-bit aligned.
                        if addr % 4 != 0 {
                            status = GEV_STATUS_BAD_ALIGNMENT;
                            break;
                        }
                        match handler.read_reg_from(peer, addr) {
                            Ok(v) => values.extend_from_slice(&v.to_be_bytes()),
                            Err(s) => {
                                status = s;
                                break;
                            }
                        }
                    }
                }
                // R-165cd: the acknowledge length reflects the registers
                // that were successfully read, even on a later failure.
                let len = values.len();
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
                        if let Err(s) = handler.write_reg_from(peer, addr, value) {
                            status = s;
                            break;
                        }
                        written += 1;
                    }
                }
                // R-138cd: an ACCESS_DENIED acknowledge carries the header
                // only (length = 0), no SCD.
                if status == GEV_STATUS_ACCESS_DENIED {
                    AckHeader::new(status, WRITEREG_ACK, 0, cmd.req_id).encode(&mut ack);
                    out.extend_from_slice(&ack);
                } else {
                    AckHeader::new(status, WRITEREG_ACK, 4, cmd.req_id).encode(&mut ack);
                    out.extend_from_slice(&ack);
                    out.extend_from_slice(&written.to_be_bytes());
                }
            }
            READMEM_CMD => {
                let mut status = GEV_STATUS_SUCCESS;
                let mut data: Vec<u8> = Vec::new();
                if payload_in.len() != 8 {
                    status = GEV_STATUS_INVALID_PARAMETER;
                } else {
                    let addr = u32::from_be_bytes([payload_in[0], payload_in[1], payload_in[2], payload_in[3]]);
                    let count = u16::from_be_bytes([payload_in[6], payload_in[7]]);
                    // R-177c/R-178cd: the read count is a positive multiple
                    // of 4; R-179c: the start address must be 32-bit aligned.
                    if addr % 4 != 0 {
                        status = GEV_STATUS_BAD_ALIGNMENT;
                    } else if count == 0 || count % 4 != 0 {
                        status = GEV_STATUS_BAD_ALIGNMENT;
                    } else {
                        match handler.read_mem_from(peer, addr, count) {
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
                let addr = if payload_in.len() >= 8 {
                    u32::from_be_bytes([payload_in[0], payload_in[1], payload_in[2], payload_in[3]])
                } else {
                    0
                };
                let mut written = 0u32;
                let mut error_index = 0u32;
                if payload_in.len() < 8 {
                    status = GEV_STATUS_INVALID_PARAMETER;
                } else if (payload_in.len() - 4) % 4 != 0 {
                    // R-188cd: a WRITEMEM length that is not a multiple of 4
                    // MUST answer BAD_ALIGNMENT.
                    status = GEV_STATUS_BAD_ALIGNMENT;
                } else if addr % 4 != 0 {
                    // R-189c: an unaligned start address MUST answer
                    // BAD_ALIGNMENT.
                    status = GEV_STATUS_BAD_ALIGNMENT;
                } else {
                    match handler.write_mem_from_progress(peer, addr, &payload_in[4..]) {
                        Ok(()) => written = payload_in.len() as u32 - 4,
                        Err((s, index)) => {
                            status = s;
                            error_index = index.min(payload_in.len() as u32 - 4 - 1);
                        }
                    }
                }
                // R-138cd: ACCESS_DENIED is answered with the header only.
                if status == GEV_STATUS_ACCESS_DENIED {
                    AckHeader::new(status, WRITEMEM_ACK, 0, cmd.req_id).encode(&mut ack);
                    out.extend_from_slice(&ack);
                } else {
                    // R-193c/R-194cd: reserved u16 + index u16. On success
                    // index = bytes written; on failure it is the 0-based
                    // byte index where the error occurred.
                    let index = if status == GEV_STATUS_SUCCESS {
                        written as u16
                    } else {
                        error_index as u16
                    };
                    let mut scd = [0u8; 4];
                    scd[2..4].copy_from_slice(&index.to_be_bytes());
                    AckHeader::new(status, WRITEMEM_ACK, 4, cmd.req_id).encode(&mut ack);
                    out.extend_from_slice(&ack);
                    out.extend_from_slice(&scd);
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
            FORCEIP_CMD => {
                // §16.2: the handler owns the ordering rules — a MAC
                // mismatch or an occupied control channel discards the
                // request (R-149cd/R-150cd), static_IP == 0 restarts the IP
                // configuration cycle with no ACK (R-151cd), and a forced
                // address is acknowledged once effective (R-152cd).
                // Malformed payloads and discard outcomes answer nothing —
                // not even an empty datagram (a discarded FORCEIP on the
                // wire is a timeout for the application, R-149cd/R-150cd).
                let Some(request) = decode_force_ip_payload(payload_in) else {
                    continue;
                };
                if handler.force_ip(peer, request) != ForceIpOutcome::Ack {
                    continue;
                }
                AckHeader::new(GEV_STATUS_SUCCESS, FORCEIP_ACK, 0, cmd.req_id)
                    .encode(&mut ack);
                out.extend_from_slice(&ack);
                if cmd.flag & FLAG_FORCEIP_ACK_BROADCAST != 0 {
                    // O-155cd: the ACK may need to be broadcast because the
                    // forced address can change the reachable subnet.
                    let _ = socket.send_to(
                        &out,
                        SocketAddr::from((Ipv4Addr::BROADCAST, GVCP_PORT)),
                    );
                    continue;
                }
            }
            // EVENT* / ACTION: not part of M1 scope.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    struct Fake;

    impl DeviceHandler for Fake {
        fn discovery_info(&self) -> DiscoveryInfo {
            DiscoveryInfo {
                spec_version_major: 3,
                spec_version_minor: 0,
                device_mode: 0,
                mac: [0x02, 0, 0, 0, 0, 1],
                ip_config_options: 0,
                ip_config_current: 0,
                current_ip: u32::from(Ipv4Addr::LOCALHOST),
                current_subnet_mask: 0,
                default_gateway: 0,
                manufacturer_name: "test".into(),
                model_name: "test".into(),
                device_version: "1".into(),
                serial_number: "1".into(),
                user_defined_name: String::new(),
                manufacturer_info: String::new(),
            }
        }

        fn read_reg(&self, addr: u32) -> Result<u32, u16> {
            if addr == 0x0020_0000 {
                return Err(GEV_STATUS_INVALID_ADDRESS);
            }
            Ok(0x1122_3344)
        }

        fn write_reg(&self, _addr: u32, _value: u32) -> Result<(), u16> {
            Ok(())
        }

        fn read_mem(&self, _addr: u32, count: u16) -> Result<Vec<u8>, u16> {
            Ok(vec![0u8; count as usize])
        }

        fn write_mem(&self, addr: u32, data: &[u8]) -> Result<(), u16> {
            if addr == 0x0020_0000 {
                return Err(GEV_STATUS_INVALID_ADDRESS);
            }
            if data.len() > 64 {
                return Err(GEV_STATUS_BAD_ALIGNMENT);
            }
            Ok(())
        }

        fn read_reg_from(&self, _peer: SocketAddr, addr: u32) -> Result<u32, u16> {
            if addr == 0x0099 {
                return Err(GEV_STATUS_ACCESS_DENIED);
            }
            self.read_reg(addr)
        }

        fn write_reg_from(&self, _peer: SocketAddr, addr: u32, value: u32) -> Result<(), u16> {
            if addr == 0x0099 {
                return Err(GEV_STATUS_ACCESS_DENIED);
            }
            self.write_reg(addr, value)
        }

        fn write_mem_from(
            &self,
            _peer: SocketAddr,
            addr: u32,
            data: &[u8],
        ) -> Result<(), u16> {
            if addr == 0x0098 {
                return Err(GEV_STATUS_ACCESS_DENIED);
            }
            self.write_mem(addr, data)
        }

        fn write_mem_from_progress(
            &self,
            _peer: SocketAddr,
            addr: u32,
            data: &[u8],
        ) -> Result<(), (u16, u32)> {
            if addr == 0x0094 {
                // Simulates a device that committed the first word and then
                // failed on the second (R-194cd byte index).
                return Err((GEV_STATUS_INVALID_ADDRESS, 4));
            }
            self.write_mem_from(_peer, addr, data)
                .map_err(|status| (status, 0))
        }
    }

    fn exchange(server: &GvcpServer, command: u16, payload: &[u8]) -> Vec<u8> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut hdr = [0u8; HEADER_LEN];
        CmdHeader::new(command, payload.len() as u16, 7).encode(&mut hdr);
        let mut datagram = hdr.to_vec();
        datagram.extend_from_slice(payload);
        socket
            .send_to(&datagram, (Ipv4Addr::LOCALHOST, server.local_port().unwrap()))
            .unwrap();
        let mut buf = [0u8; 2048];
        let (n, _) = socket.recv_from(&mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn ack_status(reply: &[u8]) -> u16 {
        AckHeader::decode(reply).expect("ack header").status
    }

    /// GigE Vision 16.3.1: at least one register must be read; an empty
    /// address list is invalid, not a successful no-op.
    #[test]
    fn empty_readreg_is_rejected() {
        let server = GvcpServer::start(0, Fake).unwrap();
        let reply = exchange(&server, READREG_CMD, &[]);
        assert_eq!(ack_status(&reply), GEV_STATUS_INVALID_PARAMETER);
        assert_eq!(reply.len(), HEADER_LEN, "no register data on error");
    }

    /// R-179c: an unaligned READMEM address must answer BAD_ALIGNMENT.
    #[test]
    fn unaligned_readmem_is_rejected() {
        let server = GvcpServer::start(0, Fake).unwrap();
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&0x49u32.to_be_bytes());
        payload[6..8].copy_from_slice(&4u16.to_be_bytes());
        let reply = exchange(&server, READMEM_CMD, &payload);
        assert_eq!(ack_status(&reply), GEV_STATUS_BAD_ALIGNMENT);

        // The aligned form still succeeds (regression guard).
        payload[..4].copy_from_slice(&0x48u32.to_be_bytes());
        let reply = exchange(&server, READMEM_CMD, &payload);
        assert_eq!(ack_status(&reply), GEV_STATUS_SUCCESS);
    }

    /// R-193c/R-194cd: WRITEMEM_ACK is reserved u16 + index u16; on success
    /// index is the number of bytes written.
    #[test]
    fn writemem_ack_carries_written_byte_count() {
        let server = GvcpServer::start(0, Fake).unwrap();
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        payload[4..].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        let reply = exchange(&server, WRITEMEM_CMD, &payload);
        assert_eq!(ack_status(&reply), GEV_STATUS_SUCCESS);
        assert_eq!(
            &reply[HEADER_LEN..],
            &[0, 0, 0, 4],
            "reserved=0, index=bytes written"
        );
    }

    /// R-194cd: a failed write reports index 0 (the failing byte offset).
    #[test]
    fn failed_writemem_ack_reports_error_index() {
        let server = GvcpServer::start(0, Fake).unwrap();
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&0x0020_0000u32.to_be_bytes());
        payload[4..].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        let reply = exchange(&server, WRITEMEM_CMD, &payload);
        assert_eq!(ack_status(&reply), GEV_STATUS_INVALID_ADDRESS);
        assert_eq!(&reply[HEADER_LEN..], &[0, 0, 0, 0]);
    }

    /// R-165cd: a multi-register READREG keeps the registers read before the
    /// failing one and reports them in the acknowledge length.
    #[test]
    fn readreg_partial_success_keeps_earlier_values() {
        let server = GvcpServer::start(0, Fake).unwrap();
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&0x40u32.to_be_bytes());
        payload[4..].copy_from_slice(&0x0020_0000u32.to_be_bytes());
        let reply = exchange(&server, READREG_CMD, &payload);
        assert_eq!(ack_status(&reply), GEV_STATUS_INVALID_ADDRESS);
        assert_eq!(
            &reply[HEADER_LEN..],
            &0x1122_3344u32.to_be_bytes(),
            "the first register is still reported"
        );
    }

    /// R-138cd: an ACCESS_DENIED acknowledge is header-only for WRITEREG and
    /// WRITEMEM (length = 0, no SCD).
    #[test]
    fn access_denied_acks_are_header_only() {
        let server = GvcpServer::start(0, Fake).unwrap();
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&0x99u32.to_be_bytes());
        let reply = exchange(&server, WRITEREG_CMD, &payload);
        assert_eq!(ack_status(&reply), GEV_STATUS_ACCESS_DENIED);
        assert_eq!(reply.len(), HEADER_LEN, "WRITEREG_ACK header only");

        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&0x98u32.to_be_bytes());
        payload[4..].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        let reply = exchange(&server, WRITEMEM_CMD, &payload);
        assert_eq!(ack_status(&reply), GEV_STATUS_ACCESS_DENIED);
        assert_eq!(reply.len(), HEADER_LEN, "WRITEMEM_ACK header only");
    }

    /// R-194cd: a WRITEMEM that fails after committing bytes reports the
    /// failing byte index, not a constant 0.
    #[test]
    fn writemem_failure_reports_progress_index() {
        let server = GvcpServer::start(0, Fake).unwrap();
        let mut payload = [0u8; 12];
        payload[..4].copy_from_slice(&0x0094u32.to_be_bytes());
        payload[4..8].copy_from_slice(&0x1111_1111u32.to_be_bytes());
        payload[8..12].copy_from_slice(&0x2222_2222u32.to_be_bytes());
        let reply = exchange(&server, WRITEMEM_CMD, &payload);
        assert_eq!(ack_status(&reply), GEV_STATUS_INVALID_ADDRESS);
        assert_eq!(&reply[HEADER_LEN..], &[0, 0, 0, 4], "index = byte 4");
    }
}
