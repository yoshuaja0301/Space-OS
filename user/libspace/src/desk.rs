//! Client side of the desktop contract ([`spaceabi::desk`], ADR-0020): a window
//! whose pixels live in a memory object this process owns and the display server
//! only reads.

use spaceabi::desk::{Msg, msg};
use spaceabi::error::Error;
use spaceabi::handle::Handle;

use crate::gfx::Surface;
use crate::sys;

/// Pixels this process draws a window into, mapped writable here.
pub struct WindowBuffer {
    ptr: *mut u32,
    pub w: u32,
    pub h: u32,
    bytes: usize,
}

impl WindowBuffer {
    /// A zeroed buffer of `w` by `h` pixels, and the handle to give the server.
    pub fn new(w: u32, h: u32) -> Result<(WindowBuffer, Handle), Error> {
        let bytes = w as usize * h as usize * 4;
        let handle = sys::vmo_create(bytes)?;
        match sys::vmo_map(handle, false) {
            Ok(p) => Ok((WindowBuffer { ptr: p as *mut u32, w, h, bytes }, handle)),
            Err(e) => {
                sys::handle_close(handle).ok();
                Err(e)
            }
        }
    }

    pub fn surface(&mut self) -> Surface<'_> {
        // SAFETY: `bytes` bytes mapped read-write for this process by `new`, alive
        // until `drop`; `&mut self` makes this the only view.
        let px = unsafe { core::slice::from_raw_parts_mut(self.ptr, self.bytes / 4) };
        Surface::new(px, self.w as i32, self.h as i32, self.w as usize)
    }
}

impl Drop for WindowBuffer {
    fn drop(&mut self) {
        sys::mem_unmap(self.ptr as *mut u8, self.bytes).ok();
    }
}

/// Send one message; the server drains its clients every turn, so a full channel
/// is a moment's wait, not a reason to lose the message.
pub fn send(chan: Handle, m: &Msg, carry: Option<Handle>) -> Result<(), Error> {
    let deadline = sys::ticks_ms() + 1000;
    loop {
        match sys::send(chan, m.as_bytes(), carry) {
            Err(Error::WouldBlock) if sys::ticks_ms() < deadline => sys::sleep_ms(2),
            r => return r,
        }
    }
}

/// The next message on `chan` within `timeout_ms`, and the handle it carried.
pub fn recv(chan: Handle, timeout_ms: u64) -> Result<Option<(Msg, Option<Handle>)>, Error> {
    match sys::wait_any(&[chan], timeout_ms) {
        Ok(_) => {}
        Err(Error::TimedOut) => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut buf = [0u8; core::mem::size_of::<Msg>()];
    match sys::recv(chan, &mut buf, true) {
        Ok((n, h)) => match Msg::from_bytes(&buf[..n]) {
            Some(m) => Ok(Some((m, h))),
            None => {
                if let Some(h) = h {
                    sys::handle_close(h).ok();
                }
                Err(Error::Invalid)
            }
        },
        Err(Error::WouldBlock) => Ok(None),
        Err(e) => Err(e),
    }
}

/// One window on the desktop.
pub struct Window {
    pub chan: Handle,
    pub id: u32,
    pub buf: WindowBuffer,
    pub focused: bool,
}

impl Window {
    /// Ask the server at `chan` for a `w` by `h` window titled `title`, its first
    /// picture drawn by `draw`.
    pub fn create(
        chan: Handle,
        w: u32,
        h: u32,
        title: &str,
        draw: impl FnOnce(&mut Surface),
    ) -> Result<Window, Error> {
        let (mut buf, handle) = WindowBuffer::new(w, h)?;
        draw(&mut buf.surface());
        let mut m = Msg::with_text(msg::CREATE, title);
        m.w = w;
        m.h = h;
        if let Err(e) = send(chan, &m, Some(handle)) {
            sys::handle_close(handle).ok();
            return Err(e);
        }
        loop {
            match recv(chan, 5000)? {
                None => return Err(Error::TimedOut),
                Some((r, h)) => {
                    if let Some(h) = h {
                        sys::handle_close(h).ok();
                    }
                    if r.kind == msg::CREATED {
                        r.result()?;
                        return Ok(Window { chan, id: r.id, buf, focused: false });
                    }
                }
            }
        }
    }

    /// The whole window changed.
    pub fn damage(&self) -> Result<(), Error> {
        let mut m = Msg::new(msg::DAMAGE);
        m.w = self.buf.w;
        m.h = self.buf.h;
        send(self.chan, &m, None)
    }

    pub fn set_title(&self, title: &str) -> Result<(), Error> {
        send(self.chan, &Msg::with_text(msg::TITLE, title), None)
    }

    /// Answer [`msg::CONFIGURE`]: a new buffer of `w` by `h`, drawn by `draw`, handed
    /// over; the old one goes when the server has the new one.
    pub fn resize(&mut self, w: u32, h: u32, draw: impl FnOnce(&mut Surface)) -> Result<(), Error> {
        let (mut buf, handle) = WindowBuffer::new(w, h)?;
        draw(&mut buf.surface());
        let mut m = Msg::new(msg::RESIZED);
        m.w = w;
        m.h = h;
        if let Err(e) = send(self.chan, &m, Some(handle)) {
            sys::handle_close(handle).ok();
            return Err(e);
        }
        self.buf = buf;
        Ok(())
    }

    /// Answer [`msg::DESCRIBE`].
    pub fn describe(&self, text: &str) -> Result<(), Error> {
        send(self.chan, &Msg::with_text(msg::DESCRIPTION, text), None)
    }

    /// The next message from the server within `timeout_ms`; carried handles are
    /// not part of any message a window receives, so they are closed.
    pub fn next(&mut self, timeout_ms: u64) -> Result<Option<Msg>, Error> {
        match recv(self.chan, timeout_ms)? {
            None => Ok(None),
            Some((m, h)) => {
                if let Some(h) = h {
                    sys::handle_close(h).ok();
                }
                if m.kind == msg::FOCUS {
                    self.focused = m.value != 0;
                }
                Ok(Some(m))
            }
        }
    }
}
