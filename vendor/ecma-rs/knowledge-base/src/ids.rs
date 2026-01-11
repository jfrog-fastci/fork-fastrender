/// Stable, content-addressed identifier for an API entry.
///
/// This is derived from the API's canonical name (e.g. `"JSON.parse"`) using a
/// deterministic 64-bit FNV-1a hash. The constants match `hir-js`'s stable
/// hasher so IDs can be reproduced across crates without depending on Rust's
/// platform-specific `Hasher` implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ApiId(u64);

impl ApiId {
  /// Hash a canonical API name into an [`ApiId`].
  pub fn from_name(name: &str) -> ApiId {
    // FNV-1a 64-bit parameters (same as `hir-js` `StableHasher`).
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    let mut hash = OFFSET_BASIS;
    for &byte in name.as_bytes() {
      hash ^= byte as u64;
      hash = hash.wrapping_mul(PRIME);
    }
    ApiId(hash)
  }

  #[inline]
  pub const fn raw(self) -> u64 {
    self.0
  }
}

