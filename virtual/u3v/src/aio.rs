//! Native Linux AIO writes for the U3V FunctionFS streaming endpoint.
//!
//! The default `FileTransfers` path is a synchronous `write_all`: the
//! emitter waits for each 512 KiB bulk transfer before preparing the next
//! one. On this board that leaves a ~0.3 ms user-space gap between
//! requests and caps the stream at roughly 250 MB/s. FunctionFS supports
//! native AIO for endpoint writes (non-sync `kiocb` in
//! `ffs_epfile_write_iter`), so keep several 512 KiB writes in flight and
//! only block when the pipeline is full.

use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

const IOCB_CMD_PWRITE: u16 = 1;
pub(crate) const AIO_DEPTH: usize = 4;
const AIO_EVENTS: usize = 16;
/// A completed 512 KiB write takes on the order of a millisecond on the
/// board. Two seconds means "the endpoint stopped draining", not "the host
/// is a little slow".
const AIO_COMPLETION_TIMEOUT: Duration = Duration::from_secs(2);
const IO_DESTROY_RETRIES: usize = 8;

const _: () = assert!(AIO_DEPTH <= AIO_EVENTS);

#[repr(C)]
#[derive(Clone, Copy)]
struct IoEvent {
    data: u64,
    obj: u64,
    res: i64,
    res2: i64,
}

struct Pending {
    id: u64,
    buf: Vec<u8>,
    /// Keep the iocb alive until the submission has been consumed; the
    /// kernel copies it during io_submit, but this also makes the buffer
    /// ownership obvious to the reader.
    iocb: libc::iocb,
}

pub(crate) struct AioWriter {
    ctx: libc::c_ulong,
    pending: VecDeque<Pending>,
    free: Vec<Vec<u8>>,
    next_id: u64,
    /// Set when a syscall leaves the context or a buffer's ownership
    /// uncertain. `send` fails closed; the next stream teardown retries
    /// `reset`, which can recreate the context.
    broken: bool,
}

