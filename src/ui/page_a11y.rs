//! Page/document accessibility helpers.
//!
//! Today FastRender's windowed browser UI (`feature = browser_ui`) uses AccessKit for accessibility.
//! When we surface a page/document accessibility tree via AccessKit, `accesskit::NodeId` values must
//! be stable **within** a single page accessibility-tree mapping and must not be reused after the
//! mapping changes (otherwise stale screen-reader action requests can be misrouted to a different
//! node).

#[cfg(feature = "a11y_accesskit")]
mod accesskit_ids {
  use crate::accessibility::accesskit_ids::{
    accesskit_id_for_page_dom_preorder, page_dom_preorder_from_accesskit,
  };
  #[cfg(test)]
  use crate::accessibility::accesskit_ids::{accesskit_id_for_chrome_wrapper, ChromeWrapperNode};
  use crate::ui::messages::TabId;

  /// Encode a page accessibility node identity into an AccessKit [`accesskit::NodeId`].
  ///
  /// This uses FastRender's marker+namespace scheme (see
  /// [`crate::accessibility::accesskit_ids`]) so page node ids can coexist with:
  /// - compositor wrapper nodes,
  /// - future renderer-chrome DOM nodes,
  /// - dom2/JS-backed node ids,
  /// - egui widget node ids,
  /// without collisions.
  pub fn encode_page_node_id(
    tab_id: TabId,
    tree_generation: u32,
    dom_node_id: usize,
  ) -> accesskit::NodeId {
    accesskit_id_for_page_dom_preorder(tab_id.0, tree_generation, dom_node_id)
  }

  /// Decode an AccessKit [`accesskit::NodeId`] produced by [`encode_page_node_id`].
  ///
  /// Returns `None` for node ids that do not belong to the page accessibility namespace (e.g. egui
  /// chrome widgets or compositor wrapper nodes).
  pub fn decode_page_node_id(node_id: accesskit::NodeId) -> Option<(TabId, u32, usize)> {
    let (tab_id, tree_generation, dom_node_id) = page_dom_preorder_from_accesskit(node_id)?;
    Some((TabId(tab_id), tree_generation, dom_node_id))
  }

  /// Helper for UI-side action routing: return a page DOM node id only when the action request
  /// targets the currently active page accessibility-tree generation.
  pub fn dom_node_id_for_current_page_action(
    node_id: accesskit::NodeId,
    current_tab_id: TabId,
    current_tree_generation: u32,
  ) -> Option<usize> {
    let (tab_id, tree_generation, dom_node_id) = decode_page_node_id(node_id)?;
    if tab_id != current_tab_id {
      return None;
    }
    if tree_generation != current_tree_generation {
      return None;
    }
    Some(dom_node_id)
  }

  #[cfg(test)]
  mod tests {
    use super::*;

    #[test]
    fn page_ids_do_not_collide_with_wrapper_ids() {
      let wrappers = [
        accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Window),
        accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Chrome),
        accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Page),
      ];

      let tab_id = TabId(1);
      let gen = 1;
      for dom in 1usize..=3 {
        let node_id = encode_page_node_id(tab_id, gen, dom);
        for wrapper in wrappers {
          assert_ne!(node_id.0.get(), wrapper.0.get());
        }
      }
    }

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
    fn stale_action_requests_are_ignored_after_tree_generation_increments() {
      use std::collections::HashMap;

      #[derive(Debug, Clone, Copy, PartialEq, Eq)]
      enum SemanticElement {
        Button,
        Link,
      }

      let tab_id = TabId(42);
      let dom_node_id = 7usize;

      // Tree A: dom_node_id=7 is a button.
      let tree_a_gen = 1u32;
      let mut tree_a = HashMap::new();
      tree_a.insert(dom_node_id, SemanticElement::Button);

      // Tree B: dom_node_id=7 now refers to a different element.
      let tree_b_gen = 2u32;
      let mut tree_b = HashMap::new();
      tree_b.insert(dom_node_id, SemanticElement::Link);

      let old_target = encode_page_node_id(tab_id, tree_a_gen, dom_node_id);
      let old_request = accesskit::ActionRequest {
        action: accesskit::Action::Default,
        target: old_target,
        data: None,
      };

      // If we interpreted this stale action request against Tree B without generation checks, we'd
      // target the wrong element. Ensure generation filtering rejects it.
      let resolved = dom_node_id_for_current_page_action(old_request.target, tab_id, tree_b_gen)
        .and_then(|id| tree_b.get(&id).copied());
      assert_eq!(resolved, None);

      let new_target = encode_page_node_id(tab_id, tree_b_gen, dom_node_id);
      let new_request = accesskit::ActionRequest {
        action: accesskit::Action::Default,
        target: new_target,
        data: None,
      };
      let resolved = dom_node_id_for_current_page_action(new_request.target, tab_id, tree_b_gen)
        .and_then(|id| tree_b.get(&id).copied());
      assert_eq!(resolved, Some(SemanticElement::Link));

      // Sanity: Tree A still maps dom_node_id=7 to the original element type.
      assert_eq!(tree_a.get(&dom_node_id).copied(), Some(SemanticElement::Button));
    }

    #[test]
    fn wrapper_node_ids_do_not_decode_as_page_nodes() {
      for wrapper in [
        accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Window),
        accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Chrome),
        accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Page),
      ] {
        assert_eq!(decode_page_node_id(wrapper), None);
      }
    }

    #[test]
    fn dom_id_one_does_not_collide_with_wrapper_id_one() {
      let wrapper = accesskit_id_for_chrome_wrapper(ChromeWrapperNode::Window);
      let dom_root = encode_page_node_id(TabId(1), 1, 1);
      assert_ne!(dom_root.0.get(), wrapper.0.get());
    }

    #[test]
    fn encode_decode_round_trips_for_typical_dom_ids() {
      let tab_id = TabId(9);
      let gen = 1u32;
      for dom in [1usize, 2, 42, 10_000] {
        let node_id = encode_page_node_id(tab_id, gen, dom);
        assert_eq!(
          decode_page_node_id(node_id),
          Some((tab_id, gen, dom.min(u32::MAX as usize)))
        );
      }
    }
  }
}

#[cfg(feature = "a11y_accesskit")]
pub use accesskit_ids::{
  decode_page_node_id, dom_node_id_for_current_page_action, encode_page_node_id,
};
