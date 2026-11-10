//! Thread-local last-error storage behind GCGetLastError, plus the internal
//! error type all modules report through. The GC_* boundary converts at the
//! edge in `gentl-gige/src/lib.rs`.

use std::fmt;

use crate::types::GCError;

thread_local! {
    static LAST: std::cell::RefCell<(GCError, String)> =
        std::cell::RefCell::new((crate::types::GC_ERR_SUCCESS, "No Error".to_owned()));
}

/// Record an error for the calling thread; retrieved via `GCGetLastError`.
pub fn set_error(code: GCError, text: &str) {
    LAST.with(|l| *l.borrow_mut() = (code, text.to_owned()));
}

/// Fetch the calling thread's last error. GCGetLastError does not clear it.
pub fn last_error() -> (GCError, String) {
    LAST.with(|l| l.borrow().clone())
}

/// Internal error type. `code()` is what crosses the GenTL boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GentlError {
    NotImplemented(&'static str),
    InvalidHandle,
    InvalidParameter(&'static str),
    InvalidId,
    NotInitialized,
    ResourceInUse,
    Io(String),
    Timeout,
    NotAvailable,
    InvalidAddress,
    BufferTooSmall,
    OutOfMemory,
    AccessDenied,
    Busy,
    /// Raw GEV status code from a GVCP acknowledge (see notes/gvcp-wirefacts.md).
    GevStatus(u16),
}

pub type GentlResult<T> = Result<T, GentlError>;

impl GentlError {
    pub fn code(&self) -> GCError {
        use crate::types::*;
        match self {
            GentlError::NotImplemented(_) => GC_ERR_NOT_IMPLEMENTED,
            GentlError::InvalidHandle => GC_ERR_INVALID_HANDLE,
            GentlError::InvalidParameter(_) => GC_ERR_INVALID_PARAMETER,
            GentlError::InvalidId => GC_ERR_INVALID_ID,
            GentlError::NotInitialized => GC_ERR_NOT_INITIALIZED,
            GentlError::ResourceInUse => GC_ERR_RESOURCE_IN_USE,
            GentlError::Io(_) => GC_ERR_IO,
            GentlError::Timeout => GC_ERR_TIMEOUT,
            GentlError::NotAvailable => GC_ERR_NOT_AVAILABLE,
            GentlError::InvalidAddress => GC_ERR_INVALID_ADDRESS,
            GentlError::BufferTooSmall => GC_ERR_BUFFER_TOO_SMALL,
            GentlError::OutOfMemory => GC_ERR_OUT_OF_MEMORY,
            GentlError::AccessDenied => GC_ERR_ACCESS_DENIED,
            GentlError::Busy => GC_ERR_BUSY,
            // Device GEV status: generic IO error, text carries the code.
            GentlError::GevStatus(_) => GC_ERR_IO,
        }
    }

    pub fn message(&self) -> String {
        match self {
            GentlError::NotImplemented(what) => format!("{what} is not implemented"),
            GentlError::InvalidHandle => "invalid handle".into(),
            GentlError::InvalidParameter(what) => format!("invalid parameter: {what}"),
            GentlError::InvalidId => "invalid ID".into(),
            GentlError::NotInitialized => "library not initialized".into(),
            GentlError::ResourceInUse => "resource in use".into(),
            GentlError::Io(msg) => format!("I/O error: {msg}"),
            GentlError::Timeout => "timeout".into(),
            GentlError::NotAvailable => "not available".into(),
            GentlError::InvalidAddress => "invalid address".into(),
            GentlError::BufferTooSmall => "buffer too small".into(),
            GentlError::OutOfMemory => "out of memory".into(),
            GentlError::AccessDenied => "access denied".into(),
            GentlError::Busy => "busy".into(),
            GentlError::GevStatus(status) => format!("device returned GEV status 0x{status:04X}"),
        }
    }
}

impl fmt::Display for GentlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for GentlError {}

impl From<std::io::Error> for GentlError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::TimedOut | ErrorKind::WouldBlock => GentlError::Timeout,
            _ => GentlError::Io(e.to_string()),
        }
    }
}

/// Record `err` as this thread's last error (for GCGetLastError).
pub fn set_last_error(err: &GentlError) {
    set_error(err.code(), &err.message());
}
