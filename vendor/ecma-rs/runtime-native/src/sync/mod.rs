//! GC-aware synchronization primitives for runtime internals.
//!
//! These wrappers integrate with the runtime's cooperative stop-the-world (STW)
//! safepoint mechanism by temporarily transitioning contended lock acquisition
//! into a GC-safe ("native") region.

pub mod gc_mutex;
pub mod gc_rwlock;

pub use gc_mutex::GcAwareMutex;
pub use gc_rwlock::GcAwareRwLock;
