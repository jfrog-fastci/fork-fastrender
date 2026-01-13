//! Shared memory helpers for the multiprocess IPC layer.
//!
//! The browser↔renderer architecture relies on OS shared memory primitives for large pixel buffers.
//! On macOS, POSIX shared-memory names passed to `shm_open(3)` are commonly limited to
//! `PSHMNAMLEN = 31` bytes **including the required leading '/'**. Keep all generated names within
//! this strict limit so the same naming scheme works across platforms.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

/// Maximum length of the OS-level name passed to `shm_open`, including the leading `/`.
///
/// Many platforms allow longer names, but macOS commonly enforces `PSHMNAMLEN = 31` bytes.
/// We keep the strictest value globally to avoid portability bugs (CI frequently runs on Linux,
/// but names must still work on macOS).
pub const MAX_SHMEM_NAME_LEN: usize = 31;

/// Maximum length of the user-facing shared-memory identifier (without the leading `/`).
pub const MAX_SHMEM_ID_LEN: usize = MAX_SHMEM_NAME_LEN - 1;

// Keep this prefix extremely short so the base64 payload has room under macOS's 31-byte limit.
const SHMEM_ID_PREFIX: &str = "fr";

/// Generate a new shared-memory identifier suitable for POSIX `shm_open`.
///
/// - Returned strings are ASCII and contain no `/` characters.
/// - The identifier length is always `<= MAX_SHMEM_NAME_LEN - 1`, so adding the leading `/` at the
///   syscall layer fits within [`MAX_SHMEM_NAME_LEN`].
pub fn generate_shmem_id() -> String {
  // 16 random bytes -> 22 chars base64url (unpadded). With the 2-byte prefix this yields 24 bytes,
  // safely under the macOS 31-byte `shm_open` name limit (including leading '/').
  let mut bytes = [0u8; 16];
  if getrandom::getrandom(&mut bytes).is_err() {
    // Extremely defensive fallback: if OS randomness is unavailable, generate a best-effort unique
    // value using a per-process counter and time. This is not intended to be cryptographically
    // unpredictable, but avoids surprising runtime failures in restricted environments.
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let pid = u64::from(std::process::id());
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let time = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap_or_default()
      .as_nanos() as u64;
    let mut state = pid ^ counter ^ time;

    for chunk in bytes.chunks_mut(8) {
      // xorshift64*
      state ^= state >> 12;
      state ^= state << 25;
      state ^= state >> 27;
      state = state.wrapping_mul(0x2545F4914F6CDD1D);
      let out = state.to_le_bytes();
      chunk.copy_from_slice(&out[..chunk.len()]);
    }
  }

  let encoded = URL_SAFE_NO_PAD.encode(bytes);
  let mut id = String::with_capacity(SHMEM_ID_PREFIX.len() + encoded.len());
  id.push_str(SHMEM_ID_PREFIX);
  id.push_str(&encoded);
  // This should be impossible unless the constants above are modified; fail fast instead of
  // emitting an ID that will later cause `shm_open` to fail on macOS.
  assert!(
    id.len() <= MAX_SHMEM_ID_LEN,
    "generated shmem id too long: {} bytes (max {})",
    id.len(),
    MAX_SHMEM_ID_LEN
  );
  id
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn generate_shmem_id_respects_macos_name_limits_and_charset() {
    for _ in 0..128 {
      let id = generate_shmem_id();
      assert!(!id.is_empty(), "id should not be empty");
      assert!(
        id.len() <= MAX_SHMEM_ID_LEN,
        "id length {} exceeded MAX_SHMEM_ID_LEN={MAX_SHMEM_ID_LEN}",
        id.len()
      );
      assert!(id.is_ascii(), "id must be ASCII: {id:?}");
      assert!(!id.contains('/'), "id must not contain '/': {id:?}");
      assert!(
        id.starts_with(SHMEM_ID_PREFIX),
        "id must start with prefix {SHMEM_ID_PREFIX:?}: {id:?}"
      );
      assert!(
        id.chars()
          .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "id contains unexpected characters: {id:?}"
      );
    }
  }
}

