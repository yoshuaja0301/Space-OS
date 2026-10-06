//! The boot count the bootloader keeps on the data volume (ADR-0027).
//!
//! With `boot_count=on` in `spaceos.cfg`, `spaceboot` adds one to
//! `/spaceos/var/boots.txt` before every boot, and three boots in a row that never
//! came up send the next one to recovery. The program that *is* the system being
//! up -- the desktop, the terminal session -- calls [`mark_up`] once it is, and the
//! count goes back to 0.

use spaceabi::error::Error;

use crate::{Handle, sys};

/// Where the count is kept, on the data volume.
pub const COUNT_PATH: &str = "/spaceos/var/boots.txt";

/// Boots since the system last came up, if the volume keeps a count.
pub fn count(root: Handle) -> Option<u32> {
    let f = sys::fs_open(root, COUNT_PATH).ok()?;
    let mut buf = [0u8; 64];
    let n = sys::fs_read(f, 0, &mut buf);
    sys::handle_close(f).ok();
    let text = core::str::from_utf8(&buf[..n.ok()?]).ok()?;
    text.lines().find_map(|l| l.trim().strip_prefix("tries=")).and_then(|v| v.trim().parse().ok())
}

/// Set the count to 0. Needs `FS | FS_WRITE` on `root`.
pub fn clear(root: Handle) -> Result<(), Error> {
    const ZERO: &[u8] = b"tries=0\n";
    let f = sys::fs_create(root, COUNT_PATH)?;
    let r = sys::fs_write(f, 0, ZERO);
    sys::handle_close(f).ok();
    match r {
        Ok(n) if n == ZERO.len() => Ok(()),
        Ok(_) => Err(Error::Fault),
        Err(e) => Err(e),
    }
}

/// Say the system came up. A volume without a count is left as it is (nothing is
/// counted there); a count already at 0 is not written again. `Ok(true)` when a
/// count was cleared.
pub fn mark_up(root: Handle) -> Result<bool, Error> {
    match count(root) {
        None | Some(0) => Ok(false),
        Some(_) => clear(root).map(|()| true),
    }
}
