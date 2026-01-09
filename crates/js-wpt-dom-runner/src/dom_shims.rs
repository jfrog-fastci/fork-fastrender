use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{parse_fragment, ParseOpts};
use markup5ever::{LocalName, Namespace, QualName};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use std::cell::RefCell;
use std::rc::Rc;

const HTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

const DOM_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (g.__fastrender_dom_installed) return;
  g.__fastrender_dom_installed = true;

  var NODE_ID = Symbol("fastrender_node_id");

  function illegal() {
    throw new TypeError("Illegal constructor");
  }

  function Node() { illegal(); }
  function Document() { illegal(); }
  function DocumentFragment() { illegal(); }
  function Element() { illegal(); }

  Object.setPrototypeOf(Document.prototype, Node.prototype);
  Object.setPrototypeOf(DocumentFragment.prototype, Node.prototype);
  Object.setPrototypeOf(Element.prototype, Node.prototype);

  // Attach the existing `document` object (created by Rust) to `Document.prototype`.
  if (typeof g.document !== "object" || g.document === null) {
    g.document = Object.create(Document.prototype);
  } else {
    Object.setPrototypeOf(g.document, Document.prototype);
  }

  // Document node id is always 0.
  g.document[NODE_ID] = 0;

  function ensureArray(o, key) {
    if (!o[key]) o[key] = [];
    return o[key];
  }

  function detachFromParent(child) {
    var parent = child.parentNode;
    if (!parent) return;
    var arr = ensureArray(parent, "childNodes");
    var idx = arr.indexOf(child);
    if (idx >= 0) arr.splice(idx, 1);
    child.parentNode = null;
  }

  function nodeIdFromThis(self) {
    if (typeof self !== "object" || self === null) {
      throw new TypeError("Illegal invocation");
    }
    var id = self[NODE_ID];
    if (typeof id !== "number") {
      throw new TypeError("Illegal invocation");
    }
    return id;
  }

  function makeNode(proto, id, tagName) {
    var o = Object.create(proto);
    o[NODE_ID] = id;
    o.parentNode = null;
    o.childNodes = [];
    if (tagName !== undefined) {
      o.tagName = String(tagName);
    }
    return o;
  }

  Document.prototype.createElement = function (tagName) {
    var id = g.__fastrender_dom_create_element(String(tagName));
    return makeNode(Element.prototype, id, String(tagName).toUpperCase());
  };

  Document.prototype.createDocumentFragment = function () {
    var id = g.__fastrender_dom_create_document_fragment();
    return makeNode(DocumentFragment.prototype, id);
  };

  Object.defineProperty(Element.prototype, "innerHTML", {
    get: function () {
      return g.__fastrender_dom_get_inner_html(nodeIdFromThis(this));
    },
    set: function (html) {
      g.__fastrender_dom_set_inner_html(nodeIdFromThis(this), String(html));
      // Best-effort: we don't create JS wrappers for parsed children yet, so clear any cached list.
      if (this.childNodes) this.childNodes.length = 0;
    },
    configurable: true,
  });

  Object.defineProperty(Element.prototype, "outerHTML", {
    get: function () {
      return g.__fastrender_dom_get_outer_html(nodeIdFromThis(this));
    },
    set: function (html) {
      g.__fastrender_dom_set_outer_html(nodeIdFromThis(this), String(html));
      // The node has been replaced in its parent; best-effort detach in JS-land.
      detachFromParent(this);
    },
    configurable: true,
  });

  Node.prototype.appendChild = function (child) {
    var parentId = nodeIdFromThis(this);
    if (typeof child !== "object" || child === null) {
      throw new TypeError("Failed to execute 'appendChild' on 'Node': parameter 1 is not of type 'Node'.");
    }
    var childId = child[NODE_ID];
    if (typeof childId !== "number") {
      throw new TypeError("Failed to execute 'appendChild' on 'Node': parameter 1 is not of type 'Node'.");
    }

    // Keep JS-level pointers/arrays in sync for the tiny smoke corpus. We do not attempt to fully
    // mirror the Rust DOM.
    if (child instanceof DocumentFragment) {
      g.__fastrender_dom_append_child(parentId, childId);

      var parentNodes = ensureArray(this, "childNodes");
      var fragNodes = ensureArray(child, "childNodes");
      var moved = fragNodes.slice();
      for (var i = 0; i < moved.length; i++) {
        var n = moved[i];
        detachFromParent(n);
        parentNodes.push(n);
        n.parentNode = this;
      }
      fragNodes.length = 0;
      return child;
    }

    g.__fastrender_dom_append_child(parentId, childId);

    detachFromParent(child);
    var nodes = ensureArray(this, "childNodes");
    nodes.push(child);
    child.parentNode = this;
    return child;
  };

  Node.prototype.removeChild = function (child) {
    var parentId = nodeIdFromThis(this);
    if (typeof child !== "object" || child === null) {
      throw new TypeError("Failed to execute 'removeChild' on 'Node': parameter 1 is not of type 'Node'.");
    }
    var childId = child[NODE_ID];
    if (typeof childId !== "number") {
      throw new TypeError("Failed to execute 'removeChild' on 'Node': parameter 1 is not of type 'Node'.");
    }
    g.__fastrender_dom_remove_child(parentId, childId);
    detachFromParent(child);
    return child;
  };

  // Provide `document.head`/`document.body` for smoke tests.
  if (typeof g.__fastrender_dom_head_id === "number") {
    g.document.head = makeNode(Element.prototype, g.__fastrender_dom_head_id, "HEAD");
    g.document.head.parentNode = g.document;
  }
  if (typeof g.__fastrender_dom_body_id === "number") {
    g.document.body = makeNode(Element.prototype, g.__fastrender_dom_body_id, "BODY");
    g.document.body.parentNode = g.document;
  }

  Object.defineProperty(g, "Node", { value: Node, configurable: true, writable: true });
  Object.defineProperty(g, "Document", { value: Document, configurable: true, writable: true });
  Object.defineProperty(g, "DocumentFragment", { value: DocumentFragment, configurable: true, writable: true });
  Object.defineProperty(g, "Element", { value: Element, configurable: true, writable: true });
})();
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DomShimError {
  HierarchyRequestError,
  NotFoundError,
  InvalidNodeType,
}

