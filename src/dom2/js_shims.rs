//! Small DOM property shims that are commonly accessed via IDL properties in real-world scripts.
//!
//! These utilities are intentionally minimal: they do not attempt to model full WebIDL semantics.
//! They exist so JS bindings can provide `element.dataset`, `element.style`, and common reflected
//! attributes without requiring scripts to fall back to `getAttribute`/`setAttribute`.

use super::style_attr::{parse_style_attribute, serialize_style_attribute};
use super::{Document, DomError, NodeId, NodeKind};
use crate::dom::HTML_NAMESPACE;

fn dataset_prop_to_attr(prop: &str) -> Option<String> {
  if prop.is_empty() {
    return None;
  }
  if prop.as_bytes()[0].is_ascii_uppercase() {
    // Real DOMStringMap only exposes lower-camel-case properties.
    return None;
  }
  if prop.as_bytes().iter().any(|b| *b == b'-') {
    // Hyphens are represented via camelCase in JS (`fooBar` <-> `data-foo-bar`).
    return None;
  }

  let mut out = String::with_capacity("data-".len() + prop.len() + 8);
  out.push_str("data-");
  for ch in prop.chars() {
    if ch.is_ascii_alphanumeric() || ch == '_' {
      if ch.is_ascii_uppercase() {
        out.push('-');
        out.push(ch.to_ascii_lowercase());
      } else {
        out.push(ch);
      }
    } else {
      // Minimal validation: ignore invalid names rather than panicking.
      return None;
    }
  }
  Some(out)
}

fn normalize_css_property_name(name: &str) -> Option<String> {
  let name = name.trim();
  if name.is_empty() {
    return None;
  }

  // Preserve custom properties verbatim.
  if name.starts_with("--") {
    return Some(name.to_string());
  }

  // If the author already provided a kebab-case property name, just lowercase it.
  if name.contains('-') {
    return Some(name.to_ascii_lowercase());
  }

  let mut out = String::with_capacity(name.len() + 8);
  for ch in name.chars() {
    if ch.is_ascii_alphanumeric() {
      if ch.is_ascii_uppercase() {
        out.push('-');
        out.push(ch.to_ascii_lowercase());
      } else {
        out.push(ch);
      }
      continue;
    }
    // Keep `_` as-is (not standard for CSS properties, but benign for our shim layer).
    if ch == '_' {
      out.push(ch);
      continue;
    }
    return None;
  }
  Some(out)
}

impl Document {
  // --- dataset ---------------------------------------------------------------

  /// Implements `Element.dataset.<prop>` read semantics.
  ///
  /// Returns `None` when the backing `data-*` attribute is missing or the requested property name is
  /// invalid.
  pub fn dataset_get(&self, element: NodeId, prop: &str) -> Option<&str> {
    let attr = dataset_prop_to_attr(prop)?;
    self.get_attribute(element, &attr).ok().flatten()
  }

  /// Implements `Element.dataset.<prop> = value`.
  pub fn dataset_set(
    &mut self,
    element: NodeId,
    prop: &str,
    value: &str,
  ) -> Result<bool, DomError> {
    let Some(attr) = dataset_prop_to_attr(prop) else {
      return Ok(false);
    };
    self.set_attribute(element, &attr, value)
  }

  /// Implements `delete Element.dataset.<prop>`.
  pub fn dataset_delete(&mut self, element: NodeId, prop: &str) -> Result<bool, DomError> {
    let Some(attr) = dataset_prop_to_attr(prop) else {
      return Ok(false);
    };
    self.remove_attribute(element, &attr)
  }

  // --- style -----------------------------------------------------------------

  /// Implements `CSSStyleDeclaration.getPropertyValue(name)`.
  ///
  /// Per the platform API, missing properties yield `""` (empty string).
  pub fn style_get_property_value(&self, element: NodeId, name: &str) -> String {
    let Some(prop) = normalize_css_property_name(name) else {
      return String::new();
    };

    let style_attr = self
      .get_attribute(element, "style")
      .ok()
      .flatten()
      .unwrap_or("");
    let decls = parse_style_attribute(style_attr);
    decls.get(&prop).cloned().unwrap_or_default()
  }

