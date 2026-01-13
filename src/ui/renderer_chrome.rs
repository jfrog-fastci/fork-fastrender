#![cfg(feature = "browser_ui")]

use crate::dom::{enumerate_dom_ids, DomNode, DomNodeType, HTML_NAMESPACE};
use crate::ui::omnibox::{OmniboxAction, OmniboxSuggestion};
use crate::ui::url::{search_url_for_query, DEFAULT_SEARCH_ENGINE_TEMPLATE};
use crate::ui::{BrowserAppState, BrowserTabController, ChromeAction, RepaintReason, TabId, UiToWorker};
use crate::{FastRender, Result};
use winit::event::VirtualKeyCode;

const CHROME_DOC_URL: &str = "about:chrome";
const ADDRESS_BAR_ID: &str = "fastr_address_bar";
const OMNIBOX_ID: &str = "fastr_omnibox";

const CHROME_FRAME_HTML: &str = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; font: 14px sans-serif; }
      #chrome { padding: 6px; }
      #fastr_address_bar { width: calc(100% - 12px); padding: 4px 6px; font: inherit; }
      #fastr_omnibox { margin-top: 4px; border: 1px solid #ccc; display: none; }
      #fastr_omnibox.open { display: block; }
      .suggestion { padding: 4px 6px; }
      .suggestion.selected { background: rgb(200, 220, 255); }
    </style>
  </head>
  <body>
    <div id="chrome">
      <input id="fastr_address_bar" type="text" value="">
      <div id="fastr_omnibox" class="omnibox"></div>
    </div>
  </body>
</html>
"#;

/// Minimal headless "renderer-chrome" document for experimenting with HTML-rendered browser chrome.
///
/// This is currently scoped to the omnibox (address bar + suggestion dropdown) so keyboard
/// navigation can be validated without wiring the full windowed renderer-chrome UI.
pub struct ChromeFrameDocument {
  controller: BrowserTabController,
}

impl ChromeFrameDocument {
  pub fn new(viewport_css: (u32, u32), dpr: f32) -> Result<Self> {
    let renderer = FastRender::new()?;
    Self::new_with_renderer(renderer, viewport_css, dpr)
  }

  pub fn new_with_renderer(renderer: FastRender, viewport_css: (u32, u32), dpr: f32) -> Result<Self> {
    let tab_id = TabId::new();
    let controller = BrowserTabController::from_html_with_renderer(
      renderer,
      tab_id,
      CHROME_FRAME_HTML,
      CHROME_DOC_URL,
      viewport_css,
      dpr,
    )?;
    Ok(Self { controller })
  }

  pub fn controller(&self) -> &BrowserTabController {
    &self.controller
  }

  pub fn dom(&self) -> &DomNode {
    self.controller.document().dom()
  }

  pub fn tab_id(&self) -> TabId {
    self.controller.tab_id()
  }

  /// Synchronize the HTML chrome document's DOM with the current browser state.
  ///
  /// This updates:
  /// - address bar input value (`#fastr_address_bar`)
  /// - omnibox dropdown visibility + suggestion rows (`#fastr_omnibox`)
  /// - selected-row highlight (CSS class `selected`)
  /// - focus (best-effort) to match `ChromeState::address_bar_has_focus`
  pub fn sync_state(&mut self, app: &BrowserAppState) -> Result<()> {
    self.controller.mutate_dom(|dom| sync_dom_state(dom, app));

    // Sync focus state (tracked out-of-DOM via InteractionState).
    let node_id = node_id_by_id_attr(self.controller.document().dom(), ADDRESS_BAR_ID);
    match (app.chrome.address_bar_has_focus, node_id) {
      (true, Some(id)) => {
        let _ = self.controller.focus_node_id(Some(id), true);
      }
      (false, _) => {
        let _ = self.controller.focus_node_id(None, false);
      }
      _ => {}
    }

    Ok(())
  }