impl DomShimError {
  fn code(self) -> &'static str {
    match self {
      DomShimError::HierarchyRequestError => "HierarchyRequestError",
      DomShimError::NotFoundError => "NotFoundError",
      DomShimError::InvalidNodeType => "InvalidNodeType",
    }
  }
}

fn dom_error_to_js_error(err: DomShimError) -> rquickjs::Error {
  rquickjs::Error::new_from_js_message("DOMException", "DOMException", err.code())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeId(usize);

#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeKind {
  Document,
  DocumentFragment,
  Element {
    tag_name: String,
    attributes: Vec<(String, String)>,
  },
  Text { content: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
  kind: NodeKind,
  parent: Option<NodeId>,
  children: Vec<NodeId>,
}

#[derive(Debug, Clone)]
struct Dom {
  nodes: Vec<Node>,
  head: NodeId,
  body: NodeId,
}

impl Dom {
  fn new() -> Self {
    let mut dom = Self {
      nodes: Vec::new(),
      head: NodeId(0),
      body: NodeId(0),
    };
    let document = dom.push_node(NodeKind::Document, None);
    debug_assert_eq!(document, NodeId(0));

    let html = dom.create_element("html");
    let head = dom.create_element("head");
    let body = dom.create_element("body");

    dom
      .append_child(document, html)
      .expect("document should accept <html>");
    dom
      .append_child(html, head)
      .expect("<html> should accept <head>");
    dom
      .append_child(html, body)
      .expect("<html> should accept <body>");

    dom.head = head;
    dom.body = body;

    dom
  }

  fn head(&self) -> NodeId {
    self.head
  }

  fn body(&self) -> NodeId {
    self.body
  }

  fn node_checked(&self, id: NodeId) -> Result<&Node, DomShimError> {
    self.nodes.get(id.0).ok_or(DomShimError::NotFoundError)
  }

  fn node_checked_mut(&mut self, id: NodeId) -> Result<&mut Node, DomShimError> {
    self
      .nodes
      .get_mut(id.0)
      .ok_or(DomShimError::NotFoundError)
  }

  fn push_node(&mut self, kind: NodeKind, parent: Option<NodeId>) -> NodeId {
    let id = NodeId(self.nodes.len());
    self.nodes.push(Node {
      kind,
      parent,
      children: Vec::new(),
    });
    if let Some(parent) = parent {
      if parent.0 < self.nodes.len() {
        self.nodes[parent.0].children.push(id);
      }
    }
    id
  }

  fn create_element(&mut self, tag_name: &str) -> NodeId {
    self.push_node(
      NodeKind::Element {
        tag_name: tag_name.to_ascii_lowercase(),
        attributes: Vec::new(),
      },
      None,
    )
  }

  fn create_text(&mut self, content: &str, parent: Option<NodeId>) -> NodeId {
    self.push_node(
      NodeKind::Text {
        content: content.to_string(),
      },
      parent,
    )
  }

  fn create_document_fragment(&mut self) -> NodeId {
    self.push_node(NodeKind::DocumentFragment, None)
  }

  fn detach_from_parent(&mut self, child: NodeId) -> Result<(), DomShimError> {
    let old_parent = self.node_checked(child)?.parent;
    let Some(old_parent) = old_parent else {
      return Ok(());
    };
    let parent_children = &mut self.node_checked_mut(old_parent)?.children;
    let idx = parent_children
      .iter()
      .position(|&id| id == child)
      .ok_or(DomShimError::NotFoundError)?;
    parent_children.remove(idx);
    self.node_checked_mut(child)?.parent = None;
    Ok(())
  }

  fn validate_parent_can_have_children(&self, parent: NodeId) -> Result<(), DomShimError> {
    match &self.node_checked(parent)?.kind {
      NodeKind::Text { .. } => Err(DomShimError::HierarchyRequestError),
      _ => Ok(()),
    }
  }

  fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), DomShimError> {
    self.node_checked(parent)?;
    self.node_checked(child)?;
    self.validate_parent_can_have_children(parent)?;

    if matches!(self.node_checked(child)?.kind, NodeKind::DocumentFragment) {
      // DocumentFragment insertion semantics: move its children into `parent` and empty the
      // fragment.
      let fragment_children = std::mem::take(&mut self.node_checked_mut(child)?.children);
      for moved in fragment_children {
        self.node_checked_mut(moved)?.parent = Some(parent);
        self.node_checked_mut(parent)?.children.push(moved);
      }
      // Fragments are never inserted into the tree.
      self.node_checked_mut(child)?.parent = None;
      return Ok(());
    }

    if self.node_checked(child)?.parent.is_some() {
      self.detach_from_parent(child)?;
    }

    self.node_checked_mut(child)?.parent = Some(parent);
    self.node_checked_mut(parent)?.children.push(child);
    Ok(())
  }

  fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), DomShimError> {
    self.node_checked(parent)?;
    self.node_checked(child)?;

    if self.node_checked(child)?.parent != Some(parent) {
      return Err(DomShimError::NotFoundError);
    }
    let parent_children = &mut self.node_checked_mut(parent)?.children;
    let idx = parent_children
      .iter()
      .position(|&id| id == child)
      .ok_or(DomShimError::NotFoundError)?;
    parent_children.remove(idx);
    self.node_checked_mut(child)?.parent = None;
    Ok(())
  }

  fn get_inner_html(&self, element: NodeId) -> Result<String, DomShimError> {
    match &self.node_checked(element)?.kind {
      NodeKind::Element { .. } => {}
      _ => return Err(DomShimError::InvalidNodeType),
    }
    let node = self.node_checked(element)?;
    let mut out = String::new();
    for &child in &node.children {
      self.serialize_node(child, &mut out)?;
    }
    Ok(out)
  }

  fn set_inner_html(&mut self, element: NodeId, html: &str) -> Result<(), DomShimError> {
    let tag_name = match &self.node_checked(element)?.kind {
      NodeKind::Element { tag_name, .. } => tag_name.clone(),
      _ => return Err(DomShimError::InvalidNodeType),
    };

    let new_children = self.parse_html_fragment(&tag_name, html);

    let old_children = std::mem::take(&mut self.node_checked_mut(element)?.children);
    for child in old_children {
      if child.0 < self.nodes.len() {
        self.node_checked_mut(child)?.parent = None;
      }
    }

    for &child in &new_children {
      self.node_checked_mut(child)?.parent = Some(element);
    }
    self.node_checked_mut(element)?.children = new_children;

    Ok(())
  }

  fn get_outer_html(&self, element: NodeId) -> Result<String, DomShimError> {
    match &self.node_checked(element)?.kind {
      NodeKind::Element { .. } => {}
      _ => return Err(DomShimError::InvalidNodeType),
    }
    let mut out = String::new();
    self.serialize_node(element, &mut out)?;
    Ok(out)
  }

  fn set_outer_html(&mut self, element: NodeId, html: &str) -> Result<(), DomShimError> {
    let Some(parent) = self.node_checked(element)?.parent else {
      // Spec: if the element has no parent, `outerHTML = ...` is a no-op.
      return Ok(());
    };

    // When possible, use the parent element tag name as the HTML fragment parsing context. For
    // non-element parents (Document / DocumentFragment) fall back to a neutral `<div>` context.
    let parent_tag = match &self.node_checked(parent)?.kind {
      NodeKind::Element { tag_name, .. } => tag_name.clone(),
      NodeKind::Document | NodeKind::DocumentFragment => "div".to_string(),
      NodeKind::Text { .. } => return Err(DomShimError::HierarchyRequestError),
    };

    let replacement_idx = self
      .node_checked(parent)?
      .children
      .iter()
      .position(|&id| id == element)
      .ok_or(DomShimError::NotFoundError)?;

    let new_nodes = self.parse_html_fragment(&parent_tag, html);

    // Detach the replaced element.
    self.node_checked_mut(element)?.parent = None;

    // Insert new nodes, then remove the old one.
    let parent_children = &mut self.node_checked_mut(parent)?.children;
    parent_children.splice(replacement_idx..replacement_idx + 1, new_nodes.iter().copied());
    for node_id in new_nodes {
      self.node_checked_mut(node_id)?.parent = Some(parent);
    }

    Ok(())
  }

  fn serialize_node(&self, root: NodeId, out: &mut String) -> Result<(), DomShimError> {
    enum Frame {
      Enter(NodeId),
      Exit(NodeId),
    }

    let mut stack = vec![Frame::Enter(root)];
    while let Some(frame) = stack.pop() {
      match frame {
        Frame::Enter(id) => {
          let node = self.node_checked(id)?;
          match &node.kind {
            NodeKind::Document | NodeKind::DocumentFragment => {
              for &child in node.children.iter().rev() {
                stack.push(Frame::Enter(child));
              }
            }
            NodeKind::Text { content } => {
              escape_text(out, content);
            }
            NodeKind::Element {
              tag_name,
              attributes,
            } => {
              out.push('<');
              out.push_str(tag_name);
              for (name, value) in attributes {
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                escape_attr_value(out, value);
                out.push('"');
              }
              out.push('>');
              if is_void_html_element(tag_name) {
                continue;
              }
              stack.push(Frame::Exit(id));
              for &child in node.children.iter().rev() {
                stack.push(Frame::Enter(child));
              }
            }
          }
        }
        Frame::Exit(id) => {
          let node = self.node_checked(id)?;
          if let NodeKind::Element { tag_name, .. } = &node.kind {
            if is_void_html_element(tag_name) {
              continue;
            }
            out.push_str("</");
            out.push_str(tag_name);
            out.push('>');
          }
        }
      }
    }
    Ok(())
  }

  fn parse_html_fragment(&mut self, context_tag: &str, html: &str) -> Vec<NodeId> {
    let context = QualName::new(
      None,
      Namespace::from(HTML_NAMESPACE),
      LocalName::from(context_tag.to_ascii_lowercase()),
    );

    let opts = ParseOpts {
      tree_builder: TreeBuilderOpts {
        scripting_enabled: true,
        ..Default::default()
      },
      ..Default::default()
    };

    // The last parameter is an html5ever fragment-parsing knob (currently a boolean). Keep it at
    // the default `true` expected by our smoke tests.
    let rcdom: RcDom = parse_fragment(RcDom::default(), opts, context, Vec::new(), true).one(html);

    let mut roots: Vec<NodeId> = Vec::new();

    #[derive(Clone)]
    struct WorkItem {
      parent: Option<NodeId>,
      handle: Handle,
    }

    let mut stack: Vec<WorkItem> = fragment_children_from_rcdom(&rcdom)
      .into_iter()
      .rev()
      .map(|handle| WorkItem { parent: None, handle })
      .collect();

    while let Some(item) = stack.pop() {
      match &item.handle.data {
        NodeData::Document => {
          for child in handle_children(&item.handle).into_iter().rev() {
            stack.push(WorkItem {
              parent: item.parent,
              handle: child,
            });
          }
        }
        NodeData::Text { contents } => {
          let content = contents.borrow().to_string();
          let id = self.create_text(&content, item.parent);
          if item.parent.is_none() {
            roots.push(id);
          }
        }
        NodeData::Element {
          name,
          attrs,
          template_contents,
          ..
        } => {
          let attrs_ref = attrs.borrow();
          let mut attributes = Vec::with_capacity(attrs_ref.len());
          for attr in attrs_ref.iter() {
            // Keep this minimal: treat everything as HTML and ignore namespaces/prefixes.
            attributes.push((attr.name.local.to_string(), attr.value.to_string()));
          }

          let id = self.push_node(
            NodeKind::Element {
              tag_name: name.local.to_string(),
              attributes,
            },
            item.parent,
          );
          if item.parent.is_none() {
            roots.push(id);
          }

          let is_template = name.local.as_ref().eq_ignore_ascii_case("template");
          let children = if is_template {
            template_contents
              .borrow()
              .as_ref()
              .map(handle_children)
              .unwrap_or_else(|| handle_children(&item.handle))
          } else {
            handle_children(&item.handle)
          };

          for child in children.into_iter().rev() {
            stack.push(WorkItem {
              parent: Some(id),
              handle: child,
            });
          }
        }
        _ => {}
      }
    }

    roots
  }
}

