//! The device lease as a smoltcp [`Device`]: every received token is one frame
//! taken with `net_recv`, every transmitted one a frame handed to `net_send`.

use libspace::spaceabi::error::Error;
use libspace::spaceabi::syscall::FRAME_MAX;
use libspace::{Handle, sys};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

/// Attempts to hand over one frame while the transmit ring is full. The device
/// drains that ring within microseconds of being notified; a frame still refused
/// after this many yields is dropped, and TCP sends it again.
const TX_ATTEMPTS: usize = 16;

pub struct Lease {
    pub nic: Handle,
    rx: [u8; FRAME_MAX],
    pub rx_frames: u64,
    pub tx_frames: u64,
    pub tx_dropped: u64,
}

impl Lease {
    pub fn new(nic: Handle) -> Lease {
        Lease { nic, rx: [0; FRAME_MAX], rx_frames: 0, tx_frames: 0, tx_dropped: 0 }
    }
}

pub struct Rx<'a> {
    frame: &'a [u8],
}

pub struct Tx<'a> {
    nic: Handle,
    sent: &'a mut u64,
    dropped: &'a mut u64,
}

impl RxToken for Rx<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.frame)
    }
}

impl TxToken for Tx<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = [0u8; FRAME_MAX];
        if len > FRAME_MAX {
            // Cannot happen with the MTU we report; if it did, the frame could not
            // go out anyway. Let the stack build it somewhere, and drop it.
            let mut big = alloc::vec![0u8; len];
            let r = f(&mut big);
            *self.dropped += 1;
            return r;
        }
        let r = f(&mut buf[..len]);
        for _ in 0..TX_ATTEMPTS {
            match sys::net_send(self.nic, &buf[..len]) {
                Ok(_) => {
                    *self.sent += 1;
                    return r;
                }
                Err(Error::WouldBlock) => sys::yield_now(),
                Err(_) => break,
            }
        }
        *self.dropped += 1;
        r
    }
}

impl Device for Lease {
    type RxToken<'a> = Rx<'a>;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _now: Instant) -> Option<(Rx<'_>, Tx<'_>)> {
        let n = sys::net_recv(self.nic, &mut self.rx).ok()?;
        self.rx_frames += 1;
        let Lease { nic, rx, tx_frames, tx_dropped, .. } = self;
        Some((Rx { frame: &rx[..n] }, Tx { nic: *nic, sent: tx_frames, dropped: tx_dropped }))
    }

    fn transmit(&mut self, _now: Instant) -> Option<Tx<'_>> {
        Some(Tx { nic: self.nic, sent: &mut self.tx_frames, dropped: &mut self.tx_dropped })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        // For Ethernet, smoltcp counts the 14-byte header in the MTU.
        caps.max_transmission_unit = FRAME_MAX;
        caps
    }
}
