//! Per-process capability table (ADR-0004, ADR-0033: a handle names a slot and
//! the generation of it, so a closed handle stays dead after its slot is reused).

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

/// What a memory object's frames are.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    /// RAM from the frame allocator, owned by the object.
    Ram,
    /// The framebuffer, leased to a display server (`SYS_DISPLAY_OPEN`). Device
    /// memory: never handed to the frame allocator.
    Display,
}

/// A block of physical frames several processes can map (the buffers of the
/// Compute ABI, or the screen). The frames belong to the object: mappings come and
/// go, the frames are freed when the last handle and the last mapping are gone.
pub struct MemoryObject {
    pub frames: Vec<PhysFrame>,
    pub len: u64,
    /// Process charged for the frames, so its quota is refunded when the object dies.
    pub owner: Weak<Process>,
    pub kind: MemoryKind,
}

impl Drop for MemoryObject {
    fn drop(&mut self) {
        if self.kind == MemoryKind::Display {
            // The screen's pages are not the allocator's to take back.
            self.frames.clear();
            crate::fb::release();
            return;
        }
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
    /// The lease on the network device; the device is released with the last one.
    Nic(Arc<crate::dev::nic::NicLease>),
    Root,
}

impl Object {
    pub fn kind(&self) -> u32 {
        match self {
            Object::Channel(_) => kind::CHANNEL,
            Object::Process(_) => kind::PROCESS,
            Object::File(_) => kind::FILE,
            Object::Memory(_) => kind::MEMORY,
            Object::Nic(_) => kind::NIC,
            Object::Root => kind::ROOT,
        }
    }

    pub fn clone_ref(&self) -> Object {
        match self {
            Object::Channel(e) => Object::Channel(e.clone()),
            Object::Process(p) => Object::Process(p.clone()),
            Object::File(f) => Object::File(f.clone()),
            Object::Memory(m) => Object::Memory(m.clone()),
            Object::Nic(n) => Object::Nic(n.clone()),
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

/// One slot of a process's table: what it holds now, and which generation of the
/// slot that is.
struct Slot {
    generation: u32,
    entry: Option<HandleEntry>,
}

/// A handle names a slot and the generation the slot was in when the handle was
/// made (PRD v0.2 K04, §8.3 "harus tahan reuse yang salah"): the index in the low
/// bits, the generation above it. Emptying a slot moves it to the next generation,
/// so a handle that was closed, or moved to another process, stays dead after its
/// slot holds something else -- it names a generation that no longer exists.
const INDEX_BITS: u32 = 8;
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;
/// Generations cycle below the one that would turn `handle::INVALID` (all ones) into
/// a handle someone could be given.
const GENERATIONS: u32 = u32::MAX >> INDEX_BITS;

const _: () = assert!(MAX_HANDLES == 1 << INDEX_BITS);

fn handle_of(index: usize, generation: u32) -> Handle {
    (generation << INDEX_BITS) | index as u32
}

pub struct HandleTable {
    slots: Vec<Slot>,
}

impl HandleTable {
    pub fn new() -> Self {
        HandleTable { slots: Vec::new() }
    }

    pub fn insert(&mut self, entry: HandleEntry) -> Result<Handle, Error> {
        if let Some(i) = self.slots.iter().position(|s| s.entry.is_none()) {
            self.slots[i].entry = Some(entry);
            return Ok(handle_of(i, self.slots[i].generation));
        }
        if self.slots.len() >= MAX_HANDLES {
            return Err(Error::TooManyHandles);
        }
        self.slots.try_reserve(1).map_err(|_| Error::NoMemory)?;
        self.slots.push(Slot { generation: 0, entry: Some(entry) });
        Ok(handle_of(self.slots.len() - 1, 0))
    }

    /// Put `entry` in slot `index` of a new table (the bootstrap handle): its
    /// generation is 0, so the handle is the index itself.
    pub fn insert_at(&mut self, index: usize, entry: HandleEntry) {
        while self.slots.len() <= index {
            self.slots.push(Slot { generation: 0, entry: None });
        }
        self.slots[index].entry = Some(entry);
    }

    /// True when `insert` can still succeed (a free slot, or room to grow).
    pub fn has_free_slot(&self) -> bool {
        self.slots.len() < MAX_HANDLES || self.slots.iter().any(|s| s.entry.is_none())
    }

    /// The slot `h` names, if it is still in the generation `h` was made in.
    fn slot(&self, h: Handle) -> Option<usize> {
        let index = (h & INDEX_MASK) as usize;
        let s = self.slots.get(index)?;
        (s.generation == h >> INDEX_BITS && s.entry.is_some()).then_some(index)
    }

    pub fn get(&self, h: Handle) -> Result<&HandleEntry, Error> {
        let i = self.slot(h).ok_or(Error::BadHandle)?;
        self.slots[i].entry.as_ref().ok_or(Error::BadHandle)
    }

    /// Empty the slot `h` names; the slot moves on to its next generation.
    pub fn take(&mut self, h: Handle) -> Result<HandleEntry, Error> {
        let i = self.slot(h).ok_or(Error::BadHandle)?;
        let s = &mut self.slots[i];
        s.generation = (s.generation + 1) % GENERATIONS;
        s.entry.take().ok_or(Error::BadHandle)
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