impl AioWriter {
    fn init_ctx() -> io::Result<libc::c_ulong> {
        loop {
            let mut ctx: libc::c_ulong = 0;
            let ret = unsafe {
                libc::syscall(
                    libc::SYS_io_setup,
                    AIO_EVENTS as libc::c_long,
                    &mut ctx as *mut libc::c_ulong,
                )
            };
            if ret == 0 {
                return Ok(ctx);
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
    }

    /// `io_destroy` waits for (or cancels) every outstanding request. It
    /// may be interrupted by the SIGINT/SIGTERM handler, so retry on
    /// `EINTR`; callers must not free pending buffers unless this returns
    /// `Ok`.
    fn destroy_ctx(ctx: libc::c_ulong) -> io::Result<()> {
        if ctx == 0 {
            return Ok(());
        }
        let mut last = None;
        for _ in 0..IO_DESTROY_RETRIES {
            let ret = unsafe { libc::syscall(libc::SYS_io_destroy, ctx as libc::c_long) };
            if ret == 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                // An already-invalid context has no outstanding requests;
                // treating EINVAL as success avoids pinning a dead writer.
                if err.raw_os_error() == Some(libc::EINVAL) {
                    return Ok(());
                }
                return Err(err);
            }
            last = Some(err);
        }
        Err(last.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::Interrupted, "io_destroy interrupted")
        }))
    }

    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            ctx: Self::init_ctx()?,
            pending: VecDeque::with_capacity(AIO_DEPTH),
            free: Vec::with_capacity(AIO_DEPTH),
            next_id: 1,
            broken: false,
        })
    }

    /// Drop all in-flight requests and create a fresh context. Called
    /// before the first frame of a new U3V session, so a later stream
    /// cannot observe completions from the previous session.
    ///
    /// On failure the writer is marked broken and the pending buffers are
    /// kept alive: an interrupted `io_destroy` may still have the kernel
    /// writing into them. A later `reset` retries the destroy.
    pub(crate) fn reset(&mut self) -> io::Result<()> {
        if self.ctx != 0 {
            if let Err(e) = Self::destroy_ctx(self.ctx) {
                self.broken = true;
                return Err(e);
            }
            self.ctx = 0;
        }
        // io_destroy completed: the kernel no longer references these
        // buffers, so they can return to the free pool.
        for p in self.pending.drain(..) {
            self.free.push(p.buf);
        }
        self.next_id = 1;
        match Self::init_ctx() {
            Ok(ctx) => {
                self.ctx = ctx;
                self.broken = false;
                Ok(())
            }
            Err(e) => {
                self.ctx = 0;
                self.broken = true;
                Err(e)
            }
        }
    }

    /// Queue one write. Blocks only when the in-flight window is full.
    pub(crate) fn send(&mut self, fd: RawFd, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.broken || self.ctx == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "AIO writer is unavailable",
            ));
        }
        while self.pending.len() >= AIO_DEPTH {
            self.reap_one()?;
        }

        let mut buf = self.free.pop().unwrap_or_default();
        buf.clear();
        buf.extend_from_slice(bytes);

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        let mut iocb: libc::iocb = unsafe { std::mem::zeroed() };
        iocb.aio_data = id;
        iocb.aio_lio_opcode = IOCB_CMD_PWRITE;
        iocb.aio_fildes = fd as u32;
        iocb.aio_buf = buf.as_ptr() as u64;
        iocb.aio_nbytes = buf.len() as u64;
        // A FunctionFS endpoint is a stream: like a pipe, the offset is
        // ignored. Do not "fix" this to an append offset.
        iocb.aio_offset = 0;

        let mut ptr: *mut libc::iocb = &mut iocb;
        let ret = loop {
            let ret = unsafe {
                libc::syscall(
                    libc::SYS_io_submit,
                    self.ctx as libc::c_long,
                    1 as libc::c_long,
                    &mut ptr as *mut *mut libc::iocb,
                )
            };
            if ret < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
            }
            break ret;
        };
        if ret != 1 {
            self.free.push(buf);
            self.broken = true;
            if ret == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "io_submit accepted no requests",
                ));
            }
            return Err(io::Error::last_os_error());
        }

        self.pending.push_back(Pending { id, buf, iocb });
        Ok(())
    }

    fn reap_one(&mut self) -> io::Result<()> {
        let mut timeout = libc::timespec {
            tv_sec: AIO_COMPLETION_TIMEOUT.as_secs() as libc::time_t,
            tv_nsec: 0,
        };
        let mut ev: IoEvent = unsafe { std::mem::zeroed() };
        let ret = loop {
            let ret = unsafe {
                libc::syscall(
                    libc::SYS_io_getevents,
                    self.ctx as libc::c_long,
                    1 as libc::c_long,
                    1 as libc::c_long,
                    &mut ev as *mut IoEvent,
                    &mut timeout as *mut libc::timespec,
                )
            };
            if ret < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
            }
            break ret;
        };
        if ret == 0 {
            self.broken = true;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "AIO completion timeout",
            ));
        }
        if ret != 1 {
            self.broken = true;
            return Err(io::Error::last_os_error());
        }

        let pos = match self.pending.iter().position(|p| p.id == ev.data) {
            Some(pos) => pos,
            None => {
                self.broken = true;
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "AIO completion id not found",
                ));
            }
        };
        let p = self.pending.remove(pos).expect("AIO pending id is valid");
        let expected = p.iocb.aio_nbytes as usize;
        self.free.push(p.buf);

        if ev.res2 != 0 {
            self.broken = true;
            return Err(io::Error::from_raw_os_error(ev.res2 as i32));
        }
        if ev.res < 0 {
            self.broken = true;
            return Err(io::Error::from_raw_os_error((-ev.res) as i32));
        }
        if ev.res as usize != expected {
            self.broken = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("AIO short write: {} of {} bytes", ev.res, expected),
            ));
        }
        Ok(())
    }
}

impl Drop for AioWriter {
    fn drop(&mut self) {
        if self.ctx == 0 {
            return;
        }
        match Self::destroy_ctx(self.ctx) {
            Ok(()) => {
                self.ctx = 0;
                self.pending.clear();
            }
            Err(e) => {
                eprintln!("vgige: U3V AIO destroy failed during drop: {e}");
                // The kernel may still own the pending buffers. Leaking
                // them is safer than a use-after-free in the UDC driver.
                std::mem::forget(std::mem::take(&mut self.pending));
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Exercise the context lifecycle without FunctionFS: submit more
    /// writes than the pipeline depth, reset while some may still be in
    /// flight, then reuse the writer. This catches leaked/broken contexts
    /// and reset hangs on a regular Linux file.
    #[test]
    fn submit_reset_and_reuse() {
        let path = std::env::temp_dir().join(format!(
            "vgige-aio-{}-{}.bin",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .expect("create AIO test file");
        let fd = file.as_raw_fd();
        let mut writer = match AioWriter::new() {
            Ok(w) => w,
            Err(e) => {
                eprintln!("skip AIO lifecycle test: {e}");
                let _ = std::fs::remove_file(&path);
                return;
            }
        };
        for i in 0..(AIO_DEPTH * 2) {
            writer.send(fd, &[i as u8; 4096]).expect("AIO send");
        }
        writer.reset().expect("AIO reset");
        writer
            .send(fd, &[0xAA; 4096])
            .expect("AIO send after reset");
        drop(writer);
        assert_eq!(std::fs::metadata(&path).expect("stat").len(), 4096);
        let _ = std::fs::remove_file(&path);
    }
}
