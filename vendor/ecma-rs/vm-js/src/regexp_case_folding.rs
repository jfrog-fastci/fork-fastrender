//! RegExp case folding tables and helpers.
//!
//! ECMA-262 defines two RegExp-related case folding operations that both rely on the same subset of
//! Unicode `CaseFolding.txt`:
//! - **`Canonicalize`** (used for `u`/`v` ignoreCase matching) uses a *simple or common* case folding
//!   mapping.
//! - **`scf`** (used for `v`-mode `UnicodeSets` ignoreCase CharSet canonicalization) uses the
//!   Unicode *simple case folding* mapping (`scf`).
//!
//! In Unicode `CaseFolding.txt`, both of these correspond to including **`C` (Common)** and
//! **`S` (Simple)** status mappings, and ignoring `F` (full/multi-code-point) and `T` (Turkic)
//! mappings.

/// Apply the Unicode RegExp case folding mapping for `Canonicalize`/`scf`.
///
/// This uses the `C` (Common) and `S` (Simple) mappings from Unicode `CaseFolding.txt`, and ignores
/// `F` (full) and `T` (Turkic) mappings.
#[inline]
pub fn regexp_case_fold(code_point: u32) -> u32 {
  // In ECMA-262, `scf` is defined using the CaseFolding.txt `C` + `S` mappings (each mapping a
  // single code point to a single code point). `Canonicalize` for `/iu` and `/iv` uses the same
  // "simple or common" case folding mappings.
  //
  // We therefore use the single `scf` table for both `Canonicalize` and `scf`.
  crate::unicode_case_folding::scf(code_point)
}
