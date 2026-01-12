use fastrender::dom::{
  compute_slot_assignment_with_ids, enumerate_dom_ids, parse_html, parse_html_with_options,
  DomNode, DomNodeType, DomParseOptions, SlotAssignment,
};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

fn find_element<'a>(node: &'a DomNode, tag: &str) -> Option<&'a DomNode> {
  if matches!(node.tag_name(), Some(t) if t.eq_ignore_ascii_case(tag)) {
    return Some(node);
  }

  for child in node.children.iter() {
    if let Some(found) = find_element(child, tag) {
      return Some(found);
    }
  }

  None
}

fn collect_classes(node: &DomNode) -> Vec<String> {
  match &node.node_type {
    DomNodeType::Element { attributes, .. } | DomNodeType::Slot { attributes, .. } => attributes
      .iter()
      .find(|(k, _)| k.eq_ignore_ascii_case("class"))
      .map(|(_, v)| v.split_whitespace().map(|s| s.to_string()).collect())
      .unwrap_or_default(),
    _ => Vec::new(),
  }
}

fn find_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  if node.has_id(id) {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

fn build_id_lookup<'a>(
  node: &'a DomNode,
  ids: &HashMap<*const DomNode, usize>,
  out: &mut HashMap<usize, &'a DomNode>,
) {
  if let Some(id) = ids.get(&(node as *const DomNode)) {
    out.insert(*id, node);
  }
  for child in node.children.iter() {
    build_id_lookup(child, ids, out);
  }
}

fn subtree_text_content(node: &DomNode) -> String {
  let mut out = String::new();
  node.walk_tree(&mut |child| {
    if let DomNodeType::Text { content } = &child.node_type {
      out.push_str(content);
    }
  });
  out
}

fn assigned_node_texts_for_slot<'a>(
  dom: &'a DomNode,
  ids: &HashMap<*const DomNode, usize>,
  lookup: &HashMap<usize, &'a DomNode>,
  assignment: &SlotAssignment,
  slot_element_id: &str,
) -> Vec<String> {
  let slot = find_by_id(dom, slot_element_id)
    .unwrap_or_else(|| panic!("slot element with id='{slot_element_id}'"));
  assert!(
    matches!(slot.node_type, DomNodeType::Slot { .. }),
    "expected element with id='{slot_element_id}' to be a <slot>"
  );
  let slot_id = *ids
    .get(&(slot as *const DomNode))
    .unwrap_or_else(|| panic!("node id for slot id='{slot_element_id}'"));
  assignment
    .slot_to_nodes
    .get(&slot_id)
    .cloned()
    .unwrap_or_default()
    .into_iter()
    .map(|node_id| {
      let node = lookup
        .get(&node_id)
        .unwrap_or_else(|| panic!("assigned node id {node_id} for slot id='{slot_element_id}'"));
      subtree_text_content(node).trim().to_string()
    })
    .collect()
}

#[test]
fn compatibility_mode_flips_expected_classes() {
  let html = "<html class='no-js foo'><body class='bar'></body></html>";

  let standard_dom = parse_html(html).expect("parse standard DOM");
  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");

  let standard_html = find_element(&standard_dom, "html").expect("standard html element");
  let standard_body = find_element(standard_html, "body").expect("standard body element");

  let compat_html = find_element(&compat_dom, "html").expect("compat html element");
  let compat_body = find_element(compat_html, "body").expect("compat body element");

  let standard_html_classes = collect_classes(standard_html);
  assert!(standard_html_classes.contains(&"no-js".to_string()));
  assert!(!standard_html_classes.contains(&"js".to_string()));
  assert!(!standard_html_classes.contains(&"js-enabled".to_string()));
  assert!(!standard_html_classes.contains(&"jsl10n-visible".to_string()));

  let standard_body_classes = collect_classes(standard_body);
  assert!(!standard_body_classes.contains(&"jsl10n-visible".to_string()));

  let compat_html_classes = collect_classes(compat_html);
  assert!(!compat_html_classes.contains(&"no-js".to_string()));
  assert!(compat_html_classes.contains(&"js".to_string()));
  assert!(compat_html_classes.contains(&"js-enabled".to_string()));
  assert!(compat_html_classes.contains(&"foo".to_string()));
  assert!(compat_html_classes.contains(&"jsl10n-visible".to_string()));

  let compat_body_classes = collect_classes(compat_body);
  assert!(compat_body_classes.contains(&"bar".to_string()));
  assert!(compat_body_classes.contains(&"jsl10n-visible".to_string()));
}

