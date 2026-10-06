//! Client side of the Task Service protocol (see `spaceabi::task`, ADR-0036).
//!
//! Whoever starts `bin/spacetask` holds the operator's channel ([`TaskService`]) and
//! hands out sessions ([`Tasks`]), each fixed to the actor it was opened for. A
//! session is only a way in: closing it leaves every task where it was.

use alloc::string::String;

use spaceabi::error::Error;
use spaceabi::handle::Handle;
use spaceabi::task::{TaskReply, TaskRequest, req};

use crate::sys;

/// A channel to the Task Service: the operator's, or a session's.
pub struct Tasks {
    chan: Option<Handle>,
}

impl Tasks {
    /// A session whose channel came from elsewhere (a worker is handed one).
    pub fn from_channel(chan: Handle) -> Tasks {
        Tasks { chan: Some(chan) }
    }

    /// Give up the channel without closing it, to hand it to someone else.
    pub fn into_channel(mut self) -> Handle {
        self.chan.take().expect("a session always has its channel until it is given away")
    }

    fn chan(&self) -> Handle {
        self.chan.expect("a session always has its channel until it is given away")
    }

    fn reply(&self) -> Result<TaskReply, Error> {
        let mut buf = [0u8; core::mem::size_of::<TaskReply>()];
        let (n, transferred) = sys::recv(self.chan(), &mut buf, false)?;
        if let Some(h) = transferred {
            sys::handle_close(h).ok();
        }
        TaskReply::from_bytes(&buf[..n]).ok_or(Error::MsgSize)
    }

    /// Send a request and read the answer; an answer that carries an error is
    /// returned as that error.
    pub fn call(&self, r: &TaskRequest) -> Result<TaskReply, Error> {
        sys::send(self.chan(), r.as_bytes(), None)?;
        let reply = self.reply()?;
        reply.result()?;
        Ok(reply)
    }

    /// A new task titled `title`, working in `workspace` (may be empty), part of
    /// task `parent` (0: none), allowed `attempts` attempts (0: the default).
    pub fn create(
        &self,
        title: &str,
        workspace: &str,
        parent: u32,
        attempts: u32,
    ) -> Result<TaskReply, Error> {
        let mut r = TaskRequest::for_task(req::CREATE, parent).with_text(title).with_scope(workspace);
        r.value = attempts;
        self.call(&r)
    }

    pub fn get(&self, id: u32) -> Result<TaskReply, Error> {
        self.call(&TaskRequest::for_task(req::GET, id))
    }

    /// The `index`-th task the service holds, oldest first; `NotFound` past the end.
    pub fn list(&self, index: u32) -> Result<TaskReply, Error> {
        let mut r = TaskRequest::new(req::LIST);
        r.value = index;
        self.call(&r)
    }

    pub fn move_to(&self, id: u32, to: u32, why: &str) -> Result<TaskReply, Error> {
        let mut r = TaskRequest::for_task(req::MOVE, id).with_text(why);
        r.to = to;
        self.call(&r)
    }

    /// Ask for work: the task that is running for the caller from now on.
    pub fn claim(&self) -> Result<TaskReply, Error> {
        self.call(&TaskRequest::new(req::CLAIM))
    }

    pub fn effect_begin(&self, id: u32, key: &str) -> Result<TaskReply, Error> {
        self.call(&TaskRequest::for_task(req::EFFECT_BEGIN, id).with_text(key))
    }

    pub fn effect_end(&self, id: u32, key: &str, happened: bool) -> Result<TaskReply, Error> {
        let mut r = TaskRequest::for_task(req::EFFECT_END, id).with_text(key);
        r.value = u32::from(happened);
        self.call(&r)
    }

    pub fn checkpoint(&self, id: u32, text: &str) -> Result<TaskReply, Error> {
        self.call(&TaskRequest::for_task(req::CHECKPOINT, id).with_text(text))
    }

    /// Stop task `id` (see `req::STOP`).
    pub fn stop(&self, id: u32) -> Result<TaskReply, Error> {
        self.call(&TaskRequest::for_task(req::STOP, id))
    }

    /// Stop everything: dispatch stops and every running task is asked to stop.
    pub fn stop_all(&self) -> Result<TaskReply, Error> {
        self.call(&TaskRequest::for_task(req::STOP, 0))
    }

    pub fn resume(&self) -> Result<TaskReply, Error> {
        self.call(&TaskRequest::new(req::RESUME))
    }

    /// Whether the worker running task `id` should stop, and the task as it stands.
    pub fn check(&self, id: u32) -> Result<(bool, TaskReply), Error> {
        let r = self.call(&TaskRequest::for_task(req::CHECK, id))?;
        Ok((r.value != 0, r))
    }

