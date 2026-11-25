//! Handle table: opaque `*mut c_void` handles over `Arc`-managed module
//! objects.
//!
//! A handle is an opaque token into a process-wide registry of module
//! references tagged with the handle-table generation. Using tokens (not
//! heap addresses) means a released handle can never alias a later one.
//! Handles are tracked so
//! `GCCloseLib` can shut the module tree down and release every box even
//! when the consumer did not call the individual Close functions (GenTL
//! 1.6 §6.3.1.1). The generation makes every handle created before the
//! close invalid afterwards, even if the library is re-initialized;
//! `from_handle` looks the pointer up in the registry instead of
//! dereferencing it, so a stale pointer answers GC_ERR_INVALID_HANDLE
//! safely.
//!
//! Consumers only pass handles back; every use goes through `from_handle`,
//! which validates the generation and concrete type. Wrong-generation or
//! wrong-type pointers map to GC_ERR_INVALID_HANDLE instead of undefined
//! behavior.

use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::error::{GentlError, GentlResult};
use crate::module::Module;

/// Handle-table generation, bumped by [`invalidate_handles`].
static HANDLE_GENERATION: AtomicU64 = AtomicU64::new(0);

struct HandleBox {
    generation: u64,
    /// Handle of the parent module, for the Close cascade (GenTL 6.1.3:
    /// closing a module frees its dependent child modules).
    parent: Option<usize>,
    module: Arc<dyn Module>,
}

/// Next opaque token. Tokens are never reused, so a stale handle token
/// cannot alias a new handle.
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

static REGISTRY: OnceLock<Mutex<HashMap<usize, HandleBox>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<usize, HandleBox>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_registry() -> MutexGuard<'static, HashMap<usize, HandleBox>> {
    registry()
        .lock()
        .expect("GenTL handle registry poisoned")
}

fn generation() -> u64 {
    HANDLE_GENERATION.load(Ordering::SeqCst)
}

/// Box an `Arc<T>` into an opaque GenTL handle. Free with `free_handle`.
pub fn to_handle<T: Module>(obj: Arc<T>) -> *mut c_void {
    to_handle_inner(obj, None)
}

/// Register `obj` as a child of the parent handle. When `parent` is null or
/// points at a freed handle the module becomes a root handle.
pub fn to_handle_child<T: Module>(obj: Arc<T>, parent: *mut c_void) -> *mut c_void {
    let parent = if parent.is_null() {
        None
    } else {
        Some(parent as usize)
    };
    to_handle_inner(obj, parent)
}

fn module_key(module: &Arc<dyn Module>) -> usize {
    Arc::as_ptr(module) as *const () as usize
}

fn to_handle_inner<T: Module>(obj: Arc<T>, parent: Option<usize>) -> *mut c_void {
    let module: Arc<dyn Module> = obj;
    let ptr = module_key(&module);
    let mut registry = lock_registry();
    // Parent-query helpers hand out the same module again; reuse the
    // canonical handle so both tokens die with the module.
    if let Some((&key, _)) = registry.iter().find(|(_, h)| module_key(&h.module) == ptr) {
        return key as *mut c_void;
    }
    let key = NEXT_HANDLE.fetch_add(1, Ordering::SeqCst) as usize;
    debug_assert!(key != 0 && key != usize::MAX);
    registry.insert(
        key,
        HandleBox {
            generation: generation(),
            parent,
            module,
        },
    );
    key as *mut c_void
}

fn child_keys(parent: usize) -> Vec<usize> {
    let registry = lock_registry();
    registry
        .iter()
        .filter(|(_, h)| h.parent == Some(parent))
        .map(|(&key, _)| key)
        .collect()
}

fn collect_tree(key: usize, out: &mut Vec<usize>) {
    for child in child_keys(key) {
        collect_tree(child, out);
    }
    out.push(key);
}

/// Close a module and every live descendant handle (GenTL 1.6 §6.1.3,
/// §6.3.2.1, §6.3.3.1, §6.3.4.1). Children are shut down and removed before
/// the parent; unknown/stale handles are a no-op.
pub fn close_handle_tree(h: *mut c_void) {
    if h.is_null() {
        return;
    }
    let mut order = Vec::new();
    collect_tree(h as usize, &mut order);
    for key in order {
        let removed = lock_registry().remove(&key);
        if let Some(handle) = removed {
            let _ = catch_unwind(AssertUnwindSafe(|| handle.module.shutdown()));
            drop(handle);
        }
    }
}

/// Recover a typed `Arc<T>` from a handle, validating the generation and
/// the concrete type.
///
/// # Safety
/// `h` must be null or a pointer returned by `to_handle`. The registry
/// lookup makes stale/freed pointers safe (no dereference of `h`).
pub unsafe fn from_handle<T: Module>(h: *mut c_void) -> GentlResult<Arc<T>> {
    if h.is_null() {
        return Err(GentlError::InvalidHandle);
    }
    let module = {
        let registry = lock_registry();
        let Some(handle) = registry.get(&(h as usize)) else {
            return Err(GentlError::InvalidHandle);
        };
        if handle.generation != generation() {
            // Created before a GCCloseLib: the library resources are gone.
            return Err(GentlError::InvalidHandle);
        }
        Arc::clone(&handle.module)
    };
    module
        .as_any_arc()
        .downcast::<T>()
        .map_err(|_| GentlError::InvalidHandle)
}

