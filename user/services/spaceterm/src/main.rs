//! `spaceterm` – the interactive front end (requirement U01).
//!
//! Booting with `init=bin/spaceterm` on the kernel command line starts a session
//! instead of the acceptance run: this process opens one `spaceshell` session,
//! hands it the capabilities a session needs, and then does nothing but wait for it
//! to end. Everything after that is driven by whoever is typing.
//!
//! It exists so that "the terminal works" can be shown with the same image the
//! tests use, rather than asserted.
#![no_std]
#![no_main]

extern crate alloc;

use libspace::shell::Session;
use libspace::spaceabi::handle::rights;
use libspace::{Handle, exit_kind, println, sys};

const ROOT: Handle = libspace::handle::BOOTSTRAP;
const SHELL_QUOTA: u64 = 256;

/// Report a failure and stop the machine.
///
/// This process is pid 1 for an interactive boot: if it gives up without shutting
/// down, nothing else ever will, and the machine sits there looking like a hang
/// instead of telling anyone what went wrong.
fn give_up(what: &str) -> i32 {
    println!("[term] {what}; shutting down");
    let _ = sys::shutdown(ROOT, 1);
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    println!("[term] Space OS interactive session");
    let (mine, theirs) = match sys::channel_create() {
        Ok(v) => v,
        Err(e) => return give_up(&alloc::format!("channel: {e}")),
    };
    let shell = match sys::spawn(ROOT, "bin/spaceshell", SHELL_QUOTA, Some(theirs)) {
        Ok(h) => h,
        Err(e) => return give_up(&alloc::format!("cannot start the session service: {e}")),
    };
    // The session gets what a session needs and nothing more: start jobs, list
    // files, read the console, and narrow its own capability to read-only files
    // for the inference worker. It cannot shut the machine down - that stays here.
    let root = match sys::handle_dup(
        ROOT,
        rights::SPAWN | rights::FS | rights::CONSOLE | rights::TRANSFER | rights::DUP,
    ) {
        Ok(h) => h,
        Err(e) => {
            sys::kill(shell).ok();
            return give_up(&alloc::format!("cannot narrow the root capability: {e}"));
        }
    };
    let session = match Session::open(mine, root) {
        Ok(s) => s,
        Err(e) => {
            sys::kill(shell).ok();
            return give_up(&alloc::format!("cannot open the session: {e}"));
        }
    };
    // A session that can read files and run and stop programs is the system being
    // up (PRD v0.2 §7.4: what counts is the core, not the model): boots that did not
    // get this far are no longer counted against it (ADR-0027). One that cannot is
    // still a terminal to type into, but not a boot that succeeded.
    match session.check() {
        Ok(true) => match libspace::boot::mark_up(ROOT) {
            Ok(true) => println!("[term] the system is up; the boot count is back to 0"),
            Ok(false) => {}
            Err(e) => println!("[term] cannot clear the boot count: {e}"),
        },
        Ok(false) => println!("[term] the session cannot do its work; the boot count is left as it is"),
        Err(e) => println!("[term] the session could not be checked ({e}); the boot count is left as it is"),
    }

    // From here the person at the console is in charge. Waiting is all this process
    // has left to do.
    let status = match sys::wait(shell) {
        Ok(st) => st,
        Err(e) => return give_up(&alloc::format!("wait: {e}")),
    };
    let code = if status.kind == exit_kind::EXITED { status.code } else { 1 };
    println!("[term] session ended: {status:?}");
    let _ = sys::shutdown(ROOT, if code == 0 { 0 } else { 1 });
    code
}