#[test]
fn compatibility_mode_flips_no_js_class_on_body() {
  let html = "<html><body class='no-js foo'></body></html>";

  let standard_dom = parse_html(html).expect("parse standard DOM");
  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");

  let standard_html = find_element(&standard_dom, "html").expect("standard html element");
  let standard_body = find_element(standard_html, "body").expect("standard body element");

  let compat_html = find_element(&compat_dom, "html").expect("compat html element");
  let compat_body = find_element(compat_html, "body").expect("compat body element");

  let standard_body_classes = collect_classes(standard_body);
  assert!(standard_body_classes.contains(&"no-js".to_string()));
  assert!(!standard_body_classes.contains(&"js".to_string()));
  assert!(!standard_body_classes.contains(&"js-enabled".to_string()));
  assert!(!standard_body_classes.contains(&"jsl10n-visible".to_string()));

  let compat_html_classes = collect_classes(compat_html);
  assert!(!compat_html_classes.contains(&"js".to_string()));
  assert!(!compat_html_classes.contains(&"js-enabled".to_string()));
  assert!(compat_html_classes.contains(&"jsl10n-visible".to_string()));

  let compat_body_classes = collect_classes(compat_body);
  assert!(!compat_body_classes.contains(&"no-js".to_string()));
  assert!(compat_body_classes.contains(&"js".to_string()));
  assert!(compat_body_classes.contains(&"js-enabled".to_string()));
  assert!(compat_body_classes.contains(&"foo".to_string()));
  assert!(compat_body_classes.contains(&"jsl10n-visible".to_string()));
}

#[test]
fn compatibility_mode_preserves_shadow_slot_distribution() {
  let html = "<html><body><div id='host'><template shadowroot='open'><div id='shadow'><slot name='named' id='named-slot'></slot><slot id='default-slot'></slot></div></template><span slot='named'>named</span><span>default</span></div></body></html>";

  let standard_dom = parse_html(html).expect("parse standard DOM");
  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");

  let standard_ids = enumerate_dom_ids(&standard_dom);
  let compat_ids = enumerate_dom_ids(&compat_dom);

  let standard_assignment = compute_slot_assignment_with_ids(&standard_dom, &standard_ids)
    .expect("compute standard slot assignment");
  let compat_assignment = compute_slot_assignment_with_ids(&compat_dom, &compat_ids)
    .expect("compute compat slot assignment");

  let standard_host = find_by_id(&standard_dom, "host").expect("standard host element");
  assert!(
    standard_host.is_shadow_host(),
    "standard parse should attach declarative shadow root"
  );
  let compat_host = find_by_id(&compat_dom, "host").expect("compat host element");
  assert!(
    compat_host.is_shadow_host(),
    "compat parse should attach declarative shadow root"
  );

  assert_eq!(
    standard_assignment.slot_to_nodes, compat_assignment.slot_to_nodes,
    "compatibility mode should not alter slot assignment"
  );

  let mut standard_lookup = HashMap::new();
  build_id_lookup(&standard_dom, &standard_ids, &mut standard_lookup);
  let mut compat_lookup = HashMap::new();
  build_id_lookup(&compat_dom, &compat_ids, &mut compat_lookup);

  let standard_named = assigned_node_texts_for_slot(
    &standard_dom,
    &standard_ids,
    &standard_lookup,
    &standard_assignment,
    "named-slot",
  );
  let standard_default = assigned_node_texts_for_slot(
    &standard_dom,
    &standard_ids,
    &standard_lookup,
    &standard_assignment,
    "default-slot",
  );

  let compat_named = assigned_node_texts_for_slot(
    &compat_dom,
    &compat_ids,
    &compat_lookup,
    &compat_assignment,
    "named-slot",
  );
  let compat_default = assigned_node_texts_for_slot(
    &compat_dom,
    &compat_ids,
    &compat_lookup,
    &compat_assignment,
    "default-slot",
  );

  assert_eq!(
    (standard_named.clone(), standard_default.clone()),
    (compat_named, compat_default),
    "compatibility mode should not change slot distribution"
  );
  assert_eq!(standard_named, vec!["named".to_string()]);
  assert_eq!(standard_default, vec!["default".to_string()]);
}

