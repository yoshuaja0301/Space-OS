//! System calls arrive as `svc #0` through the synchronous vector (`exceptions`):
//! the number in `x8`, arguments in `x0`-`x5`, the result back in `x0`.

pub use super::exceptions::TrapFrame as SyscallFrame;

impl SyscallFrame {
    pub fn number(&self) -> u64 {
        self.x[8]
    }

    pub fn args(&self) -> [u64; 6] {
        [self.x[0], self.x[1], self.x[2], self.x[3], self.x[4], self.x[5]]
    }

    /// Where the process resumes.
    pub fn return_address(&self) -> u64 {
        self.elr
    }
}

pub fn init() {
    println!("[kernel] syscalls: svc from EL0 (ABI v{})", spaceabi::ABI_VERSION);
}
