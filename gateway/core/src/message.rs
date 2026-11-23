//! Device-event relay (GigE Vision 3.0 §12 message channel, §17 EVENT /
//! EVENTDATA).
//!
//! The gateway is a *device* towards the remote host: it advertises one
//! message channel and forwards the downstream camera's asynchronous events
//! to whichever host opened that channel (MCDA + non-zero MCP, §12.2). The
//! downstream side (camera → gateway) is handled by the transport backend,
//! which forwards the raw EVENT_CMD / EVENTDATA_CMD datagram here; event ids
//! and payloads pass through unchanged because the host was given the
//! camera's own GenICam XML (which describes those event ids, §17.1
//! CO-230cd).
//!
//! ACK generation: the relay does not keep a retransmission buffer, so it
//! does not request EVENT_ACKs from the camera (the backend opens the
//! downstream message channel with MCTT = 0, CR-103cd) and it never sets the
//! ACKNOWLEDGE bit on the forwarded datagram.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Mutex, MutexGuard};

/// Shared host destination of the virtual message channel.
#[derive(Default)]
pub struct MessageRelay {
    dest: Mutex<Option<SocketAddr>>,
    socket: Mutex<Option<UdpSocket>>,
}

impl MessageRelay {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_dest(&self) -> MutexGuard<'_, Option<SocketAddr>> {
        self.dest.lock().expect("vgige message relay poisoned")
    }

    /// Set (or clear when `None`) the host destination of the virtual
    /// message channel: MCDA (address) + MCP (port), §12.2.
    pub fn set_destination(&self, dest: Option<SocketAddr>) {
        *self.lock_dest() = dest;
    }

    pub fn destination(&self) -> Option<SocketAddr> {
        *self.lock_dest()
    }

    /// Forward one raw EVENT_CMD / EVENTDATA_CMD datagram to the host that
    /// opened the message channel. Returns false when no host has it open.
    pub fn forward(&self, datagram: &[u8]) -> bool {
        let Some(dest) = self.destination() else {
            return false;
        };
        let mut guard = self.socket.lock().expect("vgige message relay poisoned");
        if guard.is_none() {
            *guard = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok();
        }
        let Some(socket) = guard.as_ref() else {
            return false;
        };
        socket.send_to(datagram, dest).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::time::Duration;

    #[test]
    fn forward_follows_the_open_destination() {
        let relay = MessageRelay::new();
        let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("rx");
        rx.set_read_timeout(Some(Duration::from_millis(300))).ok();
        // Channel closed: nothing is forwarded.
        assert!(!relay.forward(b"event"));

        relay.set_destination(Some(rx.local_addr().expect("addr")));
        assert!(relay.forward(b"event-1"));
        let mut buf = [0u8; 64];
        let (n, _) = rx.recv_from(&mut buf).expect("forwarded datagram");
        assert_eq!(&buf[..n], b"event-1");

        // Closing the channel (MCP = 0) stops the forwarding.
        relay.set_destination(None);
        assert!(!relay.forward(b"event-2"));
    }
}
