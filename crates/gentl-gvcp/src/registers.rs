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

// Net If Capability / Configuration bits (spec 36.5)
pub const IP_CFG_PERSISTENT_IP: u32 = 1 << 31;
pub const IP_CFG_DHCP: u32 = 1 << 30;
pub const IP_CFG_LLA: u32 = 1 << 29;

// CCP bits (spec 36.46)
pub const CCP_EXCLUSIVE_ACCESS: u32 = 1 << 31;
pub const CCP_CONTROL_ACCESS: u32 = 1 << 30;
pub const CCP_SWITCHOVER_ENABLE: u32 = 1 << 29;



// --- Stream channel register field layouts (spec §36.57 / §36.58) ---------
//
// SCPx: bit0 = direction (read-only), bits 14-15 = network_interface_index,
// bits 16-31 = host_port. host_port = 0 closes the channel; a non-zero write
// while streaming must be rejected with GEV_STATUS_BUSY (R-586cd). SCPx must
// be the last stream channel register written when opening the channel, and
// the first one written to close it (§36.57; SCPSx writes for packet-size
// negotiation after opening are the exception, §11.7).
pub const SCP_HOST_PORT_SHIFT: u32 = 16;

/// Build an SCPx register value from a host port.
pub fn scp_make(host_port: u16) -> u32 {
    (u32::from(host_port)) << SCP_HOST_PORT_SHIFT
}

/// Extract the host_port field (bits 16-31) of an SCPx register value.
pub fn scp_host_port(v: u32) -> u16 {
    (v >> SCP_HOST_PORT_SHIFT) as u16
}

// SCPSx: bit0 = fire_test_packet (write 1 to send one test packet, the device
// self-clears it), bit1 = do_not_fragment, bits 16-31 = packet_size. The
// packet size is the *total* packet size including all headers: IP (20) +
// UDP (8) + GVSP (§36.58); the generic payload packet data length is
// packet_size - GVSP_HEADERS_OVERHEAD. A device that cannot support the
// requested size rounds it and updates the register, so the application must
// read back after writing (R-562st). Write access is blocked while streaming.
pub const SCPS_SIZE_SHIFT: u32 = 16;
pub const SCPS_FIRE_TEST_PACKET: u32 = 1 << 0;
pub const SCPS_DO_NOT_FRAGMENT: u32 = 1 << 1;

/// Extract the packet_size field (bits 16-31): total packet size in bytes,
/// IP + UDP + GVSP headers included.
pub fn scps_packet_size(v: u32) -> u32 {
    v >> SCPS_SIZE_SHIFT
}

/// Build an SCPSx register value from a total packet size (headers included).
pub fn scps_make(packet_size: u32) -> u32 {
    packet_size << SCPS_SIZE_SHIFT
}

/// IP + UDP + GVSP header bytes counted in the SCPSx packet_size field.
pub const GVSP_HEADERS_OVERHEAD: u32 = 20 + 8 + 20; // IP + UDP + GVSP

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scp_host_port_roundtrips_and_defaults() {
        assert_eq!(scp_make(0), 0);
        assert_eq!(scp_make(3956), 3956 << 16);
        assert_eq!(scp_host_port(0), 0);
        assert_eq!(scp_host_port(3956 << 16), 3956);
        // Bits outside the field (direction/NII) do not leak into host_port.
        assert_eq!(scp_host_port((3956 << 16) | 0xC000 | 1), 3956);
    }

    #[test]
    fn scps_packet_size_roundtrips() {
        assert_eq!(scps_make(1448), 1448 << 16);
        assert_eq!(scps_packet_size(1448 << 16), 1448);
        // Flag bits (fire_test_packet / do_not_fragment) are not part of the size.
        assert_eq!(
            scps_packet_size((1448 << 16) | SCPS_FIRE_TEST_PACKET | SCPS_DO_NOT_FRAGMENT),
            1448
        );
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
}
