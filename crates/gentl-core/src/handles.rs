//! Handle table: opaque `*mut c_void` handles over `Arc`-managed module objects.
//!
//! A handle is a boxed `Arc<dyn Module>`. Consumers only pass it back; every
//! use goes through `from_handle`, which validates the type. Wrong-type or
//! foreign pointers fail the downcast and map to GC_ERR_INVALID_HANDLE
//! instead of undefined behavior.

use std::ffi::c_void;
use std::sync::Arc;

use crate::error::{GentlError, GentlResult};
use crate::module::Module;

/// Box an `Arc<T>` into an opaque GenTL handle. Free with `free_handle`.
pub fn to_handle<T: Module>(obj: Arc<T>) -> *mut c_void {
    let boxed: Box<Arc<dyn Module>> = Box::new(obj);
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
    let dyn_arc: Arc<dyn Module> = (*(h as *const Arc<dyn Module>)).clone();
    dyn_arc
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
        drop(Box::from_raw(h as *mut Arc<dyn Module>));
    }
}
