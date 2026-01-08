use crate::dom::enumerate_dom_ids;
use crate::dom::DomNode;
use crate::dom::DomNodeType;
use crate::geometry::Point;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::BoxTree;
use crate::tree::fragment_tree::FragmentTree;
use std::collections::HashMap;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputModality {
  Pointer,
  Keyboard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractionAction {
  None,
  Navigate { href: String },
  FocusChanged { node_id: Option<usize> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
  Backspace,
  Enter,
  Tab,
}

#[derive(Debug, Clone)]
pub struct InteractionEngine {
  hover_chain: Vec<usize>,
  active_chain: Vec<usize>,
  pointer_down_target: Option<usize>,
  focused: Option<usize>,
  modality: InputModality,
}

struct DomIndexMut {
  id_to_node: Vec<*mut DomNode>,
  parent: Vec<usize>,
}

impl DomIndexMut {
  fn new(root: &mut DomNode) -> Self {
    // Node ids are pre-order traversal indices, matching `crate::dom::enumerate_dom_ids`.
    let mut id_to_node: Vec<*mut DomNode> = vec![std::ptr::null_mut()];
    let mut parent: Vec<usize> = vec![0];

    // (node_ptr, parent_id)
    let mut stack: Vec<(*mut DomNode, usize)> = vec![(root as *mut DomNode, 0)];
    let mut next_id = 1usize;

    while let Some((node_ptr, parent_id)) = stack.pop() {
      let id = next_id;
      next_id = next_id.saturating_add(1);

      id_to_node.push(node_ptr);
      parent.push(parent_id);

      // SAFETY: We never mutate `children` while this traversal runs, so raw pointers remain valid.
      let node = unsafe { &mut *node_ptr };
      for child in node.children.iter_mut().rev() {
        stack.push((child as *mut DomNode, id));
      }
    }

    Self { id_to_node, parent }
  }

  fn node(&self, node_id: usize) -> Option<&DomNode> {
    let ptr = self.id_to_node.get(node_id).copied()?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: Pointers are built from a live `DomNode` tree.
    Some(unsafe { &*ptr })
  }

  fn node_mut(&mut self, node_id: usize) -> Option<&mut DomNode> {
    let ptr = self.id_to_node.get(node_id).copied()?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: We only produce a temporary mutable reference for the current call site.
    Some(unsafe { &mut *ptr })
  }
}

fn build_box_to_dom_map(box_tree: &BoxTree) -> Vec<Option<usize>> {
  let mut map: Vec<Option<usize>> = vec![None; 1];
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];

  while let Some(node) = stack.pop() {
    if node.id >= map.len() {
      map.resize(node.id + 1, None);
    }
    map[node.id] = node.styled_node_id;

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  map
}

fn nearest_element_ancestor(index: &DomIndexMut, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = index.node(node_id)?;
    if node.is_element() {
      return Some(node_id);
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  None
}

fn hit_test_deepest_element(
  index: &DomIndexMut,
  box_to_dom: &[Option<usize>],
  fragment_tree: &FragmentTree,
  page_point: Point,
) -> Option<usize> {
  for fragment in fragment_tree.hit_test(page_point) {
    let Some(box_id) = fragment.box_id() else {
      continue;
    };
    let Some(dom_id) = box_to_dom.get(box_id).and_then(|id| *id) else {
      continue;
    };
    if let Some(element_id) = nearest_element_ancestor(index, dom_id) {
      return Some(element_id);
    }
  }
  None
}

fn collect_element_chain(index: &DomIndexMut, start: usize) -> Vec<usize> {
  let mut chain = Vec::new();
  let mut current = Some(start);
  while let Some(id) = current {
    if id == 0 {
      break;
    }
    if index.node(id).is_some_and(DomNode::is_element) {
      chain.push(id);
    }
    current = index.parent.get(id).copied();
  }
  chain
}

fn set_attr(attrs: &mut Vec<(String, String)>, name: &str, value: &str) -> bool {
  if let Some((_, existing)) = attrs.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
    if existing == value {
      return false;
    }
    existing.clear();
    existing.push_str(value);
    return true;
  }
  attrs.push((name.to_string(), value.to_string()));
  true
}

fn remove_attr(attrs: &mut Vec<(String, String)>, name: &str) -> bool {
  let Some(idx) = attrs.iter().position(|(k, _)| k.eq_ignore_ascii_case(name)) else {
    return false;
  };
  attrs.remove(idx);
  true
}

fn set_node_attr(node: &mut DomNode, name: &str, value: &str) -> bool {
  match &mut node.node_type {
    DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => {
      set_attr(attributes, name, value)
    }
    _ => false,
  }
}

fn remove_node_attr(node: &mut DomNode, name: &str) -> bool {
  match &mut node.node_type {
    DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => {
      remove_attr(attributes, name)
    }
    _ => false,
  }
}

fn set_data_flag(index: &mut DomIndexMut, node_id: usize, name: &str, on: bool) -> bool {
  let Some(node) = index.node_mut(node_id) else {
    return false;
  };
  if on {
    set_node_attr(node, name, "true")
  } else {
    remove_node_attr(node, name)
  }
}

fn set_bool_attr(index: &mut DomIndexMut, node_id: usize, name: &str, on: bool) -> bool {
  let Some(node) = index.node_mut(node_id) else {
    return false;
  };
  if on {
    // Boolean HTML attributes: presence is significant; value is ignored.
    set_node_attr(node, name, "")
  } else {
    remove_node_attr(node, name)
  }
}

fn diff_flag_chain(
  index: &mut DomIndexMut,
  attr: &str,
  old_chain: &[usize],
  new_chain: &[usize],
) -> bool {
  let mut changed = false;
  for id in old_chain.iter().copied() {
    if !new_chain.contains(&id) {
      changed |= set_data_flag(index, id, attr, false);
    }
  }
  for id in new_chain.iter().copied() {
    if !old_chain.contains(&id) {
      changed |= set_data_flag(index, id, attr, true);
    }
  }
  changed
}

fn is_anchor_with_href(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("a"))
    && node
      .get_attribute_ref("href")
      .is_some_and(|href| !href.trim().is_empty())
}

fn is_label(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("label"))
}

