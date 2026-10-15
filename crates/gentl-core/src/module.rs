//! Common base for all GenTL module objects.
//!
//! GenTL handles handed to consumers are opaque raw pointers to a boxed
//! `Arc<dyn Module>`. Consumers never dereference them; we do, after checking.

use std::any::Any;

/// GenTL module kinds, matching the GenTL module hierarchy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModuleType {
    System,
    Interface,
    Device,
    DataStream,
    Port,
    Event,
    Buffer,
}

/// All module objects are usable across threads (consumer calls may come
/// from any thread; receiver threads hold references too).
pub trait Module: Send + Sync + 'static {
    fn module_type(&self) -> ModuleType;

    /// Downcast support: `self` as the concrete type behind `dyn Any`.
    fn as_any(&self) -> &dyn Any;

    /// Arc-preserving downcast, needed to recover `Arc<T>` from a handle.
    fn as_any_arc(self: std::sync::Arc<Self>) -> std::sync::Arc<dyn Any + Send + Sync>;
}
