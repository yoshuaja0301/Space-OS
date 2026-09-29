//! A workload that never ends on its own, for the stability run (ADR-0019) to kill
//! at a moment of its choosing.
//!
//! The deterministic tests kill a process where they know it is: blocked in
//! `recv`, asleep, waiting. This one is killed wherever it happens to be -- inside
//! a syscall, between two, halfway through rewriting a file, with a memory object
//! mapped or a child half started -- and the kernel must take it apart just as
//! cleanly. Nothing here ends on its own: an exit is an error the parent reports.
//!
//! The first message names the mode; a handle sent with it is what the mode
//! needs (a root for `disk` and `spawn`).
#![no_std]
#![no_main]

use libspace::spaceabi::handle::rights;
use libspace::{Handle, handle, println, sys};

/// Bytes rewritten per round in `disk` mode: two FAT32 clusters and a bit, so
/// a round allocates and frees clusters rather than rewriting one in place.
const DISK_BYTES: usize = 9 * 1024;

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let mut buf = [0u8; 64];
    let (len, passed) = match sys::recv(handle::BOOTSTRAP, &mut buf, false) {
        Ok(m) => m,
        Err(e) => {
            println!("[churn] no mode: {e}");
            return 1;
        }
    };
    let mode = core::str::from_utf8(&buf[..len]).unwrap_or("?");
    // Tell the parent the workload is about to start, so its clock for "kill
    // after N ms" runs from work, not from process creation.
    if sys::send(handle::BOOTSTRAP, b"go", None).is_err() {
        return 1;
    }
    let (what, arg) = mode.split_once(' ').unwrap_or((mode, ""));
    let failed = match (what, passed) {
        ("disk", Some(root)) => disk(root, arg),
        ("mem", _) => mem(),
        ("ipc", _) => ipc(),
        ("spawn", Some(root)) => spawn(root),
        _ => Err((2, "unknown mode, or a mode without its handle")),
    };
    let (code, why) = match failed {
        Ok(never) => match never {},
        Err(e) => e,
    };
    println!("[churn] {mode}: {why}");
    code
}

enum Never {}

/// Exit code and what went wrong.
type Failure = (i32, &'static str);

/// Rewrite `/spaceos/var/churn<n>.dat` and read it back, again and again.
fn disk(root: Handle, n: &str) -> Result<Never, Failure> {
    let mut path = *b"/spaceos/var/churnX.dat";
    let digit = n.bytes().next().filter(u8::is_ascii_digit).ok_or((3, "disk: no file number"))?;
    path[18] = digit;
    let path = core::str::from_utf8(&path).map_err(|_| (3, "disk: bad path"))?;
    let mut data = [0u8; DISK_BYTES];
    let mut back = [0u8; 2048];
    let mut round: u32 = 0;
    loop {
        round = round.wrapping_add(1);
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i as u32).wrapping_mul(31).wrapping_add(round) as u8;
        }
        let f = sys::fs_create(root, path).map_err(|_| (10, "disk: create failed"))?;
        let mut off = 0;
        while off < data.len() {
            let end = (off + 3000).min(data.len());
            let w = sys::fs_write(f, off as u64, &data[off..end]).map_err(|_| (11, "disk: write failed"))?;
            if w != end - off {
                return Err((12, "disk: short write"));
            }
            off = end;
        }
        sys::handle_close(f).map_err(|_| (13, "disk: close failed"))?;
        let f = sys::fs_open(root, path).map_err(|_| (14, "disk: reopen failed"))?;
        let mut off = 0;
        while off < data.len() {
            let n = sys::fs_read(f, off as u64, &mut back).map_err(|_| (15, "disk: read failed"))?;
            if n == 0 || back[..n] != data[off..off + n] {
                return Err((16, "disk: the file does not read back as written"));
            }
            off += n;
        }
        sys::handle_close(f).map_err(|_| (17, "disk: close failed"))?;
    }
}

