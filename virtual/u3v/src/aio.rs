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

const IOCB_CMD_PWRITE: u16 = 1;
const AIO_DEPTH: usize = 4;
const AIO_EVENTS: usize = 16;

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
    ctx: u64,
    pending: VecDeque<Pending>,
    free: Vec<Vec<u8>>,
    next_id: u64,
}

impl AioWriter {
    fn init_ctx() -> io::Result<u64> {
        let mut ctx: u64 = 0;
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_setup,
                AIO_EVENTS as libc::c_long,
                &mut ctx as *mut u64,
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ctx)
    }

    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            ctx: Self::init_ctx()?,
            pending: VecDeque::with_capacity(AIO_DEPTH),
            free: Vec::with_capacity(AIO_DEPTH),
            next_id: 1,
        })
    }

    /// Drop all in-flight requests and create a fresh context. Called when
    /// the U3V stream is disabled, so a later stream start cannot observe
    /// completions from the previous session.
    pub(crate) fn reset(&mut self) {
        unsafe {
            let _ = libc::syscall(libc::SYS_io_destroy, self.ctx as libc::c_long);
        }
        self.ctx = 0;
        self.pending.clear();
        self.free.clear();
        self.next_id = 1;
        match Self::new() {
            Ok(new) => {
                self.ctx = new.ctx;
                // `new` has no other owned resources; prevent its Drop
                // from destroying the context we just adopted.
                std::mem::forget(new);
            }
            Err(e) => {
                eprintln!("vgige: U3V stream AIO reset failed: {e}");
            }
        }
    }

    /// Queue one write. Blocks only when the in-flight window is full.
    pub(crate) fn send(&mut self, fd: RawFd, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
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
        iocb.aio_offset = 0;

        let mut ptr: *mut libc::iocb = &mut iocb;
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_submit,
                self.ctx as libc::c_long,
                1 as libc::c_long,
                &mut ptr as *mut *mut libc::iocb,
            )
        };
        if ret != 1 {
            self.free.push(buf);
            return Err(io::Error::last_os_error());
        }

        self.pending.push_back(Pending { id, buf, iocb });
        Ok(())
    }

    fn reap_one(&mut self) -> io::Result<()> {
        let mut ev: IoEvent = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_getevents,
                self.ctx as libc::c_long,
                1 as libc::c_long,
                1 as libc::c_long,
                &mut ev as *mut IoEvent,
                std::ptr::null_mut::<libc::timespec>(),
            )
        };
        if ret != 1 {
            return Err(io::Error::last_os_error());
        }

        let pos = self
            .pending
            .iter()
            .position(|p| p.id == ev.data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "AIO completion id not found"))?;
        let p = self
            .pending
            .remove(pos)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "AIO completion missing"))?;

        let expected = p.iocb.aio_nbytes as usize;
        self.free.push(p.buf);

        if ev.res2 != 0 {
            return Err(io::Error::from_raw_os_error(ev.res2 as i32));
        }
        if ev.res < 0 {
            return Err(io::Error::from_raw_os_error((-ev.res) as i32));
        }
        if ev.res as usize != expected {
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
        while !self.pending.is_empty() {
            if self.reap_one().is_err() {
                break;
            }
        }
        unsafe {
            let _ = libc::syscall(libc::SYS_io_destroy, self.ctx as libc::c_long);
        }
    }
}
