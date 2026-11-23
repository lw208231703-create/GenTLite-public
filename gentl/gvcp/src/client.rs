//! GVCP client: control-channel transactions from the producer to a device.

use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use gentl_core::{GentlError, GentlResult};

use crate::registers;
use crate::wire::*;

const DEFAULT_ACK_TIMEOUT: Duration = Duration::from_millis(500);
const DEFAULT_RETRIES: u32 = 2;
/// Added to a PENDING_ACK `time_to_completion` before re-arming the socket
/// read timeout (搂16.8.1: "plus some margin to allow for network latencies").
const PENDING_ACK_NETWORK_MARGIN: Duration = Duration::from_millis(250);

/// Transaction client bound to one local UDP socket. Not thread-bound:
/// guard with a Mutex at the call site if sharing.
pub struct GvcpClient {
    socket: UdpSocket,
    device_addr: (Ipv4Addr, u16),
    next_req_id: AtomicU16,
    retries: u32,
}

impl GvcpClient {
    /// Bind an ephemeral local port and target `device_ip:3956` (or a
    /// custom port via `GENTLITE_GVCP_PORT` on the server side).
    pub fn connect(device_ip: Ipv4Addr, device_port: u16) -> GentlResult<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .map_err(|e| GentlError::Io(format!("bind: {e}")))?;
        socket
            .set_read_timeout(Some(DEFAULT_ACK_TIMEOUT))
            .map_err(GentlError::from)?;
        Ok(Self {
            socket,
            device_addr: (device_ip, device_port),
            next_req_id: AtomicU16::new(1),
            retries: DEFAULT_RETRIES,
        })
    }

    fn alloc_req_id(&self) -> u16 {
        // req_id must never be 0 (spec §15.1); wrap skips 0.
        let id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            self.next_req_id.store(2, Ordering::Relaxed);
            1
        } else {
            id
        }
    }

    /// Send one command, wait for its acknowledge, retry on timeout.
    /// Returns the full ack datagram (header + payload).
    ///
    /// R-047ca: a resent command MUST keep the same req_id — the device
    /// deduplicates retransmissions by it. The id is therefore allocated
    /// once, outside the retry loop.
    fn transaction(&self, command: u16, payload: &[u8]) -> GentlResult<Vec<u8>> {
        let req_id = self.alloc_req_id();
        let mut attempts = self.retries + 1;
        loop {
            let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
            let mut hdr = [0u8; HEADER_LEN];
            CmdHeader::new(command, payload.len() as u16, req_id).encode(&mut hdr);
            buf.extend_from_slice(&hdr);
            buf.extend_from_slice(payload);

            self.socket
                .send_to(&buf, self.device_addr)
                .map_err(|e| GentlError::Io(format!("send {command:#06X}: {e}")))?;

            match self.wait_ack(req_id, command)? {
                Ok(datagram) => return Ok(datagram),
                Err(RetryReason::Timeout) => {
                    attempts -= 1;
                    if attempts == 0 {
                        return Err(GentlError::Timeout);
                    }
                }
            }
        }
    }

    /// Receive datagrams until the matching ack arrives. PENDING_ACK (0x0089)
    /// extends the wait by its time_to_completion (first payload u16, ms).
    fn wait_ack(&self, req_id: u16, _command: u16) -> GentlResult<Result<Vec<u8>, RetryReason>> {
        let mut buf = [0u8; 65536];
        loop {
            let (n, _) = match self.socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock => {
                    let _ = self.socket.set_read_timeout(Some(DEFAULT_ACK_TIMEOUT));
                    return Ok(Err(RetryReason::Timeout))
                }
                Err(e) => return Err(GentlError::Io(format!("recv: {e}"))),
            };
            let datagram = &buf[..n];
            let Some(hdr) = AckHeader::decode(datagram) else {
                continue; // not a GVCP ack; ignore
            };
            if hdr.ack_id != req_id {
                continue; // stale ack from an earlier attempt
            }
            if hdr.acknowledge == PENDING_ACK {
                // 16.8.1: reset the acknowledge timeout to the indicated
                // time_to_completion plus a network margin. Ignoring it makes
                // long device commands look like lost requests and triggers
                // spurious retransmissions (and a timeout when the device
                // really needs the announced time).
                if let Some(ms) = decode_pending_ack_timeout(&datagram[HEADER_LEN..]) {
                    let wait = Duration::from_millis(u64::from(ms))
                        .saturating_add(PENDING_ACK_NETWORK_MARGIN)
                        .max(DEFAULT_ACK_TIMEOUT);
                    let _ = self.socket.set_read_timeout(Some(wait));
                }
                continue;
            }
            // Restore the default cadence for the next transaction; an
            // extended timeout belongs to the command that asked for it.
            let _ = self.socket.set_read_timeout(Some(DEFAULT_ACK_TIMEOUT));
            return Ok(Ok(datagram.to_vec()));
        }
    }

    fn ack_status(datagram: &[u8]) -> GentlResult<u16> {
        let hdr = AckHeader::decode(datagram)
            .ok_or_else(|| GentlError::Io("short ack".into()))?;
        Ok(hdr.status)
    }

    /// Broadcast DISCOVERY_CMD on `local` sockets is handled by
    /// [`discover`]; this is the unicast transaction path.

    /// READREG of one 32-bit register (spec §16.3).
    pub fn read_reg(&self, addr: u32) -> GentlResult<u32> {
        let datagram = self.transaction(READREG_CMD, &encode_readreg(&[addr]))?;
        let status = Self::ack_status(&datagram)?;
        if status != GEV_STATUS_SUCCESS {
            return Err(GentlError::GevStatus(status));
        }
        let values = decode_readreg_payload(&datagram[HEADER_LEN..])
            .ok_or_else(|| GentlError::Io("malformed READREG_ACK".into()))?;
        values
            .first()
            .copied()
            .ok_or_else(|| GentlError::Io("empty READREG_ACK".into()))
    }

    /// WRITEREG of one 32-bit register (spec §16.4).
    pub fn write_reg(&self, addr: u32, value: u32) -> GentlResult<()> {
        let datagram = self.transaction(WRITEREG_CMD, &encode_writereg(&[(addr, value)]))?;
        let status = Self::ack_status(&datagram)?;
        if status != GEV_STATUS_SUCCESS {
            return Err(GentlError::GevStatus(status));
        }
        Ok(())
    }

    /// READMEM: arbitrary aligned-length memory read (spec §16.5).
    /// `buf.len()` must be a multiple of 4; reads larger than 65535-8 bytes
    /// must be split by the caller.
    pub fn read_mem(&self, addr: u32, buf: &mut [u8]) -> GentlResult<()> {
        if buf.len() % 4 != 0 || buf.len() > u16::MAX as usize - 8 {
            return Err(GentlError::InvalidParameter("read_mem size"));
        }
        let payload = encode_readmem_cmd_payload(addr, buf.len() as u16);
        let datagram = self.transaction(READMEM_CMD, &payload)?;
        let status = Self::ack_status(&datagram)?;
        if status != GEV_STATUS_SUCCESS {
            return Err(GentlError::GevStatus(status));
        }
        let (ack_addr, data) = decode_readmem_payload(&datagram[HEADER_LEN..])
            .ok_or_else(|| GentlError::Io("malformed READMEM_ACK".into()))?;
        if ack_addr != addr || data.len() != buf.len() {
            return Err(GentlError::Io("READMEM_ACK mismatch".into()));
        }
        buf.copy_from_slice(data);
        Ok(())
    }

    /// WRITEMEM (spec §16.6): write `data` at `addr`. `data.len()` must be a
    /// multiple of 4 and `addr` 32-bit aligned (R-187c/R-189c); the device
    /// must echo the address in the ack (R-190cd). Larger writes must be
    /// split by the caller to stay under the control-channel max packet.
    pub fn write_mem(&self, addr: u32, data: &[u8]) -> GentlResult<()> {
        if data.is_empty() || data.len() % 4 != 0 || addr % 4 != 0 {
            return Err(GentlError::InvalidParameter("write_mem alignment"));
        }
        if data.len() > u16::MAX as usize - 8 {
            return Err(GentlError::InvalidParameter("write_mem size"));
        }
        let payload = encode_writemem_cmd_payload(addr, data);
        let datagram = self.transaction(WRITEMEM_CMD, &payload)?;
        let status = Self::ack_status(&datagram)?;
        if status != GEV_STATUS_SUCCESS {
            return Err(GentlError::GevStatus(status));
        }
        let ack_addr = decode_writemem_ack_payload(&datagram[HEADER_LEN..])
            .ok_or_else(|| GentlError::Io("malformed WRITEMEM_ACK".into()))?;
        if ack_addr != addr {
            return Err(GentlError::Io("WRITEMEM_ACK address mismatch".into()));
        }
        Ok(())
    }

    /// Number of message channels the device provides (§36.22); 0 means the
    /// device has no message channel.
    pub fn num_message_channels(&self) -> GentlResult<u32> {
        self.read_reg(registers::NUM_MESSAGE_CHANNELS)
    }

    /// Open the message channel (§12.2): destination address first, then the
    /// host port in MCP as the last write. The transmission timeout is set to
    /// 0 so the device clears the ACKNOWLEDGE bit (CR-103cd) and no
    /// EVENT_ACK is required.
    pub fn open_message_channel(&self, host_ip: Ipv4Addr, host_port: u16) -> GentlResult<()> {
        self.write_reg(registers::MESSAGE_CHANNEL_DEST_ADDRESS, u32::from(host_ip))?;
        self.write_reg(registers::MESSAGE_CHANNEL_RETRY_COUNT, 0)?;
        self.write_reg(registers::MESSAGE_CHANNEL_TRANSMISSION_TIMEOUT, 0)?;
        self.write_reg(registers::MESSAGE_CHANNEL_PORT, registers::mcp_make(host_port))
    }

    /// Close the message channel: MCP must be the first register written
    /// (§12.4, CR-106ca).
    pub fn close_message_channel(&self) -> GentlResult<()> {
        self.write_reg(registers::MESSAGE_CHANNEL_PORT, 0)
    }

    /// Fetch the First URL bootstrap register content (512 bytes), trimmed
    /// at the first NUL.
    pub fn xml_url(&self) -> GentlResult<String> {
        let mut buf = [0u8; 512];
        self.read_mem(registers::FIRST_URL, &mut buf)?;
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8(buf[..end].to_vec())
            .map_err(|_| GentlError::Io("XML URL is not ASCII/UTF-8".into()))
    }

    /// PACKETRESEND_CMD (0x0040) for one packet-id range of a block.
    ///
    /// Fire-and-forget by spec: CR-198ca forbids the ACKNOWLEDGE flag and
    /// CR-201cd defines no PACKETRESEND_ACK — the command is sent without
    /// waiting for anything, and §16.7 explicitly allows it to go out
    /// asynchronously, even while another command's ack is pending. The
    /// request id still advances (it must never be 0 or reused, spec
    /// §15.1). Retransmission of the REQUEST itself (on no response) is
    /// the receiver's job (§16.7.3, M3-1); this method only sends.
    pub fn packet_resend(&self, stream_channel: u16, block_id: u64, first: u32, last: u32) {
        let payload = encode_packet_resend_payload(stream_channel, block_id, first, last);
        let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
        let mut hdr = [0u8; HEADER_LEN];
        let mut cmd = CmdHeader::new(PACKETRESEND_CMD, payload.len() as u16, self.alloc_req_id());
        cmd.flag = FLAG_EXTENDED_ID; // CR-200c: EI set, payload carries block_id64.
        cmd.encode(&mut hdr);
        buf.extend_from_slice(&hdr);
        buf.extend_from_slice(&payload);
        // Best effort: a failed send surfaces as the block never
        // recovering, i.e. the frame-retention timeout (§16.7.3 QoI).
        let _ = self.socket.send_to(&buf, self.device_addr);
    }

    /// Reset the device heartbeat timer by reading the CCP register
    /// (recommended method, spec §10.5). Errors are swallowed by design:
    /// heartbeat loss is surfaced as IO on the next real transaction.
    pub fn heartbeat(&self) {
        let _ = self.read_reg(registers::CCP);
    }
}

