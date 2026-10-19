//! GigE Vision bootstrap register map (interface #0).
//! Source: GigE Vision 3.0 spec, Table 36-1 "Bootstrap Registers"
//! (see notes/gvcp-wirefacts.md). Addresses are 32-bit aligned.

/// Version: two most-significant bytes = major, two least = minor.
pub const VERSION: u32 = 0x0000;
pub const DEVICE_MODE: u32 = 0x0004;
pub const DEVICE_MAC_HIGH: u32 = 0x0008;
pub const DEVICE_MAC_LOW: u32 = 0x000C;
pub const NET_IF_CAPABILITY: u32 = 0x0010;
pub const NET_IF_CONFIGURATION: u32 = 0x0014;
pub const CURRENT_IP: u32 = 0x0024;
pub const CURRENT_SUBNET_MASK: u32 = 0x0034;
pub const CURRENT_DEFAULT_GATEWAY: u32 = 0x0044;
pub const MANUFACTURER_NAME: u32 = 0x0048;
pub const MODEL_NAME: u32 = 0x0068;
pub const DEVICE_VERSION: u32 = 0x0088;
pub const MANUFACTURER_INFO: u32 = 0x00A8;
pub const SERIAL_NUMBER: u32 = 0x00D8;
pub const USER_DEFINED_NAME: u32 = 0x00E8;
pub const FIRST_URL: u32 = 0x0200;
pub const SECOND_URL: u32 = 0x0400;
pub const NUM_NETWORK_INTERFACES: u32 = 0x0600;
pub const NUM_MESSAGE_CHANNELS: u32 = 0x0900;
pub const NUM_STREAM_CHANNELS: u32 = 0x0904;
pub const STREAMING_CAPABILITY: u32 = 0x092C;
pub const MESSAGE_CHANNEL_CAPABILITY: u32 = 0x0930;
pub const GVCP_CAPABILITY: u32 = 0x0934;
pub const HEARTBEAT_TIMEOUT: u32 = 0x0938;
pub const TIMESTAMP_TICK_FREQ_HIGH: u32 = 0x093C;
pub const TIMESTAMP_TICK_FREQ_LOW: u32 = 0x0940;
pub const TIMESTAMP_CONTROL: u32 = 0x0944;
pub const TIMESTAMP_VALUE_HIGH: u32 = 0x0948;
pub const TIMESTAMP_VALUE_LOW: u32 = 0x094C;
pub const DISCOVERY_ACK_DELAY: u32 = 0x0950;
pub const GVCP_CONFIGURATION: u32 = 0x0954;
pub const PENDING_TIMEOUT: u32 = 0x0958;
pub const CONTROL_SWITCHOVER_KEY: u32 = 0x095C;
pub const STREAMING_CONFIGURATION: u32 = 0x0960;
pub const CCP: u32 = 0x0A00;
pub const CONTROL_CHANNEL_MAX_PACKET_SIZE: u32 = 0x0A24;

// --- Bit numbering convention -------------------------------------------------
//
// The spec numbers bits MSB-first in every register diagram: "the bit labeled
// 0 is the most significant bit" (3.0 §15.1 Figure 15-4; 1.2 §13). A field
// printed as "bits 16-31" therefore occupies the LOW 16 bits of the u32, and
// "bit 0" is 0x8000_0000. Verified against the spec's own worked example
// (3.0 Figure 15-8 / 1.2 Figure 13-8: SCPS0 = 0xC000219C = fire_test
// (0x80000000) | do_not_fragment (0x40000000) | size 8604 in the low half),
// the 1.2 SCPS0 factory default 0x40000240 (DF + 576), and independently
// against Aravis (src/arvgvcpprivate.h) and pyGigEVision — Q31 for the full
// evidence chain.

// --- Streaming Capability (§36.27) / Configuration (§36.38) bit fields ------
//
// R-607s: a device MUST support at least one of GVSP/GVRSP. Writes of an
// unsupported protocol return GEV_STATUS_INVALID_PARAMETER (Table 36-2 +
// R-668cd); writes while any stream channel is open return GEV_STATUS_BUSY
// (R-667cd). Print bits are converted MSB-first (see convention above).
pub const SCAP_SCSPX_SUPPORTED: u32 = 1 << 31; // print bit 0
pub const SCAP_SCEBA_SUPPORTED: u32 = 1 << 28; // print bit 3 (CR-665cd)
pub const SCAP_GVSP_MODE_NOT_SUPPORTED: u32 = 1 << 27; // print bit 4
pub const SCAP_GVRSP_MODE_SUPPORTED: u32 = 1 << 26; // print bit 5
pub const SCONF_DEPRECATED_BIT: u32 = 0x4000_0000; // print bit 1, always 1
pub const SCONF_STREAMING_PROTOCOL_MASK: u32 = 0x0C00_0000; // print bits 4-5
pub const SCONF_STREAMING_PROTOCOL_SHIFT: u32 = 26;
/// Field values of the streaming_protocol field (§36.38).
pub const SCONF_PROTOCOL_GVSP: u32 = 0;
pub const SCONF_PROTOCOL_GVRSP: u32 = 1;

