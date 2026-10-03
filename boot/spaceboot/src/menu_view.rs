use alloc::{format, string::String, vec::Vec};
use core::fmt::Write;
use uefi::proto::console::text::{Color, Key, ScanCode};
use uefi::{println, system};

use crate::preflight::Report;
use crate::ui::{self, Rect, Ui};

pub const LABELS: [&str; 7] = [
    "Start Space OS",
    "Recovery terminal",
    "Hardware information",
    "Network check",
    "Install boot files",
    "Restart",
    "Return to firmware",
];
const DESCRIPTIONS: [&str; 7] = [
    "Continue to your terminal",
    "Start without mounting disks",
    "Review this computer",
    "Read adapters and test DHCP",
    "Choose an existing EFI partition",
    "Restart this computer",
    "Choose another boot device",
];

fn console_line(row: usize, label: &str, selected: bool) {
    system::with_stdout(|out| {
        let (columns, rows) =
            out.current_mode().ok().flatten().map_or((80, 25), |mode| (mode.columns(), mode.rows()));
        if row >= rows.saturating_sub(1) {
            return;
        }
        let _ = out.set_cursor_position(2, row);
        let _ = out.set_color(
            if selected { Color::Black } else { Color::LightGray },
            if selected { Color::Cyan } else { Color::Black },
        );
        for ch in label.chars().take(columns.saturating_sub(4)) {
            let _ = out.write_char(ch);
        }
    });
}

fn console_begin(title: &str, subtitle: &str) {
    system::with_stdout(|out| {
        let _ = out.set_color(Color::LightGray, Color::Black);
        let _ = out.clear();
        let _ = out.enable_cursor(false);
    });
    console_line(1, "SPACE OS / BOOT", false);
    console_line(3, title, false);
    console_line(5, subtitle, false);
}

pub fn home(canvas: &mut Option<Ui>, selected: usize, info: &Report) -> Result<(), &'static str> {
    console_begin("Welcome to Space OS", "Choose how you want to start");
    for (index, label) in LABELS.iter().enumerate() {
        console_line(7 + index * 2, &format!("{}  {label}", index + 1), index == selected);
    }
    println!("Esc firmware | Up / Down + Enter | 1-7 select");
    let Some(ui) = canvas else {
        return Ok(());
    };
    ui.begin("Welcome to Space OS", "Your computer. Your next step.");
    let inset = ui.inset();
    let gap = 24;
    let width = ui.width() - inset * 2;
    let left = width * 3 / 5;
    let pitch = ((ui.height() - 190) / 7).min(70);
    for (index, label) in LABELS.iter().enumerate() {
        let y = 146 + index * pitch;
        let area = Rect { x: inset, y, width: left - gap, height: pitch - 8 };
        ui.panel(area);
        if index == selected {
            ui.rect(area, ui::SOFT);
            ui.rect(Rect { width: 4, ..area }, ui::ACCENT);
        }
        ui.text(inset + 16, y + 10, &format!("{}", index + 1), 1, ui::ACCENT);
        fit(ui, (inset + 44, y + 10), label, (left - gap - 60, ui::INK));
        if pitch >= 54 {
            fit(ui, (inset + 44, y + 31), DESCRIPTIONS[index], (left - gap - 60, ui::MUTED));
        }
    }
    let x = inset + left;
    ui.panel(Rect { x, y: 146, width: width - left, height: 224 });
    ui.text(x + 20, 164, "THIS COMPUTER", 1, ui::ACCENT);
    let vendor = core::str::from_utf8(&info.vendor).unwrap_or("Unknown CPU");
    for (index, row) in [
        format!("{vendor} / x86-64"),
        format!("{} MiB memory", info.memory_mib),
        String::from("UEFI firmware"),
        String::from("Keyboard navigation"),
        String::from("No automatic writes"),
    ]
    .iter()
    .enumerate()
    {
        fit(ui, (x + 20, 198 + index * 30), row, (width - left - 40, ui::INK));
    }
    if ui.height() >= 640 {
        ui.text(x + 20, 402, "BEFORE YOU START", 1, ui::ACCENT);
        fit(ui, (x + 20, 436), "Network check reads firmware.", (width - left - 40, ui::MUTED));
        fit(ui, (x + 20, 464), "Wi-Fi needs a supported driver.", (width - left - 40, ui::MUTED));
        fit(ui, (x + 20, 492), "Installer preserves other files.", (width - left - 40, ui::MUTED));
    }
    let footer = ui.height() - 35;
    ui.text(inset, footer, "Up / Down + Enter   |   1-7 select   |   Esc firmware", 1, ui::MUTED);
    ui.present().map_err(|_| "Cannot display the boot screen.")
}

