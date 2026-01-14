//! Alternative AccessKit `NodeId` encoding for page DOM nodes.
//!
//! This module defines a tag-bit (bit 127) namespacing scheme (`page_node_id` /
//! `decode_page_node_id`) that can cheaply distinguish "page/content" nodes from chrome/egui nodes.
//!
//! Note: FastRender's current preferred encoding is the marker+namespace scheme in
//! `crate::accessibility::accesskit_ids` (wrapped by `crate::ui::page_a11y`). This tag-bit variant
//! is kept mainly for compatibility and does **not** include a tree generation, so it cannot be
//! used to filter stale action requests across navigations or page accessibility-tree rebuilds.
//!
//! Note: the windowed browser UI's injected page subtree currently uses the
//! `(tab_id, tree_generation, dom_node_id)` encoding in [`crate::ui::page_a11y`]
//! (`encode_page_node_id`) so stale action requests from previous navigations / tree generations can
//! be rejected. This tag-bit variant is kept as a compact, reversible alternative encoding.

#![cfg(feature = "browser_ui")]

use crate::ui::TabId;
use std::num::NonZeroU128;

/// Tag bit used to identify "page" (document) accessibility nodes.
///
/// The egui-based chrome uses AccessKit for its own widget tree. To make it cheap and reliable to
/// route AccessKit action requests destined for the rendered page (DOM) vs egui widgets, we reserve
/// the highest bit of AccessKit's 128-bit `NodeId` space.
///
/// Encoding (u128):
/// - Bit 127 = 1 (page node tag)
/// - Bits 64..=126 = tab id (63 bits; stored with bit 63 masked off)
/// - Bits 0..=63 = DOM node id (`usize` encoded as `u64`)
pub const PAGE_NODE_ID_TAG: u128 = 1u128 << 127;

const TAB_ID_MASK_63: u128 = (1u128 << 63) - 1;
const DOM_NODE_ID_MASK_64: u128 = (1u128 << 64) - 1;

// FastRender's preferred AccessKit `NodeId` encoding (see `crate::accessibility::accesskit_ids`)
// reserves the top 8 bits as a fixed marker (`0xFA`). Since this tag-bit scheme also sets bit 127,
// we must explicitly exclude ids that use the marker+namespace format; otherwise, those canonical
// ids would be misclassified as "tagged page ids" and decoded incorrectly.
const FASTR_ACCESSKIT_MARKER: u8 = 0xFA;

/// Build an AccessKit `NodeId` for a page accessibility node.
///
/// The returned ID is guaranteed non-zero and will always satisfy [`is_page_node_id`].
pub fn page_node_id(tab_id: TabId, dom_node_id: usize) -> accesskit::NodeId {
  let tab_bits = (tab_id.0 & 0x7fff_ffff_ffff_ffff_u64) as u128;
  let dom_bits = dom_node_id as u64 as u128;

  let raw = PAGE_NODE_ID_TAG | (tab_bits << 64) | dom_bits;
  debug_assert_ne!(raw, 0, "page node ids must be non-zero");
  // SAFETY: `PAGE_NODE_ID_TAG` always sets bit 127, so `raw` can never be zero.
  accesskit::NodeId(unsafe { NonZeroU128::new_unchecked(raw) })
}

/// Returns true if `id` is in the page node ID namespace.
pub fn is_page_node_id(id: accesskit::NodeId) -> bool {
  let raw = id.0.get();
  if (raw & PAGE_NODE_ID_TAG) == 0 {
    return false;
  }
  // Exclude FastRender's marker+namespace scheme (`0xFAxx…`), which also sets the high bit.
  (raw >> 120) as u8 != FASTR_ACCESSKIT_MARKER
}

/// Decode a page `NodeId` back into `(TabId, dom_node_id)` if it matches the page namespace.
pub fn decode_page_node_id(id: accesskit::NodeId) -> Option<(TabId, usize)> {
  if !is_page_node_id(id) {
    return None;
  }
  let raw = id.0.get();

  let tab_bits = (raw >> 64) & TAB_ID_MASK_63;
  let dom_bits = raw & DOM_NODE_ID_MASK_64;

  let dom_u64 = dom_bits as u64;
  let dom_usize = usize::try_from(dom_u64).ok()?;
  if tab_bits == 0 || dom_usize == 0 {
    return None;
  }

  Some((TabId(tab_bits as u64), dom_usize))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tagged_ids_are_detectable() {
    let id = page_node_id(TabId(1), 42);
    assert!(is_page_node_id(id));

    let non_page = accesskit::NodeId(NonZeroU128::new(1).unwrap());
    assert!(!is_page_node_id(non_page));
  }

  #[test]
  fn fastrender_namespaced_ids_are_not_mistaken_for_tagged_ids() {
    // FastRender's marker+namespace encoding uses `0xFA` in the top byte, which also sets the high
    // bit. These must *not* be treated as legacy tag-bit ids.
    let raw = ((FASTR_ACCESSKIT_MARKER as u128) << 120) | 123;
    let id = accesskit::NodeId(NonZeroU128::new(raw).unwrap());
    assert!(!is_page_node_id(id));
    assert_eq!(decode_page_node_id(id), None);
  }

  #[test]
  fn round_trip_decode() {
    let tab_id = TabId(123);
    let dom_node_id = 456usize;
    let id = page_node_id(tab_id, dom_node_id);

    assert_eq!(decode_page_node_id(id), Some((tab_id, dom_node_id)));

    let non_page = accesskit::NodeId(NonZeroU128::new(1).unwrap());
    assert_eq!(decode_page_node_id(non_page), None);
  }

  #[test]
  fn different_pairs_produce_different_ids() {
    let ids = [
      page_node_id(TabId(1), 1),
      page_node_id(TabId(1), 2),
      page_node_id(TabId(2), 1),
      page_node_id(TabId(2), 2),
    ];

    for (i, a) in ids.iter().enumerate() {
      for (j, b) in ids.iter().enumerate() {
        if i == j {
          continue;
        }
        assert_ne!(a.0.get(), b.0.get(), "ids at {i} and {j} should differ");
      }
    }
  }

  #[test]
  fn page_ids_do_not_collide_with_small_wrapper_ids() {
    // Historically the compositor tree used small integer node ids like 1/2/3 for Window/Chrome/Page
    // wrapper nodes. Even with that scheme (and with DOM node ids starting at 1), page ids must not
    // collide.
    for dom_node_id in 1usize..=3 {
      let id = page_node_id(TabId(1), dom_node_id);
      assert!(
        id.0.get() >= PAGE_NODE_ID_TAG,
        "expected page ids to always set the tag bit"
      );
      assert_ne!(id.0.get(), 1);
      assert_ne!(id.0.get(), 2);
      assert_ne!(id.0.get(), 3);
    }
  }

  #[test]
  fn decode_rejects_zero_tab_id_or_dom_node_id() {
    // Even though the tag bit is set, we treat (tab_id=0) and (dom_id=0) as invalid so they are
    // never mistaken for a real page node.
    let zero_tab = accesskit::NodeId(NonZeroU128::new(PAGE_NODE_ID_TAG | 1).unwrap());
    assert_eq!(decode_page_node_id(zero_tab), None);

    let zero_dom = accesskit::NodeId(NonZeroU128::new(PAGE_NODE_ID_TAG | (1u128 << 64)).unwrap());
    assert_eq!(decode_page_node_id(zero_dom), None);
  }
}
