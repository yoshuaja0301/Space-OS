pub const MIN_MEMORY_MIB: u64 = 128;

pub const fn cpu_check(extended_edx: u32) -> Result<(), &'static str> {
    if extended_edx & (1 << 20) == 0 {
        return Err("CPU: NX is unavailable. Enable Execute Disable in firmware.");
    }
    if extended_edx & (1 << 11) == 0 {
        return Err("CPU: SYSCALL/SYSRET is unavailable on this processor.");
    }
    Ok(())
}

pub fn option<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace().find_map(|token| {
        let (name, value) = token.split_once('=')?;
        (name == key).then_some(value)
    })
}

pub fn startup_line(configured: &str, recovery: bool) -> Result<String, &'static str> {
    let mut line = String::new();
    line.try_reserve(configured.len().saturating_add(48)).map_err(|_| "Cannot allocate boot options.")?;
    for token in configured.split_whitespace() {
        let name = token.split_once('=').map(|(name, _)| name);
        if name == Some("bootmenu") || (recovery && matches!(name, Some("init" | "storage"))) {
            continue;
        }
        line.push_str(token);
        line.push(' ');
    }
    if recovery {
        line.push_str("init=bin/spaceterm storage=off");
    } else if option(configured, "init").is_none() {
        line.push_str("init=bin/spaceterm");
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_required_cpu_features() {
        assert!(cpu_check(0).is_err());
        assert!(cpu_check(1 << 11).is_err());
        assert!(cpu_check(1 << 20).is_err());
        assert!(cpu_check((1 << 20) | (1 << 11)).is_ok());
    }

    #[test]
    fn boot_options_require_complete_tokens() {
        assert_eq!(option("init=bin/init bootmenu=on", "bootmenu"), Some("on"));
        assert_eq!(option("notbootmenu=on bootmenu=off", "bootmenu"), Some("off"));
        assert_eq!(option("bootmenu", "bootmenu"), None);
    }

    #[test]
    fn normal_boot_preserves_configured_storage_and_init() {
        let line = startup_line("bootmenu=on storage=off init=bin/hello test=value", false).unwrap();
        assert_eq!(option(&line, "storage"), Some("off"));
        assert_eq!(option(&line, "init"), Some("bin/hello"));
        assert_eq!(option(&line, "test"), Some("value"));
        assert_eq!(option(&line, "bootmenu"), None);
    }

    #[test]
    fn recovery_overrides_all_storage_and_init_options() {
        let line = startup_line("storage=on init=bin/init storage=on test=value", true).unwrap();
        assert_eq!(option(&line, "storage"), Some("off"));
        assert_eq!(option(&line, "init"), Some("bin/spaceterm"));
        assert_eq!(option(&line, "test"), Some("value"));
        assert_eq!(line.matches("storage=").count(), 1);
    }
}
extern crate alloc;
use alloc::string::String;
