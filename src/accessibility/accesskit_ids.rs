#![cfg(feature = "a11y_accesskit")]

use crate::dom2;
use std::num::NonZeroU128;

/// AccessKit `NodeId` encoding used by FastRender.
///
/// AccessKit node ids are `NonZeroU128` (`accesskit::NodeId`). When we compose multiple subtrees
/// (egui widgets, compositor wrapper nodes, FastRender DOM nodes, JS/dom2 nodes, multiple tabs, …)
/// into a single AccessKit tree, we must guarantee there are no `NodeId` collisions.
///
/// FastRender reserves the high 16 bits of the 128-bit space:
///
/// Layout (big-endian, MSB → LSB):
/// - 8 bits: fixed FastRender marker (`FASTR_ACCESSKIT_MARKER`) to avoid colliding with
///   non-FastRender producers.
/// - 8 bits: namespace (`FASTR_ACCESSKIT_NAMESPACE_*`).
/// - 112 bits: payload (layout depends on namespace).
///
/// This makes IDs:
/// - collision-free across independently generated subtrees,
/// - reversible (namespace + payload can be recovered without a global map),
/// - stable across updates (callers choose stable payloads).
/// Chosen to have the highest bit clear so it does not overlap with the legacy tag-bit page id
/// encoding (`ui::page_accesskit_ids` sets bit 127).
const FASTR_ACCESSKIT_MARKER: u8 = 0x46; // 'F'

/// Namespace for `dom2::NodeId`-backed nodes.
///
/// Payload: `dom2::NodeId.index() + 1` (so `0` is never used, satisfying invariants even if we ever
/// drop the fixed marker).
const FASTR_ACCESSKIT_NAMESPACE_DOM2: u8 = 0x01;

/// Namespace for chrome/compositor wrapper nodes (window root, chrome region, page region).
///
/// Payload: [`ChromeWrapperNode`] discriminant.
const FASTR_ACCESSKIT_NAMESPACE_CHROME_WRAPPER: u8 = 0x02;

/// Namespace for DOM pre-order ids belonging to the browser chrome document (renderer-chrome).
///
/// Payload: 1-indexed DOM pre-order node id.
const FASTR_ACCESSKIT_NAMESPACE_CHROME_DOM_PREORDER: u8 = 0x03;

/// Namespace for DOM pre-order ids belonging to a rendered page/document.
///
/// Payload layout (112 bits):
/// - bits 111..64: tab id (48 bits, non-zero)
/// - bits 63..32: page accessibility-tree generation (32 bits)
/// - bits 31..0: DOM pre-order node id (u32, non-zero; clamped from `usize`)
const FASTR_ACCESSKIT_NAMESPACE_PAGE_DOM_PREORDER: u8 = 0x04;

/// Namespace for renderer preorder node ids (used as a fallback when dom2 mapping is unavailable).
///
/// Payload: 1-indexed renderer preorder node id.
const FASTR_ACCESSKIT_NAMESPACE_RENDERER_PREORDER: u8 = 0x05;

const PAYLOAD_BITS: u32 = 112;
const PAYLOAD_MASK: u128 = (1u128 << PAYLOAD_BITS) - 1;

const PAGE_TAB_ID_BITS: u32 = 48;
const PAGE_TAB_ID_MAX: u64 = (1u64 << PAGE_TAB_ID_BITS) - 1;

fn encode_namespaced_id(namespace: u8, payload: u128) -> accesskit::NodeId {
  debug_assert!(
    payload <= PAYLOAD_MASK,
    "AccessKit id payload too large for FastRender encoding"
  );
  let raw = ((FASTR_ACCESSKIT_MARKER as u128) << 120)
    | ((namespace as u128) << 112)
    | (payload & PAYLOAD_MASK);
  debug_assert_ne!(
    raw, 0,
    "encoded AccessKit NodeId must be non-zero (marker/namespace scheme invariant)"
  );
  // SAFETY: `raw` always contains the fixed marker in the high bits, so it can never be zero.
  accesskit::NodeId(unsafe { NonZeroU128::new_unchecked(raw) })
}

fn decode_namespaced_id(node: accesskit::NodeId) -> Option<(u8, u128)> {
  let raw = node.0.get();
  let marker = (raw >> 120) as u8;
  if marker != FASTR_ACCESSKIT_MARKER {
    return None;
  }
  let namespace = ((raw >> 112) & 0xFF) as u8;
  let payload = raw & PAYLOAD_MASK;
  Some((namespace, payload))
}

