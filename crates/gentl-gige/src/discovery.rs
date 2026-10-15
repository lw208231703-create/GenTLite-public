//! Local adapter enumeration (GenTL Interfaces) and GVCP device discovery.
//!
//! Adapter enumeration uses `GetAdaptersAddresses` (iphlpapi). Discovery
//! sends DISCOVERY_CMD to each interface's directed broadcast address
//! (`ip | !subnet_mask`) plus the limited broadcast `255.255.255.255:3956`
//! (notes/gvcp-wirefacts.md §16.1). Test hook: `GENTLITE_DISCOVERY_TARGETS`
//! (comma-separated `ip:port`) adds unicast targets whose replies are
//! attached without subnet filtering (loopback devices are not reachable
//! via broadcast and 127.0.0.0/8 has no GenTL interface).

use std::net::Ipv4Addr;
use std::time::Duration;

use gentl_core::GentlResult;
use gentl_gvcp::wire::DiscoveryInfo;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
    GAA_FLAG_SKIP_MULTICAST, IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
    IP_ADAPTER_UNICAST_ADDRESS_LH,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

/// GenTL GVCP port (spec §16.1); the device-side port for every target.
pub const GVCP_PORT: u16 = 3956;

/// Limited broadcast, the spec-defined discovery address (§5.1.1:
/// "a UDP broadcast message with a destination IP address of
/// 255.255.255.255"; Table 16-1). Multihomed delivery is left
/// unspecified by the spec — sending it via every NIC's route is the
/// common resolution.
const GLOBAL_DISCOVERY_IP: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 255);

/// A local network adapter (internal only — MVS-style aggregate interface
/// means NICs are discovery inputs, not GenTL interfaces).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NicInfo {
    pub ipv4: Ipv4Addr,
    pub subnet_mask: Ipv4Addr,
}

/// Aggregate interface identity, Basler-style (observed on
/// `ProducerGEV.cti`, 2026-10-12): ID = vendor-scoped module name,
/// DISPLAYNAME = clean user-facing "GigE Vision". MVS uses "GigE Vision"
/// for both. Both are single aggregate interfaces spanning all NICs
/// (notes/OPEN-QUESTIONS Q3).
pub const AGGREGATE_IFACE_ID: &str = "GenTLite GEV GenTL Interface Module";
pub const AGGREGATE_IFACE_DISPLAY_NAME: &str = "GigE Vision";

