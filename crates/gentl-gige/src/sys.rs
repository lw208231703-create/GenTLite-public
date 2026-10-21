//! OS backend abstraction (Phase A Linux port, notes/PENDING.md "Linux
//! 可移植"): host-side operations the producer needs but std does not
//! expose — NIC enumeration, socket receive-buffer sizing, receiver-thread
//! CPU affinity. One API per operation, two implementations selected by
//! `cfg(windows)` / `cfg(unix)`; the Windows bodies are the pre-port code
//! moved here verbatim, so Windows behavior is unchanged.
//!
//! The GigE Vision / GenTL specs are silent on host enumeration and
//! tuning; everything in this module is QoI (quality of implementation),
//! as noted per function. Failures never panic: a producer without
//! interfaces or without tuning degrades, it must not crash the consumer.

use std::net::{Ipv4Addr, UdpSocket};

/// One enumerated NIC: (interface name, IPv4 address, subnet mask). The
/// name is the aggregation key on Unix (one `getifaddrs` interface yields
/// one row per address family); on Windows it is informational only —
/// the pre-port enumeration never used names.
pub(crate) type NicEnum = (String, Ipv4Addr, Ipv4Addr);

/// Enumerate this host's IPv4-capable, up, non-loopback network adapters.
/// Failures of the OS call surface as an empty list: a producer without
/// interfaces is useless but must not crash the consumer.
#[cfg(windows)]
pub(crate) fn nic_enums() -> Vec<NicEnum> {
    use std::ffi::CStr;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
        IP_ADAPTER_UNICAST_ADDRESS_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

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
                let name = if a.AdapterName.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(a.AdapterName as *const std::os::raw::c_char)
                        .to_string_lossy()
                        .into_owned()
                };
                out.push((name, ip, mask));
                break; // one address per NIC is enough
            }
        }
    }
    out
}

/// IPv4 prefix length (CIDR) to dotted mask.
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

/// Enumerate this host's IPv4-capable, up, non-loopback network interfaces
/// via `getifaddrs`, one row per interface name (the first AF_INET address
/// wins, matching the Windows "one address per NIC" rule).
///
/// QoI: getifaddrs is the Unix host-side enumeration detail the specs are
/// silent about (same standing as GetAdaptersAddresses on Windows). Unlike
/// the Windows path we cannot demand a 6-byte hardware MAC here (getifaddrs
/// carries none), so any up non-loopback AF_INET interface with a netmask
/// qualifies. Entries without a netmask are skipped: no mask, no directed
/// broadcast to target.
#[cfg(unix)]
pub(crate) fn nic_enums() -> Vec<NicEnum> {
    let mut out: Vec<NicEnum> = Vec::new();
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 || head.is_null() {
            return out; // enumeration failure: empty list, as on Windows
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null()
                || (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET
            {
                continue; // no address or not IPv4
            }
            let flags = ifa.ifa_flags as u32;
            if flags & (libc::IFF_UP as u32) == 0 {
                continue; // administratively down
            }
            if flags & (libc::IFF_LOOPBACK as u32) != 0 {
                continue; // loopback (no GVCP), parity with the Windows skip
            }
            if ifa.ifa_netmask.is_null() {
                continue; // no mask -> no directed broadcast (QoI)
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                .to_string_lossy()
                .into_owned();
            // Aggregate by interface name: one IPv4 row per interface.
            if out.iter().any(|(n, _, _)| *n == name) {
                continue;
            }
            let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let nmsin = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
            let mask = Ipv4Addr::from(u32::from_be(nmsin.sin_addr.s_addr));
            out.push((name, ip, mask));
        }
        libc::freeifaddrs(head);
    }
    out
}

/// Set SO_RCVBUF on a stream socket and return the granted buffer size in
/// bytes (0 when the call failed). The OS clamps the request to a kernel
/// maximum, so the getsockopt readback is the only truthful value — the
/// requested number is never reported anywhere. Failure is harmless by
/// design: the socket keeps its OS default and streaming still works.
#[cfg(windows)]
pub(crate) fn set_recv_buffer(socket: &UdpSocket, kb: usize) -> usize {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{getsockopt, setsockopt, SO_RCVBUF, SOL_SOCKET};

    const SIZE_OF_I32: i32 = std::mem::size_of::<i32>() as i32;
    let s = socket.as_raw_socket() as usize;
    let requested: i32 = kb.saturating_mul(1024).min(i32::MAX as usize) as i32;
    let r = unsafe {
        setsockopt(
            s,
            SOL_SOCKET,
            SO_RCVBUF,
            (&requested as *const i32).cast(),
            SIZE_OF_I32,
        )
    };
    if r != 0 {
        return 0;
    }
    let mut actual: i32 = 0;
    let mut len = SIZE_OF_I32;
    let r = unsafe { getsockopt(s, SOL_SOCKET, SO_RCVBUF, (&mut actual as *mut i32).cast(), &mut len) };
    if r != 0 { 0 } else { actual.max(0) as usize }
}

/// Unix side of [`set_recv_buffer`]: identical contract. QoI note: Linux
/// may clamp the request to `net.core.rmem_max` and (per Linux socket
/// buffer accounting) reports the doubled value on readback — the readback
/// remains the only truthful granted size, same as on Windows.
#[cfg(unix)]
pub(crate) fn set_recv_buffer(socket: &UdpSocket, kb: usize) -> usize {
    use std::os::unix::io::AsRawFd;

    let fd = socket.as_raw_fd();
    let requested: libc::c_int =
        kb.saturating_mul(1024).min(libc::c_int::MAX as usize) as libc::c_int;
    let len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let r = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&requested as *const libc::c_int).cast(),
            len,
        )
    };
    if r != 0 {
        return 0;
    }
    let mut actual: libc::c_int = 0;
    let mut len = len;
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&mut actual as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if r != 0 { 0 } else { actual.max(0) as usize }
}

/// Bind the calling thread to the CPU set in `mask` (bit i = CPU i), best
/// effort: an out-of-range mask fails silently and the thread keeps its
/// default placement. QoI: the spec is silent on host thread placement
/// and core selection is a deployment-side decision (see
/// `stream.rs::desired_rx_affinity`). `mask == 0` is a no-op on both
/// platforms.
#[cfg(windows)]
pub(crate) fn set_thread_affinity(mask: u64) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
    unsafe {
        SetThreadAffinityMask(GetCurrentThread(), mask as usize);
    }
}

/// Unix side of [`set_thread_affinity`]: `sched_setaffinity` on the
/// calling thread (pid/tid 0). Only the lower 64 CPUs are settable
/// through the u64 mask on both platforms; other Unix targets than
/// Linux/Android lack sched_setaffinity in libc and get the same silent
/// no-op as a failed call (QoI — placement tuning is best-effort).
#[cfg(unix)]
pub(crate) fn set_thread_affinity(mask: u64) {
    if mask == 0 {
        return;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        for cpu in 0..64 {
            if mask & (1u64 << cpu) != 0 {
                libc::CPU_SET(cpu as usize, &mut set);
            }
        }
        // 0 = the calling thread. A failed call (e.g. no such CPU) leaves
        // the thread at its default placement, matching Windows.
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let _ = mask;
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn mask_from_prefix() {
        assert_eq!(prefix_len_to_mask(24), 0xFFFFFF00);
        assert_eq!(prefix_len_to_mask(0), 0);
        assert_eq!(prefix_len_to_mask(32), u32::MAX);
    }
}
