//! Text segmentation helpers for grapheme clusters.
//!
//! These helpers operate purely on Unicode data and do not rely on font shaping
//! results, making them deterministic and suitable as fallbacks when shaping
//! does not report cluster boundaries.

use crate::text::emoji;
use unicode_segmentation::UnicodeSegmentation;

/// Returns byte offsets for each grapheme cluster boundary in `text`.
///
/// The returned offsets always include the start of the string (0) and the end
/// (`text.len()`), and are sorted and deduplicated.
pub fn segment_grapheme_clusters(text: &str) -> Vec<usize> {
  // Even for empty strings we return the start/end boundary (both 0) so callers can treat the
  // output as "all valid boundaries" without special-casing.
  if text.is_empty() {
    return vec![0];
  }

  let mut offsets: Vec<usize> = text.grapheme_indices(true).map(|(idx, _)| idx).collect();
  offsets.push(text.len());
  offsets.sort_unstable();
  offsets.dedup();

  // Unicode grapheme segmentation (UAX #29) generally keeps emoji sequences intact, but the
  // `unicode-segmentation` crate's tables can lag behind the latest UTS #51 recommendations.
  //
  // Keep our segmentation consistent with the shaping pipeline by ensuring that any detected emoji
  // sequence span is treated as an indivisible cluster boundary range.
  if !text.is_ascii() && contains_emoji_sequence_triggers(text) {
    let mut sequences = emoji::find_emoji_sequence_spans(text);
    if !sequences.is_empty() {
      sequences.sort_by_key(|seq| seq.start);
      for seq in sequences {
        offsets.retain(|&b| b <= seq.start || b >= seq.end);
        offsets.push(seq.start);
        offsets.push(seq.end);
      }
      offsets.sort_unstable();
      offsets.dedup();
    }
  }
  offsets
}

#[inline]
fn contains_emoji_sequence_triggers(text: &str) -> bool {
  // Avoid invoking the heavier emoji sequence parser when the string cannot contain multi-scalar
  // emoji sequences. These sequences are always signaled by joiners/selectors/modifiers or tag
  // characters.
  text.chars().any(|ch| {
    let cp = ch as u32;
    matches!(cp, 0x200d | 0xfe0e | 0xfe0f | 0x20e3)
      || (0x1f1e0..=0x1f1ff).contains(&cp)
      || (0x1f3fb..=0x1f3ff).contains(&cp)
      || (0xe0020..=0xe007f).contains(&cp)
  })
}
