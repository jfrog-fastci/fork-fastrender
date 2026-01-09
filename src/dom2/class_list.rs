use super::{Document, DomError, NodeId, NodeKind};

// DOM "ASCII whitespace" for tokenization / validation:
// <https://infra.spec.whatwg.org/#ascii-whitespace>
// Note: This intentionally does *not* include U+000B VERTICAL TAB (which Rust treats as ASCII
// whitespace).
#[inline]
fn is_dom_ascii_whitespace(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}')
}

#[inline]
fn token_contains_dom_ascii_whitespace(token: &str) -> bool {
  token.chars().any(is_dom_ascii_whitespace)
}

#[inline]
fn validate_token(token: &str) -> Result<(), DomError> {
  if token.is_empty() {
    return Err(DomError::SyntaxError);
  }
  if token_contains_dom_ascii_whitespace(token) {
    return Err(DomError::SyntaxError);
  }
  Ok(())
}

fn validate_tokens(tokens: &[&str]) -> Result<(), DomError> {
  for &token in tokens {
    validate_token(token)?;
  }
  Ok(())
}

fn ensure_element_or_slot(doc: &Document, node: NodeId) -> Result<(), DomError> {
  let Some(node_ref) = doc.nodes.get(node.index()) else {
    return Err(DomError::NotFoundError);
  };
  match &node_ref.kind {
    NodeKind::Element { .. } | NodeKind::Slot { .. } => Ok(()),
    _ => Err(DomError::InvalidNodeType),
  }
}

fn parse_class_attribute(value: &str) -> Vec<String> {
  let mut out: Vec<String> = Vec::new();
  for token in value.split(is_dom_ascii_whitespace) {
    if token.is_empty() {
      continue;
    }
    // Preserve insertion order; first occurrence wins.
    if out.iter().any(|existing| existing == token) {
      continue;
    }
    out.push(token.to_string());
  }
  out
}

fn serialize_tokens(tokens: &[String]) -> String {
  tokens.join(" ")
}

fn write_class_tokens(doc: &mut Document, node: NodeId, tokens: &[String]) -> Result<(), DomError> {
  if tokens.is_empty() {
    // Chosen behavior: remove the `class` attribute when the token set becomes empty.
    // DOM implementations vary here (some set it to the empty string); either is acceptable as
    // long as we're consistent.
    doc.remove_attribute(node, "class")?;
    return Ok(());
  }

  let serialized = serialize_tokens(tokens);
  doc.set_attribute(node, "class", &serialized)?;
  Ok(())
}

impl Document {
  pub fn class_list_tokens(&self, node: NodeId) -> Result<Vec<String>, DomError> {
    ensure_element_or_slot(self, node)?;
    let value = self.get_attribute(node, "class")?.unwrap_or_default();
    Ok(parse_class_attribute(value))
  }

  pub fn class_list_contains(&self, node: NodeId, token: &str) -> Result<bool, DomError> {
    ensure_element_or_slot(self, node)?;
    validate_token(token)?;

    let value = self.get_attribute(node, "class")?.unwrap_or_default();
    let tokens = parse_class_attribute(value);
    Ok(tokens.iter().any(|t| t == token))
  }

  /// Adds `tokens` to the element's class list.
  ///
  /// Returns `Ok(true)` when the token set changed.
  pub fn class_list_add(&mut self, node: NodeId, tokens: &[&str]) -> Result<bool, DomError> {
    ensure_element_or_slot(self, node)?;
    validate_tokens(tokens)?;
    if tokens.is_empty() {
      return Ok(false);
    }

    let value = self.get_attribute(node, "class")?.unwrap_or_default();
    let mut token_set = parse_class_attribute(value);

    let mut changed = false;
    for &token in tokens {
      if token_set.iter().any(|t| t == token) {
        continue;
      }
      token_set.push(token.to_string());
      changed = true;
    }

    if changed {
      write_class_tokens(self, node, &token_set)?;
    }

    Ok(changed)
  }

  /// Removes `tokens` from the element's class list.
  ///
  /// Returns `Ok(true)` when the token set changed.
  pub fn class_list_remove(&mut self, node: NodeId, tokens: &[&str]) -> Result<bool, DomError> {
    ensure_element_or_slot(self, node)?;
    validate_tokens(tokens)?;
    if tokens.is_empty() {
      return Ok(false);
    }

    let value = self.get_attribute(node, "class")?.unwrap_or_default();
    let mut token_set = parse_class_attribute(value);
    let old_len = token_set.len();

    token_set.retain(|t| !tokens.iter().any(|&rm| rm == t));
    let changed = token_set.len() != old_len;

    if changed {
      write_class_tokens(self, node, &token_set)?;
    }

    Ok(changed)
  }

  /// Toggles `token` in the element's class list.
  ///
  /// Returns whether `token` is present after the operation.
  pub fn class_list_toggle(
    &mut self,
    node: NodeId,
    token: &str,
    force: Option<bool>,
  ) -> Result<bool, DomError> {
    ensure_element_or_slot(self, node)?;
    validate_token(token)?;

    let value = self.get_attribute(node, "class")?.unwrap_or_default();
    let mut token_set = parse_class_attribute(value);
    let pos = token_set.iter().position(|t| t == token);

    match (pos, force) {
      (Some(_), Some(true)) => Ok(true),
      (Some(pos), Some(false) | None) => {
        token_set.remove(pos);
        write_class_tokens(self, node, &token_set)?;
        Ok(false)
      }
      (None, Some(false)) => Ok(false),
      (None, Some(true) | None) => {
        token_set.push(token.to_string());
        write_class_tokens(self, node, &token_set)?;
        Ok(true)
      }
    }
  }

  /// Replaces `token` with `new_token`.
  ///
  /// Returns `Ok(true)` when `token` existed (even if `new_token` was already present).
  pub fn class_list_replace(
    &mut self,
    node: NodeId,
    token: &str,
    new_token: &str,
  ) -> Result<bool, DomError> {
    ensure_element_or_slot(self, node)?;
    validate_token(token)?;
    validate_token(new_token)?;

    let value = self.get_attribute(node, "class")?.unwrap_or_default();
    let mut token_set = parse_class_attribute(value);

    let Some(pos) = token_set.iter().position(|t| t == token) else {
      return Ok(false);
    };

    if token == new_token {
      return Ok(true);
    }

    if token_set.iter().any(|t| t == new_token) {
      // `new_token` already exists elsewhere; just remove the old token.
      token_set.remove(pos);
    } else {
      token_set[pos].clear();
      token_set[pos].push_str(new_token);
    }

    write_class_tokens(self, node, &token_set)?;
    Ok(true)
  }
}