/// Free a handle previously created with `to_handle`. Call exactly once
/// from the module's Close function.
///
/// # Safety
/// Same preconditions as `from_handle`; stale pointers are a no-op.
pub unsafe fn free_handle(h: *mut c_void) {
    if h.is_null() {
        return;
    }
    let removed = lock_registry().remove(&(h as usize));
    drop(removed);
}

/// Invalidate and release every live handle (GCCloseLib). Each module's
/// `shutdown` runs before its box is dropped, so open data streams stop
/// their receiver threads, devices stop heartbeat/event pumps, and the
/// handle-box memory is freed even when the consumer never called Close.
pub fn invalidate_handles() {
    HANDLE_GENERATION.fetch_add(1, Ordering::SeqCst);
    let drained: Vec<HandleBox> = {
        let mut registry = lock_registry();
        registry.drain().map(|(_, handle)| handle).collect()
    };
    for handle in &drained {
        // Keep shutdown best-effort: one broken module must not prevent the
        // remaining handles from being released.
        let _ = catch_unwind(AssertUnwindSafe(|| handle.module.shutdown()));
    }
    drop(drained);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::Any;
    use std::sync::atomic::AtomicUsize;

    struct TestModule {
        shutdowns: Arc<AtomicUsize>,
        #[allow(dead_code)] // keeps a strong-count witness for the test
        marker: Arc<()>,
    }

    impl Module for TestModule {
        fn module_type(&self) -> crate::module::ModuleType {
            crate::module::ModuleType::System
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
        fn shutdown(&self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Handle tests share the process-wide registry; invalidate_handles in one
    /// test must not invalidate another test's handles.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("handle test lock")
    }

    #[test]
    fn handles_are_released_by_invalidate_and_free() {
        let _guard = test_lock();
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let marker = Arc::new(());
        let module = Arc::new(TestModule {
            shutdowns: Arc::clone(&shutdowns),
            marker: Arc::clone(&marker),
        });
        assert_eq!(Arc::strong_count(&marker), 2);

        let h = to_handle(module);
        assert_eq!(Arc::strong_count(&marker), 2, "the handle owns one Arc");
        unsafe {
            assert!(from_handle::<TestModule>(h).is_ok());
        }
        invalidate_handles();
        assert_eq!(
            shutdowns.load(Ordering::SeqCst),
            1,
            "GCCloseLib runs shutdown before dropping the module"
        );
        assert_eq!(
            Arc::strong_count(&marker),
            1,
            "the HandleBox and its Arc were freed"
        );
        assert!(
            unsafe { from_handle::<TestModule>(h) }.is_err(),
            "invalidated handles are rejected without dereferencing the pointer"
        );
        unsafe { free_handle(h) }; // stale pointer: safe no-op

        // Normal close path still frees exactly once.
        let h2 = to_handle(Arc::new(TestModule {
            shutdowns: Arc::clone(&shutdowns),
            marker: Arc::clone(&marker),
        }));
        assert_eq!(Arc::strong_count(&marker), 2);
        unsafe { free_handle(h2) };
        assert_eq!(Arc::strong_count(&marker), 1);
    }

    #[test]
    fn close_handle_tree_removes_children_first_and_runs_shutdown() {
        let _guard = test_lock();
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let marker = Arc::new(());
        let make = || {
            Arc::new(TestModule {
                shutdowns: Arc::clone(&shutdowns),
                marker: Arc::clone(&marker),
            })
        };
        let parent = to_handle(make());
        let child = to_handle_child(make(), parent);
        let grandchild = to_handle_child(make(), child);
        assert_eq!(Arc::strong_count(&marker), 4);
        unsafe {
            assert!(from_handle::<TestModule>(parent).is_ok());
            assert!(from_handle::<TestModule>(child).is_ok());
            assert!(from_handle::<TestModule>(grandchild).is_ok());
        }
        close_handle_tree(parent);
        assert_eq!(
            shutdowns.load(Ordering::SeqCst),
            3,
            "parent and every descendant are shut down"
        );
        assert_eq!(Arc::strong_count(&marker), 1, "all handles released");
        unsafe {
            assert!(from_handle::<TestModule>(child).is_err());
            assert!(from_handle::<TestModule>(grandchild).is_err());
        }
    }

    /// Parent-query helpers must get the canonical token, not a second one.
    #[test]
    fn same_module_reuses_its_handle() {
        let _guard = test_lock();
        let module = Arc::new(TestModule {
            shutdowns: Arc::new(AtomicUsize::new(0)),
            marker: Arc::new(()),
        });
        let a = to_handle(Arc::clone(&module));
        let b = to_handle_child(Arc::clone(&module), a);
        assert_eq!(a, b);
    }
}