fn is_input(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
}

fn is_textarea(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("textarea"))
}

fn input_type(node: &DomNode) -> &str {
  node.get_attribute_ref("type").unwrap_or("text")
}

fn is_checkbox_input(node: &DomNode) -> bool {
  is_input(node) && input_type(node).eq_ignore_ascii_case("checkbox")
}

fn is_radio_input(node: &DomNode) -> bool {
  is_input(node) && input_type(node).eq_ignore_ascii_case("radio")
}

fn is_text_input(node: &DomNode) -> bool {
  if !is_input(node) {
    return false;
  }

  let t = input_type(node);
  // MVP heuristic: treat any non-button-ish, non-choice-ish input as a text control.
  !t.eq_ignore_ascii_case("checkbox")
    && !t.eq_ignore_ascii_case("radio")
    && !t.eq_ignore_ascii_case("button")
    && !t.eq_ignore_ascii_case("submit")
    && !t.eq_ignore_ascii_case("reset")
    && !t.eq_ignore_ascii_case("hidden")
    && !t.eq_ignore_ascii_case("range")
    && !t.eq_ignore_ascii_case("color")
    && !t.eq_ignore_ascii_case("file")
    && !t.eq_ignore_ascii_case("image")
}

fn is_focusable_text_control(node: &DomNode) -> bool {
  is_textarea(node) || is_text_input(node)
}

fn semantic_target(index: &DomIndexMut, deepest_element: usize) -> usize {
  let mut current = deepest_element;
  loop {
    let Some(node) = index.node(current) else {
      return deepest_element;
    };
    if is_label(node) || is_input(node) || is_textarea(node) || is_anchor_with_href(node) {
      return current;
    }
    let parent = *index.parent.get(current).unwrap_or(&0);
    if parent == 0 {
      return deepest_element;
    }
    current = parent;
  }
}

fn is_ancestor_or_self(index: &DomIndexMut, ancestor: usize, mut node: usize) -> bool {
  while node != 0 {
    if node == ancestor {
      return true;
    }
    node = *index.parent.get(node).unwrap_or(&0);
  }
  false
}

fn resolve_url(base_url: &str, href: &str) -> Option<String> {
  let href = href.trim();
  if href.is_empty() {
    return None;
  }
  if href.len() >= 11 && href[..11].eq_ignore_ascii_case("javascript:") {
    return None;
  }

  if let Ok(base) = Url::parse(base_url) {
    if let Ok(joined) = base.join(href) {
      if joined.scheme().eq_ignore_ascii_case("javascript") {
        return None;
      }
      return Some(joined.to_string());
    }
  }

  let absolute = Url::parse(href).ok()?;
  (!absolute.scheme().eq_ignore_ascii_case("javascript")).then(|| absolute.to_string())
}

