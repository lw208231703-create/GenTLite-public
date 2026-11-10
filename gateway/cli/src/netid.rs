//! Up-facing NIC identity resolution: the virtual device claims the up
//! NIC's MAC and subnet (the design note's identity table). `gentl_gige::
//! sys::nic_enums` is crate-private and carries no MAC on Linux (getifaddrs
//! has none), so the lookup lives here: getifaddrs maps the configured up
//! IP to an interface name and its netmask, /sys/class/net/<name>/address
//! supplies the MAC on Linux; GetAdaptersAddresses yields both in one call
//! on Windows.

use std::net::Ipv4Addr;

use gentl_core::{GentlError, GentlResult};

/// The up-facing link's identity: hardware MAC + subnet mask (the mask is
/// the virtual device's discovery-payload subnet — it lives on the up
/// network).
pub struct UpLink {
    pub mac: [u8; 6],
    pub mask: Ipv4Addr,
}

/// Resolve the MAC and subnet mask of the interface carrying `up_ip`.
///
/// Fails cleanly when no local interface owns the address: the gateway must
/// not invent a MAC (collision risk on the client's LAN) and the GVCP bind
/// of the same address would fail anyway.
pub fn resolve_up_link(up_ip: Ipv4Addr) -> GentlResult<UpLink> {
    resolve_up_link_impl(up_ip)
}

#[cfg(unix)]
fn resolve_up_link_impl(up_ip: Ipv4Addr) -> GentlResult<UpLink> {
    let (ifname, mask) = iface_for_ip(up_ip)?;
    let path = format!("/sys/class/net/{ifname}/address");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| GentlError::Io(format!("read {path}: {e}")))?;
    let mac = parse_mac(text.trim())
        .ok_or_else(|| GentlError::Io(format!("cannot parse MAC from {path}: {text:?}")))?;
    Ok(UpLink { mac, mask })
}

/// getifaddrs scan: first interface whose IPv4 address equals `up_ip`,
/// with its netmask.
#[cfg(unix)]
fn iface_for_ip(want: Ipv4Addr) -> GentlResult<(String, Ipv4Addr)> {
    use std::ffi::CStr;

    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 || head.is_null() {
            return Err(GentlError::Io("getifaddrs failed".into()));
        }
        let mut cur = head;
        let mut found = None;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if ip == want {
                let name = CStr::from_ptr(ifa.ifa_name)
                    .to_string_lossy()
                    .into_owned();
                let mask = if ifa.ifa_netmask.is_null() {
                    Ipv4Addr::UNSPECIFIED
                } else {
                    let nmsin = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
                    Ipv4Addr::from(u32::from_be(nmsin.sin_addr.s_addr))
                };
                found = Some((name, mask));
                break;
            }
        }
        libc::freeifaddrs(head);
        found.ok_or_else(|| {
            GentlError::Io(format!(
                "no local interface owns {want}; --up must be an address configured on this host"
            ))
        })
    }
}

#[cfg(windows)]
fn resolve_up_link_impl(up_ip: Ipv4Addr) -> GentlResult<UpLink> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_UNICAST_ADDRESS_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

    unsafe {
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size: u32 = 0;
        let r = GetAdaptersAddresses(AF_INET as u32, flags, std::ptr::null(), std::ptr::null_mut(), &mut size);
        if r != 0 && r != windows_sys::Win32::Foundation::ERROR_BUFFER_OVERFLOW {
            return Err(GentlError::Io("GetAdaptersAddresses sizing failed".into()));
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
            return Err(GentlError::Io(format!("GetAdaptersAddresses: {r}")));
        }
        let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !cur.is_null() {
            let a = &*cur;
            cur = a.Next;
            if a.PhysicalAddressLength < 6 {
                continue;
            }
            let mut ua = a.FirstUnicastAddress;
            while !ua.is_null() {
                let u: &IP_ADAPTER_UNICAST_ADDRESS_LH = &*ua;
                ua = u.Next;
                let sa = u.Address.lpSockaddr as *const SOCKADDR_IN;
                if sa.is_null() || (*sa).sin_family != AF_INET {
                    continue;
                }
                let ip = Ipv4Addr::from(u32::from_be((*sa).sin_addr.S_un.S_addr));
                if ip == up_ip {
                    let mac: [u8; 6] = [
                        a.PhysicalAddress[0],
                        a.PhysicalAddress[1],
                        a.PhysicalAddress[2],
                        a.PhysicalAddress[3],
                        a.PhysicalAddress[4],
                        a.PhysicalAddress[5],
                    ];
                    return Ok(UpLink {
                        mac,
                        mask: Ipv4Addr::from(prefix_len_to_mask(u.OnLinkPrefixLength)),
                    });
                }
            }
        }
        Err(GentlError::Io(format!(
            "no adapter owns {up_ip}; --up must be an address configured on this host"
        )))
    }
}

/// IPv4 prefix length (CIDR) to dotted mask (same math as the producer's
/// NIC enumeration; notes/gvcp-wirefacts.md Q31 era).
#[cfg(windows)]
fn prefix_len_to_mask(prefix: u8) -> u32 {
    if prefix >= 32 {
        u32::MAX
    } else if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

/// "aa:bb:cc:dd:ee:ff" -> [u8; 6]. (Windows resolves the MAC inside
/// GetAdaptersAddresses and does not need the parser outside tests.)
#[cfg_attr(windows, allow(dead_code))]
fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split(':');
    for b in &mut mac {
        *b = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_colon_mac() {
        assert_eq!(
            parse_mac("02:01:02:03:04:05"),
            Some([0x02, 0x01, 0x02, 0x03, 0x04, 0x05])
        );
        assert_eq!(parse_mac("02:01:02:03:04"), None);
        assert_eq!(parse_mac("02:01:02:03:04:05:06"), None);
        assert_eq!(parse_mac("zz:01:02:03:04:05"), None);
    }

    /// The unknown-identity path must fail cleanly on both platforms.
    #[test]
    fn unknown_ip_is_a_clean_error() {
        let bogus = Ipv4Addr::new(203, 0, 113, 99); // RFC 5737, never local
        assert!(resolve_up_link(bogus).is_err());
    }
}
