use fastrender::dom2::Document;
use selectors::context::QuirksMode;

#[test]
fn dataset_get_set_delete_reflects_to_data_attributes() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = doc.create_element("div", "");

  assert_eq!(doc.dataset_get(el, "rtc"), None);
  assert_eq!(doc.dataset_set(el, "rtc", "1").unwrap(), true);
  assert_eq!(doc.dataset_get(el, "rtc"), Some("1"));
  assert_eq!(doc.get_attribute(el, "data-rtc").unwrap(), Some("1"));

  // Writing the attribute directly is observable via the camelCase dataset property.
  doc.set_attribute(el, "data-foo-bar", "baz").unwrap();
  assert_eq!(doc.dataset_get(el, "fooBar"), Some("baz"));

  assert_eq!(doc.dataset_delete(el, "rtc").unwrap(), true);
  assert_eq!(doc.dataset_get(el, "rtc"), None);
  assert_eq!(doc.get_attribute(el, "data-rtc").unwrap(), None);

  // Invalid property names should not panic and should not mutate.
  assert_eq!(doc.dataset_set(el, "Foo", "x").unwrap(), false);
  assert_eq!(doc.dataset_set(el, "foo-bar", "x").unwrap(), false);
  assert_eq!(doc.get_attribute(el, "data-foo").unwrap(), None);
}

#[test]
fn style_set_property_and_get_property_value_roundtrip() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let el = doc.create_element("div", "");

  assert_eq!(doc.style_get_property_value(el, "display"), "");
  doc.style_set_display(el, "none").unwrap();
  assert_eq!(doc.style_display(el), "none");

  // setProperty/getPropertyValue.
  doc.style_set_property(el, "cursor", "pointer").unwrap();
  assert_eq!(doc.style_cursor(el), "pointer");
  assert_eq!(doc.style_get_property_value(el, "cursor"), "pointer");

  // Property name normalization: camelCase is accepted.
  doc.style_set_property(el, "backgroundColor", "red").unwrap();
  assert_eq!(doc.style_get_property_value(el, "background-color"), "red");

  // Live reflection: overriding `style` attribute updates the accessor results.
  doc
    .set_attribute(el, "style", "display: block; cursor: move;")
    .unwrap();
  assert_eq!(doc.style_display(el), "block");
  assert_eq!(doc.style_cursor(el), "move");

  // Empty values clear the property (and can remove the entire style attribute).
  doc.style_set_display(el, "").unwrap();
  assert_eq!(doc.style_display(el), "");
}

#[test]
fn reflected_idl_attributes_map_to_dom2_attributes() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let script = doc.create_element("script", "");

  doc.set_element_id(script, "boot").unwrap();
  assert_eq!(doc.element_id(script), "boot");
  assert_eq!(doc.get_attribute(script, "id").unwrap(), Some("boot"));

  doc.set_element_class_name(script, "a b").unwrap();
  assert_eq!(doc.element_class_name(script), "a b");
  assert_eq!(doc.get_attribute(script, "class").unwrap(), Some("a b"));

  doc.set_element_src(script, "https://example.com/app.js").unwrap();
  assert_eq!(doc.element_src(script), "https://example.com/app.js");
  assert_eq!(
    doc.get_attribute(script, "src").unwrap(),
    Some("https://example.com/app.js")
  );

  assert!(!doc.element_async(script));
  doc.set_element_async(script, true).unwrap();
  assert!(doc.element_async(script));
  assert!(doc.has_attribute(script, "async").unwrap());
  doc.set_element_async(script, false).unwrap();
  assert!(!doc.element_async(script));
  assert!(!doc.has_attribute(script, "async").unwrap());

  doc.set_element_defer(script, true).unwrap();
  assert!(doc.element_defer(script));
  assert!(doc.has_attribute(script, "defer").unwrap());

  doc.set_element_type(script, "module").unwrap();
  assert_eq!(doc.get_attribute(script, "type").unwrap(), Some("module"));

  doc.set_element_charset(script, "utf-8").unwrap();
  assert_eq!(doc.get_attribute(script, "charset").unwrap(), Some("utf-8"));

  doc.set_element_cross_origin(script, "anonymous").unwrap();
  assert_eq!(
    doc.get_attribute(script, "crossorigin").unwrap(),
    Some("anonymous")
  );
  assert_eq!(doc.element_cross_origin(script), "anonymous");
}

#[test]
fn bootstrap_like_element_mutations_do_not_error() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let root = doc.root();
  let body = doc.create_element("body", "");
  doc.append_child(root, body).unwrap();

  // A common real-world pattern: dynamically create a `<script>`, set reflected attributes, and
  // append it.
  let script = doc.create_element("script", "");
  doc
    .set_element_src(script, "https://example.com/bootstrap.js")
    .unwrap();
  doc.set_element_async(script, true).unwrap();
  let text = doc.create_text("console.log('boot');");
  doc.append_child(script, text).unwrap();
  doc.append_child(body, script).unwrap();

  // Similarly, scripts often create iframes and tweak both reflected attributes and `style`.
  let iframe = doc.create_element("iframe", "");
  doc.set_element_id(iframe, "frame").unwrap();
  doc.style_set_display(iframe, "none").unwrap();
  doc.set_element_height(iframe, "0").unwrap();
  doc.set_element_width(iframe, "0").unwrap();
  doc.append_child(body, iframe).unwrap();
}
