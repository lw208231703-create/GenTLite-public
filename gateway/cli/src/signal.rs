//! Shutdown flag. Unix: a libc signal handler sets an AtomicBool on
//! SIGINT/SIGTERM; the main loop polls it (the classic self-pipe-free
//! pattern — the handler does nothing but set the flag, so it is
//! async-signal-safe). Windows: no console signal handling in v1 — Ctrl+C
//! terminates the process by the default handler, and the polite path is a
//! plain infinite wait (design note, v1 limitations).

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

pub fn install() {
    install_impl();
}

pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}

#[cfg(unix)]
fn install_impl() {
    extern "C" fn on_signal(_sig: libc::c_int) {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }

    unsafe {
        // `sighandler_t` is `size_t`, so the handler has to cross the FFI
        // boundary as an address. Go through `*const ()` instead of casting
        // the function item straight to an integer: the direct cast is what
        // `function_casts_as_integer` warns about, and it also loses the
        // fact that this is a code pointer.
        let handler = on_signal as *const () as libc::sighandler_t;
        // SIGINT (Ctrl+C) and SIGTERM (service managers / docker stop).
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

#[cfg(windows)]
fn install_impl() {
    // No signal installation: the default Ctrl+C handler terminates the
    // process, which is the v1 shutdown path on Windows.
}
