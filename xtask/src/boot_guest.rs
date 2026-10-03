use std::sync::atomic::AtomicBool;

use super::*;

pub(super) struct Guest {
    child: std::process::Child,
    monitor: std::os::unix::net::UnixStream,
    socket: PathBuf,
    log: PathBuf,
    artifact: PathBuf,
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Err(error) = fs::copy(&self.log, &self.artifact) {
            eprintln!("cannot preserve boot log: {error}");
        }
        let _ = fs::remove_file(&self.log);
        let _ = fs::remove_file(&self.socket);
    }
}

impl Guest {
    pub(super) fn text(&self) -> Result<String, String> {
        fs::read_to_string(&self.log).map_err(|e| e.to_string())
    }

    pub(super) fn set_link(&mut self, id: &str, up: bool) -> Result<(), String> {
        let state = if up { "on" } else { "off" };
        monitor_cmd(&mut self.monitor, &format!("set_link {id} {state}"), Duration::from_millis(100))?;
        Ok(())
    }

    pub(super) fn start(machine: &Machine, image: &Path, name: &str) -> Result<Self, String> {
        let dir = root().join("build/logs/boot");
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let (code, vars) = find_firmware()?;
        let copy = dir.join(format!("{name}-vars.fd"));
        fs::copy(vars, &copy).map_err(|e| e.to_string())?;
        let artifact = dir.join(format!("{name}.log"));
        fs::write(&artifact, "").map_err(|e| e.to_string())?;
        let log = std::env::temp_dir().join(format!("spaceos-boot-{}.log", std::process::id()));
        fs::write(&log, "").map_err(|e| e.to_string())?;
        let socket = std::env::temp_dir().join(format!("spaceos-boot-{}.sock", std::process::id()));
        let _ = fs::remove_file(&socket);
        let args = qemu_args(
            machine,
            image,
            &root().join("build/data.img"),
            &copy,
            &code,
            false,
            Some(&log),
            Typing::Keyboard,
            Some(&socket),
        )?;
        let mut child = Command::new(qemu_bin())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| e.to_string())?;
        let monitor = match connect_monitor(&socket, &AtomicBool::new(false)) {
            Ok(monitor) => monitor,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&socket);
                return Err(error);
            }
        };
        Ok(Self { child, monitor, socket, log, artifact })
    }

    pub(super) fn wait(&mut self, marker: &str, from: usize) -> Result<(), String> {
        let start = Instant::now();
        loop {
            let text = fs::read_to_string(&self.log).map_err(|e| e.to_string())?;
            if text.get(from..).is_some_and(|new| new.contains(marker)) {
                return Ok(());
            }
            if self.child.try_wait().map_err(|e| e.to_string())?.is_some() {
                return Err(format!("guest exited before {marker:?}"));
            }
            if start.elapsed() > BOOT_TIMEOUT {
                return Err(format!("timeout waiting for {marker:?}"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub(super) fn key(&mut self, key: &str) -> Result<usize, String> {
        let position = fs::metadata(&self.log).map_err(|e| e.to_string())?.len() as usize;
        monitor_cmd(&mut self.monitor, &format!("sendkey {key}"), Duration::from_millis(100))?;
        Ok(position)
    }

    pub(super) fn press(&mut self, key: &str, marker: &str) -> Result<(), String> {
        let from = self.key(key)?;
        self.wait(marker, from)
    }

    pub(super) fn capture(&mut self, name: &str) -> Result<(), String> {
        // Firmware serial output can precede completion of its GOP repaint.
        std::thread::sleep(Duration::from_secs(2));
        let path = root().join(format!("build/logs/boot/{name}.png"));
        if path.exists() {
            fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        let command = format!("screendump \"{}\" -f png", path.display());
        let output = monitor_cmd(&mut self.monitor, &command, Duration::from_millis(500))?;
        if !path.exists() {
            return Err(format!("screenshot failed: {output}"));
        }
        Ok(())
    }

    pub(super) fn terminal(&mut self) -> Result<(), String> {
        self.wait(TERMINAL_READY, 0)?;
        let name = self.artifact.file_stem().ok_or("missing artifact name")?.to_string_lossy().into_owned();
        if !self.text()?.contains("GOP false") {
            self.capture(&format!("{name}-terminal"))?;
        }
        for line in ["help", "status", "quit"] {
            for ch in line.chars() {
                monitor_cmd(&mut self.monitor, &format!("sendkey {}", key_name(ch)?), KEY_DRAIN)?;
                std::thread::sleep(Duration::from_millis(30));
            }
            monitor_cmd(&mut self.monitor, "sendkey ret", KEY_DRAIN)?;
            std::thread::sleep(Duration::from_millis(500));
        }
        self.wait("[shell] worker idle (-), last exit code 0, commands served 3", 0)?;
        self.wait("[shell] closing the session", 0)?;
        self.wait("[kernel] shutdown requested", 0)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
                if status.code() == Some(EXIT_SUCCESS) {
                    return Ok(());
                }
                return Err(format!("terminal exited with {status}"));
            }
            if Instant::now() > deadline {
                return Err("shutdown did not exit QEMU".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub(super) fn require(&self, markers: &[&str], forbidden: &[&str]) -> Result<(), String> {
        let text = fs::read_to_string(&self.log).map_err(|e| e.to_string())?;
        for marker in markers {
            if !text.contains(marker) {
                return Err(format!("missing marker {marker:?}"));
            }
        }
        for marker in forbidden {
            if text.contains(marker) {
                return Err(format!("unexpected marker {marker:?}"));
            }
        }
        fs::copy(&self.log, &self.artifact).map_err(|e| format!("preserve boot evidence: {e}"))?;
        Ok(())
    }
}
