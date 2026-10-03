use alloc::{string::String, vec};
use uefi::proto::console::text::{Color, Key, ScanCode};
use uefi::{Status, boot, runtime, system};

use crate::{boot_pages, menu_view, preflight::Report, ui::Ui};

pub enum Choice {
    Continue,
    Recovery,
    Firmware,
}

pub(crate) fn key() -> Result<Key, &'static str> {
    loop {
        match system::with_stdin(|input| input.read_key()) {
            Ok(Some(key)) => return Ok(key),
            Ok(None) => boot::stall(10_000),
            Err(_) => return Err("Firmware keyboard is unavailable. Use a UEFI-supported keyboard."),
        }
    }
}

pub fn choose(info: &Report, windows: bool) -> Result<Choice, &'static str> {
    let mut canvas = Ui::new();
    if let Some(ui) = &canvas {
        uefi::println!("GOP resolution: {}x{}", ui.width(), ui.height());
    }
    let mut selected = 0;
    loop {
        menu_view::home(&mut canvas, selected, info)?;
        let choice = match key()? {
            Key::Special(ScanCode::UP) => {
                selected = (selected + 6) % 7;
                None
            }
            Key::Special(ScanCode::DOWN) => {
                selected = (selected + 1) % 7;
                None
            }
            Key::Special(ScanCode::ESCAPE) => Some(6),
            Key::Printable(ch) => match char::from(ch) {
                '\r' => Some(selected),
                '1'..='7' => usize::try_from(u32::from(char::from(ch)) - u32::from('1')).ok(),
                _ => None,
            },
            Key::Special(_) => None,
        };
        match choice {
            Some(0) | Some(1) => {
                drop(canvas);
                restore();
                return Ok(if choice == Some(0) { Choice::Continue } else { Choice::Recovery });
            }
            Some(2) => boot_pages::hardware(&mut canvas, info, windows)?,
            Some(3) => boot_pages::networks(&mut canvas)?,
            Some(4) => boot_pages::installer(&mut canvas)?,
            Some(5) => runtime::reset(runtime::ResetType::COLD, Status::SUCCESS, None),
            Some(6) => {
                drop(canvas);
                restore();
                return Ok(Choice::Firmware);
            }
            Some(_) | None => {}
        }
    }
}

pub fn error(reason: &str) {
    let mut canvas = Ui::new();
    let rows = vec![
        String::from(reason),
        String::from("The computer is still in firmware. The kernel has not started."),
        String::from("1 Restart"),
        String::from("2 Return to firmware"),
        String::from("Check hardware requirements or replace the boot image."),
    ];
    loop {
        match menu_view::detail_key(
            &mut canvas,
            "Space OS could not start",
            &rows,
            "1 restart   |   2 / Esc return to firmware",
        ) {
            Ok(Key::Printable(ch)) if char::from(ch) == '1' => {
                runtime::reset(runtime::ResetType::COLD, Status::SUCCESS, None)
            }
            Ok(Key::Printable(ch)) if char::from(ch) == '2' => break,
            Ok(Key::Special(ScanCode::ESCAPE)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    drop(canvas);
    restore();
}

fn restore() {
    system::with_stdout(|out| {
        let _ = out.set_color(Color::LightGray, Color::Black);
        let _ = out.clear();
        let _ = out.enable_cursor(true);
    });
}