    /// Settle task `id`'s open effect: it `happened` or not, as found out by `how`.
    pub fn reconcile(&self, id: u32, happened: bool, how: &str) -> Result<TaskReply, Error> {
        let mut r = TaskRequest::for_task(req::RECONCILE, id).with_text(how);
        r.value = u32::from(happened);
        self.call(&r)
    }

    /// The `n`-th record the journal holds for task `id`.
    pub fn history(&self, id: u32, n: u32) -> Result<TaskReply, Error> {
        let mut r = TaskRequest::for_task(req::HISTORY, id);
        r.value = n;
        self.call(&r)
    }

    /// One of the task's texts (`spaceabi::task::text`).
    pub fn text(&self, id: u32, which: u32) -> Result<String, Error> {
        let mut r = TaskRequest::for_task(req::TEXT, id);
        r.value = which;
        Ok(String::from(self.call(&r)?.reason()))
    }

    pub fn stats(&self) -> Result<TaskReply, Error> {
        self.call(&TaskRequest::new(req::STATS))
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        if let Some(h) = self.chan.take() {
            sys::handle_close(h).ok();
        }
    }
}

/// The operator's hold on a running `bin/spacetask`.
pub struct TaskService {
    ops: Option<Tasks>,
    process: Handle,
    ended: bool,
}

impl TaskService {
    /// Start `bin/spacetask` (spawned with `spawn_cap`) and hand it `fs`, the file
    /// capability (`FS | FS_WRITE`) its journal is kept with. The answer is the
    /// journal's `STATS` as the service found it.
    pub fn start(spawn_cap: Handle, fs: Handle, quota: u64) -> Result<(TaskService, TaskReply), Error> {
        let (mine, theirs) = sys::channel_create().inspect_err(|_| {
            sys::handle_close(fs).ok();
        })?;
        let process = sys::spawn(spawn_cap, "bin/spacetask", quota, Some(theirs)).inspect_err(|_| {
            for h in [fs, mine, theirs] {
                sys::handle_close(h).ok();
            }
        })?;
        let svc = TaskService { ops: Some(Tasks::from_channel(mine)), process, ended: false };
        let hello = TaskRequest::new(req::HELLO);
        if let Err(e) = sys::send(mine, hello.as_bytes(), Some(fs)) {
            sys::handle_close(fs).ok();
            return Err(e);
        }
        // From here `fs` belongs to the service, whatever it answers. A refusal ends
        // the service again (`Drop`).
        let stats = svc.ops().reply().and_then(|r| r.result().map(|()| r))?;
        Ok((svc, stats))
    }

    /// The operator's own channel: requests on it act for the user.
    pub fn ops(&self) -> &Tasks {
        self.ops.as_ref().expect("the operator's channel lives as long as the service")
    }

    /// A session for a client acting as `actor` (`USER` or `AGENT`).
    pub fn session(&self, actor: u32) -> Result<Tasks, Error> {
        let (mine, theirs) = sys::channel_create()?;
        let mut r = TaskRequest::new(req::SESSION);
        r.actor = actor;
        let ops = self.ops();
        if let Err(e) = sys::send(ops.chan(), r.as_bytes(), Some(theirs)) {
            sys::handle_close(theirs).ok();
            sys::handle_close(mine).ok();
            return Err(e);
        }
        let session = Tasks::from_channel(mine);
        ops.reply()?.result()?;
        Ok(session)
    }

    /// Compact the journal into its other file now.
    pub fn compact(&self) -> Result<TaskReply, Error> {
        self.ops().call(&TaskRequest::new(req::COMPACT))
    }

    pub fn process(&self) -> Handle {
        self.process
    }

    /// Ask the service to exit, and make sure it has.
    pub fn quit(mut self) {
        let _ = self.ops().call(&TaskRequest::new(req::QUIT));
        self.end();
    }

    /// End the service without a word, the way a crash or a power cut would.
    pub fn crash(mut self) {
        sys::kill(self.process).ok();
        self.end();
    }

    fn end(&mut self) {
        if self.ended {
            return;
        }
        self.ended = true;
        // Without its operator the service has nothing left to serve, and exits.
        self.ops.take();
        let until = sys::ticks_ms() + 1000;
        while sys::wait_nonblocking(self.process).is_err() && sys::ticks_ms() < until {
            sys::sleep_ms(5);
        }
        sys::kill(self.process).ok();
        sys::wait(self.process).ok();
        sys::handle_close(self.process).ok();
    }
}

impl Drop for TaskService {
    fn drop(&mut self) {
        self.end();
    }
}