fn is_void_html_element(tag: &str) -> bool {
  // https://html.spec.whatwg.org/#void-elements
  matches!(
    tag,
    "area"
      | "base"
      | "br"
      | "col"
      | "embed"
      | "hr"
      | "img"
      | "input"
      | "link"
      | "meta"
      | "param"
      | "source"
      | "track"
      | "wbr"
  )
}

fn escape_text(out: &mut String, text: &str) {
  for ch in text.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      _ => out.push(ch),
    }
  }
}

fn escape_attr_value(out: &mut String, value: &str) {
  for ch in value.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '"' => out.push_str("&quot;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      _ => out.push(ch),
    }
  }
}

fn handle_children(handle: &Handle) -> Vec<Handle> {
  handle.children.borrow().iter().cloned().collect()
}

fn fragment_children_from_rcdom(rcdom: &RcDom) -> Vec<Handle> {
  let children = handle_children(&rcdom.document);
  let significant: Vec<Handle> = children
    .iter()
    .filter(|handle| !matches!(handle.data, NodeData::Doctype { .. } | NodeData::Comment { .. }))
    .cloned()
    .collect();

  // `html5ever`'s RcDom fragment parsing currently returns a synthetic `<html>` element as the sole
  // significant child of the document, with the actual fragment nodes as its children.
  if significant.len() == 1 {
    if let NodeData::Element { name, .. } = &significant[0].data {
      if name.ns.to_string() == HTML_NAMESPACE && name.local.as_ref().eq_ignore_ascii_case("html") {
        return handle_children(&significant[0]);
      }
    }
  }

  significant
}

