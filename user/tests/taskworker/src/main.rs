//! A worker for the Task Service tests (T02, G02; ADR-0036).
//!
//! The first message on the bootstrap channel names the mode and carries a session
//! with the Task Service. The worker claims a task over it and says so on the
//! bootstrap channel (`claimed <id>`), then:
//!
//! * `work`: steps until it is asked to stop. Each step looks whether it should
//!   stop, saves a checkpoint, and causes one effect outside the machine (begun,
//!   then confirmed). Asked to stop, it says the task stopped -- cancelled, by its
//!   worker -- reports `stopped <id> after <n> effects` and exits 0.
//! * `hang`: begins an effect and never looks again: the worker Stop has to end
//!   from outside.
#![no_std]
#![no_main]

extern crate alloc;

use libspace::spaceabi::error::Error;
use libspace::spaceabi::task::state;
use libspace::task::Tasks;
use libspace::{handle, println, sys};

/// How long to keep asking for work before giving up.
const CLAIM_MS: u64 = 3000;
/// How long one effect takes, and the pause between steps.
const EFFECT_MS: u64 = 20;
const STEP_MS: u64 = 10;

fn say(text: &str) {
    let _ = sys::send(handle::BOOTSTRAP, text.as_bytes(), None);
}

fn claim(tasks: &Tasks) -> Result<u32, Error> {
    let until = sys::ticks_ms() + CLAIM_MS;
    loop {
        match tasks.claim() {
            Ok(t) => return Ok(t.id),
            Err(Error::NotFound | Error::WouldBlock) if sys::ticks_ms() < until => sys::sleep_ms(5),
            Err(e) => return Err(e),
        }
    }
}

/// Step until asked to stop; the effects that happened.
fn work(tasks: &Tasks, id: u32) -> Result<u32, Error> {
    let mut effects = 0u32;
    for step in 1.. {
        let (stop, t) = tasks.check(id)?;
        if stop {
            if t.state == state::RUNNING {
                let why = alloc::format!("stopped by its worker after {effects} effect(s)");
                tasks.move_to(id, state::CANCELLED, &why)?;
            }
            return Ok(effects);
        }
        tasks.checkpoint(id, &alloc::format!("after step {}", step - 1))?;
        let key = alloc::format!("step-{step}");
        match tasks.effect_begin(id, &key) {
            Ok(_) => {}
            // A stop came between the look and the effect: none is begun.
            Err(Error::Busy) => continue,
            Err(e) => return Err(e),
        }
        sys::sleep_ms(EFFECT_MS);
        tasks.effect_end(id, &key, true)?;
        effects += 1;
        sys::sleep_ms(STEP_MS);
    }
    Ok(effects)
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let mut mode = [0u8; 16];
    let (n, session) = match sys::recv(handle::BOOTSTRAP, &mut mode, false) {
        Ok((n, Some(h))) => (n, h),
        Ok(_) => {
            println!("[taskworker] no session came with the mode");
            return 2;
        }
        Err(e) => {
            println!("[taskworker] no mode: {e}");
            return 2;
        }
    };
    let tasks = Tasks::from_channel(session);
    let id = match claim(&tasks) {
        Ok(id) => id,
        Err(e) => {
            println!("[taskworker] no task to claim: {e}");
            return 3;
        }
    };
    match &mode[..n] {
        b"work" => {
            say(&alloc::format!("claimed {id}"));
            match work(&tasks, id) {
                Ok(effects) => {
                    say(&alloc::format!("stopped {id} after {effects} effects"));
                    0
                }
                Err(e) => {
                    println!("[taskworker] task {id}: {e}");
                    4
                }
            }
        }
        b"hang" => {
            if let Err(e) = tasks.effect_begin(id, &alloc::format!("charge-card-{id}")) {
                println!("[taskworker] task {id}: effect: {e}");
                return 4;
            }
            say(&alloc::format!("claimed {id}"));
            // Stuck in the middle of it: no more looks, no more messages.
            loop {
                core::hint::spin_loop();
            }
        }
        _ => {
            println!("[taskworker] unknown mode");
            5
        }
    }
}
