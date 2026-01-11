use std::ptr::NonNull;

use crate::gc::{GcHooks, GcRoot};

/// A byte buffer whose address is stable for the lifetime of an in-flight kernel op.
///
/// # Safety
/// Implementors must ensure that the pointer returned by [`IoBuf::stable_ptr`] remains valid and
/// points to `len()` bytes for as long as the buffer value is alive.
///
/// In particular, do **not** return pointers into movable/compacting GC memory unless the backing
/// store is pinned for the buffer's lifetime.
pub unsafe trait IoBuf: Send + 'static {
    /// Stable pointer to the start of the buffer.
    fn stable_ptr(&self) -> NonNull<u8>;
    /// Buffer length in bytes.
    fn len(&self) -> usize;
}

/// A mutable buffer suitable for read operations.
///
/// # Safety
/// Implementors must ensure that the pointer returned by [`IoBufMut::stable_mut_ptr`] is valid for
/// writes of `len()` bytes for as long as the buffer value is alive.
pub unsafe trait IoBufMut: IoBuf {
    /// Stable mutable pointer to the start of the buffer.
    fn stable_mut_ptr(&mut self) -> NonNull<u8>;
}

/// Non-GC owned buffer backed by a `Vec<u8>` (heap allocation is non-moving).
#[derive(Debug)]
pub struct OwnedIoBuf {
    buf: Vec<u8>,
}

impl OwnedIoBuf {
    pub fn from_vec(buf: Vec<u8>) -> Self {
        Self { buf }
    }

    pub fn new_zeroed(len: usize) -> Self {
        Self { buf: vec![0; len] }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

unsafe impl IoBuf for OwnedIoBuf {
    fn stable_ptr(&self) -> NonNull<u8> {
        // Vec's allocation address is stable as long as we keep the Vec alive and do not
        // reallocate; this type never exposes APIs that can grow the Vec while in-flight.
        NonNull::new(self.buf.as_ptr() as *mut u8).expect("Vec::as_ptr is never null")
    }

    fn len(&self) -> usize {
        self.buf.len()
    }
}

unsafe impl IoBufMut for OwnedIoBuf {
    fn stable_mut_ptr(&mut self) -> NonNull<u8> {
        NonNull::new(self.buf.as_mut_ptr()).expect("Vec::as_mut_ptr is never null")
    }
}

/// GC-backed buffer that is rooted + pinned for the lifetime of this value.
///
/// The root prevents collection; the pin guard prevents relocation, making `stable_ptr()` safe to
/// hand to the kernel.
#[derive(Debug)]
pub struct GcIoBuf<R: GcRoot> {
    root: R,
    _pin: R::PinGuard,
    ptr: NonNull<u8>,
    len: usize,
}

// `NonNull<u8>` is not `Send`, but this wrapper only uses it as an address; the actual thread-safety
// is carried by the GC root + pin guard types.
unsafe impl<R: GcRoot> Send for GcIoBuf<R> {}

impl<R: GcRoot> GcIoBuf<R> {
    pub fn new(root: R) -> Self {
        let len = root.len();
        let pin = root.pin();
        let raw = root.stable_ptr(&pin);
        let ptr = match NonNull::new(raw) {
            Some(p) => p,
            None => {
                assert_eq!(
                    len, 0,
                    "GC stable_ptr returned null for a non-empty buffer"
                );
                NonNull::dangling()
            }
        };
        Self {
            root,
            _pin: pin,
            ptr,
            len,
        }
    }

    /// Root + pin a GC-managed buffer for use in an in-flight I/O op.
    pub fn from_gc<H>(hooks: &H, buffer: H::Buffer) -> Self
    where
        H: GcHooks<Root = R>,
    {
        Self::new(hooks.root(buffer))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn stable_ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    pub fn into_root(self) -> R {
        self.root
    }
}

unsafe impl<R: GcRoot> IoBuf for GcIoBuf<R> {
    fn stable_ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    fn len(&self) -> usize {
        self.len
    }
}

unsafe impl<R: GcRoot> IoBufMut for GcIoBuf<R> {
    fn stable_mut_ptr(&mut self) -> NonNull<u8> {
        self.ptr
    }
}