/// `00:11:22:33:44:55` style device ID (Q4).
pub fn device_id(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

fn prefix_len_to_mask(prefix: u8) -> u32 {
    if prefix >= 32 {
        u32::MAX
    } else if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

/// Enumerate this host's IPv4-capable network adapters. Failures of the
/// OS call surface as an empty list: a producer without interfaces is
/// useless but must not crash the consumer.
pub fn list_nics() -> Vec<NicInfo> {
    let mut out = Vec::new();
    unsafe {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size: u32 = 0;
        // First call: required size.
        let r = GetAdaptersAddresses(AF_INET as u32, flags, std::ptr::null(), std::ptr::null_mut(), &mut size);
        if r != 0 && r != windows_sys::Win32::Foundation::ERROR_BUFFER_OVERFLOW {
            return out;
        }
        let mut buf = vec![0u8; size as usize];
        let r = GetAdaptersAddresses(
            AF_INET as u32,
            flags,
            std::ptr::null(),
            buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
            &mut size,
        );
        if r != 0 {
            return out;
        }

        let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !cur.is_null() {
            let a = &*cur;
            cur = a.Next;

            if a.IfType == IF_TYPE_SOFTWARE_LOOPBACK || a.OperStatus != IfOperStatusUp {
                continue; // loopback (no GVCP) or down
            }
            if a.PhysicalAddressLength < 6 {
                continue; // no hardware MAC: not a real NIC for our purposes
            }

            // First IPv4 unicast address of this adapter (we asked for
            // AF_INET only, so anything listed here is IPv4).
            let mut ua = a.FirstUnicastAddress;
            while !ua.is_null() {
                let u: &IP_ADAPTER_UNICAST_ADDRESS_LH = &*ua;
                ua = u.Next;
                let sa = u.Address.lpSockaddr as *const SOCKADDR_IN;
                if sa.is_null() || (*sa).sin_family != AF_INET {
                    continue;
                }
                let ip = Ipv4Addr::from(u32::from_be((*sa).sin_addr.S_un.S_addr));
                let mask = Ipv4Addr::from(u32::from_be(prefix_len_to_mask(u.OnLinkPrefixLength)));
                out.push(NicInfo {
                    ipv4: ip,
                    subnet_mask: mask,
                });
                break; // one address per NIC is enough
            }
        }
    }
    out
}

/// Broadcast targets for one interface: directed broadcast plus the global
/// discovery address (wirefacts §16.1).
pub fn broadcast_targets(info: &NicInfo) -> Vec<(Ipv4Addr, u16)> {
    let ip = u32::from(info.ipv4);
    let mask = u32::from(info.subnet_mask);
    vec![
        (Ipv4Addr::from(ip | !mask), GVCP_PORT),
        (GLOBAL_DISCOVERY_IP, GVCP_PORT),
    ]
}

/// Extra unicast discovery targets from `GENTLITE_DISCOVERY_TARGETS`
/// ("ip:port,ip:port"); used for loopback test devices and WAN cameras.
fn extra_targets() -> Vec<(Ipv4Addr, u16)> {
    let Ok(spec) = std::env::var("GENTLITE_DISCOVERY_TARGETS") else {
        return Vec::new();
    };
    spec.split(',')
        .filter_map(|t| {
            let (ip, port) = t.trim().rsplit_once(':')?;
            Some((ip.parse().ok()?, port.parse().ok()?))
        })
        .collect()
}

/// Run GVCP discovery across ALL NICs for the MVS-style aggregate
/// interface: every NIC's directed broadcast plus the global discovery
/// address, plus explicit extra targets. Replies are de-duplicated by
/// device MAC (no subnet filter: the aggregate interface spans subnets).
pub fn discover_all(nics: &[NicInfo], timeout: Duration) -> GentlResult<Vec<DiscoveryInfo>> {
    let mut found: Vec<DiscoveryInfo> = Vec::new();
    let mut merge = |list: Vec<DiscoveryInfo>| {
        for d in list {
            if found.iter().any(|f| f.mac == d.mac) {
                continue;
            }
            found.push(d);
        }
    };

    let mut targets: Vec<(Ipv4Addr, u16)> = Vec::new();
    for nic in nics {
        targets.extend(broadcast_targets(nic));
    }
    targets.extend(extra_targets());
    // De-duplicate targets (global address repeats per NIC).
    targets.sort();
    targets.dedup();

    if !targets.is_empty() {
        merge(gentl_gvcp::client::discover(&targets, timeout)?);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_format() {
        let mac = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        assert_eq!(device_id(&mac), "00:11:22:33:44:55");
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        assert_eq!(device_id(&mac), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn aggregate_identity_is_basler_style() {
        assert_eq!(AGGREGATE_IFACE_ID, "GenTLite GEV GenTL Interface Module");
        assert_eq!(AGGREGATE_IFACE_DISPLAY_NAME, "GigE Vision");
    }

    #[test]
    fn directed_broadcast_uses_inverted_mask() {
        let info = NicInfo {
            ipv4: Ipv4Addr::new(192, 168, 1, 10),
            subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
        };
        let t = broadcast_targets(&info);
        assert_eq!(t[0], (Ipv4Addr::new(192, 168, 1, 255), GVCP_PORT));
        assert_eq!(t[1], (GLOBAL_DISCOVERY_IP, GVCP_PORT));
    }

    #[test]
    fn mask_from_prefix() {
        assert_eq!(prefix_len_to_mask(24), 0xFFFFFF00);
        assert_eq!(prefix_len_to_mask(0), 0);
        assert_eq!(prefix_len_to_mask(32), u32::MAX);
    }
}
