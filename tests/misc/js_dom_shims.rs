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

  assert!(!doc.element_hidden(script));
  doc.set_element_hidden(script, true).unwrap();
  assert!(doc.element_hidden(script));
  assert!(doc.has_attribute(script, "hidden").unwrap());
  doc.set_element_hidden(script, false).unwrap();
  assert!(!doc.element_hidden(script));
  assert!(!doc.has_attribute(script, "hidden").unwrap());

  doc.set_element_title(script, "hello").unwrap();
  assert_eq!(doc.element_title(script), "hello");
  assert_eq!(doc.get_attribute(script, "title").unwrap(), Some("hello"));

  doc.set_element_lang(script, "en").unwrap();
  assert_eq!(doc.element_lang(script), "en");
  assert_eq!(doc.get_attribute(script, "lang").unwrap(), Some("en"));

  doc.set_element_dir(script, "rtl").unwrap();
  assert_eq!(doc.element_dir(script), "rtl");
  assert_eq!(doc.get_attribute(script, "dir").unwrap(), Some("rtl"));

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
fn input_helpers_reflect_to_dom2_attributes() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let input = doc.create_element("input", "");

  // The `dom2` form control shim tracks value as internal state (with a dirty flag), not as a
  // reflected content attribute.
  assert_eq!(doc.input_value(input).unwrap(), "");
  doc.set_attribute(input, "value", "attr").unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "attr");

  doc.set_input_value(input, "state").unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "state");
  // Updating the content attribute no longer affects the current value once dirty.
  doc.set_attribute(input, "value", "newattr").unwrap();
  assert_eq!(doc.input_value(input).unwrap(), "state");

  assert!(!doc.input_checked(input).unwrap());
  doc.set_input_checked(input, true).unwrap();
  assert!(doc.input_checked(input).unwrap());
  doc.set_input_checked(input, false).unwrap();
  assert!(!doc.input_checked(input).unwrap());

  assert!(!doc.input_disabled(input));
  doc.set_input_disabled(input, true).unwrap();
  assert!(doc.input_disabled(input));
  assert!(doc.has_attribute(input, "disabled").unwrap());
  doc.set_input_disabled(input, false).unwrap();
  assert!(!doc.input_disabled(input));
  assert!(!doc.has_attribute(input, "disabled").unwrap());
}

#[test]
fn textarea_value_uses_text_content() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let textarea = doc.create_element("textarea", "");
  let text = doc.create_text("hello");
  doc.append_child(textarea, text).unwrap();
  assert_eq!(doc.textarea_value(textarea).unwrap(), "hello");

  // Setting the runtime value does not mutate the underlying child text nodes.
  doc.set_textarea_value(textarea, "world").unwrap();
  assert_eq!(doc.textarea_value(textarea).unwrap(), "world");
  let children = doc.children(textarea).unwrap();
  assert_eq!(children.len(), 1);
  assert_eq!(doc.text_data(children[0]).unwrap(), "hello");

  // Form reset restores the default value based on the current descendant text nodes.
  let form = doc.create_element("form", "");
  doc.append_child(form, textarea).unwrap();
  doc.form_reset(form).unwrap();
  assert_eq!(doc.textarea_value(textarea).unwrap(), "hello");
}

#[test]
fn select_helpers_model_minimal_option_selection() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let select = doc.create_element("select", "");

  let option_a = doc.create_element("option", "");
  doc.set_attribute(option_a, "value", "a").unwrap();
  let option_b = doc.create_element("option", "");
  doc.set_attribute(option_b, "value", "b").unwrap();
  let option_two = doc.create_element("option", "");
  let option_two_text = doc.create_text("Two");
  doc.append_child(option_two, option_two_text).unwrap();

  let optgroup = doc.create_element("optgroup", "");
  let option_c = doc.create_element("option", "");
  doc.set_attribute(option_c, "value", "c").unwrap();
  doc.append_child(optgroup, option_c).unwrap();

  doc.append_child(select, option_a).unwrap();
  doc.append_child(select, option_b).unwrap();
  doc.append_child(select, option_two).unwrap();
  doc.append_child(select, optgroup).unwrap();

  assert_eq!(
    doc.select_options(select),
    vec![option_a, option_b, option_two, option_c]
  );
  assert_eq!(doc.select_selected_index(select), -1);
  assert_eq!(doc.select_value(select), "");

  doc.set_select_selected_index(select, 1).unwrap();
  assert_eq!(doc.select_selected_index(select), 1);
  assert_eq!(doc.select_value(select), "b");
  assert!(!doc.has_attribute(option_a, "selected").unwrap());
  assert!(doc.has_attribute(option_b, "selected").unwrap());
  assert!(!doc.has_attribute(option_two, "selected").unwrap());
  assert!(!doc.has_attribute(option_c, "selected").unwrap());

  doc.set_select_value(select, "Two").unwrap();
  assert_eq!(doc.select_selected_index(select), 2);
  assert_eq!(doc.select_value(select), "Two");
  assert!(doc.has_attribute(option_two, "selected").unwrap());

  doc.set_select_value(select, "missing").unwrap();
  assert_eq!(doc.select_selected_index(select), -1);
  assert_eq!(doc.select_value(select), "");
  assert!(!doc.has_attribute(option_a, "selected").unwrap());
  assert!(!doc.has_attribute(option_b, "selected").unwrap());
  assert!(!doc.has_attribute(option_two, "selected").unwrap());
  assert!(!doc.has_attribute(option_c, "selected").unwrap());
}

#[test]
fn form_elements_collect_descendant_controls() {
  let mut doc = Document::new(QuirksMode::NoQuirks);
  let form = doc.create_element("form", "");
  let input = doc.create_element("input", "");
  let wrapper = doc.create_element("div", "");
  let select = doc.create_element("select", "");
  let nested = doc.create_element("div", "");
  let textarea = doc.create_element("textarea", "");

  doc.append_child(form, input).unwrap();
  doc.append_child(form, wrapper).unwrap();
  doc.append_child(wrapper, select).unwrap();
  doc.append_child(wrapper, nested).unwrap();
  doc.append_child(nested, textarea).unwrap();

  assert_eq!(doc.form_elements(form), vec![input, select, textarea]);

  // `submit()`/`reset()` should be callable (reset implements minimal default-value semantics).
  doc.form_submit(form).unwrap();
  doc.form_reset(form).unwrap();
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
