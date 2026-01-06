use fastrender::debug::runtime::{set_runtime_toggles, RuntimeToggles};
use fastrender::dom::{DomNode, DomNodeType};
use fastrender::style::defaults::get_default_styles_for_element;
use fastrender::Length;
use std::collections::HashMap;
use std::sync::Arc;

const ENV_COMPAT_REPLACED_MAX_WIDTH_100: &str = "FASTR_COMPAT_REPLACED_MAX_WIDTH_100";

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
fn replaced_element_max_width_100_can_be_toggled() {
  let img = element("img");
  let video = element("video");

  let _guard_off = set_runtime_toggles(Arc::new(RuntimeToggles::from_map(HashMap::from([(
    ENV_COMPAT_REPLACED_MAX_WIDTH_100.to_string(),
    "0".to_string(),
  )]))));

  let img_styles = get_default_styles_for_element(&img);
  assert_eq!(img_styles.max_width, None);

  let video_styles = get_default_styles_for_element(&video);
  assert_eq!(video_styles.max_width, None);

  drop(_guard_off);

  let _guard_on = set_runtime_toggles(Arc::new(RuntimeToggles::from_map(HashMap::from([(
    ENV_COMPAT_REPLACED_MAX_WIDTH_100.to_string(),
    "1".to_string(),
  )]))));

  let img_styles = get_default_styles_for_element(&img);
  assert_eq!(img_styles.max_width, Some(Length::percent(100.0)));

  let video_styles = get_default_styles_for_element(&video);
  assert_eq!(video_styles.max_width, Some(Length::percent(100.0)));
}
