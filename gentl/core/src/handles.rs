//! Handle table: opaque `*mut c_void` handles over `Arc`-managed module objects.
//!
//! A handle is a boxed module reference tagged with the handle-table
//! generation. `GCCloseLib` calls [`invalidate_handles`], so every handle
//! created before the close is rejected afterwards even if the library is
//! re-initialized (GenTL 1.6 §6.3.1.1: GCCloseLib frees the library
//! resources and only one System instance may exist per process).
//!
//! Consumers only pass handles back; every use goes through `from_handle`,
//! which validates the generation and concrete type. Wrong-generation or
//! wrong-type pointers map to GC_ERR_INVALID_HANDLE instead of undefined
//! behavior.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::error::{GentlError, GentlResult};
use crate::module::Module;

/// Handle-table generation, bumped by [`invalidate_handles`].
static HANDLE_GENERATION: AtomicU64 = AtomicU64::new(0);

struct HandleBox {
    generation: u64,
    module: Arc<dyn Module>,
}

/// Invalidate every handle created before this call (GCCloseLib).
pub fn invalidate_handles() {
    HANDLE_GENERATION.fetch_add(1, Ordering::SeqCst);
}

fn generation() -> u64 {
    HANDLE_GENERATION.load(Ordering::SeqCst)
}

/// Box an `Arc<T>` into an opaque GenTL handle. Free with `free_handle`.
pub fn to_handle<T: Module>(obj: Arc<T>) -> *mut c_void {
    let boxed = Box::new(HandleBox {
        generation: generation(),
        module: obj,
    });
    Box::into_raw(boxed) as *mut c_void
}

/// Recover a typed `Arc<T>` from a handle, validating the concrete type.
///
/// # Safety
/// `h` must be null or a pointer returned by `to_handle` that has not been
/// freed. A foreign (non-GenTL) pointer may cause a crash on the downcast
/// read; GenTL consumers are expected to pass back what we gave them.
pub unsafe fn from_handle<T: Module>(h: *mut c_void) -> GentlResult<Arc<T>> {
    if h.is_null() {
        return Err(GentlError::InvalidHandle);
    }
    let handle = &*(h as *const HandleBox);
    if handle.generation != generation() {
        // Created before a GCCloseLib: the library resources are gone.
        return Err(GentlError::InvalidHandle);
    }
    handle
        .module
        .clone()
        .as_any_arc()
        .downcast::<T>()
        .map_err(|_| GentlError::InvalidHandle)
}

/// Free a handle previously created with `to_handle`. Call exactly once
/// from the module's Close function.
///
/// # Safety
/// Same preconditions as `from_handle`; the handle must not be used after.
pub unsafe fn free_handle(h: *mut c_void) {
    if !h.is_null() {
        drop(Box::from_raw(h as *mut HandleBox));
    }
}