fn find_element_by_id_attr(index: &DomIndexMut, html_id: &str) -> Option<usize> {
  for (node_id, ptr) in index.id_to_node.iter().copied().enumerate().skip(1) {
    if ptr.is_null() {
      continue;
    }
    let node = unsafe { &*ptr };
    if !node.is_element() {
      continue;
    }
    if node.get_attribute_ref("id") == Some(html_id) {
      return Some(node_id);
    }
  }
  None
}

fn find_label_associated_control(index: &DomIndexMut, label_id: usize) -> Option<usize> {
  let label = index.node(label_id)?;
  if !is_label(label) {
    return None;
  }

  if let Some(for_attr) = label
    .get_attribute_ref("for")
    .map(str::trim)
    .filter(|v| !v.is_empty())
  {
    // Spec-ish: `for` matches element IDs in the same tree.
    return find_element_by_id_attr(index, for_attr);
  }

  // Fallback: first descendant control.
  // Pre-order ids are contiguous for a subtree, so scan forward until we leave the label subtree.
  let mut end = label_id;
  for id in (label_id + 1)..index.id_to_node.len() {
    if is_ancestor_or_self(index, label_id, id) {
      end = id;
    } else {
      break;
    }
  }
  for id in (label_id + 1)..=end {
    let Some(node) = index.node(id) else {
      continue;
    };
    if is_input(node)
      || is_textarea(node)
      || node
        .tag_name()
        .is_some_and(|t| t.eq_ignore_ascii_case("select"))
    {
      return Some(id);
    }
  }

  None
}

fn collect_text_children_value(node: &DomNode) -> String {
  let mut value = String::new();
  for child in &node.children {
    if let DomNodeType::Text { content } = &child.node_type {
      value.push_str(content);
    }
  }
  value
}

/// Set textarea value by updating (existing) text children. Returns `(changed, inserted_child)`.
fn set_textarea_text_children_value(node: &mut DomNode, value: &str) -> (bool, bool) {
  let mut found_text = false;
  let mut changed = false;

  for child in node.children.iter_mut() {
    if let DomNodeType::Text { content } = &mut child.node_type {
      if !found_text {
        found_text = true;
        if content != value {
          content.clear();
          content.push_str(value);
          changed = true;
        }
      } else if !content.is_empty() {
        content.clear();
        changed = true;
      }
    }
  }

  if found_text {
    return (changed, false);
  }

  // No existing text node. Insert one; this changes pre-order ids for subsequent nodes.
  node.children.push(DomNode {
    node_type: DomNodeType::Text {
      content: value.to_string(),
    },
    children: Vec::new(),
  });
  (true, true)
}

impl InteractionEngine {
  pub fn new() -> Self {
    Self {
      hover_chain: Vec::new(),
      active_chain: Vec::new(),
      pointer_down_target: None,
      focused: None,
      modality: InputModality::Pointer,
    }
  }

  fn set_focus(
    &mut self,
    index: &mut DomIndexMut,
    new_focused: Option<usize>,
    focus_visible: bool,
  ) -> bool {
    let mut changed = false;
    if self.focused != new_focused {
      if let Some(old) = self.focused {
        changed |= set_data_flag(index, old, "data-fastr-focus", false);
        changed |= set_data_flag(index, old, "data-fastr-focus-visible", false);
      }
    }

    if let Some(new_id) = new_focused {
      changed |= set_data_flag(index, new_id, "data-fastr-focus", true);
      changed |= set_data_flag(index, new_id, "data-fastr-focus-visible", focus_visible);
    }

    self.focused = new_focused;
    changed
  }

  /// Update hover state (data-fastr-hover on target + ancestors).
  pub fn pointer_move(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    page_point: Point,
  ) -> bool {
    let mut index = DomIndexMut::new(dom);
    let box_to_dom = build_box_to_dom_map(box_tree);

    let new_chain = hit_test_deepest_element(&index, &box_to_dom, fragment_tree, page_point)
      .map(|target| collect_element_chain(&index, target))
      .unwrap_or_default();

    let changed = diff_flag_chain(
      &mut index,
      "data-fastr-hover",
      &self.hover_chain,
      &new_chain,
    );
    self.hover_chain = new_chain;
    changed
  }

  /// Begin active state (data-fastr-active on target + ancestors) and set modality=Pointer.
  pub fn pointer_down(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    page_point: Point,
  ) -> bool {
    self.modality = InputModality::Pointer;

    let mut index = DomIndexMut::new(dom);
    let box_to_dom = build_box_to_dom_map(box_tree);

    let down_target = hit_test_deepest_element(&index, &box_to_dom, fragment_tree, page_point)
      .map(|deepest| semantic_target(&index, deepest));
    let new_chain = down_target
      .map(|target| collect_element_chain(&index, target))
      .unwrap_or_default();

    let changed = diff_flag_chain(
      &mut index,
      "data-fastr-active",
      &self.active_chain,
      &new_chain,
    );
    self.active_chain = new_chain;
    self.pointer_down_target = down_target;
    changed
  }