/// Encode a stable `dom2::NodeId` as an AccessKit `NodeId`.
pub fn accesskit_id_for_dom2(node: dom2::NodeId) -> accesskit::NodeId {
  let idx = node.index() as u128;
  let payload = idx.saturating_add(1);
  debug_assert!(
    payload <= PAYLOAD_MASK,
    "dom2::NodeId index too large to encode into AccessKit NodeId"
  );
  encode_namespaced_id(FASTR_ACCESSKIT_NAMESPACE_DOM2, payload)
}

/// Attempt to decode an AccessKit `NodeId` back into a stable `dom2::NodeId`.
///
/// Returns `None` if the node id is not in the `dom2` namespace.
pub fn dom2_id_from_accesskit(node: accesskit::NodeId) -> Option<dom2::NodeId> {
  let (namespace, payload) = decode_namespaced_id(node)?;
  if namespace != FASTR_ACCESSKIT_NAMESPACE_DOM2 {
    return None;
  }
  let idx = payload.checked_sub(1)?;
  if idx > (usize::MAX as u128) {
    return None;
  }
  Some(dom2::NodeId::from_index(idx as usize))
}

/// Stable wrapper nodes for FastRender-driven browser chrome accessibility trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ChromeWrapperNode {
  /// Platform/window root node.
  Window = 1,
  /// Chrome region wrapper (toolbar/tab strip/etc).
  Chrome = 2,
  /// Page/content region wrapper.
  Page = 3,
}

pub fn accesskit_id_for_chrome_wrapper(node: ChromeWrapperNode) -> accesskit::NodeId {
  encode_namespaced_id(FASTR_ACCESSKIT_NAMESPACE_CHROME_WRAPPER, node as u8 as u128)
}

pub fn chrome_wrapper_from_accesskit(node: accesskit::NodeId) -> Option<ChromeWrapperNode> {
  let (namespace, payload) = decode_namespaced_id(node)?;
  if namespace != FASTR_ACCESSKIT_NAMESPACE_CHROME_WRAPPER {
    return None;
  }
  match payload {
    1 => Some(ChromeWrapperNode::Window),
    2 => Some(ChromeWrapperNode::Chrome),
    3 => Some(ChromeWrapperNode::Page),
    _ => None,
  }
}

/// Encode a 1-indexed DOM pre-order node id belonging to the chrome document (renderer-chrome).
pub fn accesskit_id_for_chrome_dom_preorder(dom_node_id: usize) -> accesskit::NodeId {
  debug_assert!(
    dom_node_id != 0,
    "expected DOM preorder ids to be 1-indexed"
  );
  encode_namespaced_id(
    FASTR_ACCESSKIT_NAMESPACE_CHROME_DOM_PREORDER,
    dom_node_id as u128,
  )
}

pub fn chrome_dom_preorder_from_accesskit(node: accesskit::NodeId) -> Option<usize> {
  let (namespace, payload) = decode_namespaced_id(node)?;
  if namespace != FASTR_ACCESSKIT_NAMESPACE_CHROME_DOM_PREORDER {
    return None;
  }
  let dom = usize::try_from(payload).ok()?;
  if dom == 0 {
    return None;
  }
  Some(dom)
}

/// Encode a 1-indexed DOM pre-order node id belonging to a rendered page/document.
pub fn accesskit_id_for_page_dom_preorder(
  tab_id: u64,
  tree_generation: u32,
  dom_node_id: usize,
) -> accesskit::NodeId {
  // `tab_id=0` is reserved for invalid page node ids; returning a non-panicking ID that fails to
  // decode avoids crashing rendering if an unexpected zero sneaks in.
  let tab_id = if tab_id == 0 {
    debug_assert!(false, "tab_id=0 is reserved for invalid page node ids");
    0
  } else if tab_id > PAGE_TAB_ID_MAX {
    // Prevent truncation/collisions: out-of-range tab ids would be masked down to 48 bits.
    debug_assert!(
      false,
      "tab_id={tab_id} too large for FastRender AccessKit NodeId encoding (max {PAGE_TAB_ID_MAX})"
    );
    0
  } else {
    tab_id
  };
  debug_assert!(dom_node_id != 0, "expected DOM preorder ids to be 1-indexed");

  let dom_u32 = if dom_node_id > u32::MAX as usize {
    u32::MAX
  } else {
    dom_node_id as u32
  };

  let payload =
    ((tab_id as u128) << 64) | ((tree_generation as u128) << 32) | (dom_u32 as u128);
  encode_namespaced_id(FASTR_ACCESSKIT_NAMESPACE_PAGE_DOM_PREORDER, payload)
}

