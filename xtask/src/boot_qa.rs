use super::*;

pub(super) use super::boot_guest::Guest;

pub fn run(release: bool) -> Result<(), String> {
    let built = build(release)?;
    let image = root().join("build/esp-boot-menu.img");
    make_image(&built, "bootmenu=on", &image)?;
    make_data_disk(&root().join("build/data.img"))?;
    {
        let mut guest = Guest::start(&LAB, &image, "recovery")?;
        guest.wait("Esc firmware", 0)?;
        guest.capture("menu")?;
        guest.press("down", "Esc firmware")?;
        guest.capture("menu-focus")?;
        guest.press("3", "Any key to return.")?;
        guest.capture("hardware")?;
        guest.press("esc", "Esc firmware")?;
        guest.key("ret")?;
        guest.terminal()?;
        guest.require(
            &["storage probing disabled", "vfs: no block device", "[shell] commands:"],
            &["KERNEL PANIC", "FAT32 mounted"],
        )?;
        println!("PASS boot menu: hardware, arrows, recovery, help/status/quit");
    }
    {
        let mut guest = Guest::start(&LAB, &image, "normal")?;
        guest.wait("Esc firmware", 0)?;
        guest.key("1")?;
        guest.terminal()?;
        guest.require(&["vfs: FAT32 mounted", "[shell] commands:"], &["KERNEL PANIC"])?;
        println!("PASS boot menu: normal terminal with storage");
    }
    for (name, extra, graphics) in [
        (
            "small-gop",
            &[
                "-vga",
                "none",
                "-device",
                "VGA,xres=640,yres=480",
                "-nic",
                "user,model=e1000,id=smallnet1",
                "-nic",
                "user,model=e1000,id=smallnet2",
                "-nic",
                "user,model=e1000,id=smallnet3",
            ][..],
            true,
        ),
        ("no-gop", &["-vga", "none"][..], false),
    ] {
        let machine = Machine { name, extra, ..LAB };
        let mut guest = Guest::start(&machine, &image, name)?;
        guest.wait("Esc firmware", 0)?;
        guest.require(
            &[
                "1  Start Space OS",
                "2  Recovery terminal",
                "3  Hardware information",
                "4  Network check",
                "5  Install boot files",
                "6  Restart",
                "7  Return to firmware",
                if graphics { "GOP resolution: 640x480" } else { "GOP false" },
            ],
            &["jumping to kernel", "KERNEL PANIC"],
        )?;
        if graphics {
            guest.capture(&format!("{name}-menu"))?;
        }
        for key in ["3", "pgdn", "pgup"] {
            guest.press(key, "Hardware and boot readiness: Page 1 / 1")?;
        }
        if graphics {
            guest.capture(&format!("{name}-hardware"))?;
        }
        guest.press("esc", "Esc firmware")?;
        guest.press("4", if graphics { "Network check: Page 1 / 2" } else { "Network check: Page 1 / " })?;
        if graphics {
            guest.capture(&format!("{name}-network"))?;
            guest.press("pgdn", "Network check: Page 2 / 2")?;
            guest.capture(&format!("{name}-network-page2"))?;
            guest.press("pgup", "Network check: Page 1 / 2")?;
        }
        guest.press("esc", "Esc firmware")?;
        guest.key("2")?;
        guest.terminal()?;
        guest.require(&["storage probing disabled", "[shell] commands:"], &["KERNEL PANIC"])?;
        println!("PASS startup display: {name}, hardware navigation, network, recovery help/status/quit");
    }
    for (name, cpu, memory, reason) in [
        ("no-nx", "qemu64,-nx", "2G", "CPU: NX is unavailable"),
        ("no-syscall", "qemu64,-syscall", "2G", "CPU: SYSCALL/SYSRET is unavailable"),
        ("low-memory", "qemu64", "128M", "Memory: less than 128 MiB"),
    ] {
        let machine = Machine { name, cpu, memory, ..LAB };
        let mut guest = Guest::start(&machine, &image, name)?;
        guest.wait("Check hardware requirements", 0)?;
        guest.require(&[reason], &["jumping to kernel", "spacekernel 0.1.0"])?;
        guest.capture(name)?;
        guest.key("2")?;
        println!("PASS boot preflight: {name} refused before kernel entry");
    }
    println!("== boot menu and preflight scenarios passed");
    Ok(())
}
