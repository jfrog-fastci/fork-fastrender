use fastrender::api::{FastRender, RenderOptions};
use fastrender::dom::{enumerate_dom_ids, DomNode, DomNodeType};
use fastrender::error::Result;
use fastrender::interaction::InteractionState;
use fastrender::tree::box_tree::{FormControl, FormControlKind, ReplacedType};
use fastrender::{BoxNode, BoxType, BrowserDocument};

fn find_node_mut<'a>(
  node: &'a mut DomNode,
  predicate: &impl Fn(&DomNode) -> bool,
) -> Option<&'a mut DomNode> {
  if predicate(node) {
    return Some(node);
  }
  for child in node.children.iter_mut() {
    if let Some(found) = find_node_mut(child, predicate) {
      return Some(found);
    }
  }
  None
}

fn find_node<'a>(node: &'a DomNode, predicate: &impl Fn(&DomNode) -> bool) -> Option<&'a DomNode> {
  if predicate(node) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_node(child, predicate) {
      return Some(found);
    }
  }
  None
}

fn set_attribute(node: &mut DomNode, name: &str, value: &str) {
  match &mut node.node_type {
    DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => {
      if let Some((_, existing)) = attributes
        .iter_mut()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
      {
        *existing = value.to_string();
      } else {
        attributes.push((name.to_string(), value.to_string()));
      }
    }
    DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. } | DomNodeType::Text { .. } => {}
  }
}

fn find_first_form_control<'a>(node: &'a BoxNode) -> Option<&'a FormControl> {
  if let BoxType::Replaced(replaced) = &node.box_type {
    if let ReplacedType::FormControl(control) = &replaced.replaced_type {
      return Some(control);
    }
  }
  if let Some(body) = node.footnote_body.as_deref() {
    if let Some(found) = find_first_form_control(body) {
      return Some(found);
    }
  }
  for child in &node.children {
    if let Some(found) = find_first_form_control(child) {
      return Some(found);
    }
  }
  None
}

#[test]
fn prepare_dom_with_options_round_trips_focus_state() -> Result<()> {
  let renderer = FastRender::new()?;
  let html = r#"<input id="target" type="text" />"#;
  let mut doc = BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(64, 64))?;
  let ids = enumerate_dom_ids(doc.dom());
  let input = find_node(doc.dom(), &|node| {
    node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
      && node
        .get_attribute_ref("id")
        .is_some_and(|id| id == "target")
  })
  .expect("input element");
  let input_id = *ids.get(&(input as *const DomNode)).expect("node id");

  let mut interaction_state = InteractionState::default();
  interaction_state.focused = Some(input_id);
  interaction_state.set_focus_chain(vec![input_id]);
  let _frame =
    doc.render_frame_with_scroll_state_and_interaction_state(Some(&interaction_state))?;
  let prepared = doc.prepared().expect("prepared");
  let control =
    find_first_form_control(&prepared.box_tree().root).expect("form control replaced box");
  assert!(control.focused);
  Ok(())
}

#[test]
fn prepare_dom_with_options_round_trips_text_value_attribute() -> Result<()> {
  let mut renderer = FastRender::new()?;
  let mut dom = renderer.parse_html(r#"<input id="target" type="text" value="before" />"#)?;

  let input = find_node_mut(&mut dom, &|node| {
    node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
      && node
        .get_attribute_ref("id")
        .is_some_and(|id| id == "target")
  })
  .expect("input element");
  set_attribute(input, "value", "after");

  let report =
    renderer.prepare_dom_with_options(dom, None, RenderOptions::new().with_viewport(64, 64))?;
  let control =
    find_first_form_control(&report.document.box_tree().root).expect("form control replaced box");
  match &control.control {
    FormControlKind::Text { value, .. } => assert_eq!(value, "after"),
    other => panic!("expected text form control, got: {other:?}"),
  }
  Ok(())
}
