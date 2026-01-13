use crate::chrome_frame::ChromeFrameDocument;
use crate::chrome_frame::ChromeFrameEvent;
use crate::dom::DomNode;
use crate::geometry::Point;
use crate::interaction::absolute_bounds_by_styled_node_id;
use crate::text::font_db::FontConfig;
use crate::{FastRender, RenderOptions, Result};

fn find_preorder_id_by_id_attr(root: &DomNode, id: &str) -> Option<usize> {
  let mut next_id = 1usize;
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    let current_id = next_id;
    next_id += 1;
    if node
      .is_element()
      .then(|| node.get_attribute_ref("id"))
      .flatten()
      .is_some_and(|value| value == id)
    {
      return Some(current_id);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn input_value_by_id(root: &DomNode, id: &str) -> Option<String> {
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node
      .is_element()
      .then(|| node.get_attribute_ref("id"))
      .flatten()
      .is_some_and(|value| value == id)
    {
      return node.get_attribute_ref("value").map(|value| value.to_string());
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn click_point_for_styled_node(chrome: &ChromeFrameDocument, node_id: usize) -> Point {
  let prepared = chrome
    .document()
    .prepared()
    .expect("chrome frame should have cached layout after render");
  let bounds = absolute_bounds_by_styled_node_id(prepared.box_tree(), prepared.fragment_tree());
  let rect = bounds
    .get(&node_id)
    .expect("expected bounds for target styled node");
  Point::new(
    rect.origin.x + rect.size.width * 0.5,
    rect.origin.y + rect.size.height * 0.5,
  )
}

#[test]
fn chrome_frame_address_bar_supports_clipboard_and_ime() -> Result<()> {
  let renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .build()?;

  let mut chrome = ChromeFrameDocument::new(renderer, RenderOptions::new().with_viewport(360, 40))?;
  chrome.render()?;

  let address_id = find_preorder_id_by_id_attr(chrome.document().dom(), "address-bar")
    .expect("expected #address-bar element id");
  let click_point = click_point_for_styled_node(&chrome, address_id);

  let outcome = chrome.click_viewport_point(click_point)?;
  assert!(outcome.changed, "expected click to update chrome interaction state");
  assert_eq!(
    chrome.interaction_state().focused,
    Some(address_id),
    "expected click to focus #address-bar"
  );

  assert_eq!(
    chrome.text_input("hello"),
    vec![ChromeFrameEvent::AddressBarTextChanged("hello".to_string())]
  );
  assert_eq!(
    input_value_by_id(chrome.document().dom(), "address-bar").as_deref(),
    Some("hello"),
    "expected typed text to update <input value>"
  );

  assert!(chrome.select_all(), "expected Ctrl+A to select address bar text");
  let edit = chrome
    .interaction_state()
    .text_edit_for(address_id)
    .expect("expected text edit state for focused input");
  assert_eq!(edit.selection, Some((0, 5)), "expected selection to cover full input");

  assert_eq!(
    chrome.copy().as_deref(),
    Some("hello"),
    "expected Ctrl+C to copy selected text"
  );

  assert_eq!(
    chrome.paste("world"),
    vec![ChromeFrameEvent::AddressBarTextChanged("world".to_string())]
  );
  assert_eq!(
    input_value_by_id(chrome.document().dom(), "address-bar").as_deref(),
    Some("world"),
    "expected paste to mutate input value"
  );

  // IME preedit should update interaction state without mutating the DOM value.
  assert!(chrome.ime_preedit("あ", Some((1, 1))));
  assert_eq!(
    input_value_by_id(chrome.document().dom(), "address-bar").as_deref(),
    Some("world"),
    "expected IME preedit not to mutate committed value"
  );
  let preedit = chrome
    .interaction_state()
    .ime_preedit
    .as_ref()
    .expect("expected IME preedit state to be set");
  assert_eq!(preedit.node_id, address_id);
  assert_eq!(preedit.text, "あ");
  assert_eq!(preedit.cursor, Some((1, 1)));

  assert_eq!(
    chrome.ime_commit("い"),
    vec![ChromeFrameEvent::AddressBarTextChanged("worldい".to_string())]
  );
  assert!(chrome.interaction_state().ime_preedit.is_none());
  assert_eq!(
    input_value_by_id(chrome.document().dom(), "address-bar").as_deref(),
    Some("worldい"),
    "expected committed IME text to be inserted into the input value"
  );

  assert!(chrome.ime_preedit("x", Some((1, 1))));
  assert!(chrome.ime_cancel());
  assert!(chrome.interaction_state().ime_preedit.is_none());
  assert_eq!(
    input_value_by_id(chrome.document().dom(), "address-bar").as_deref(),
    Some("worldい"),
    "expected IME cancel not to mutate committed value"
  );

  // Ensure rendering works with the updated caret/selection/preedit state (no panics).
  chrome.render()?;

  Ok(())
}