  /// Convenience: request a repaint from the underlying controller after a state sync.
  pub fn request_repaint(&mut self) -> Result<()> {
    let _ = self.controller.handle_message(UiToWorker::RequestRepaint {
      tab_id: self.controller.tab_id(),
      reason: RepaintReason::Explicit,
    })?;
    Ok(())
  }

  /// Intercept omnibox keyboard navigation keys before forwarding them to the underlying HTML
  /// document.
  ///
  /// Returns `Some(actions)` when the key was consumed by omnibox navigation (even if `actions` is
  /// empty). Returns `None` when the key should be forwarded to the `ChromeFrameDocument` as normal
  /// (for example, caret movement when the dropdown is closed).
  pub fn preempt_omnibox_key_action(
    &mut self,
    app: &mut BrowserAppState,
    key: VirtualKeyCode,
  ) -> Option<Vec<ChromeAction>> {
    if !app.chrome.address_bar_has_focus || !app.chrome.omnibox.open {
      return None;
    }

    match key {
      VirtualKeyCode::Down | VirtualKeyCode::Up => {
        let len = app.chrome.omnibox.suggestions.len();
        if len == 0 {
          return None;
        }

        let next = if matches!(key, VirtualKeyCode::Down) {
          match app.chrome.omnibox.selected {
            None => 0,
            Some(i) => (i + 1) % len,
          }
        } else {
          // Up
          match app.chrome.omnibox.selected {
            None => len - 1,
            Some(i) => (i + len - 1) % len,
          }
        };

        if app.chrome.omnibox.selected.is_none() && app.chrome.omnibox.original_input.is_none() {
          app.chrome.omnibox.original_input = Some(app.chrome.address_bar_text.clone());
        }
        app.chrome.omnibox.selected = Some(next);

        if let Some(suggestion) = app.chrome.omnibox.suggestions.get(next) {
          if let Some(fill) = omnibox_suggestion_fill_text(suggestion) {
            app.chrome.address_bar_text = fill.to_string();
          }
        }

        let _ = self.sync_state(app);
        Some(Vec::new())
      }
      VirtualKeyCode::Escape => {
        app.chrome.omnibox.open = false;
        app.chrome.omnibox.selected = None;
        if let Some(original) = app.chrome.omnibox.original_input.take() {
          app.chrome.address_bar_text = original;
        }
        let _ = self.sync_state(app);
        Some(Vec::new())
      }
      VirtualKeyCode::Return | VirtualKeyCode::NumpadEnter => {
        let suggestion = app
          .chrome
          .omnibox
          .selected
          .and_then(|i| app.chrome.omnibox.suggestions.get(i));
        let Some(suggestion) = suggestion else {
          return None;
        };

        let accept_action = omnibox_suggestion_accept_action(suggestion);

        // Mirror egui chrome behaviour: if the resolved action is a NavigateTo, force the address bar
        // text to the final URL so it reflects the committed navigation immediately.
        if let ChromeAction::NavigateTo(url) = &accept_action {
          app.chrome.address_bar_text = url.clone();
        }

        app.chrome.address_bar_editing = false;
        app.chrome.address_bar_has_focus = false;
        app.chrome.omnibox.reset();

        let _ = self.sync_state(app);

        Some(vec![accept_action, ChromeAction::AddressBarFocusChanged(false)])
      }
      _ => None,
    }
  }
}

fn omnibox_suggestion_fill_text(suggestion: &OmniboxSuggestion) -> Option<&str> {
  match &suggestion.action {
    OmniboxAction::NavigateToUrl(url) => Some(url),
    OmniboxAction::ActivateTab(_) => suggestion.url.as_deref(),
    OmniboxAction::Search(query) => Some(query),
  }
}