pub fn page_dom_preorder_from_accesskit(node: accesskit::NodeId) -> Option<(u64, u32, usize)> {
  let (namespace, payload) = decode_namespaced_id(node)?;
  if namespace != FASTR_ACCESSKIT_NAMESPACE_PAGE_DOM_PREORDER {
    return None;
  }
  let tab_id = (payload >> 64) as u64;
  if tab_id == 0 {
    return None;
  }
  let tree_generation = ((payload >> 32) & 0xFFFF_FFFF) as u32;
  let dom_node_id = (payload & 0xFFFF_FFFF) as u32;
  if dom_node_id == 0 {
    return None;
  }
  Some((tab_id, tree_generation, dom_node_id as usize))
}

/// Encode a 1-indexed renderer preorder id in its own namespace.
pub fn accesskit_id_for_renderer_preorder(preorder_id: usize) -> accesskit::NodeId {
  debug_assert!(
    preorder_id != 0,
    "expected renderer preorder ids to be 1-indexed"
  );
  encode_namespaced_id(
    FASTR_ACCESSKIT_NAMESPACE_RENDERER_PREORDER,
    preorder_id as u128,
  )
}

pub fn renderer_preorder_from_accesskit(node: accesskit::NodeId) -> Option<usize> {
  let (namespace, payload) = decode_namespaced_id(node)?;
  if namespace != FASTR_ACCESSKIT_NAMESPACE_RENDERER_PREORDER {
    return None;
  }
  let preorder = usize::try_from(payload).ok()?;
  if preorder == 0 {
    return None;
  }
  Some(preorder)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashSet;

  #[test]
  fn chrome_dom_preorder_round_trips_for_typical_ids() {
    for dom in [1usize, 2, 42, 10_000] {
      let id = accesskit_id_for_chrome_dom_preorder(dom);
      assert_eq!(chrome_dom_preorder_from_accesskit(id), Some(dom));
    }
  }

  #[test]
  fn page_dom_preorder_round_trips_and_includes_tab_and_generation() {
    let tab_id = 7u64;
    let gen = 42u32;
    for dom in [1usize, 2, 99, 123_456] {
      let id = accesskit_id_for_page_dom_preorder(tab_id, gen, dom);
      assert_eq!(
        page_dom_preorder_from_accesskit(id),
        Some((tab_id, gen, dom.min(u32::MAX as usize)))
      );
    }
  }

  #[test]
  fn wrapper_ids_never_decode_as_dom_ids() {
    let wrapper_ids = [
      accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Window),
      accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Chrome),
      accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Page),
    ];
    for id in wrapper_ids {
      assert_eq!(chrome_dom_preorder_from_accesskit(id), None);
      assert_eq!(page_dom_preorder_from_accesskit(id), None);
    }
  }

  #[test]
  fn marker_scheme_ids_do_not_set_page_tag_bit() {
    // The legacy tag-bit encoding uses bit 127; ensure our marker-based scheme keeps that bit clear
    // so callers can distinguish between the two encodings without ambiguity.
    const TAG: u128 = 1u128 << 127;
    let id = accesskit_id_for_page_dom_preorder(1, 1, 1).0.get();
    assert_eq!(id & TAG, 0);
    let wrapper = accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Window)
      .0
      .get();
    assert_eq!(wrapper & TAG, 0);
  }

  #[test]
  fn dom_id_one_does_not_collide_with_wrapper_ids() {
    let wrapper_ids = [
      accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Window),
      accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Chrome),
      accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Page),
    ];
    let page_dom_root = accesskit_id_for_page_dom_preorder(1, 1, 1);
    for wrapper in wrapper_ids {
      assert_ne!(page_dom_root.0.get(), wrapper.0.get());
    }
  }

  #[test]
  fn different_wrapper_nodes_have_distinct_ids() {
    let mut set = HashSet::new();
    for wrapper in [
      ChromeWrapperNode::Window,
      ChromeWrapperNode::Chrome,
      ChromeWrapperNode::Page,
    ] {
      assert!(set.insert(accesskit_id_for_chrome_wrapper(wrapper).0.get()));
    }
  }
}
