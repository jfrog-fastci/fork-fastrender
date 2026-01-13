//! Page/document accessibility helpers.
//!
//! Today FastRender's windowed browser UI (`feature = browser_ui`) uses AccessKit for accessibility.
//! When we surface a page/document accessibility tree via AccessKit, `accesskit::NodeId` values must
//! be stable **within** a document and must not be reused across navigations (otherwise stale
//! screen-reader action requests can be misrouted to a different node in the newly loaded page).

#[cfg(feature = "a11y_accesskit")]
mod accesskit_ids {
  use crate::ui::messages::TabId;
  use std::num::NonZeroU128;

  /// Encode a page accessibility node identity into an AccessKit [`accesskit::NodeId`].
  ///
  /// Layout (u128):
  /// - bits 127..64: tab id
  /// - bits 63..32: document generation
  /// - bits 31..0: DOM preorder node id (truncated to u32, but clamped instead of wrapped)
  ///
  /// The tab id lives in the upper 64 bits so the resulting `NodeId` is extremely unlikely to
  /// collide with egui's widget node ids (which are typically derived from a 64-bit hash and placed
  /// in the lower bits).
  pub fn encode_page_node_id(
    tab_id: TabId,
    document_generation: u32,
    dom_node_id: usize,
  ) -> accesskit::NodeId {
    let dom_node_id_u32 = if dom_node_id > u32::MAX as usize {
      u32::MAX
    } else {
      dom_node_id as u32
    };

    let value = ((tab_id.0 as u128) << 64)
      | ((document_generation as u128) << 32)
      | (dom_node_id_u32 as u128);

    // TabId is process-unique and never 0, so the packed value is non-zero.
    accesskit::NodeId(NonZeroU128::new(value).expect("packed page node id must be non-zero"))
  }

  /// Decode an AccessKit [`accesskit::NodeId`] produced by [`encode_page_node_id`].
  ///
  /// Returns `None` for node ids that do not belong to the page accessibility space (e.g. egui
  /// chrome nodes).
  pub fn decode_page_node_id(node_id: accesskit::NodeId) -> Option<(TabId, u32, usize)> {
    let value = node_id.0.get();
    let tab_id = (value >> 64) as u64;
    if tab_id == 0 {
      return None;
    }
    let generation = ((value >> 32) & 0xFFFF_FFFF) as u32;
    let dom_node_id = (value & 0xFFFF_FFFF) as u32;
    if dom_node_id == 0 {
      return None;
    }
    Some((TabId(tab_id), generation, dom_node_id as usize))
  }

  /// Helper for UI-side action routing: return a page DOM node id only when the action request
  /// targets the currently active document generation.
  pub fn dom_node_id_for_current_page_action(
    node_id: accesskit::NodeId,
    current_tab_id: TabId,
    current_document_generation: u32,
  ) -> Option<usize> {
    let (tab_id, generation, dom_node_id) = decode_page_node_id(node_id)?;
    if tab_id != current_tab_id {
      return None;
    }
    if generation != current_document_generation {
      return None;
    }
    Some(dom_node_id)
  }

  #[cfg(test)]
  mod tests {
    use super::*;
    use std::num::NonZeroU128;

    #[test]
    fn encode_decode_round_trip_includes_generation() {
      let tab_id = TabId(123);
      let gen = 42;
      let dom = 99usize;
      let node_id = encode_page_node_id(tab_id, gen, dom);
      let decoded = decode_page_node_id(node_id).expect("decode");
      assert_eq!(decoded, (tab_id, gen, dom));
    }

    #[test]
    fn node_ids_differ_across_generations() {
      let tab_id = TabId(5);
      let dom = 1usize;
      let a = encode_page_node_id(tab_id, 1, dom);
      let b = encode_page_node_id(tab_id, 2, dom);
      assert_ne!(a.0.get(), b.0.get());
    }

    #[test]
    fn action_routing_ignores_stale_generations() {
      let tab_id = TabId(7);
      let dom = 10usize;
      let current_gen = 3;
      let stale_node_id = encode_page_node_id(tab_id, current_gen - 1, dom);
      assert_eq!(
        dom_node_id_for_current_page_action(stale_node_id, tab_id, current_gen),
        None
      );
      let current_node_id = encode_page_node_id(tab_id, current_gen, dom);
      assert_eq!(
        dom_node_id_for_current_page_action(current_node_id, tab_id, current_gen),
        Some(dom)
      );
    }

    #[test]
    fn wrapper_node_ids_do_not_decode_as_page_nodes() {
      // Wrapper nodes (window/chrome/page roots) typically use small ids like `1`/`2`/`3` in the
      // lower bits. These must never be interpreted as page/content nodes.
      let wrapper = accesskit::NodeId(NonZeroU128::new(1).unwrap());
      assert_eq!(decode_page_node_id(wrapper), None);
    }

    #[test]
    fn dom_id_one_does_not_collide_with_wrapper_id_one() {
      let wrapper = accesskit::NodeId(NonZeroU128::new(1).unwrap());
      let dom_root = encode_page_node_id(TabId(1), 1, 1);
      assert_ne!(dom_root.0.get(), wrapper.0.get());
    }

    #[test]
    fn encode_decode_round_trips_for_typical_dom_ids() {
      let tab_id = TabId(9);
      let gen = 1u32;
      for dom in [1usize, 2, 42, 10_000] {
        let node_id = encode_page_node_id(tab_id, gen, dom);
        assert_eq!(decode_page_node_id(node_id), Some((tab_id, gen, dom.min(u32::MAX as usize))));
      }
    }
  }
}

#[cfg(feature = "a11y_accesskit")]
pub use accesskit_ids::{
  decode_page_node_id, dom_node_id_for_current_page_action, encode_page_node_id,
};