  fn remap_engine_ids_after_dom_change(
    &mut self,
    old_index: &DomIndexMut,
    new_ids: &HashMap<*const DomNode, usize>,
  ) {
    fn remap_vec(
      ids: &mut Vec<usize>,
      old_index: &DomIndexMut,
      new_ids: &HashMap<*const DomNode, usize>,
    ) {
      for id in ids.iter_mut() {
        let Some(ptr) = old_index.id_to_node.get(*id).copied() else {
          continue;
        };
        if ptr.is_null() {
          continue;
        }
        if let Some(new_id) = new_ids.get(&(ptr as *const DomNode)) {
          *id = *new_id;
        }
      }
    }
    fn remap_opt(
      id: &mut Option<usize>,
      old_index: &DomIndexMut,
      new_ids: &HashMap<*const DomNode, usize>,
    ) {
      let Some(old) = *id else { return };
      let Some(ptr) = old_index.id_to_node.get(old).copied() else {
        *id = None;
        return;
      };
      if ptr.is_null() {
        *id = None;
        return;
      }
      *id = new_ids.get(&(ptr as *const DomNode)).copied();
    }

    remap_vec(&mut self.hover_chain, old_index, new_ids);
    remap_vec(&mut self.active_chain, old_index, new_ids);
    remap_opt(&mut self.pointer_down_target, old_index, new_ids);
    remap_opt(&mut self.focused, old_index, new_ids);
  }

  /// End active state, and if click qualifies, perform action:
  /// - link: return Navigate
  /// - checkbox/radio: toggle/activate
  /// - text control/textarea: focus
  pub fn pointer_up(
    &mut self,
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    page_point: Point,
    base_url: &str,
  ) -> (bool, InteractionAction) {
    let prev_focus = self.focused;

    let mut index = DomIndexMut::new(dom);
    let box_to_dom = build_box_to_dom_map(box_tree);

    let up_deepest = hit_test_deepest_element(&index, &box_to_dom, fragment_tree, page_point);
    let up_semantic = up_deepest.map(|deepest| semantic_target(&index, deepest));

    let down_semantic = self.pointer_down_target;

    // Clear active chain unconditionally.
    let mut dom_changed = false;
    for id in self.active_chain.iter().copied() {
      dom_changed |= set_data_flag(&mut index, id, "data-fastr-active", false);
    }
    self.active_chain.clear();
    self.pointer_down_target = None;

    let click_qualifies = match (down_semantic, up_semantic) {
      (Some(down), Some(up)) => down == up || is_ancestor_or_self(&index, down, up),
      (None, None) => true,
      _ => false,
    };

    let mut action = InteractionAction::None;

    let mut click_target = if click_qualifies { down_semantic } else { None };
    if let Some(target_id) = click_target {
      if index.node(target_id).is_some_and(is_label) {
        if let Some(control) = find_label_associated_control(&index, target_id) {
          click_target = Some(control);
        }
      }
    }

    if click_qualifies {
      if let Some(target_id) = click_target {
        let anchor_href = index.node(target_id).and_then(|node| {
          is_anchor_with_href(node)
            .then(|| node.get_attribute_ref("href").unwrap_or("").to_string())
        });

        if let Some(href) = anchor_href {
          if let Some(resolved) = resolve_url(base_url, &href) {
            dom_changed |= set_data_flag(&mut index, target_id, "data-fastr-visited", true);
            action = InteractionAction::Navigate { href: resolved };
          }
        } else if index.node(target_id).is_some_and(is_checkbox_input) {
          let checked = index
            .node(target_id)
            .and_then(|node| node.get_attribute_ref("checked"))
            .is_some();
          dom_changed |= set_bool_attr(&mut index, target_id, "checked", !checked);
        } else if index.node(target_id).is_some_and(is_radio_input) {
          let name = index
            .node(target_id)
            .and_then(|node| node.get_attribute_ref("name"))
            .map(str::to_string);
          if let Some(name) = name.as_deref() {
            for id in 1..index.id_to_node.len() {
              let should_clear = index.node(id).is_some_and(|radio| {
                is_radio_input(radio) && radio.get_attribute_ref("name") == Some(name)
              });
              if should_clear {
                dom_changed |= set_bool_attr(&mut index, id, "checked", false);
              }
            }
          }
          dom_changed |= set_bool_attr(&mut index, target_id, "checked", true);
        } else if index.node(target_id).is_some_and(is_focusable_text_control) {
          dom_changed |= self.set_focus(&mut index, Some(target_id), false);
        }
      }

      // Blur when clicking outside focusable controls.
      let clicked_focusable =
        click_target.is_some_and(|id| index.node(id).is_some_and(is_focusable_text_control));
      if !clicked_focusable && prev_focus.is_some() {
        dom_changed |= self.set_focus(&mut index, None, false);
      }
    }

    if !matches!(action, InteractionAction::Navigate { .. }) && self.focused != prev_focus {
      action = InteractionAction::FocusChanged {
        node_id: self.focused,
      };
    }

    (dom_changed, action)
  }