pub fn details(
    canvas: &mut Option<Ui>,
    title: &str,
    rows: &[String],
    footer: &str,
) -> Result<(), &'static str> {
    details_page(canvas, title, rows, footer, 0).map(|_| ())
}

pub fn detail_key(
    canvas: &mut Option<Ui>,
    title: &str,
    rows: &[String],
    footer: &str,
) -> Result<Key, &'static str> {
    let mut page = 0;
    loop {
        let pages = details_page(canvas, title, rows, footer, page)?;
        match crate::menu::key()? {
            Key::Special(ScanCode::PAGE_UP) => page = page.saturating_sub(1),
            Key::Special(ScanCode::PAGE_DOWN) => page = (page + 1).min(pages - 1),
            key => return Ok(key),
        }
    }
}

fn fit(
    ui: &mut Ui,
    origin: (usize, usize),
    label: &str,
    style: (usize, uefi::proto::console::gop::BltPixel),
) {
    let columns = style.0 / ui.char_width();
    let end = label.char_indices().nth(columns).map_or(label.len(), |(index, _)| index);
    ui.text(origin.0, origin.1, &label[..end], 1, style.1);
}

fn wrapped(rows: &[String], columns: usize) -> Vec<(&str, bool)> {
    let mut lines = Vec::new();
    for row in rows {
        let warning = row.contains("unavailable") || row.contains("did not complete");
        let mut remaining = row.as_str();
        loop {
            let end = remaining.char_indices().nth(columns).map_or(remaining.len(), |(index, _)| index);
            lines.push((&remaining[..end], warning));
            remaining = &remaining[end..];
            if remaining.is_empty() {
                break;
            }
        }
    }
    lines
}

fn details_page(
    canvas: &mut Option<Ui>,
    title: &str,
    rows: &[String],
    footer: &str,
    page: usize,
) -> Result<usize, &'static str> {
    println!("{title}");
    for row in rows {
        println!("{row}");
    }
    println!("{footer}");
    let (console_columns, console_rows) = system::with_stdout(|out| {
        out.current_mode().ok().flatten().map_or((80, 25), |mode| (mode.columns(), mode.rows()))
    });
    let (columns, capacity) = canvas.as_ref().map_or(
        (console_columns.saturating_sub(4).max(1), console_rows.saturating_sub(11).max(1)),
        |ui| {
            (
                (ui.width() - ui.inset() * 2 - 40) / ui.char_width(),
                (ui.height() - 270) / (ui::TEXT_HEIGHT + 4),
            )
        },
    );
    let lines = wrapped(rows, columns);
    let pages = lines.len().div_ceil(capacity).max(1);
    let page = page.min(pages - 1);
    let indicator = format!("Page {} / {}   |   PgUp / PgDn", page + 1, pages);
    println!("{title}: {indicator}");
    console_begin(title, "Space OS startup tools");
    for (index, (line, _)) in lines.iter().skip(page * capacity).take(capacity).enumerate() {
        console_line(7 + index, line, false);
    }
    console_line(console_rows.saturating_sub(4), footer, false);
    console_line(console_rows.saturating_sub(2), &indicator, false);
    let Some(ui) = canvas else {
        return Ok(pages);
    };
    ui.begin(title, "Space OS startup tools");
    let inset = ui.inset();
    let body = Rect { x: inset, y: 146, width: ui.width() - inset * 2, height: ui.height() - 230 };
    ui.panel(body);
    for (index, (line, warning)) in lines.iter().skip(page * capacity).take(capacity).enumerate() {
        ui.text(
            inset + 20,
            body.y + 22 + index * (ui::TEXT_HEIGHT + 4),
            line,
            1,
            if *warning { ui::WARNING } else { ui::INK },
        );
    }
    fit(ui, (inset, ui.height() - 56), footer, (body.width, ui::ACCENT));
    ui.text(inset, ui.height() - 30, &indicator, 1, ui::MUTED);
    ui.present().map_err(|_| "Cannot display the startup tool.")?;
    Ok(pages)
}
