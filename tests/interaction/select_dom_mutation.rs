use fastrender::dom::parse_html;
use fastrender::interaction::dom_index::DomIndex;
use fastrender::interaction::dom_mutation::activate_select_option;

#[test]
fn single_select_replaces_selection() {
  let mut dom = parse_html(
    r#"<!doctype html>
      <select id="s">
        <option id="o1" selected>One</option>
        <option id="o2">Two</option>
      </select>"#,
  )
  .unwrap();

  let index = DomIndex::build(&mut dom);
  let select_id = *index.id_by_element_id.get("s").unwrap();
  let o1_id = *index.id_by_element_id.get("o1").unwrap();
  let o2_id = *index.id_by_element_id.get("o2").unwrap();

  assert!(activate_select_option(&mut dom, select_id, o2_id, false));

  let mut index = DomIndex::build(&mut dom);
  index
    .with_node_mut(o1_id, |node| assert!(node.get_attribute_ref("selected").is_none()))
    .unwrap();
  index
    .with_node_mut(o2_id, |node| assert!(node.get_attribute_ref("selected").is_some()))
    .unwrap();
}

#[test]
fn single_select_clicking_selected_is_noop() {
  let mut dom = parse_html(
    r#"<!doctype html>
      <select id="s">
        <option id="o1" selected>One</option>
        <option id="o2">Two</option>
      </select>"#,
  )
  .unwrap();

  let index = DomIndex::build(&mut dom);
  let select_id = *index.id_by_element_id.get("s").unwrap();
  let o1_id = *index.id_by_element_id.get("o1").unwrap();
  let o2_id = *index.id_by_element_id.get("o2").unwrap();

  assert!(!activate_select_option(&mut dom, select_id, o1_id, false));

  let mut index = DomIndex::build(&mut dom);
  index
    .with_node_mut(o1_id, |node| assert!(node.get_attribute_ref("selected").is_some()))
    .unwrap();
  index
    .with_node_mut(o2_id, |node| assert!(node.get_attribute_ref("selected").is_none()))
    .unwrap();
}

#[test]
fn multiple_select_toggles_selection() {
  let mut dom = parse_html(
    r#"<!doctype html>
      <select id="s" multiple>
        <option id="o1">One</option>
        <option id="o2" selected>Two</option>
      </select>"#,
  )
  .unwrap();

  let index = DomIndex::build(&mut dom);
  let select_id = *index.id_by_element_id.get("s").unwrap();
  let o1_id = *index.id_by_element_id.get("o1").unwrap();
  let o2_id = *index.id_by_element_id.get("o2").unwrap();

  assert!(activate_select_option(&mut dom, select_id, o1_id, true));

  let mut index = DomIndex::build(&mut dom);
  index
    .with_node_mut(o1_id, |node| assert!(node.get_attribute_ref("selected").is_some()))
    .unwrap();
  index
    .with_node_mut(o2_id, |node| assert!(node.get_attribute_ref("selected").is_some()))
    .unwrap();

  // Toggle back off.
  assert!(activate_select_option(&mut dom, select_id, o1_id, true));
  let mut index = DomIndex::build(&mut dom);
  index
    .with_node_mut(o1_id, |node| assert!(node.get_attribute_ref("selected").is_none()))
    .unwrap();
  index
    .with_node_mut(o2_id, |node| assert!(node.get_attribute_ref("selected").is_some()))
    .unwrap();
}

#[test]
fn multiple_select_replacement_clears_other_selections() {
  let mut dom = parse_html(
    r#"<!doctype html>
      <select id="s" multiple>
        <option id="o1" selected>One</option>
        <option id="o2" selected>Two</option>
      </select>"#,
  )
  .unwrap();

  let index = DomIndex::build(&mut dom);
  let select_id = *index.id_by_element_id.get("s").unwrap();
  let o1_id = *index.id_by_element_id.get("o1").unwrap();
  let o2_id = *index.id_by_element_id.get("o2").unwrap();

  assert!(activate_select_option(&mut dom, select_id, o1_id, false));

  let mut index = DomIndex::build(&mut dom);
  index
    .with_node_mut(o1_id, |node| assert!(node.get_attribute_ref("selected").is_some()))
    .unwrap();
  index
    .with_node_mut(o2_id, |node| assert!(node.get_attribute_ref("selected").is_none()))
    .unwrap();

  // Already the only selected option; should be a no-op.
  assert!(!activate_select_option(&mut dom, select_id, o1_id, false));
}

#[test]
fn disabled_option_cannot_be_selected() {
  let mut dom = parse_html(
    r#"<!doctype html>
      <select id="s">
        <option id="o1" selected>One</option>
        <option id="o2" disabled>Two</option>
      </select>"#,
  )
  .unwrap();

  let index = DomIndex::build(&mut dom);
  let select_id = *index.id_by_element_id.get("s").unwrap();
  let o1_id = *index.id_by_element_id.get("o1").unwrap();
  let o2_id = *index.id_by_element_id.get("o2").unwrap();

  assert!(!activate_select_option(&mut dom, select_id, o2_id, false));

  let mut index = DomIndex::build(&mut dom);
  index
    .with_node_mut(o1_id, |node| assert!(node.get_attribute_ref("selected").is_some()))
    .unwrap();
  index
    .with_node_mut(o2_id, |node| assert!(node.get_attribute_ref("selected").is_none()))
    .unwrap();
}

#[test]
fn disabled_optgroup_blocks_option_selection() {
  let mut dom = parse_html(
    r#"<!doctype html>
      <select id="s">
        <option id="o1" selected>One</option>
        <optgroup disabled label="Group">
          <option id="o2">Two</option>
        </optgroup>
      </select>"#,
  )
  .unwrap();

  let index = DomIndex::build(&mut dom);
  let select_id = *index.id_by_element_id.get("s").unwrap();
  let o1_id = *index.id_by_element_id.get("o1").unwrap();
  let o2_id = *index.id_by_element_id.get("o2").unwrap();

  assert!(!activate_select_option(&mut dom, select_id, o2_id, false));

  let mut index = DomIndex::build(&mut dom);
  index
    .with_node_mut(o1_id, |node| assert!(node.get_attribute_ref("selected").is_some()))
    .unwrap();
  index
    .with_node_mut(o2_id, |node| assert!(node.get_attribute_ref("selected").is_none()))
    .unwrap();
}

#[test]
fn non_descendant_option_id_does_nothing() {
  let mut dom = parse_html(
    r#"<!doctype html>
      <select id="s1">
        <option id="o1" selected>One</option>
      </select>
      <select id="s2">
        <option id="o2">Two</option>
      </select>"#,
  )
  .unwrap();

  let index = DomIndex::build(&mut dom);
  let select1_id = *index.id_by_element_id.get("s1").unwrap();
  let o1_id = *index.id_by_element_id.get("o1").unwrap();
  let o2_id = *index.id_by_element_id.get("o2").unwrap();

  assert!(!activate_select_option(&mut dom, select1_id, o2_id, false));

  let mut index = DomIndex::build(&mut dom);
  index
    .with_node_mut(o1_id, |node| assert!(node.get_attribute_ref("selected").is_some()))
    .unwrap();
  index
    .with_node_mut(o2_id, |node| assert!(node.get_attribute_ref("selected").is_none()))
    .unwrap();
}