fn omnibox_suggestion_accept_action(suggestion: &OmniboxSuggestion) -> ChromeAction {
  match &suggestion.action {
    OmniboxAction::NavigateToUrl(url) => ChromeAction::NavigateTo(url.clone()),
    OmniboxAction::ActivateTab(tab_id) => ChromeAction::ActivateTab(*tab_id),
    OmniboxAction::Search(query) => ChromeAction::NavigateTo(
      search_url_for_query(query, DEFAULT_SEARCH_ENGINE_TEMPLATE).unwrap_or_else(|_| query.clone()),
    ),
  }
}

fn sync_dom_state(dom: &mut DomNode, app: &BrowserAppState) -> bool {
  let input_path = find_path_by_id_attr(dom, ADDRESS_BAR_ID);
  if let Some(path) = input_path {
    let node = node_mut_by_path(dom, &path);
    node.set_attribute("value", &app.chrome.address_bar_text);
  }

  let omnibox_path = find_path_by_id_attr(dom, OMNIBOX_ID);
  if let Some(path) = omnibox_path {
    let node = node_mut_by_path(dom, &path);
    let open = app.chrome.omnibox.open;
    if open {
      node.set_attribute("class", "omnibox open");
    } else {
      node.set_attribute("class", "omnibox");
    }

    node.children.clear();
    for (idx, suggestion) in app.chrome.omnibox.suggestions.iter().enumerate() {
      let selected = app.chrome.omnibox.selected == Some(idx);
      node.children.push(build_suggestion_row(idx, suggestion, selected));
    }
  }

  // Always mark the document dirty: this is currently a small chrome-only DOM and we want changes
  // to be reflected deterministically in subsequent paints.
  true
}

fn build_suggestion_row(idx: usize, suggestion: &OmniboxSuggestion, selected: bool) -> DomNode {
  let mut class = "suggestion".to_string();
  if selected {
    class.push_str(" selected");
  }

  let mut attrs = vec![
    ("class".to_string(), class),
    ("data-index".to_string(), idx.to_string()),
  ];
  if selected {
    attrs.push(("data-selected".to_string(), "".to_string()));
  }

  let label = suggestion_label(suggestion);
  DomNode {
    node_type: DomNodeType::Element {
      tag_name: "div".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: attrs,
    },
    children: vec![DomNode {
      node_type: DomNodeType::Text { content: label },
      children: Vec::new(),
    }],
  }
}

fn suggestion_label(suggestion: &OmniboxSuggestion) -> String {
  if let Some(title) = suggestion.title.as_deref().filter(|t| !t.trim().is_empty()) {
    return title.trim().to_string();
  }
  if let Some(url) = suggestion.url.as_deref().filter(|u| !u.trim().is_empty()) {
    return url.trim().to_string();
  }
  match &suggestion.action {
    OmniboxAction::NavigateToUrl(url) => url.clone(),
    OmniboxAction::ActivateTab(tab_id) => format!("Activate tab {tab_id:?}"),
    OmniboxAction::Search(query) => query.clone(),
  }
}

fn find_path_by_id_attr(root: &DomNode, id_attr: &str) -> Option<Vec<usize>> {
  let mut stack: Vec<(&DomNode, Vec<usize>)> = vec![(root, Vec::new())];
  while let Some((node, path)) = stack.pop() {
    if node.get_attribute_ref("id").is_some_and(|id| id == id_attr) {
      return Some(path);
    }
    for (idx, child) in node.children.iter().enumerate().rev() {
      let mut next = path.clone();
      next.push(idx);
      stack.push((child, next));
    }
  }
  None
}

fn node_mut_by_path<'a>(root: &'a mut DomNode, path: &[usize]) -> &'a mut DomNode {
  let mut node = root;
  for &idx in path {
    node = &mut node.children[idx];
  }
  node
}

fn node_id_by_id_attr(root: &DomNode, id_attr: &str) -> Option<usize> {
  let ids = enumerate_dom_ids(root);
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id").is_some_and(|id| id == id_attr) {
      return ids.get(&(node as *const DomNode)).copied();
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}
