//! Per-process capability table (ADR-0004).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use x86_64::structures::paging::PhysFrame;

use spaceabi::error::Error;
use spaceabi::handle::{Handle, MAX_HANDLES, kind};

use super::Process;
use crate::fs::fat32::FileNode;
use crate::ipc::channel::Endpoint;

/// An open file on a mounted volume.
pub struct OpenFile {
    /// Behind a lock because writing changes it: a file that grew must report its
    /// new size to the next reader through the same handle.
    pub node: crate::sync::SpinLock<FileNode>,
}

/// A block of physical frames several processes can map (the buffers of the
/// Compute ABI). The frames belong to the object: mappings come and go, the frames
/// are freed when the last handle to the object is closed.
pub struct MemoryObject {
    pub frames: Vec<PhysFrame>,
    pub len: u64,
    /// Process charged for the frames, so its quota is refunded when the object dies.
    pub owner: Weak<Process>,
}

impl Drop for MemoryObject {
    fn drop(&mut self) {
        if let Some(p) = self.owner.upgrade() {
            // `try_lock`: an owner that is tearing down already holds its address
            // space lock and is dropping the whole quota anyway.
            if let Some(mut space) = p.space.try_lock() {
                space.used_pages = space.used_pages.saturating_sub(self.frames.len());
            }
        }
        for f in self.frames.drain(..) {
            crate::mm::frame::free(f);
        }
    }
}

pub enum Object {
    Channel(Arc<Endpoint>),
    Process(Arc<Process>),
    File(Arc<OpenFile>),
    Memory(Arc<MemoryObject>),
    Root,
}

impl Object {
    pub fn kind(&self) -> u32 {
        match self {
            Object::Channel(_) => kind::CHANNEL,
            Object::Process(_) => kind::PROCESS,
            Object::File(_) => kind::FILE,
            Object::Memory(_) => kind::MEMORY,
            Object::Root => kind::ROOT,
        }
    }

    pub fn clone_ref(&self) -> Object {
        match self {
            Object::Channel(e) => Object::Channel(e.clone()),
            Object::Process(p) => Object::Process(p.clone()),
            Object::File(f) => Object::File(f.clone()),
            Object::Memory(m) => Object::Memory(m.clone()),
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
        self.slots.try_reserve(1).map_err(|_| Error::NoMemory)?;
        self.slots.push(Some(entry));
        Ok((self.slots.len() - 1) as Handle)
    }

    pub fn insert_at(&mut self, index: usize, entry: HandleEntry) {
        while self.slots.len() <= index {
            self.slots.push(None);
        }
        self.slots[index] = Some(entry);
    }

    /// True when `insert` can still succeed (a free slot, or room to grow).
    pub fn has_free_slot(&self) -> bool {
        self.slots.len() < MAX_HANDLES || self.slots.iter().any(Option::is_none)
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
