//! GC integration boundary.
//!
//! The io_uring driver must never hand the kernel a pointer into GC-managed memory unless the
//! backing store has been pinned (i.e. the GC will not relocate it).
//!
//! This module defines a minimal trait surface so the I/O layer can:
//! - create a root/handle that keeps a GC object alive across awaits
//! - pin the backing store to prevent relocation
//! - obtain a stable pointer while pinned

/// GC entrypoint used by the I/O layer to create a GC root.
///
/// The I/O layer keeps the returned [`GcRoot`] alive for the full duration of any in-flight kernel
/// operation referencing the object's memory.
pub trait GcHooks {
    /// A GC-managed buffer object (e.g. a JS `ArrayBuffer`).
    type Buffer: Send + 'static;
    /// A rooting handle that keeps the object alive across collections.
    type Root: GcRoot;

    /// Create a root for `buffer` that keeps it alive across collections.
    fn root(&self, buffer: Self::Buffer) -> Self::Root;
}

/// A rooting handle that keeps a GC object alive.
///
/// Note: this is deliberately small; real GC implementations can store whatever state they need
/// inside the concrete root object.
pub trait GcRoot: Send + 'static {
    /// Guard type that prevents relocation of the backing store.
    type PinGuard: GcPinGuard;

    /// Byte length of the backing store.
    fn len(&self) -> usize;

    /// Pin the backing store so its address will not change until the guard is dropped.
    fn pin(&self) -> Self::PinGuard;

    /// Stable pointer to the backing store.
    ///
    /// Implementations may assume `pin` is alive and corresponds to this root.
    fn stable_ptr(&self, pin: &Self::PinGuard) -> *mut u8;
}

/// Marker trait for a GC pin guard.
pub trait GcPinGuard: Send + 'static {}