  /// Implements `CSSStyleDeclaration.setProperty(name, value)` and common `style.foo = "x"`
  /// assignments.
  ///
  /// For convenience, both kebab-case (`background-color`) and camelCase (`backgroundColor`) names
  /// are accepted and normalized.
  pub fn style_set_property(
    &mut self,
    element: NodeId,
    name: &str,
    value: &str,
  ) -> Result<bool, DomError> {
    let Some(prop) = normalize_css_property_name(name) else {
      return Ok(false);
    };

    let style_attr = self.get_attribute(element, "style")?.unwrap_or("");
    let mut decls = parse_style_attribute(style_attr);

    let value = value.trim();
    if value.is_empty() {
      // Setting an empty value removes the property. Treat removing a missing property as a no-op
      // so host invalidation can be skipped.
      if !decls.contains_key(&prop) {
        return Ok(false);
      }
      decls.remove(&prop);
    } else {
      // Avoid rewriting the `style` attribute (and triggering host invalidation) when the semantic
      // value is unchanged. This is particularly important because our serializer normalizes
      // whitespace/trailing semicolons.
      if decls.get(&prop).is_some_and(|existing| existing == value) {
        return Ok(false);
      }
      decls.insert(prop, value.to_string());
    }

    let serialized = serialize_style_attribute(&decls);
    if serialized.is_empty() {
      self.remove_attribute(element, "style")
    } else {
      self.set_attribute(element, "style", &serialized)
    }
  }

  pub fn style_display(&self, element: NodeId) -> String {
    self.style_get_property_value(element, "display")
  }

  pub fn style_set_display(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.style_set_property(element, "display", value)
  }

  pub fn style_cursor(&self, element: NodeId) -> String {
    self.style_get_property_value(element, "cursor")
  }

  pub fn style_set_cursor(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.style_set_property(element, "cursor", value)
  }

  pub fn style_height(&self, element: NodeId) -> String {
    self.style_get_property_value(element, "height")
  }

  pub fn style_set_height(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.style_set_property(element, "height", value)
  }

  pub fn style_width(&self, element: NodeId) -> String {
    self.style_get_property_value(element, "width")
  }

  pub fn style_set_width(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.style_set_property(element, "width", value)
  }

  // --- Common reflected attributes ------------------------------------------

  fn reflected_string(&self, element: NodeId, attr: &str) -> &str {
    self
      .get_attribute(element, attr)
      .ok()
      .flatten()
      .unwrap_or("")
  }

  fn set_reflected_string(
    &mut self,
    element: NodeId,
    attr: &str,
    value: &str,
  ) -> Result<bool, DomError> {
    self.set_attribute(element, attr, value)
  }

  fn reflected_bool(&self, element: NodeId, attr: &str) -> bool {
    self.has_attribute(element, attr).unwrap_or(false)
  }

  fn set_reflected_bool(
    &mut self,
    element: NodeId,
    attr: &str,
    present: bool,
  ) -> Result<bool, DomError> {
    self.set_bool_attribute(element, attr, present)
  }

  pub fn element_id(&self, element: NodeId) -> &str {
    self.reflected_string(element, "id")
  }