enum RetryReason {
    Timeout,
}

/// Broadcast DISCOVERY_CMD on the given subnet broadcast addresses and
/// collect DISCOVERY_ACKs until `timeout` elapses.
pub fn discover(
    broadcast_targets: &[(Ipv4Addr, u16)],
    timeout: Duration,
) -> GentlResult<Vec<DiscoveryInfo>> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|e| GentlError::Io(format!("bind: {e}")))?;
    socket
        .set_broadcast(true)
        .map_err(|e| GentlError::Io(format!("set_broadcast: {e}")))?;
    socket.set_read_timeout(Some(timeout)).map_err(GentlError::from)?;

    let mut buf = [0u8; HEADER_LEN];
    CmdHeader::new(DISCOVERY_CMD, 0, 1).encode(&mut buf);
    for target in broadcast_targets {
        // Best effort: some subnets reject broadcast sends.
        let _ = socket.send_to(&buf, target);
    }

    let mut found = Vec::new();
    let deadline = std::time::Instant::now() + timeout;
    let mut inbuf = [0u8; 1024];
    while std::time::Instant::now() < deadline {
        match socket.recv_from(&mut inbuf) {
            Ok((n, _)) => {
                let Some(hdr) = AckHeader::decode(&inbuf[..n]) else { continue };
                if hdr.acknowledge != DISCOVERY_ACK
                    || hdr.status != GEV_STATUS_SUCCESS
                    || n < HEADER_LEN + DISCOVERY_ACK_PAYLOAD_LEN
                {
                    continue;
                }
                if let Some(info) = DiscoveryInfo::decode_payload(&inbuf[HEADER_LEN..n]) {
                    found.push(info);
                }
            }
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut
                    || e.kind() == io::ErrorKind::WouldBlock =>
            {
                break
            }
            Err(e) => return Err(GentlError::Io(format!("recv: {e}"))),
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::time::Instant;

    /// A PENDING_ACK must re-arm the wait instead of being silently skipped:
    /// the command is not retransmitted while the announced completion time
    /// (plus margin) is still running.
    #[test]
    fn pending_ack_extends_the_ack_timeout() {
        let device = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("device socket");
        let device_addr = device.local_addr().expect("device addr");
        let listener = device.try_clone().expect("clone");
        let fake = std::thread::spawn(move || {
            listener
                .set_read_timeout(Some(Duration::from_millis(50)))
                .expect("timeout");
            let mut buf = [0u8; 512];
            let mut commands = 0u32;
            let mut peer = None;
            let mut req_id = 0u16;
            let deadline = Instant::now() + Duration::from_millis(750);
            while Instant::now() < deadline {
                if let Ok((n, from)) = listener.recv_from(&mut buf) {
                    let Some(cmd) = CmdHeader::decode(&buf[..n]) else {
                        continue;
                    };
                    commands += 1;
                    peer = Some(from);
                    req_id = cmd.req_id;
                    let mut out = [0u8; HEADER_LEN + 4];
                    let mut hdr = [0u8; HEADER_LEN];
                    AckHeader::new(GEV_STATUS_SUCCESS, PENDING_ACK, 4, cmd.req_id)
                        .encode(&mut hdr);
                    out[..HEADER_LEN].copy_from_slice(&hdr);
                    out[HEADER_LEN + 2..].copy_from_slice(&900u16.to_be_bytes());
                    let _ = listener.send_to(&out, from);
                }
            }
            let mut ack = [0u8; HEADER_LEN + 4];
            let mut hdr = [0u8; HEADER_LEN];
            AckHeader::new(GEV_STATUS_SUCCESS, READREG_ACK, 4, req_id).encode(&mut hdr);
            ack[..HEADER_LEN].copy_from_slice(&hdr);
            ack[HEADER_LEN..].copy_from_slice(&0x1234_5678u32.to_be_bytes());
            let _ = listener.send_to(&ack, peer.expect("a command arrived"));
            commands
        });

        let client = GvcpClient::connect(Ipv4Addr::LOCALHOST, device_addr.port())
            .expect("client");
        let value = client.read_reg(0xDEAD_BEEF).expect("read after PENDING_ACK");
        assert_eq!(value, 0x1234_5678);
        let commands = fake.join().expect("fake device thread");
        assert_eq!(
            commands, 1,
            "PENDING_ACK must prevent a retransmission inside the announced window"
        );
    }
}
