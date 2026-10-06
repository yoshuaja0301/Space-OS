//! The storage contract (D02, ADR-0037), from user space: a write that does not fit
//! is refused whole, a device error is reported and never mistaken for success, a
//! document is replaced through staging and one committing sector write, and a file
//! somebody holds open is not emptied, removed or replaced under them.
//!
//! The volume is shared with every earlier test, pass, boot and scenario, so each
//! test first gives back whatever an earlier run left lost: the counts it then
//! checks are its own.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libspace::spaceabi::error::Error;
use libspace::spaceabi::handle::rights;
use libspace::spaceabi::syscall::{FsCheck, debug_op};
use libspace::{println, sys};

use crate::ROOT;

fn check(repair: bool) -> Result<FsCheck, String> {
    sys::fs_check(ROOT, repair).map_err(|e| format!("volume check: {e}"))
}

/// Give back what earlier runs left lost; nothing may be cross-linked.
fn clean_start() -> Result<FsCheck, String> {
    let r = check(true)?;
    if r.crosslinked != 0 {
        return Err(format!("the volume has {} cross-linked cluster(s)", r.crosslinked));
    }
    if r.freed != 0 {
        println!("[init] storage: {} cluster(s) an earlier run left lost were given back", r.freed);
    }
    check(false)
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn write_file(path: &str, data: &[u8]) -> Result<(), String> {
    let f = sys::fs_create(ROOT, path).map_err(|e| format!("create {path}: {e}"))?;
    let w = sys::fs_write(f, 0, data);
    sys::handle_close(f).ok();
    match w {
        Ok(n) if n == data.len() => Ok(()),
        Ok(n) => Err(format!("write {path}: {n} of {} bytes", data.len())),
        Err(e) => Err(format!("write {path}: {e}")),
    }
}

fn read_file(path: &str) -> Result<Vec<u8>, String> {
    let f = sys::fs_open(ROOT, path).map_err(|e| format!("open {path}: {e}"))?;
    let size = sys::fs_stat(f).map(|s| s.size as usize);
    let mut out = Vec::new();
    let r = size.and_then(|size| {
        out.resize(size, 0);
        let mut done = 0;
        while done < size {
            let n = sys::fs_read(f, done as u64, &mut out[done..])?;
            if n == 0 {
                break;
            }
            done += n;
        }
        out.truncate(done);
        Ok(())
    });
    sys::handle_close(f).ok();
    r.map_err(|e| format!("read {path}: {e}"))?;
    Ok(out)
}

fn space_limit(clusters: Option<u32>) -> Result<(), String> {
    sys::debug_arg(ROOT, debug_op::FS_SPACE, clusters.map_or(u64::MAX, u64::from))
        .map_err(|e| format!("space limit: {e}"))
}

fn fail_writes(ok: u32, fail: u32) -> Result<(), String> {
    sys::debug_arg(ROOT, debug_op::BLOCK_FAIL, (u64::from(ok) << 32) | u64::from(fail))
        .map_err(|e| format!("injected failure: {e}"))
}

/// D02: a write the volume has no room for is refused whole.
pub fn no_space() -> Result<(), String> {
    const PATH: &str = "/spaceos/var/full.dat";
    let start = clean_start()?;
    let first = pattern(1000, 1);
    write_file(PATH, &first)?;
    let before = check(false)?;
    let big = pattern(5000, 2);
    let f = sys::fs_open_write(ROOT, PATH).map_err(|e| format!("open for writing: {e}"))?;
    // Room for two more clusters; the write needs ten.
    space_limit(Some(2))?;
    let refused = sys::fs_write(f, 1000, &big);
    let lifted = space_limit(None);
    let size = sys::fs_stat(f).map(|s| s.size);
    sys::handle_close(f).ok();
    lifted?;
    match refused {
        Err(Error::NoSpace) => {}
        other => return Err(format!("a write with no room for it gave {other:?}")),
    }
    if size != Ok(1000) || read_file(PATH)? != first {
        return Err(format!("the refused write changed the file ({size:?} bytes)"));
    }
    let after = check(false)?;
    if after.free != before.free || after.lost != 0 {
        return Err(format!(
            "the refused write kept clusters: {} free before, {} after, {} lost",
            before.free, after.free, after.lost
        ));
    }
    // Without the limit the same write goes through.
    let f = sys::fs_open_write(ROOT, PATH).map_err(|e| format!("open for writing: {e}"))?;
    let again = sys::fs_write(f, 1000, &big);
    sys::handle_close(f).ok();
    again.map_err(|e| format!("the write after the limit: {e}"))?;
    let mut want = first.clone();
    want.extend_from_slice(&big);
    if read_file(PATH)? != want {
        return Err(String::from("the file does not read back as written after the limit"));
    }
    sys::fs_remove(ROOT, PATH).map_err(|e| format!("remove: {e}"))?;
    let end = check(false)?;
    if end.free != start.free || end.lost != 0 {
        return Err(format!("removing the file left {} free clusters of {} before it", end.free, start.free));
    }
    println!(
        "[init] storage: a 5000-byte write with room for 2 more clusters was refused ({}): the file kept its 1000 bytes and the volume its {} free clusters; without the limit it went through, and removing the file gave back every cluster ({} free)",
        Error::NoSpace,
        before.free,
        end.free
    );
    Ok(())
}

/// D02: a device error is reported, never as success, and leaves the file as it was.
pub fn io_error() -> Result<(), String> {
    const PATH: &str = "/spaceos/var/ioerr.dat";
    clean_start()?;
    let first = pattern(600, 3);
    write_file(PATH, &first)?;
    let more = pattern(2000, 4);
    let over = pattern(600, 9);
    let f = sys::fs_open_write(ROOT, PATH).map_err(|e| format!("open for writing: {e}"))?;
    let tried = (|| -> Result<(), String> {
        // A growing write whose first device write fails, one that fails in the middle
        // of claiming its clusters, and an overwrite whose data write fails.
        let cases: [(u32, &str, u64, &[u8]); 3] = [
            (0, "first device write", 600, &more),
            (3, "device write in the middle of claiming clusters", 600, &more),
            (0, "data write, overwriting what is there", 0, &over),
        ];
        for (ok, what, at, data) in cases {
            fail_writes(ok, 1)?;
            let r = sys::fs_write(f, at, data);
            fail_writes(0, 0)?;
            match r {
                Err(Error::Io) => {}
                other => return Err(format!("a write whose {what} failed gave {other:?}")),
            }
            if sys::fs_stat(f).map(|s| s.size) != Ok(600) || read_file(PATH)? != first {
                return Err(format!("a write whose {what} failed changed the file"));
            }
            let c = check(false)?;
            if c.lost != 0 || c.crosslinked != 0 {
                return Err(format!("a write whose {what} failed left {} lost cluster(s)", c.lost));
            }
        }
        // The volume keeps working.
        sys::fs_write(f, 600, &more).map_err(|e| format!("the next write: {e}"))?;
        Ok(())
    })();
    sys::handle_close(f).ok();
    tried?;
    let mut want = first.clone();
    want.extend_from_slice(&more);
    if read_file(PATH)? != want {
        return Err(String::from("the file does not read back as written after the errors"));
    }
    sys::fs_remove(ROOT, PATH).map_err(|e| format!("remove: {e}"))?;
    println!(
        "[init] storage: a device error on a growing write's first device write, in the middle of claiming its clusters, and on an overwrite's data write was reported ({}) each time; the file kept its 600 bytes, no cluster was lost, and the next write went through",
        Error::Io
    );
    Ok(())
}

/// D02: a document is replaced through staging and commit -- the old version or the
/// new one, never a mix -- and only when nobody holds either open.
pub fn replace() -> Result<(), String> {
    const DOC: &str = "/spaceos/var/doc.txt";
    const STAGE: &str = "/spaceos/var/doc.new";
    const RENAMED: &str = "/spaceos/var/doc2.txt";
    const ELSEWHERE: &str = "/spaceos/doc.new";
    clean_start()?;
    write_file(DOC, b"version 1")?;
    let v2 = pattern(1500, 5);
    write_file(STAGE, &v2)?;
    let before = check(false)?;
    sys::fs_replace(ROOT, STAGE, DOC).map_err(|e| format!("replace: {e}"))?;
    if read_file(DOC)? != v2 {
        return Err(String::from("after the commit the document is not the staged version"));
    }
    if sys::fs_open(ROOT, STAGE).map(sys::handle_close).is_ok() {
        return Err(String::from("the staging file is still there after the commit"));
    }
    let after = check(false)?;
    if after.lost != 0 || after.free != before.free + 1 {
        return Err(format!(
            "after the commit: {} lost, {} free (was {})",
            after.lost, after.free, before.free
        ));
    }

    // A commit that fails: the staging entry is gone, the commit write is not.
    write_file(STAGE, b"version 3, never committed")?;
    fail_writes(1, 1)?;
    let r = sys::fs_replace(ROOT, STAGE, DOC);
    fail_writes(0, 0)?;
    match r {
        Err(Error::Io) => {}
        other => return Err(format!("a commit whose write failed gave {other:?}")),
    }
    if read_file(DOC)? != v2 {
        return Err(String::from("a failed commit changed the document"));
    }
    let lost = check(false)?;
    if lost.lost != 1 || lost.crosslinked != 0 {
        return Err(format!(
            "after the failed commit: {} lost, {} cross-linked",
            lost.lost, lost.crosslinked
        ));
    }
    let repaired = check(true)?;
    if repaired.freed != 1 || check(false)?.lost != 0 {
        return Err(format!("the repair gave back {} cluster(s)", repaired.freed));
    }

    // Held open, the document is not replaced; let go, it is.
    write_file(STAGE, b"version 4")?;
    let held = sys::fs_open(ROOT, DOC).map_err(|e| format!("open: {e}"))?;
    let busy = sys::fs_replace(ROOT, STAGE, DOC);
    sys::handle_close(held).ok();
    if busy != Err(Error::Busy) {
        return Err(format!("replacing a document held open gave {busy:?}"));
    }
    sys::fs_replace(ROOT, STAGE, DOC).map_err(|e| format!("replace once let go: {e}"))?;
    if read_file(DOC)? != b"version 4" {
        return Err(String::from("the document is not version 4"));
    }

    // Only within one directory; without a target, the staging file is renamed.
    write_file(ELSEWHERE, b"elsewhere")?;
    let across = sys::fs_replace(ROOT, ELSEWHERE, DOC);
    sys::fs_remove(ROOT, ELSEWHERE).ok();
    if across != Err(Error::Invalid) {
        return Err(format!("a replace across directories gave {across:?}"));
    }
    write_file(STAGE, b"renamed")?;
    sys::fs_replace(ROOT, STAGE, RENAMED).map_err(|e| format!("rename: {e}"))?;
    if read_file(RENAMED)? != b"renamed" || sys::fs_open(ROOT, STAGE).map(sys::handle_close).is_ok() {
        return Err(String::from("the staging file was not renamed"));
    }
    for p in [DOC, RENAMED] {
        sys::fs_remove(ROOT, p).map_err(|e| format!("remove {p}: {e}"))?;
    }
    if check(false)?.lost != 0 {
        return Err(String::from("clusters were lost"));
    }
    println!(
        "[init] storage: a document was replaced by its staged version with one sector write; a commit whose write failed ({}) left the old version and 1 lost cluster, which a repair gave back; a document held open was not replaced ({})",
        Error::Io,
        Error::Busy
    );
    Ok(())
}

/// D02: a removed file is gone and its space comes back; a file held open is not
/// removed or emptied.
pub fn remove() -> Result<(), String> {
    const PATH: &str = "/spaceos/var/gone.dat";
    let start = clean_start()?;
    write_file(PATH, &pattern(3000, 6))?;
    let held = sys::fs_open(ROOT, PATH).map_err(|e| format!("open: {e}"))?;
    let removed = sys::fs_remove(ROOT, PATH);
    let emptied = sys::fs_create(ROOT, PATH).map(sys::handle_close);
    sys::handle_close(held).ok();
    if removed != Err(Error::Busy) || emptied.map(|_| ()) != Err(Error::Busy) {
        return Err(format!("a file held open: remove gave {removed:?}, create gave {emptied:?}"));
    }
    sys::fs_remove(ROOT, PATH).map_err(|e| format!("remove: {e}"))?;
    if sys::fs_open(ROOT, PATH).map(sys::handle_close) != Err(Error::NotFound) {
        return Err(String::from("the removed file can still be opened"));
    }
    let end = check(false)?;
    if end.free != start.free || end.lost != 0 {
        return Err(format!("{} free clusters after removing, {} before writing", end.free, start.free));
    }
    let again = sys::fs_remove(ROOT, PATH);
    let dir = sys::fs_remove(ROOT, "/spaceos/var");
    let read_only = sys::handle_dup(ROOT, rights::FS).map_err(|e| format!("dup: {e}"))?;
    let denied = sys::fs_remove(read_only, "/spaceos/manifest.txt");
    sys::handle_close(read_only).ok();
    if again != Err(Error::NotFound) || dir != Err(Error::Invalid) || denied != Err(Error::Denied) {
        return Err(format!(
            "removing again gave {again:?}, a directory {dir:?}, without FS_WRITE {denied:?}"
        ));
    }
    println!(
        "[init] storage: a file held open was neither removed nor emptied ({}); once let go it was removed and its {} cluster(s) came back ({} free)",
        Error::Busy,
        (3000usize).div_ceil(end.cluster_bytes.max(1) as usize),
        end.free
    );
    Ok(())
}
