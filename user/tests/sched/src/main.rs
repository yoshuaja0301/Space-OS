//! Scheduler service classes from inside a process (ADR-0035).
//!
//! The first message names the mode. `rules`, sent with a root that may spawn, to a
//! process started in the background class: it may start nothing more urgent than
//! itself (and a refused start keeps the handle it was to carry), a class that does
//! not exist is refused, and a child started without a class gets this process's
//! class -- not that of whoever holds the root. `whoami`: exit with this process's
//! class, which is how `rules` reads its children's.
#![no_std]
#![no_main]

use libspace::spaceabi::error::Error;
use libspace::spaceabi::syscall::sched_class;
use libspace::{Handle, handle, println, sys};

/// Start `bin/sched whoami` asking for `class`, and read the class it got.
fn child_class(root: Handle, class: u32) -> Result<u32, Error> {
    let (mine, theirs) = sys::channel_create()?;
    let p = match sys::spawn_in(root, "bin/sched", 64, Some(theirs), class) {
        Ok(p) => p,
        Err(e) => {
            // A refused class is refused before the handle is taken: it is still ours.
            let kept = sys::handle_info(theirs).is_ok();
            sys::handle_close(theirs).ok();
            sys::handle_close(mine).ok();
            return if kept { Err(e) } else { Err(Error::Fault) };
        }
    };
    let sent = sys::send(mine, b"whoami", None);
    let st = sys::wait(p);
    sys::handle_close(p).ok();
    sys::handle_close(mine).ok();
    sent?;
    let st = st?;
    if st.reason != 0 {
        return Err(Error::Fault);
    }
    Ok(st.code as u32)
}

fn rules(root: Handle) -> Result<(), &'static str> {
    let me = sys::self_info().map_err(|_| "self_info failed")?.class;
    if me != sched_class::BACKGROUND {
        return Err("this process was not started in the background class");
    }
    for class in [sched_class::INTERACTIVE, sched_class::NORMAL] {
        match child_class(root, class) {
            Err(Error::Denied) => {}
            Err(Error::Fault) => return Err("a refused start took the handle it was to carry"),
            Ok(_) => return Err("a child more urgent than its parent was started"),
            Err(_) => return Err("a start more urgent than its parent failed, but not with Denied"),
        }
    }
    if child_class(root, sched_class::BACKGROUND + 1) != Err(Error::Invalid) {
        return Err("a class that does not exist was not refused with Invalid");
    }
    if child_class(root, sched_class::BACKGROUND) != Ok(sched_class::BACKGROUND) {
        return Err("a child in its parent's own class did not run in it");
    }
    if child_class(root, sched_class::INHERIT) != Ok(sched_class::BACKGROUND) {
        return Err("a child started without a class did not get its parent's");
    }
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let mut buf = [0u8; 16];
    let (n, passed) = match sys::recv(handle::BOOTSTRAP, &mut buf, false) {
        Ok(m) => m,
        Err(e) => {
            println!("[sched] no mode: {e}");
            return 2;
        }
    };
    match (&buf[..n], passed) {
        (b"whoami", _) => sys::self_info().map_or(-1, |i| i.class as i32),
        (b"rules", Some(root)) => match rules(root) {
            Ok(()) => {
                println!(
                    "[sched] background: more urgent children Denied, class 4 Invalid, a child without a class runs in the background"
                );
                0
            }
            Err(why) => {
                println!("[sched] FAIL {why}");
                1
            }
        },
        _ => {
            println!("[sched] unknown mode, or a mode without its handle");
            2
        }
    }
}
