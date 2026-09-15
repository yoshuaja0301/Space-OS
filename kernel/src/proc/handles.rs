//! Per-process capability table (ADR-0004).

use alloc::sync::Arc;
use alloc::vec::Vec;

use spaceabi::error::Error;
use spaceabi::handle::{Handle, MAX_HANDLES, kind};

use super::Process;
use crate::ipc::channel::Endpoint;

pub enum Object {
    Channel(Arc<Endpoint>),
    Process(Arc<Process>),
    Root,
}

impl Object {
    pub fn kind(&self) -> u32 {
        match self {
            Object::Channel(_) => kind::CHANNEL,
            Object::Process(_) => kind::PROCESS,
            Object::Root => kind::ROOT,
        }
    }

    pub fn clone_ref(&self) -> Object {
        match self {
            Object::Channel(e) => Object::Channel(e.clone()),
            Object::Process(p) => Object::Process(p.clone()),
            Object::Root => Object::Root,
        }
    }
}

pub struct HandleEntry {
    pub object: Object,
    pub rights: u32,
}

impl HandleEntry {
    pub fn has(&self, r: u32) -> bool {
        self.rights & r == r
    }
}

pub struct HandleTable {
    slots: Vec<Option<HandleEntry>>,
}

impl HandleTable {
    pub fn new() -> Self {
        HandleTable { slots: Vec::new() }
    }

    pub fn insert(&mut self, entry: HandleEntry) -> Result<Handle, Error> {
        if let Some(i) = self.slots.iter().position(Option::is_none) {
            self.slots[i] = Some(entry);
            return Ok(i as Handle);
        }
        if self.slots.len() >= MAX_HANDLES {
            return Err(Error::TooManyHandles);
        }
        self.slots.push(Some(entry));
        Ok((self.slots.len() - 1) as Handle)
    }

    pub fn insert_at(&mut self, index: usize, entry: HandleEntry) {
        while self.slots.len() <= index {
            self.slots.push(None);
        }
        self.slots[index] = Some(entry);
    }

    pub fn get(&self, h: Handle) -> Result<&HandleEntry, Error> {
        self.slots.get(h as usize).and_then(Option::as_ref).ok_or(Error::BadHandle)
    }

    pub fn take(&mut self, h: Handle) -> Result<HandleEntry, Error> {
        self.slots.get_mut(h as usize).and_then(Option::take).ok_or(Error::BadHandle)
    }

    pub fn clear(&mut self) {
        self.slots.clear();
    }
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}
