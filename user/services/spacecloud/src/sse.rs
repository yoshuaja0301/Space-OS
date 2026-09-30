//! Server-sent events: `event:` and `data:` lines, an empty line ends an event.

use alloc::string::String;
use alloc::vec::Vec;

use libspace::spaceabi::cloud::fail;

use crate::Fail;

/// Longest line, and longest event, the adapter accepts.
const LINE_MAX: usize = 16 * 1024;
const EVENT_MAX: usize = 64 * 1024;

#[derive(Default)]
pub struct Parser {
    line: Vec<u8>,
    event: String,
    data: String,
}

impl Parser {
    /// Feed body bytes; `on` gets every event completed by them, as (name, data).
    pub fn feed(
        &mut self,
        bytes: &[u8],
        on: &mut dyn FnMut(&str, &str) -> Result<(), Fail>,
    ) -> Result<(), Fail> {
        for &b in bytes {
            if b != b'\n' {
                if self.line.len() >= LINE_MAX {
                    return Err(Fail::new(fail::PROTOCOL, "an event line is too long"));
                }
                self.line.push(b);
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            let line = core::mem::take(&mut self.line);
            if line.is_empty() {
                if !self.data.is_empty() || !self.event.is_empty() {
                    let name = if self.event.is_empty() { "message" } else { self.event.as_str() };
                    on(name, &self.data)?;
                }
                self.event.clear();
                self.data.clear();
                continue;
            }
            let text = String::from_utf8(line)
                .map_err(|_| Fail::new(fail::PROTOCOL, "an event that is not UTF-8"))?;
            let (field, value) = match text.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (text.as_str(), ""),
            };
            match field {
                "event" => {
                    self.event.clear();
                    self.event.push_str(value);
                }
                "data" => {
                    if !self.data.is_empty() {
                        self.data.push('\n');
                    }
                    if self.data.len() + value.len() > EVENT_MAX {
                        return Err(Fail::new(fail::PROTOCOL, "an event is too long"));
                    }
                    self.data.push_str(value);
                }
                // Comments (an empty field name), `id`, `retry`: nothing to do here.
                _ => {}
            }
        }
        Ok(())
    }
}