#[test]
fn compatibility_mode_lifts_data_default_src_images() {
  let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/dom_compat_lazy_data_attr_matrix");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let standard_dom = parse_html(&html).expect("parse standard DOM");
  let compat_dom =
    parse_html_with_options(&html, DomParseOptions::compatibility()).expect("parse compat DOM");

  let standard_default_src = find_by_id(&standard_dom, "default-src")
    .expect("standard img#default-src")
    .get_attribute_ref("src")
    .unwrap_or("");
  assert!(
    standard_default_src.starts_with("data:image/gif"),
    "expected standard img#default-src to keep placeholder; got {standard_default_src:?}"
  );
  let compat_default_src = find_by_id(&compat_dom, "default-src")
    .expect("compat img#default-src")
    .get_attribute_ref("src")
    .unwrap_or("");
  assert_eq!(compat_default_src, "red.svg");

  let compat_priority_src = find_by_id(&compat_dom, "data-src-priority")
    .expect("compat img#data-src-priority")
    .get_attribute_ref("src")
    .unwrap_or("");
  assert_eq!(
    compat_priority_src, "blue.svg",
    "expected `data-src` to remain higher priority than `data-default-src`"
  );

  let compat_authored_src = find_by_id(&compat_dom, "authored-src")
    .expect("compat img#authored-src")
    .get_attribute_ref("src")
    .unwrap_or("");
  assert_eq!(
    compat_authored_src, "blue.svg",
    "expected compat mode to preserve non-placeholder authored src"
  );
}

#[test]
fn compatibility_mode_lifts_data_orig_file_images() {
  let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/dom_compat_lazy_img_orig_file");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let standard_dom = parse_html(&html).expect("parse standard DOM");
  let compat_dom =
    parse_html_with_options(&html, DomParseOptions::compatibility()).expect("parse compat DOM");

  let standard_src = find_by_id(&standard_dom, "orig-file")
    .expect("standard img#orig-file")
    .get_attribute_ref("src")
    .unwrap_or("");
  assert!(
    standard_src.starts_with("data:image/gif"),
    "expected standard img#orig-file to keep placeholder; got {standard_src:?}"
  );

  let compat_src = find_by_id(&compat_dom, "orig-file")
    .expect("compat img#orig-file")
    .get_attribute_ref("src")
    .unwrap_or("");
  assert_eq!(compat_src, "red.svg");
}

#[test]
fn compatibility_mode_lifts_svg_placeholder_img_src_from_data_src() {
  let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/dom_compat_svg_placeholder");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let dom =
    parse_html_with_options(&html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let img = find_by_id(&dom, "img").expect("img element");
  assert_eq!(img.get_attribute_ref("src"), Some("assets/real.png"));
}

#[test]
fn compatibility_mode_overwrites_1x1_png_placeholder_img_src_from_data_src() {
  let placeholder = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=";
  let html = format!(r#"<html><body><img src="{placeholder}" data-src="real.jpg"></body></html>"#);

  let standard_dom = parse_html(&html).expect("parse standard DOM");
  let standard_img = find_element(&standard_dom, "img").expect("standard img element");
  assert_eq!(
    standard_img.get_attribute_ref("src"),
    Some(placeholder),
    "standard mode should preserve placeholder src"
  );

  let compat_dom =
    parse_html_with_options(&html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("real.jpg"),
    "compat mode should overwrite 1×1 PNG placeholder src with data-src"
  );
}

#[test]
fn compatibility_mode_overwrites_base64_image_header_without_payload_img_src_from_data_src() {
  let html = r#"<html><body><img src="data:image/png;base64" data-src="real.jpg"></body></html>"#;

  let standard_dom = parse_html(html).expect("parse standard DOM");
  let standard_img = find_element(&standard_dom, "img").expect("standard img element");
  assert_eq!(
    standard_img.get_attribute_ref("src"),
    Some("data:image/png;base64"),
    "standard mode should preserve placeholder src"
  );

  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("real.jpg"),
    "compat mode should overwrite base64 header placeholder src with data-src"
  );
}

#[test]
fn compatibility_mode_lifts_video_poster_and_src_from_wrapper_data_attrs() {
  let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/dom_compat_lazy_video_poster_wrapper");
  let html_path = fixture_dir.join("index.html");
  let html = fs::read_to_string(&html_path).expect("read fixture HTML");

  let standard_dom = parse_html(&html).expect("parse standard DOM");
  let compat_dom =
    parse_html_with_options(&html, DomParseOptions::compatibility()).expect("parse compat DOM");

  let standard_video = find_by_id(&standard_dom, "video").expect("standard video element");
  assert!(
    standard_video.get_attribute_ref("poster").is_none(),
    "standard mode should not populate video poster"
  );
  assert!(
    standard_video.get_attribute_ref("src").is_none(),
    "standard mode should not populate video src"
  );

  let compat_video = find_by_id(&compat_dom, "video").expect("compat video element");
  assert_eq!(
    compat_video.get_attribute_ref("poster"),
    Some("red.svg"),
    "compat mode should lift wrapper data-poster-url into video poster"
  );
  assert_eq!(
    compat_video.get_attribute_ref("src"),
    Some("movie.mp4"),
    "compat mode should lift wrapper data-video-urls into video src"
  );
}

#[test]
fn compatibility_mode_lifts_img_src_from_lazy_data_attributes() {
  let html = r#"<html><body><img data-src="https://example.com/a.jpg"></body></html>"#;

  let standard_dom = parse_html(html).expect("parse standard DOM");
  let standard_img = find_element(&standard_dom, "img").expect("standard img element");
  assert!(
    standard_img.get_attribute_ref("src").is_none(),
    "standard mode should not mutate img src"
  );

  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("https://example.com/a.jpg"),
    "compat mode should lift data-src into src"
  );
}

