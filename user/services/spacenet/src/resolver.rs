//! Name lookups: one A query per lookup, over its own TCP connection to the DNS
//! server (RFC 7766 makes TCP something every server must answer on). The message
//! format lives in [`spaceabi::dns`], where it is unit-tested on the host.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use libspace::println;
use libspace::spaceabi::dns::{self, DnsError};
use libspace::spaceabi::error::Error;
use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::wire::{IpAddress, Ipv4Address};

/// Largest answer accepted (the length prefix included). A-record answers for
/// names up to 64 bytes are a few hundred bytes; anything much bigger is not one.
const REPLY_MAX: usize = 2048;

/// Who is waiting for a lookup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Waiter {
    /// A RESOLVE request on this session.
    Session(u32),
    /// A CONNECT to a name, on this connection.
    Conn(u32),
}

pub struct Query {
    pub id: u16,
    pub name: String,
    pub waiter: Waiter,
    pub deadline: u64,
    pub started: u64,
    pub sock: SocketHandle,
    sent: bool,
    reply: Vec<u8>,
}

impl Query {
    /// Open the connection to `server` for a lookup of `name` (already validated).
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        sockets: &mut SocketSet<'static>,
        iface: &mut Interface,
        server: (Ipv4Address, u16),
        local_port: u16,
        id: u16,
        name: &str,
        waiter: Waiter,
        deadline: u64,
    ) -> Result<Query, Error> {
        let mut s = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; REPLY_MAX]),
            tcp::SocketBuffer::new(vec![0u8; 512]),
        );
        s.set_nagle_enabled(false);
        s.set_ack_delay(None);
        s.connect(iface.context(), (IpAddress::Ipv4(server.0), server.1), local_port)
            .map_err(|_| Error::Unreachable)?;
        let sock = sockets.add(s);
        Ok(Query {
            id,
            name: String::from(name),
            waiter,
            deadline,
            started: libspace::sys::ticks_ms(),
            sock,
            sent: false,
            reply: Vec::new(),
        })
    }

    /// Move the lookup along. `Some` once it is over, one way or the other; the
    /// caller then lets go of the socket.
    pub fn advance(&mut self, sockets: &mut SocketSet<'static>, now: u64) -> Option<Result<[u8; 4], Error>> {
        let s = sockets.get_mut::<tcp::Socket>(self.sock);
        if !self.sent && s.may_send() {
            let mut q = [0u8; 2 + 12 + dns::NAME_MAX + 2 + 4];
            let Some(n) = dns::build_query(self.id, &self.name, &mut q) else {
                return Some(Err(Error::Invalid));
            };
            match s.send_slice(&q[..n]) {
                Ok(sent) if sent == n => self.sent = true,
                // The buffer is empty on a fresh connection and larger than any
                // query, so this is a broken connection, not a full buffer.
                _ => return Some(Err(Error::Unreachable)),
            }
        }
        while s.can_recv() && self.reply.len() < REPLY_MAX {
            let room = REPLY_MAX - self.reply.len();
            let reply = &mut self.reply;
            let _ = s.recv(|d| {
                let take = d.len().min(room);
                reply.extend_from_slice(&d[..take]);
                (take, ())
            });
        }
        if self.reply.len() >= 2 {
            let need = 2 + u16::from_be_bytes([self.reply[0], self.reply[1]]) as usize;
            if need > REPLY_MAX {
                println!("[net] DNS: a {need}-byte answer for {} is not an A answer; refused", self.name);
                return Some(Err(Error::Invalid));
            }
            if self.reply.len() >= need {
                return Some(match dns::parse_answer(self.id, &self.name, &self.reply[2..need]) {
                    Ok(a) => Ok(a),
                    Err(DnsError::NotFound) => Err(Error::NotFound),
                    Err(DnsError::ServerFailure(code)) => {
                        println!("[net] DNS: the server failed to answer for {} (rcode {code})", self.name);
                        Err(Error::NotFound)
                    }
                    Err(e) => {
                        println!("[net] DNS: unusable answer for {}: {e:?}", self.name);
                        Err(Error::Invalid)
                    }
                });
            }
        }
        if s.state() == tcp::State::Closed {
            // Refused, reset, or closed before a whole answer arrived.
            return Some(Err(Error::Unreachable));
        }
        if now >= self.deadline {
            return Some(Err(Error::TimedOut));
        }
        None
    }
}
