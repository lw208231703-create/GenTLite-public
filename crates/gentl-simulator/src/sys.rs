//! OS backend abstraction (Phase A Linux port, notes/PENDING.md "Linux
//! 可移植"): the per-socket IP don't-fragment option for the §24.5 test
//! packet ("The don't fragment bit must also be set in the IP Header").
//! One API, two implementations behind `cfg(windows)` / `cfg(unix)`; the
//! Windows body is the pre-port code moved here verbatim.
//!
//! Best-effort on both platforms (QoI): a failed setsockopt must not
//! block the test packet — the emitter treats DF as advisory, matching a
//! real camera whose stack refuses the option.

use std::net::UdpSocket;

/// Set/clear IP don't-fragment on the sink socket for a §24.5 test
/// packet. Windows exposes the bit directly (IP_DONTFRAGMENT).
#[cfg(windows)]
pub(crate) fn set_ip_do_not_fragment(socket: &UdpSocket, on: bool) {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{setsockopt, IPPROTO_IP, IP_DONTFRAGMENT};

    let flag: i32 = i32::from(on);
    unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            IPPROTO_IP,
            IP_DONTFRAGMENT,
            (&flag as *const i32).cast(),
            std::mem::size_of::<i32>() as i32,
        );
    }
}

/// Unix side: Linux has no per-packet DF socket option; IP_MTU_DISCOVER =
/// IP_PMTUDISC_DO makes the kernel set the DF bit on every outgoing
/// datagram (and do PMTU discovery itself — the intent of §24.5's "don't
/// fragment"). Failure is silent on both platforms (QoI — see module
/// docs); non-Linux Unix targets get the same silent no-op.
#[cfg(unix)]
pub(crate) fn set_ip_do_not_fragment(socket: &UdpSocket, on: bool) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;

        let mode: libc::c_int = if on {
            libc::IP_PMTUDISC_DO
        } else {
            libc::IP_PMTUDISC_DONT
        };
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_MTU_DISCOVER,
                (&mode as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (socket, on);
}
