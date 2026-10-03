use alloc::boxed::Box;
use core::mem;
use uefi::Status;
use uefi::proto::network::snp::{ReceiveFlags, SimpleNetwork};

pub(super) struct Session<'a> {
    pub(super) network: &'a SimpleNetwork,
    pub(super) started: bool,
    pub(super) initialized: bool,
    pub(super) tx: [Option<Box<[u8; 342]>>; 2],
    pub(super) pending: [bool; 2],
    pub(super) added_filters: ReceiveFlags,
}

impl Session<'_> {
    pub(super) fn restore(&mut self) -> Result<(), Status> {
        let filter_status = if self.added_filters.is_empty() {
            None
        } else {
            let result = self.network.receive_filters(ReceiveFlags::empty(), self.added_filters, false, None);
            self.added_filters = ReceiveFlags::empty();
            result.err().map(|e| e.status())
        };
        if self.initialized {
            if let Err(error) = self.network.shutdown() {
                for buffer in &mut self.tx {
                    if let Some(buffer) = buffer.take() {
                        mem::forget(buffer);
                    }
                }
                return Err(error.status());
            }
            self.initialized = false;
            self.pending = [false; 2];
        }
        for index in 0..2 {
            if self.pending[index] {
                if let Some(buffer) = self.tx[index].take() {
                    mem::forget(buffer);
                }
                self.pending[index] = false;
            }
        }
        if self.started {
            self.network.stop().map_err(|e| e.status())?;
            self.started = false;
        }
        match filter_status {
            Some(status) => Err(status),
            None => Ok(()),
        }
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