  /// Insert typed text into focused text control (input/textarea) and set focus-visible.
  pub fn text_input(&mut self, dom: &mut DomNode, text: &str) -> bool {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.focused else {
      return false;
    };

    let mut index = DomIndexMut::new(dom);

    let mut changed = false;
    changed |= self.set_focus(&mut index, Some(focused), true);

    if index.node(focused).is_some_and(is_text_input) {
      let current = index
        .node(focused)
        .and_then(|node| node.get_attribute_ref("value"))
        .unwrap_or("")
        .to_string();
      let mut next = current;
      next.push_str(text);
      changed |= set_node_attr(
        index.node_mut(focused).expect("node exists"),
        "value",
        &next,
      );
      return changed;
    }

    if index.node(focused).is_some_and(is_textarea) {
      let current = index
        .node(focused)
        .map(collect_text_children_value)
        .unwrap_or_default();
      let mut next = current;
      next.push_str(text);

      let old_index = index;
      let mut index = DomIndexMut::new(dom);
      let Some(node_mut) = index.node_mut(focused) else {
        return changed;
      };
      let (changed_value, inserted) = set_textarea_text_children_value(node_mut, &next);
      changed |= changed_value;
      if inserted {
        let new_ids = enumerate_dom_ids(dom);
        self.remap_engine_ids_after_dom_change(&old_index, &new_ids);
      }
      return changed;
    }

    changed
  }

  /// Handle special keys: Backspace, Enter (textarea newline only), Tab (optional: focus traversal stub).
  pub fn key_action(&mut self, dom: &mut DomNode, key: KeyAction) -> bool {
    self.modality = InputModality::Keyboard;
    let Some(focused) = self.focused else {
      return false;
    };

    let mut index = DomIndexMut::new(dom);
    let mut changed = false;
    changed |= self.set_focus(&mut index, Some(focused), true);

    match key {
      KeyAction::Backspace => {
        if index.node(focused).is_some_and(is_text_input) {
          let current = index
            .node(focused)
            .and_then(|node| node.get_attribute_ref("value"))
            .unwrap_or("")
            .to_string();
          let mut next = current;
          if next.pop().is_some() {
            changed |= set_node_attr(
              index.node_mut(focused).expect("node exists"),
              "value",
              &next,
            );
          }
        } else if index.node(focused).is_some_and(is_textarea) {
          let current = index
            .node(focused)
            .map(collect_text_children_value)
            .unwrap_or_default();
          let mut next = current;
          if next.pop().is_some() {
            let old_index = index;
            let mut index = DomIndexMut::new(dom);
            if let Some(node_mut) = index.node_mut(focused) {
              let (changed_value, inserted) = set_textarea_text_children_value(node_mut, &next);
              changed |= changed_value;
              if inserted {
                let new_ids = enumerate_dom_ids(dom);
                self.remap_engine_ids_after_dom_change(&old_index, &new_ids);
              }
            }
          }
        }
      }
      KeyAction::Enter => {
        if index.node(focused).is_some_and(is_textarea) {
          let current = index
            .node(focused)
            .map(collect_text_children_value)
            .unwrap_or_default();
          let mut next = current;
          next.push('\n');
          let old_index = index;
          let mut index = DomIndexMut::new(dom);
          if let Some(node_mut) = index.node_mut(focused) {
            let (changed_value, inserted) = set_textarea_text_children_value(node_mut, &next);
            changed |= changed_value;
            if inserted {
              let new_ids = enumerate_dom_ids(dom);
              self.remap_engine_ids_after_dom_change(&old_index, &new_ids);
            }
          }
        }
      }
      KeyAction::Tab => {
        // Focus traversal is intentionally left as a stub for MVP.
      }
    }

    changed
  }
}