/// Map, touch and unmap anonymous memory and memory objects of changing sizes.
fn mem() -> Result<Never, Failure> {
    const PAGE: usize = 4096;
    let mut round: usize = 0;
    loop {
        round = round.wrapping_add(1);
        let len = (1 + round % 16) * PAGE;
        let p = sys::mem_map(len).map_err(|_| (20, "mem: map failed"))?;
        // SAFETY: `len` bytes just mapped read-write for this process alone.
        let area = unsafe { core::slice::from_raw_parts_mut(p, len) };
        if area.iter().any(|&b| b != 0) {
            return Err((21, "mem: fresh memory was not zero"));
        }
        for page in area.chunks_mut(PAGE) {
            page[0] = round as u8;
            page[PAGE - 1] = !(round as u8);
        }
        if area.chunks(PAGE).any(|page| page[0] != round as u8 || page[PAGE - 1] != !(round as u8)) {
            return Err((22, "mem: memory did not keep a write"));
        }
        sys::mem_unmap(p, len).map_err(|_| (23, "mem: unmap failed"))?;

        let len = (1 + round % 8) * PAGE;
        let v = sys::vmo_create(len).map_err(|_| (24, "mem: vmo_create failed"))?;
        let q = sys::vmo_map(v, false).map_err(|_| (25, "mem: vmo_map failed"))?;
        // Close the handle while mapped every other round: the mapping alone must
        // keep the object, and the unmap must then free it.
        if round.is_multiple_of(2) {
            sys::handle_close(v).map_err(|_| (26, "mem: close failed"))?;
        }
        // SAFETY: `len` bytes of the object, mapped read-write.
        unsafe { core::ptr::write_bytes(q, 0x5A, len) };
        sys::mem_unmap(q, len).map_err(|_| (27, "mem: unmapping the object failed"))?;
        if !round.is_multiple_of(2) {
            sys::handle_close(v).map_err(|_| (26, "mem: close failed"))?;
        }
    }
}

/// Channels made, used, passed through each other and closed.
fn ipc() -> Result<Never, Failure> {
    let mut round: u32 = 0;
    let mut msg = [0u8; 256];
    let mut got = [0u8; 256];
    loop {
        round = round.wrapping_add(1);
        let (a, b) = sys::channel_create().map_err(|_| (30, "ipc: channel_create failed"))?;
        let (c, d) = sys::channel_create().map_err(|_| (30, "ipc: channel_create failed"))?;
        for (i, m) in msg.iter_mut().enumerate() {
            *m = (i as u32 ^ round) as u8;
        }
        // Send one end of the second channel through the first, full-size message.
        let carried = sys::handle_dup(d, rights::SEND | rights::RECV | rights::TRANSFER)
            .map_err(|_| (31, "ipc: dup failed"))?;
        sys::send(a, &msg, Some(carried)).map_err(|_| (32, "ipc: send failed"))?;
        let (n, h) = sys::recv(b, &mut got, true).map_err(|_| (33, "ipc: recv failed"))?;
        let h = h.ok_or((34, "ipc: the handle did not arrive"))?;
        if got[..n] != msg[..] {
            return Err((35, "ipc: the message changed on the way"));
        }
        // The carried end still works.
        sys::send(c, b"x", None).map_err(|_| (36, "ipc: send on the other channel failed"))?;
        let (n, _) = sys::recv(h, &mut got, true).map_err(|_| (37, "ipc: recv on the carried end failed"))?;
        if &got[..n] != b"x" {
            return Err((38, "ipc: wrong message on the carried end"));
        }
        // Leave a message queued in every other round: closing must free it.
        if round.is_multiple_of(2) {
            sys::send(a, &msg, None).map_err(|_| (39, "ipc: send failed"))?;
        }
        for x in [a, b, c, d, h] {
            sys::handle_close(x).map_err(|_| (40, "ipc: close failed"))?;
        }
    }
}

/// Start `bin/hello` and wait for it, again and again: killed mid-spawn or
/// mid-wait, the child must still end and be taken apart. Paced, because every
/// exit is a line in the kernel log.
fn spawn(root: Handle) -> Result<Never, Failure> {
    loop {
        sys::sleep_ms(10);
        let p = sys::spawn(root, "bin/hello", 128, None).map_err(|_| (50, "spawn: spawn failed"))?;
        let st = sys::wait(p).map_err(|_| (51, "spawn: wait failed"))?;
        if !st.is_exited_with(0) {
            return Err((52, "spawn: hello did not exit 0"));
        }
        sys::handle_close(p).map_err(|_| (53, "spawn: close failed"))?;
    }
}
