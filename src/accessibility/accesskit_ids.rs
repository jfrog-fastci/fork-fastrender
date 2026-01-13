#![cfg(feature = "a11y_accesskit")]

use crate::dom2;
use std::num::NonZeroU128;

/// AccessKit `NodeId` bit layout for nodes backed by `dom2::NodeId`.
///
/// We encode `dom2` node ids into AccessKit ids so that:
/// - IDs are stable across renderer preorder changes (DOM insertions/removals).
/// - Action routing can recover the underlying `dom2::NodeId`.
/// - Multiple ID spaces (chrome vs content, future remote documents, etc.) can coexist without
///   collisions by reserving high bits for a namespace.
///
/// Layout (big-endian, MSB → LSB):
/// - 8 bits: fixed FastRender marker (`0xFA`) to avoid colliding with other toolkits.
/// - 8 bits: namespace (`FASTR_ACCESSKIT_NAMESPACE_*`).
/// - 112 bits: payload.
///
/// For `dom2` nodes, the payload is `dom2::NodeId.index() + 1` (so `0` is never used, satisfying
/// AccessKit's `NonZeroU128` requirement).
const FASTR_ACCESSKIT_MARKER: u8 = 0xFA;

/// Namespace for `dom2::NodeId`-backed nodes.
///
/// Reserved high bits allow future composition of multiple FastRender trees (e.g. chrome + content)
/// without `NodeId` collisions.
const FASTR_ACCESSKIT_NAMESPACE_DOM2: u8 = 0x01;

const PAYLOAD_BITS: u32 = 112;
const PAYLOAD_MASK: u128 = (1u128 << PAYLOAD_BITS) - 1;

/// Encode a stable `dom2::NodeId` as an AccessKit `NodeId`.
pub fn accesskit_id_for_dom2(node: dom2::NodeId) -> accesskit::NodeId {
  let idx = node.index() as u128;
  let payload = idx.saturating_add(1);
  debug_assert!(
    payload <= PAYLOAD_MASK,
    "dom2::NodeId index too large to encode into AccessKit NodeId"
  );

  let raw = ((FASTR_ACCESSKIT_MARKER as u128) << 120)
    | ((FASTR_ACCESSKIT_NAMESPACE_DOM2 as u128) << 112)
    | (payload & PAYLOAD_MASK);
  accesskit::NodeId(NonZeroU128::new(raw).expect("encoded AccessKit NodeId must be non-zero")) // fastrender-allow-unwrap
}

/// Attempt to decode an AccessKit `NodeId` back into a stable `dom2::NodeId`.
///
/// Returns `None` if the node id is not in the `dom2` namespace.
pub fn dom2_id_from_accesskit(node: accesskit::NodeId) -> Option<dom2::NodeId> {
  let raw = node.0.get();
  let marker = (raw >> 120) as u8;
  if marker != FASTR_ACCESSKIT_MARKER {
    return None;
  }

  let namespace = ((raw >> 112) & 0xFF) as u8;
  if namespace != FASTR_ACCESSKIT_NAMESPACE_DOM2 {
    return None;
  }

  let payload = raw & PAYLOAD_MASK;
  let idx = payload.checked_sub(1)?;
  if idx > (usize::MAX as u128) {
    return None;
  }
  Some(dom2::NodeId::from_index(idx as usize))
}