pub fn install_dom_shims<'js>(ctx: Ctx<'js>, globals: &Object<'js>) -> JsResult<()> {
  let dom = Rc::new(RefCell::new(Dom::new()));

  let (head_id, body_id) = {
    let dom = dom.borrow();
    (dom.head().0 as i32, dom.body().0 as i32)
  };
  globals.set("__fastrender_dom_head_id", head_id)?;
  globals.set("__fastrender_dom_body_id", body_id)?;

  let create_element = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |tag_name: String| -> JsResult<i32> {
      let id = dom.borrow_mut().create_element(&tag_name);
      Ok(id.0 as i32)
    }
  })?;

  let create_document_fragment = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move || -> JsResult<i32> {
      let id = dom.borrow_mut().create_document_fragment();
      Ok(id.0 as i32)
    }
  })?;

  let get_inner_html = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<String> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .get_inner_html(NodeId(node_id as usize))
        .map_err(dom_error_to_js_error)
    }
  })?;

  let set_inner_html = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32, html: String| -> JsResult<()> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .set_inner_html(NodeId(node_id as usize), &html)
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let get_outer_html = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<String> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .get_outer_html(NodeId(node_id as usize))
        .map_err(dom_error_to_js_error)
    }
  })?;

  let set_outer_html = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32, html: String| -> JsResult<()> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .set_outer_html(NodeId(node_id as usize), &html)
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let append_child = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |parent_id: i32, child_id: i32| -> JsResult<()> {
      if parent_id < 0 || child_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .append_child(NodeId(parent_id as usize), NodeId(child_id as usize))
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let remove_child = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |parent_id: i32, child_id: i32| -> JsResult<()> {
      if parent_id < 0 || child_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .remove_child(NodeId(parent_id as usize), NodeId(child_id as usize))
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  globals.set("__fastrender_dom_create_element", create_element)?;
  globals.set(
    "__fastrender_dom_create_document_fragment",
    create_document_fragment,
  )?;
  globals.set("__fastrender_dom_get_inner_html", get_inner_html)?;
  globals.set("__fastrender_dom_set_inner_html", set_inner_html)?;
  globals.set("__fastrender_dom_get_outer_html", get_outer_html)?;
  globals.set("__fastrender_dom_set_outer_html", set_outer_html)?;
  globals.set("__fastrender_dom_append_child", append_child)?;
  globals.set("__fastrender_dom_remove_child", remove_child)?;

  ctx.eval::<(), _>(DOM_SHIM)?;
  Ok(())
}