// Stream channel #0
pub const SCP0: u32 = 0x0D00;
pub const SCPS0: u32 = 0x0D04;
pub const SCPD0: u32 = 0x0D08;
pub const SCDA0: u32 = 0x0D18;
pub const SCSP0: u32 = 0x0D1C;
pub const SCC0: u32 = 0x0D20;
pub const SCCFG0: u32 = 0x0D24;
pub const SCMPC0: u32 = 0x0D30;
pub const SCMBS0_HIGH: u32 = 0x0D34;
pub const SCMBS0_LOW: u32 = 0x0D38;
pub const SCEBA0: u32 = 0x0D3C;

// --- Stream channel register address map (spec §36.57–§36.60) --------------
//
// Stream channel x's registers occupy the 0x40-byte block at
// 0x0D00 + 0x40*x: SCPx @ +0x00 (§36.57), SCPSx @ +0x04 (§36.58),
// SCPDx @ +0x08 (§36.59), SCDAx @ +0x18 (§36.60). Channels 1..511 are
// optional (§36.57): a device without channel x answers accesses to its
// registers with GEV_STATUS_INVALID_ADDRESS, which is how an application
// probes the implemented channel count. 0 <= x < 512.
/// Register block stride between consecutive stream channels.
pub const STREAM_CHANNEL_STRIDE: u32 = 0x40;

/// Offset of SCPx within a stream channel's register block (spec §36.57).
pub const SCP_OFFSET: u32 = 0x00;
/// Offset of SCPSx within a stream channel's register block (spec §36.58).
pub const SCPS_OFFSET: u32 = 0x04;
/// Offset of SCPDx within a stream channel's register block (spec §36.59).
pub const SCPD_OFFSET: u32 = 0x08;
/// Offset of SCDAx within a stream channel's register block (spec §36.60).
pub const SCDA_OFFSET: u32 = 0x18;

/// SCPx: stream channel #x port register (spec §36.57).
pub const fn scp(x: u32) -> u32 {
    SCP0 + STREAM_CHANNEL_STRIDE * x
}

/// SCPSx: stream channel #x packet size register (spec §36.58).
pub const fn scps(x: u32) -> u32 {
    SCPS0 + STREAM_CHANNEL_STRIDE * x
}

/// SCPDx: stream channel #x packet delay register (spec §36.59).
pub const fn scpd(x: u32) -> u32 {
    SCPD0 + STREAM_CHANNEL_STRIDE * x
}

/// SCDAx: stream channel #x destination IP register (spec §36.60).
pub const fn scda(x: u32) -> u32 {
    SCDA0 + STREAM_CHANNEL_STRIDE * x
}

/// Reverse map of the four per-channel stream channel registers: an address
/// inside a stream channel block decodes to (channel number, register
/// offset). Addresses outside the SCPx/SCPSx/SCPDx/SCDAx slots answer
/// `None` (reserved, like the unimplemented channel-0 slots SCSP0..SCEBA0
/// do for reads), as do channel numbers past 511 — the spec defines
/// channels 0..511, and unchecked the formula would reach far beyond
/// (e.g. 0x0010_0000, the sim's XML blob base, decodes as channel 262092).
pub fn stream_channel_addr(addr: u32) -> Option<(u32, u32)> {
    let rel = addr.checked_sub(SCP0)?;
    let (x, off) = (rel / STREAM_CHANNEL_STRIDE, rel % STREAM_CHANNEL_STRIDE);
    if x >= 512 {
        return None;
    }
    matches!(off, SCP_OFFSET | SCPS_OFFSET | SCPD_OFFSET | SCDA_OFFSET).then_some((x, off))
}

// Net If Capability / Configuration bits (spec §36.5, print bits 0/1/2:
// persistent IP / DHCP / LLA supported, respectively).
pub const IP_CFG_PERSISTENT_IP: u32 = 1 << 0;
pub const IP_CFG_DHCP: u32 = 1 << 1;
pub const IP_CFG_LLA: u32 = 1 << 2;

