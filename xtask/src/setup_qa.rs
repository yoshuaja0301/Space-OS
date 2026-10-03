use super::boot_qa::Guest;
use super::*;

#[path = "setup_disk.rs"]
mod disk;

const NETWORK: &[&str] = &[
    "-netdev",
    "user,id=setupnet",
    "-device",
    "e1000,netdev=setupnet,id=setupnic",
    "-drive",
    "format=raw,file=build/setup-target.img,index=1",
];
const HOME: &str = "Esc firmware";
const TARGETS: &str = "Enter review";
const CONFIRM: &str = "Any other key cancel";
const KERNEL: &[&str] = &["jumping to kernel", "spacekernel 0.1.0", "KERNEL PANIC"];

fn press(guest: &mut Guest, key: &str, marker: &str) -> Result<usize, String> {
    let from = guest.key(key)?;
    guest.wait(marker, from)?;
    Ok(from)
}

fn sole_target(page: &str) -> bool {
    let mut rows = page
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("> EFI partition ") || line.starts_with("EFI partition "));
    let expected = format!("> EFI partition {}", disk::TARGET_GUID);
    matches!(rows.next(), Some(row) if row.eq_ignore_ascii_case(&expected)) && rows.next().is_none()
}

pub(super) fn run(release: bool) -> Result<(), String> {
    let built = build(release)?;
    let image = root().join("build/esp-setup-menu.img");
    let target = root().join("build/setup-target.img");
    make_image(&built, "bootmenu=on", &image)?;
    make_data_disk(&root().join("build/data.img"))?;
    disk::prepare(&image, &target)?;
    let expected = disk::files(&image, ESP_SIZE)?;
    let original = fs::read(&target).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    {
        let machine = Machine { extra: &["-net", "none"], ..LAB };
        let mut guest = Guest::start(&machine, &image, &format!("setup-{stamp}-no-nic"))?;
        guest.wait(HOME, 0)?;
        press(&mut guest, "4", "D test DHCP")?;
        guest.require(&["Readable firmware network interfaces: 0"], KERNEL)?;
        press(&mut guest, "d", "DHCP: firmware adapter cannot be safely probed.")?;
        guest.capture(&format!("setup-{stamp}-no-nic"))?;
        press(&mut guest, "esc", HOME)?;
        press(&mut guest, "5", "No eligible destination EFI partition was found.")?;
        guest.require(&["The current boot partition cannot be selected."], KERNEL)?;
        guest.capture(&format!("setup-{stamp}-source-excluded"))?;
        press(&mut guest, "esc", HOME)?;
        guest.key("2")?;
        guest.terminal()?;
        guest.require(&["storage probing disabled", "[kernel] shutdown requested"], &["KERNEL PANIC"])?;
    }
    {
        let machine = Machine { extra: NETWORK, ..LAB };
        let mut guest = Guest::start(&machine, &image, &format!("setup-{stamp}-install"))?;
        guest.wait(HOME, 0)?;
        press(&mut guest, "4", "D test DHCP")?;
        press(&mut guest, "d", "DHCP confirmed: 10.0.2.15")?;
        guest.require(&["A DHCP lease does not verify internet access"], KERNEL)?;
        guest.capture(&format!("setup-{stamp}-dhcp"))?;
        guest.set_link("setupnic", false)?;
        press(&mut guest, "d", "DHCP: cable/link disconnected.")?;
        guest.require(&[], KERNEL)?;
        guest.capture(&format!("setup-{stamp}-link-down"))?;
        guest.set_link("setupnic", true)?;
        press(&mut guest, "esc", HOME)?;
        let from = press(&mut guest, "5", TARGETS)?;
        let text = guest.text()?;
        let page = text.get(from..).ok_or("installer log offset is invalid")?;
        if !sole_target(page) {
            return Err("installer did not offer exactly the synthetic target ESP".into());
        }
        guest.require(&[], KERNEL)?;
        guest.capture(&format!("setup-{stamp}-targets"))?;
        press(&mut guest, "ret", CONFIRM)?;
        guest.capture(&format!("setup-{stamp}-confirmation"))?;
        press(&mut guest, "esc", TARGETS)?;
        if fs::read(&target).map_err(|e| e.to_string())? != original {
            return Err("canceling installation modified the target disk".into());
        }
        guest.require(&[], &["Installation complete:"])?;
        press(&mut guest, "ret", CONFIRM)?;
        press(&mut guest, "i", "Installation complete: 4 files")?;
        guest.require(&[], KERNEL)?;
        guest.capture(&format!("setup-{stamp}-installed"))?;
        disk::verify(&target, &expected, &original)?;
        let installed = fs::read(&target).map_err(|e| e.to_string())?;
        press(&mut guest, "esc", TARGETS)?;
        press(&mut guest, "esc", HOME)?;
        press(&mut guest, "5", TARGETS)?;
        press(&mut guest, "ret", CONFIRM)?;
        press(&mut guest, "i", "existing files are protected.")?;
        guest.require(&["Installation did not complete."], KERNEL)?;
        guest.capture(&format!("setup-{stamp}-conflict"))?;
        if fs::read(&target).map_err(|e| e.to_string())? != installed {
            return Err("refusing an existing installation modified the target disk".into());
        }
        press(&mut guest, "esc", TARGETS)?;
        press(&mut guest, "esc", HOME)?;
        guest.key("2")?;
        guest.terminal()?;
        guest.require(&["storage probing disabled", "[kernel] shutdown requested"], &["KERNEL PANIC"])?;
    }
    disk::verify(&target, &expected, &original)?;
    if disk::files(&image, ESP_SIZE)? != expected {
        return Err("setup flow modified installer source files".into());
    }
    {
        let machine = Machine { name: "installed-target", block_device: "", extra: &["-net", "none"], ..LAB };
        let mut guest = Guest::start(&machine, &target, &format!("setup-{stamp}-destination-boot"))?;
        guest.wait(HOME, 0)?;
        guest.require(&["spaceboot 0.1.0", "Recovery terminal"], KERNEL)?;
        guest.capture(&format!("setup-{stamp}-destination-menu"))?;
        guest.key("2")?;
        guest.terminal()?;
        guest.require(&["storage probing disabled", "[kernel] shutdown requested"], &["KERNEL PANIC"])?;
    }
    disk::verify(&target, &expected, &original)?;
    println!(
        "PASS setup: DHCP ACK, disconnected link, absent NIC, source exclusion, cancel, copy, conflict protection, destination UEFI boot"
    );
    println!("== setup evidence: build/logs/boot/setup-{stamp}-*.log and *.png");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn target_count_ignores_console_repaint_but_rejects_other_destinations() {
        let row = format!("> EFI partition {}", super::disk::TARGET_GUID);
        let page = format!("\x1b[09;03H{row}\x1b[10;03HProtected.Install boot files\n{row}\nEnter review");
        assert!(super::sole_target(&page));
        let extra = format!("{page}\n  EFI partition 53504143-454f-4000-8000-000000000001\n");
        assert!(!super::sole_target(&extra));
        assert!(!super::sole_target("\n> EFI partition 53504143-454f-4000-8000-000000000001\n"));
    }
}