  pub fn set_element_id(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "id", value)
  }

  pub fn element_class_name(&self, element: NodeId) -> &str {
    self.reflected_string(element, "class")
  }

  pub fn set_element_class_name(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "class", value)
  }

  pub fn element_src(&self, element: NodeId) -> &str {
    self.reflected_string(element, "src")
  }

  pub fn set_element_src(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "src", value)
  }

  pub fn element_srcset(&self, element: NodeId) -> &str {
    self.reflected_string(element, "srcset")
  }

  pub fn set_element_srcset(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "srcset", value)
  }

  pub fn element_sizes(&self, element: NodeId) -> &str {
    self.reflected_string(element, "sizes")
  }

  pub fn set_element_sizes(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "sizes", value)
  }

  pub fn element_href(&self, element: NodeId) -> &str {
    self.reflected_string(element, "href")
  }

  pub fn set_element_href(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "href", value)
  }

  pub fn element_rel(&self, element: NodeId) -> &str {
    self.reflected_string(element, "rel")
  }

  pub fn set_element_rel(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "rel", value)
  }

  pub fn element_type(&self, element: NodeId) -> &str {
    self.reflected_string(element, "type")
  }

  pub fn set_element_type(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "type", value)
  }

  pub fn element_charset(&self, element: NodeId) -> &str {
    self.reflected_string(element, "charset")
  }

  pub fn set_element_charset(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "charset", value)
  }

  pub fn element_cross_origin(&self, element: NodeId) -> &str {
    self.reflected_string(element, "crossorigin")
  }

  pub fn set_element_cross_origin(
    &mut self,
    element: NodeId,
    value: &str,
  ) -> Result<bool, DomError> {
    self.set_reflected_string(element, "crossorigin", value)
  }

  pub fn element_async(&self, element: NodeId) -> bool {
    self.reflected_bool(element, "async")
  }

  pub fn set_element_async(&mut self, element: NodeId, value: bool) -> Result<bool, DomError> {
    self.set_reflected_bool(element, "async", value)
  }

  pub fn element_defer(&self, element: NodeId) -> bool {
    self.reflected_bool(element, "defer")
  }

  pub fn set_element_defer(&mut self, element: NodeId, value: bool) -> Result<bool, DomError> {
    self.set_reflected_bool(element, "defer", value)
  }

  // Common on iframe/img and used by some bootstrap scripts.
  pub fn element_height(&self, element: NodeId) -> &str {
    self.reflected_string(element, "height")
  }

  pub fn set_element_height(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "height", value)
  }

  pub fn element_width(&self, element: NodeId) -> &str {
    self.reflected_string(element, "width")
  }

  pub fn set_element_width(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "width", value)
  }

  // --- HTMLElement global reflected attributes --------------------------------

  pub fn element_hidden(&self, element: NodeId) -> bool {
    self.reflected_bool(element, "hidden")
  }

  pub fn set_element_hidden(&mut self, element: NodeId, value: bool) -> Result<bool, DomError> {
    self.set_reflected_bool(element, "hidden", value)
  }

  pub fn element_title(&self, element: NodeId) -> &str {
    self.reflected_string(element, "title")
  }

  pub fn set_element_title(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "title", value)
  }

  pub fn element_lang(&self, element: NodeId) -> &str {
    self.reflected_string(element, "lang")
  }

  pub fn set_element_lang(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "lang", value)
  }

  pub fn element_dir(&self, element: NodeId) -> &str {
    self.reflected_string(element, "dir")
  }

  pub fn set_element_dir(&mut self, element: NodeId, value: &str) -> Result<bool, DomError> {
    self.set_reflected_string(element, "dir", value)
  }

  // --- Form controls ----------------------------------------------------------
  pub fn input_disabled(&self, input: NodeId) -> bool {
    self.reflected_bool(input, "disabled")
  }

  pub fn set_input_disabled(&mut self, input: NodeId, value: bool) -> Result<bool, DomError> {
    self.set_reflected_bool(input, "disabled", value)
  }

  fn subtree_text_content(&self, root: NodeId) -> (String, bool) {
    let mut out = String::new();
    let mut saw_text = false;
    for id in self.subtree_preorder(root) {
      let NodeKind::Text { content } = &self.node(id).kind else {
        continue;
      };
      saw_text = true;
      out.push_str(content);
    }
    (out, saw_text)
  }

  fn is_html_element_tag(&self, node: NodeId, tag: &str) -> bool {
    let NodeKind::Element {
      tag_name,
      namespace,
      ..
    } = &self.node(node).kind
    else {
      return false;
    };
    (namespace.is_empty() || namespace == HTML_NAMESPACE) && tag_name.eq_ignore_ascii_case(tag)
  }

  pub fn select_options(&self, select: NodeId) -> Vec<NodeId> {
    self
      .subtree_preorder(select)
      .filter(|&id| self.is_html_element_tag(id, "option"))
      .collect()
  }

  pub fn select_selected_index(&self, select: NodeId) -> i32 {
    for (idx, option) in self.select_options(select).into_iter().enumerate() {
      if self.has_attribute(option, "selected").unwrap_or(false) {
        return idx as i32;
      }
    }
    -1
  }

  pub fn set_select_selected_index(&mut self, select: NodeId, index: i32) -> Result<bool, DomError> {
    let options = self.select_options(select);
    let mut changed = false;
    if index < 0 || (index as usize) >= options.len() {
      for option in options {
        changed |= self.set_bool_attribute(option, "selected", false)?;
      }
      return Ok(changed);
    }

    let target = index as usize;
    for (idx, option) in options.into_iter().enumerate() {
      changed |= self.set_bool_attribute(option, "selected", idx == target)?;
    }
    Ok(changed)
  }

  fn option_value(&self, option: NodeId) -> String {
    if let Some(value) = self.get_attribute(option, "value").ok().flatten() {
      return value.to_string();
    }
    let (content, _) = self.subtree_text_content(option);
    content
  }

  pub fn select_value(&self, select: NodeId) -> String {
    for option in self.select_options(select) {
      if self.has_attribute(option, "selected").unwrap_or(false) {
        return self.option_value(option);
      }
    }
    String::new()
  }

  pub fn set_select_value(&mut self, select: NodeId, value: &str) -> Result<bool, DomError> {
    let options = self.select_options(select);
    let target = options
      .iter()
      .enumerate()
      .find_map(|(idx, &option)| (self.option_value(option) == value).then_some(idx));

    let mut changed = false;
    for (idx, option) in options.into_iter().enumerate() {
      changed |= self.set_bool_attribute(option, "selected", Some(idx) == target)?;
    }
    Ok(changed)
  }

  pub fn form_elements(&self, form: NodeId) -> Vec<NodeId> {
    self
      .subtree_preorder(form)
      .filter(|&id| {
        self.is_html_element_tag(id, "input")
          || self.is_html_element_tag(id, "select")
          || self.is_html_element_tag(id, "textarea")
      })
      .collect()
  }

  pub fn form_submit(&mut self, _form: NodeId) -> Result<(), DomError> {
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::Document;
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
    doc
      .style_set_property(el, "backgroundColor", "red")
      .unwrap();
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

    doc
      .set_element_src(script, "https://example.com/app.js")
      .unwrap();
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
}
