use crate::dom::HTML_NAMESPACE;

use super::{DomError, Document, NodeId, NodeKind};

#[inline]
fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

#[inline]
fn name_matches(existing: &str, query: &str, is_html: bool) -> bool {
  if is_html {
    existing.eq_ignore_ascii_case(query)
  } else {
    existing == query
  }
}

fn attrs_and_is_html(kind: &NodeKind) -> Option<(&Vec<(String, String)>, bool)> {
  match kind {
    NodeKind::Element {
      namespace,
      attributes,
      ..
    }
    | NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => Some((attributes, is_html_namespace(namespace))),
    _ => None,
  }
}

fn attrs_and_is_html_mut(kind: &mut NodeKind) -> Option<(&mut Vec<(String, String)>, bool)> {
  match kind {
    NodeKind::Element {
      namespace,
      attributes,
      ..
    }
    | NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => Some((attributes, is_html_namespace(namespace))),
    _ => None,
  }
}

impl Document {
  pub fn get_attribute(&self, node: NodeId, name: &str) -> Option<&str> {
    let node = self.nodes.get(node.index())?;
    let kind = &node.kind;
    let (attrs, is_html) = attrs_and_is_html(kind)?;
    attrs
      .iter()
      .find(|(k, _)| name_matches(k.as_str(), name, is_html))
      .map(|(_, v)| v.as_str())
  }

  pub fn has_attribute(&self, node: NodeId, name: &str) -> bool {
    let Some(node) = self.nodes.get(node.index()) else {
      return false;
    };
    let kind = &node.kind;
    let Some((attrs, is_html)) = attrs_and_is_html(kind) else {
      return false;
    };
    attrs
      .iter()
      .any(|(k, _)| name_matches(k.as_str(), name, is_html))
  }

  pub fn set_attribute(
    &mut self,
    node: NodeId,
    name: &str,
    value: &str,
  ) -> Result<bool, DomError> {
    let node = self
      .nodes
      .get_mut(node.index())
      .ok_or(DomError::NotFoundError)?;
    let kind = &mut node.kind;
    let Some((attrs, is_html)) = attrs_and_is_html_mut(kind) else {
      return Err(DomError::InvalidNodeType);
    };

    if let Some((_, existing)) = attrs
      .iter_mut()
      .find(|(k, _)| name_matches(k.as_str(), name, is_html))
    {
      if existing == value {
        return Ok(false);
      }
      existing.clear();
      existing.push_str(value);
      return Ok(true);
    }

    attrs.push((name.to_string(), value.to_string()));
    Ok(true)
  }

  pub fn remove_attribute(&mut self, node: NodeId, name: &str) -> Result<bool, DomError> {
    let node = self
      .nodes
      .get_mut(node.index())
      .ok_or(DomError::NotFoundError)?;
    let kind = &mut node.kind;
    let Some((attrs, is_html)) = attrs_and_is_html_mut(kind) else {
      return Err(DomError::InvalidNodeType);
    };

    if let Some(idx) = attrs
      .iter()
      .position(|(k, _)| name_matches(k.as_str(), name, is_html))
    {
      attrs.remove(idx);
      return Ok(true);
    }

    Ok(false)
  }

  pub fn set_bool_attribute(
    &mut self,
    node: NodeId,
    name: &str,
    present: bool,
  ) -> Result<bool, DomError> {
    if present {
      let node = self
        .nodes
        .get_mut(node.index())
        .ok_or(DomError::NotFoundError)?;
      let kind = &mut node.kind;
      let Some((attrs, is_html)) = attrs_and_is_html_mut(kind) else {
        return Err(DomError::InvalidNodeType);
      };
      if attrs
        .iter()
        .any(|(k, _)| name_matches(k.as_str(), name, is_html))
      {
        return Ok(false);
      }
      attrs.push((name.to_string(), String::new()));
      Ok(true)
    } else {
      self.remove_attribute(node, name)
    }
  }

  pub fn id(&self, node: NodeId) -> Option<&str> {
    self.get_attribute(node, "id")
  }

  pub fn class_name(&self, node: NodeId) -> Option<&str> {
    self.get_attribute(node, "class")
  }
}