// CCP bits (spec §36.46, print bits 31/30/29; pyGigEVision takes control by
// writing 0x2 and Aravis uses the same masks — Q31).
pub const CCP_EXCLUSIVE_ACCESS: u32 = 1 << 0;
pub const CCP_CONTROL_ACCESS: u32 = 1 << 1;
pub const CCP_SWITCHOVER_ENABLE: u32 = 1 << 2;
/// control_switchover_key field (print bits 0-15); always reads as zero.
pub const CCP_SWITCHOVER_KEY_MASK: u32 = 0x0000_FFFF;
/// connection_count field, 3.0 §36.46 print bits 16-19 (low nibble of the
/// high half-word).
pub const CCP_CONNECTION_COUNT_MASK: u32 = 0x0000_F000;
pub const CCP_CONNECTION_COUNT_SHIFT: u32 = 12;

// --- GVCP Capability (0x0934) bit fields (1.2 §27.28 / 3.0 §36.29) --------
//
// MSB-first print numbering (Q31): print bit N occupies 1 << (31 - N).
pub const GVCP_CAPABILITY_USER_DEFINED_NAME: u32 = 1 << 31; // print bit 0 (UN)
pub const GVCP_CAPABILITY_SERIAL_NUMBER: u32 = 1 << 30; // print bit 1 (SN)
/// test_data (TD), print bit 6: the device fills GVSP test packet payloads
/// with the §24.5.1 LFSR stream (CR-280st). The sim does not declare it.
pub const GVCP_CAPABILITY_TEST_DATA: u32 = 0x0200_0000; // print bit 6 (TD)
/// packet_resend, print bit 29 (1.2 §27.28; 3.0 §36.29 same position;
/// Aravis GVCP_CAPABILITY_PACKET_RESEND = 1 << 2): the device answers
/// PACKETRESEND_CMD (§16.7). R-197ca: a requester MUST NOT send resend
/// requests to a device without this bit.
pub const GVCP_CAPABILITY_PACKET_RESEND: u32 = 0x0000_0004; // print bit 29



// --- Stream channel register field layouts (spec §36.57 / §36.58) ---------
//
// SCPx: print bit 0 = direction (read-only), print bits 14-15 = network
// interface index (3.0 §36.57, verified on the page figure; 1.2 §27.45 has
// bits 12-15), print bits 16-31 = host_port. host_port = 0 closes the
// channel; a non-zero write while streaming must be rejected with
// GEV_STATUS_BUSY (R-586cd). SCPx must be the last stream channel register
// written when opening the channel, and the first one written to close it
// (§36.57; SCPSx writes for packet-size negotiation after opening are the
// exception, §11.7). Per the MSB-first convention, host_port is the LOW 16
// bits: a bare port number is the spec-layout write (3.0 Figure 15-8's
// sibling registers and Aravis agree — Q31).
pub const SCP_HOST_PORT_MASK: u32 = 0x0000_FFFF;

/// Build an SCPx register value from a host port.
pub fn scp_make(host_port: u16) -> u32 {
    u32::from(host_port) & SCP_HOST_PORT_MASK
}

/// Extract the host_port field of an SCPx register value.
pub fn scp_host_port(v: u32) -> u16 {
    (v & SCP_HOST_PORT_MASK) as u16
}

// SCPSx: print bit 0 = fire_test_packet (write 1 to send one test packet,
// the device self-clears it), print bit 1 = do_not_fragment, print bit 2 =
// pixel endianness (optional), print bits 16-31 = packet_size. Per the
// MSB-first convention the flags are the TOP bits (fire = 0x8000_0000,
// DF = 0x4000_0000) and packet_size is the LOW 16 bits — exactly the layout
// of the spec's own worked example 0xC000219C (3.0 Figure 15-8) and the 1.2
// factory default 0x40000240 (DF + size 576). The packet size is the *total*
// packet size including all headers: IP (20) + UDP (8) + GVSP (§36.58); the
// generic payload packet data length is packet_size - GVSP_HEADERS_OVERHEAD.
// A device that cannot support the requested size rounds it and updates the
// register, so the application must read back after writing (R-562st). Write
// access is blocked while streaming.
pub const SCPS_SIZE_MASK: u32 = 0x0000_FFFF;
pub const SCPS_FIRE_TEST_PACKET: u32 = 1 << 31;
pub const SCPS_DO_NOT_FRAGMENT: u32 = 1 << 30;
pub const SCPS_PIXEL_ENDIANNESS: u32 = 1 << 29; // print bit 2; 1.2 §27.46 only — deprecated in 3.0 §36.58

/// Extract the packet_size field: total packet size in bytes, IP + UDP +
/// GVSP headers included.
pub fn scps_packet_size(v: u32) -> u32 {
    v & SCPS_SIZE_MASK
}

