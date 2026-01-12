use fastrender::debug::runtime::{set_runtime_toggles, RuntimeToggles};
use fastrender::dom::{DomNode, DomNodeType};
use fastrender::style::defaults::get_default_styles_for_element;
use fastrender::Length;
use std::collections::HashMap;
use std::sync::Arc;

const ENV_COMPAT_REPLACED_MAX_WIDTH_100: &str = "FASTR_COMPAT_REPLACED_MAX_WIDTH_100";
const REPLACED_ELEMENTS: [&str; 7] = [
  "img", "video", "audio", "canvas", "iframe", "embed", "object",
];

fn element(tag_name: &str) -> DomNode {
  DomNode {
    node_type: DomNodeType::Element {
      tag_name: tag_name.to_string(),
      namespace: "".to_string(),
      attributes: Vec::new(),
    },
    children: Vec::new(),
  }
}

#[test]
fn replaced_element_max_width_100_is_disabled_by_default() {
  let _guard_default = set_runtime_toggles(Arc::new(RuntimeToggles::from_map(HashMap::new())));

  for tag in REPLACED_ELEMENTS {
    let node = element(tag);
    let styles = get_default_styles_for_element(&node);
    assert_eq!(styles.max_width, None, "expected no max-width for <{tag}>");
  }
}

#[test]
fn replaced_element_max_width_100_can_be_toggled() {
  let _guard_off = set_runtime_toggles(Arc::new(RuntimeToggles::from_map(HashMap::from([(
    ENV_COMPAT_REPLACED_MAX_WIDTH_100.to_string(),
    "0".to_string(),
  )]))));

  for tag in REPLACED_ELEMENTS {
    let node = element(tag);
    let styles = get_default_styles_for_element(&node);
    assert_eq!(styles.max_width, None, "expected no max-width for <{tag}>");
  }

  drop(_guard_off);

  let _guard_on = set_runtime_toggles(Arc::new(RuntimeToggles::from_map(HashMap::from([(
    ENV_COMPAT_REPLACED_MAX_WIDTH_100.to_string(),
    "1".to_string(),
  )]))));

  for tag in REPLACED_ELEMENTS {
    let node = element(tag);
    let styles = get_default_styles_for_element(&node);
    assert_eq!(
      styles.max_width,
      Some(Length::percent(100.0)),
      "expected max-width: 100% for <{tag}>"
    );
  }
}
