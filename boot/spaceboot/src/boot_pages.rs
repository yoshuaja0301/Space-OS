use alloc::{format, string::String, vec, vec::Vec};
use uefi::proto::console::text::{Key, ScanCode};

use crate::{install, menu_view, network, preflight::Report, ui::Ui};

pub fn hardware(canvas: &mut Option<Ui>, info: &Report, windows: bool) -> Result<(), &'static str> {
    let vendor = core::str::from_utf8(&info.vendor).unwrap_or("Unknown CPU");
    let rows = vec![
        format!("CPU: {vendor} / NX + SYSCALL supported"),
        format!("Available memory: {} MiB / {} regions", info.memory_mib, info.regions),
        format!("UEFI graphics: {} / ACPI 2.0: {}", info.graphics, info.acpi),
        format!("Firmware block handles: {} (includes partitions)", info.block_handles),
        format!("Windows loader on this boot volume: {windows}"),
        String::from("Native storage: modern VirtIO. NVMe / SATA / USB unavailable."),
        String::from("Kernel keyboard: PS/2 or COM2. UEFI USB input ends at boot."),
    ];
    menu_view::detail_key(canvas, "Hardware and boot readiness", &rows, "Any key to return.")?;
    Ok(())
}

fn address(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

pub fn networks(canvas: &mut Option<Ui>) -> Result<(), &'static str> {
    let mut result = None;
    loop {
        let inventory = network::inventory();
        let mut rows = vec![
            format!("Readable firmware network interfaces: {}", inventory.adapter_count),
            format!("Firmware Wi-Fi control interfaces: {}", inventory.wifi2_count),
        ];
        for (index, adapter) in inventory.adapters.iter().flatten().enumerate() {
            let link = match adapter.link {
                network::Link::Up => "link present",
                network::Link::Down => "link down",
                network::Link::Unknown => "not checked",
            };
            let mac = adapter.mac.map_or_else(
                || String::from("unknown address"),
                |bytes| {
                    format!(
                        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
                    )
                },
            );
            rows.push(format!("Interface {}: {mac} / {link}", index + 1));
            if let Some(status) = adapter.status {
                rows.push(format!("Interface {} firmware state: {status:?}", index + 1));
            }
        }
        if let Some(status) = inventory.status {
            rows.push(format!("Firmware enumeration error: {status:?}"));
        }
        rows.push(String::from("Wi-Fi scan/connect and native OS networking are unavailable."));
        rows.push(String::from("No firmware adapter does not mean no Wi-Fi hardware."));
        if let Some(probe) = result {
            match probe {
                network::ProbeResult::Lease(lease) => rows.push(format!(
                    "DHCP confirmed: {} ({} seconds)",
                    address(lease.address),
                    lease.seconds
                )),
                network::ProbeResult::NoLink => rows.push(String::from("DHCP: cable/link disconnected.")),
                network::ProbeResult::Timeout => {
                    rows.push(String::from("DHCP: no lease received within the probe limit."))
                }
                network::ProbeResult::Unsupported => {
                    rows.push(String::from("DHCP: firmware adapter cannot be safely probed."))
                }
                network::ProbeResult::FirmwareError(status) => {
                    rows.push(format!("DHCP: firmware error {status:?}"))
                }
            }
            rows.push(String::from("A DHCP lease does not verify internet access or kernel Wi-Fi."));
        }
        match menu_view::detail_key(
            canvas,
            "Network check",
            &rows,
            "D test DHCP   |   R refresh   |   Esc back",
        )? {
            Key::Special(ScanCode::ESCAPE) => return Ok(()),
            Key::Printable(ch) => match char::from(ch).to_ascii_lowercase() {
                'd' => {
                    menu_view::details(
                        canvas,
                        "Checking network",
                        &[String::from("Waiting for DHCP. This check has a finite polling limit.")],
                        "Please wait",
                    )?;
                    result = Some(network::probe_dhcp());
                }
                'r' => result = None,
                _ => {}
            },
            Key::Special(_) => {}
        }
    }
}

pub fn installer(canvas: &mut Option<Ui>) -> Result<(), &'static str> {
    let targets = match install::targets() {
        Ok(targets) => targets,
        Err(error) => {
            menu_view::detail_key(
                canvas,
                "Install boot files",
                &[String::from(error)],
                "Any key to return.",
            )?;
            return Ok(());
        }
    };
    if targets.is_empty() {
        menu_view::detail_key(
            canvas,
            "Install boot files",
            &[
                String::from("No eligible destination EFI partition was found."),
                String::from("An existing writable GPT EFI partition is required."),
                String::from("The current boot partition cannot be selected."),
                String::from("This installer does not create or format partitions."),
            ],
            "Any key to return.",
        )?;
        return Ok(());
    }
    let mut selected = 0;
    loop {
        let mut rows = Vec::new();
        rows.push(String::from("Choose the EFI partition for Space OS boot files."));
        for (index, target) in targets.iter().enumerate().skip(selected / 4 * 4).take(4) {
            rows.push(format!("{} {}", if index == selected { ">" } else { " " }, target.label));
        }
        rows.push(String::from("Existing boot files are protected. No disk formatting."));
        let action = menu_view::detail_key(
            canvas,
            "Install boot files",
            &rows,
            "Up / Down select   |   Enter review   |   Esc back",
        )?;
        match action {
            Key::Special(ScanCode::ESCAPE) => return Ok(()),
            Key::Special(ScanCode::UP) => selected = (selected + targets.len() - 1) % targets.len(),
            Key::Special(ScanCode::DOWN) => selected = (selected + 1) % targets.len(),
            Key::Printable(ch) if char::from(ch) == '\r' => {
                let confirmation = menu_view::detail_key(
                    canvas,
                    "Confirm installation",
                    &[
                        targets[selected].label.clone(),
                        String::from(
                            "Copy bootloader, kernel, initrd and configuration to this EFI partition.",
                        ),
                        String::from("Existing OS files and partition tables are preserved."),
                        String::from("No native Wi-Fi driver or separate data volume is installed."),
                        String::from("A failed copy may leave new incomplete files on this partition."),
                    ],
                    "I install boot files   |   Any other key cancel",
                )?;
                if matches!(confirmation, Key::Printable(ch) if char::from(ch).eq_ignore_ascii_case(&'i')) {
                    menu_view::details(
                        canvas,
                        "Installing boot files",
                        &[String::from("Writing and flushing the selected EFI partition.")],
                        "Please wait",
                    )?;
                    let rows = match install::install(targets[selected].id) {
                        Ok(summary) => vec![
                            format!(
                                "Installation complete: {} files / {} bytes",
                                summary.files, summary.bytes
                            ),
                            String::from("Choose this disk's UEFI boot option to start Space OS."),
                            String::from(
                                "No firmware boot entry was created. Disk formatting was not needed.",
                            ),
                        ],
                        Err(error) => {
                            vec![String::from("Installation did not complete."), String::from(error)]
                        }
                    };
                    menu_view::detail_key(canvas, "Installation result", &rows, "Any key to return.")?;
                }
            }
            Key::Special(_) | Key::Printable(_) => {}
        }
    }
}