#[test]
fn compatibility_mode_lifts_img_src_from_data_default_src() {
  let html = r#"<html><body><img data-default-src="default.jpg"></body></html>"#;
  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("default.jpg"),
    "compat mode should lift data-default-src into src"
  );
}

#[test]
fn compatibility_mode_lifts_img_src_from_data_src_retina() {
  let html = r#"<html><body><img data-src-retina="retina.jpg"></body></html>"#;
  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("retina.jpg"),
    "compat mode should lift data-src-retina into src"
  );
}

#[test]
fn compatibility_mode_lifts_img_src_from_data_delayed_url() {
  let html = r#"<html><body><img data-delayed-url="https://example.com/real.png"></body></html>"#;
  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("https://example.com/real.png"),
    "compat mode should lift data-delayed-url into src"
  );
}

#[test]
fn compatibility_mode_ignores_placeholder_data_delayed_url() {
  let html = r##"<html><body><img data-delayed-url="#" data-actualsrc="real.jpg"></body></html>"##;
  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("real.jpg"),
    "compat mode should ignore placeholder data-delayed-url candidates"
  );
}

#[test]
fn compatibility_mode_lifts_img_src_from_data_img_url() {
  let html = r#"<html><body><img data-img-url="img.png"></body></html>"#;
  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("img.png"),
    "compat mode should lift data-img-url into src"
  );

  let html = r#"<html><body><img src="about:blank" data-img-url="https://example.com/real.png"></body></html>"#;

  let standard_dom = parse_html(html).expect("parse standard DOM");
  let standard_img = find_element(&standard_dom, "img").expect("standard img element");
  assert_eq!(
    standard_img.get_attribute_ref("src"),
    Some("about:blank"),
    "standard mode should preserve placeholder src"
  );

  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("https://example.com/real.png"),
    "compat mode should lift data-img-url into src"
  );
}

#[test]
fn compatibility_mode_ignores_placeholder_data_img_url() {
  let html = r##"<html><body><img src="about:blank" data-img-url="#" data-default-src="real.jpg"></body></html>"##;

  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("real.jpg"),
    "compat mode should ignore placeholder data-img-url candidates"
  );
}

#[test]
fn compatibility_mode_overwrites_placeholder_img_src_but_not_real_src() {
  let placeholder_html =
    r#"<html><body><img src="about:blank" data-src="assets/photo.jpg"></body></html>"#;
  let standard_dom = parse_html(placeholder_html).expect("parse standard DOM");
  let compat_dom = parse_html_with_options(placeholder_html, DomParseOptions::compatibility())
    .expect("parse compat DOM");

  let standard_img = find_element(&standard_dom, "img").expect("standard img element");
  assert_eq!(
    standard_img.get_attribute_ref("src"),
    Some("about:blank"),
    "standard mode should preserve placeholder src"
  );

  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("assets/photo.jpg"),
    "compat mode should overwrite placeholder src with data-src"
  );

  let real_html = r#"<html><body><img src="real.jpg" data-src="lazy.jpg"></body></html>"#;
  let compat_dom =
    parse_html_with_options(real_html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("src"),
    Some("real.jpg"),
    "compat mode should not overwrite a non-placeholder src"
  );
}

