use std::sync::OnceLock;

use dashmap::DashMap;
use diagnostics::FileId;

use crate::db::LowerResultWithDiagnostics;
use crate::lib_support::FileKind;
use crate::queries::parse::ParseResult;
use crate::FileKey;

/// Global cache for bundled TypeScript lib parse/lower artifacts.
///
/// # Why this exists
/// The conformance/harness runners construct thousands of `Program` instances in-process.
/// Without a global cache every `Program` re-parses and re-lowers the bundled `lib.*.d.ts`
/// files, dominating wall time.
///
/// # Boundedness
/// This cache is intentionally **bounded to the finite bundled-lib set**:
/// - it only caches file keys of the form `lib:{filename}`
/// - and only when the `text_hash` matches the embedded TypeScript lib text for that file.
///
/// Host-provided `.d.ts` files (including `Host::lib_files`) are therefore not cached here,
/// avoiding an unbounded cache keyed by arbitrary user inputs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PreparedLibKey {
  pub file_id: FileId,
  pub file_key: FileKey,
  pub file_kind: FileKind,
  pub text_hash: u64,
}

static PARSE_CACHE: OnceLock<DashMap<PreparedLibKey, ParseResult>> = OnceLock::new();
static LOWER_CACHE: OnceLock<DashMap<PreparedLibKey, LowerResultWithDiagnostics>> = OnceLock::new();

fn parse_cache() -> &'static DashMap<PreparedLibKey, ParseResult> {
  PARSE_CACHE.get_or_init(DashMap::new)
}

fn lower_cache() -> &'static DashMap<PreparedLibKey, LowerResultWithDiagnostics> {
  LOWER_CACHE.get_or_init(DashMap::new)
}

/// Compute a stable hash of the file text for cache keys.
///
/// This uses FNV-1a 64-bit to remain deterministic across runs and toolchains.
fn stable_text_hash(text: &str) -> u64 {
  const OFFSET: u64 = 0xcbf29ce484222325;
  const PRIME: u64 = 0x100000001b3;
  let mut hash = OFFSET;
  for b in text.as_bytes() {
    hash ^= *b as u64;
    hash = hash.wrapping_mul(PRIME);
  }
  hash
}

#[cfg(feature = "bundled-libs")]
fn canonical_bundled_hash(filename: &str) -> Option<u64> {
  // Avoid hashing the canonical lib text more than once per filename.
  static HASHES: OnceLock<DashMap<String, u64>> = OnceLock::new();
  let hashes = HASHES.get_or_init(DashMap::new);
  if let Some(found) = hashes.get(filename) {
    return Some(*found);
  }

  let canonical = super::bundled::bundled_lib_text(filename)?;
  let hash = stable_text_hash(canonical);
  // Only insert once we have a fully computed hash; no panics inside map locks.
  hashes.insert(filename.to_string(), hash);
  Some(hash)
}

#[cfg(not(feature = "bundled-libs"))]
fn canonical_bundled_hash(filename: &str) -> Option<u64> {
  if filename != "core_globals.d.ts" {
    return None;
  }
  static HASH: OnceLock<u64> = OnceLock::new();
  Some(*HASH.get_or_init(|| stable_text_hash(super::FALLBACK_CORE_GLOBAL_TYPES)))
}

/// If this file key corresponds to a bundled TypeScript lib, return the stable
/// expected text hash for the embedded version.
///
/// This is used by salsa queries to quickly identify cacheable bundled libs
/// *without hashing the file contents* (host-provided `.d.ts` files short-circuit
/// before touching their text).
pub(crate) fn bundled_lib_expected_hash(file_key: &FileKey, file_kind: FileKind) -> Option<u64> {
  if file_kind != FileKind::Dts {
    return None;
  }
  let filename = file_key.as_str().strip_prefix("lib:")?;
  canonical_bundled_hash(filename)
}

pub(crate) fn get_parsed(key: &PreparedLibKey) -> Option<ParseResult> {
  parse_cache().get(key).map(|entry| entry.clone())
}

pub(crate) fn store_parsed(key: PreparedLibKey, parsed: ParseResult) {
  parse_cache().entry(key).or_insert(parsed);
}

pub(crate) fn get_lowered(key: &PreparedLibKey) -> Option<LowerResultWithDiagnostics> {
  lower_cache().get(key).map(|entry| entry.clone())
}

pub(crate) fn store_lowered(key: PreparedLibKey, lowered: LowerResultWithDiagnostics) {
  lower_cache().entry(key).or_insert(lowered);
}