/// Build an SCPSx register value from a total packet size (headers included).
pub fn scps_make(packet_size: u32) -> u32 {
    packet_size & SCPS_SIZE_MASK
}

/// IP + UDP + GVSP header bytes counted in the SCPSx packet_size field.
pub const GVSP_HEADERS_OVERHEAD: u32 = 20 + 8 + 20; // IP + UDP + GVSP

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scp_host_port_roundtrips_and_defaults() {
        assert_eq!(scp_make(0), 0);
        assert_eq!(scp_make(3956), 3956, "host_port is the low 16 bits (§36.57)");
        assert_eq!(scp_host_port(0), 0);
        assert_eq!(scp_host_port(3956), 3956);
        // Bits outside the field (direction at the top, NII above host_port)
        // do not leak into host_port.
        assert_eq!(scp_host_port(0x8000_0000 | 0x000F_0000 | 3956), 3956);
        // The legacy "<<16" layout leaves the host_port field at zero, i.e.
        // a channel-close write — devices must reject it as an invalid
        // no-op (this is the mistake Q31 corrects).
        assert_eq!(scp_host_port(3956 << 16), 0);
    }

    #[test]
    fn scps_packet_size_roundtrips() {
        assert_eq!(scps_make(1448), 1448);
        assert_eq!(scps_packet_size(1448), 1448);
        // Flag bits (fire_test_packet / do_not_fragment) are not part of the
        // size; both are TOP bits per the MSB-first convention.
        assert_eq!(
            scps_packet_size(SCPS_FIRE_TEST_PACKET | SCPS_DO_NOT_FRAGMENT | 1448),
            1448
        );
        // The spec's own worked example (3.0 Figure 15-8 / 1.2 Figure 13-8):
        // 0xC000219C fires one unfragmented test packet of 8604 bytes.
        assert_eq!(scps_packet_size(0xC000_219C), 8604);
        assert_eq!(0xC000_219C & SCPS_FIRE_TEST_PACKET, SCPS_FIRE_TEST_PACKET);
        assert_eq!(0xC000_219C & SCPS_DO_NOT_FRAGMENT, SCPS_DO_NOT_FRAGMENT);
        // The 1.2 factory default (§27.46): DF set, size 576 in the low half.
        assert_eq!(scps_packet_size(0x4000_0240), 576);
        // An unwritten register reads as total size 0 (payload fallback is
        // the producer's job, see notes/OPEN-QUESTIONS.md).
        assert_eq!(scps_packet_size(0), 0);
    }



    #[test]
    fn gvsp_headers_overhead_matches_spec() {
        // §36.58: "IP header = 20 bytes, UDP header = 8 bytes and GVSP header".
        assert_eq!(GVSP_HEADERS_OVERHEAD, 48);
        // Typical 1500-byte MTU -> 1452 payload bytes per full packet.
        assert_eq!(1500 - GVSP_HEADERS_OVERHEAD, 1452);
    }

    #[test]
    fn stream_channel_address_map() {
        // The helpers must reproduce the channel-0 constants (Table 36-1)
        // and the 0x0D00 + 0x40*x layout of spec §36.57-§36.60.
        assert_eq!(scp(0), SCP0);
        assert_eq!(scps(0), SCPS0);
        assert_eq!(scpd(0), SCPD0);
        assert_eq!(scda(0), SCDA0);
        assert_eq!(scp(1), 0x0D40);
        assert_eq!(scps(1), 0x0D44);
        assert_eq!(scpd(1), 0x0D48);
        assert_eq!(scda(1), 0x0D58);
        assert_eq!(scp(511), 0x0D00 + 0x40 * 511);
        // Reverse map: the four mapped slots decode, reserved slots don't.
        assert_eq!(stream_channel_addr(SCP0), Some((0, SCP_OFFSET)));
        assert_eq!(stream_channel_addr(scda(3)), Some((3, SCDA_OFFSET)));
        assert_eq!(stream_channel_addr(scps(511)), Some((511, SCPS_OFFSET)));
        assert_eq!(stream_channel_addr(SCSP0), None);
        assert_eq!(stream_channel_addr(SCP0 - 4), None);
        // 0x0D3C (SCEBA0) is inside block 0 but not a mapped slot.
        assert_eq!(stream_channel_addr(SCEBA0), None);
        // Past the 512-slot map the formula must not claim addresses
        // (0x0010_0000 is the sim's GenICam XML blob base).
        assert_eq!(stream_channel_addr(scp(512)), None);
        assert_eq!(stream_channel_addr(0x0010_0000), None);
    }
}