#[test]
fn compatibility_mode_lifts_srcset_for_img_and_picture_sources() {
  let img_html = r#"<html><body><img data-original-set="a.jpg 1x, b.jpg 2x"></body></html>"#;
  let standard_dom = parse_html(img_html).expect("parse standard DOM");
  let standard_img = find_element(&standard_dom, "img").expect("standard img element");
  assert!(
    standard_img.get_attribute_ref("srcset").is_none(),
    "standard mode should not mutate img srcset"
  );

  let compat_dom =
    parse_html_with_options(img_html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("srcset"),
    Some("a.jpg 1x, b.jpg 2x"),
    "compat mode should lift data-original-set into srcset"
  );

  let picture_html = r#"<html><body><picture><source data-srcset="c.webp 1x"><img src="data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw=="></picture></body></html>"#;
  let standard_dom = parse_html(picture_html).expect("parse standard DOM");
  let compat_dom = parse_html_with_options(picture_html, DomParseOptions::compatibility())
    .expect("parse compat DOM");

  let standard_source = find_element(&standard_dom, "source").expect("standard source element");
  assert!(
    standard_source.get_attribute_ref("srcset").is_none(),
    "standard mode should not mutate <picture><source> srcset"
  );

  let compat_source = find_element(&compat_dom, "source").expect("compat source element");
  assert_eq!(
    compat_source.get_attribute_ref("srcset"),
    Some("c.webp 1x"),
    "compat mode should lift data-srcset into <picture><source> srcset"
  );
}

#[test]
fn compatibility_mode_overwrites_placeholder_srcset_from_data_srcset() {
  let html = r#"<html><body><img srcset="data:image/gif;base64,R0lGODlhAQABAAAAACH5BAEKAAEALAAAAAABAAEAAAICTAEAOw==" data-srcset="real.jpg 1x, real2.jpg 2x"></body></html>"#;

  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("srcset"),
    Some("real.jpg 1x, real2.jpg 2x"),
    "compat mode should lift data-srcset when authored srcset is placeholder-only"
  );
}

#[test]
fn compatibility_mode_lifts_sizes_from_data_sizes() {
  let html = r#"<html><body><img data-src="a.jpg" data-sizes="100vw"></body></html>"#;

  let standard_dom = parse_html(html).expect("parse standard DOM");
  let standard_img = find_element(&standard_dom, "img").expect("standard img element");
  assert!(
    standard_img.get_attribute_ref("sizes").is_none(),
    "standard mode should not mutate img sizes"
  );

  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_img = find_element(&compat_dom, "img").expect("compat img element");
  assert_eq!(
    compat_img.get_attribute_ref("sizes"),
    Some("100vw"),
    "compat mode should lift data-sizes into sizes"
  );
}

#[test]
fn compatibility_mode_lifts_iframe_src_from_data_src() {
  let html = r#"<html><body><iframe data-src="https://example.com/embed"></iframe></body></html>"#;

  let standard_dom = parse_html(html).expect("parse standard DOM");
  let standard_iframe = find_element(&standard_dom, "iframe").expect("standard iframe element");
  assert!(
    standard_iframe.get_attribute_ref("src").is_none(),
    "standard mode should not mutate iframe src"
  );

  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_iframe = find_element(&compat_dom, "iframe").expect("compat iframe element");
  assert_eq!(
    compat_iframe.get_attribute_ref("src"),
    Some("https://example.com/embed"),
    "compat mode should lift iframe data-src into src"
  );

  let json_html = r#"<html><body><iframe data-src='{"url":"real.html"}'></iframe></body></html>"#;
  let compat_dom =
    parse_html_with_options(json_html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_iframe = find_element(&compat_dom, "iframe").expect("compat iframe element");
  assert_eq!(
    compat_iframe.get_attribute_ref("src"),
    Some("real.html"),
    "compat mode should extract iframe src from JSON-ish data-src payloads"
  );

  let placeholder_html = r#"<html><body><iframe src="about:blank" data-src="https://example.com/embed"></iframe></body></html>"#;
  let compat_dom = parse_html_with_options(placeholder_html, DomParseOptions::compatibility())
    .expect("parse compat DOM");
  let compat_iframe = find_element(&compat_dom, "iframe").expect("compat iframe element");
  assert_eq!(
    compat_iframe.get_attribute_ref("src"),
    Some("https://example.com/embed"),
    "compat mode should overwrite placeholder iframe src"
  );
}

#[test]
fn compatibility_mode_lifts_iframe_src_from_data_live_path() {
  let html =
    r#"<html><body><iframe src="about:blank" data-live-path="frame.html"></iframe></body></html>"#;

  let standard_dom = parse_html(html).expect("parse standard DOM");
  let standard_iframe = find_element(&standard_dom, "iframe").expect("standard iframe element");
  assert_eq!(
    standard_iframe.get_attribute_ref("src"),
    Some("about:blank"),
    "standard mode should preserve authored iframe src"
  );

  let compat_dom =
    parse_html_with_options(html, DomParseOptions::compatibility()).expect("parse compat DOM");
  let compat_iframe = find_element(&compat_dom, "iframe").expect("compat iframe element");
  assert_eq!(
    compat_iframe.get_attribute_ref("src"),
    Some("frame.html"),
    "compat mode should lift iframe data-live-path into src when authored src is placeholder"
  );
}
