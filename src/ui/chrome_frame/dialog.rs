use crate::api::{BrowserDocument, FastRender, Pixmap, RenderOptions};
use crate::error::Result;
use crate::interaction::{InteractionAction, InteractionEngine, KeyAction};

/// Built-in "modal dialog" kinds used by browser chrome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogKind {
  Alert,
  Confirm,
  Prompt { default: String },
}

/// An HTML/CSS-rendered modal dialog document for the chrome frame.
///
/// This is a lightweight wrapper around a [`BrowserDocument`] and an [`InteractionEngine`].
/// Callers are expected to drive interaction by routing input events to `interaction` while passing
/// `document.dom_mut()` as the DOM root.
pub struct DialogDocument {
  pub document: BrowserDocument,
  pub interaction: InteractionEngine,
}

impl DialogDocument {
  /// Creates a new modal dialog document using default renderer options.
  pub fn new(kind: DialogKind, message: impl AsRef<str>) -> Result<Self> {
    Self::with_options(kind, message, RenderOptions::default())
  }

  /// Creates a new modal dialog document with custom renderer options.
  pub fn with_options(
    kind: DialogKind,
    message: impl AsRef<str>,
    options: RenderOptions,
  ) -> Result<Self> {
    let html = dialog_html(&kind, message.as_ref());
    let mut renderer = FastRender::new()?;
    renderer.set_fetcher(crate::ui::trusted_chrome_fetcher::trusted_chrome_fetcher());
    let document = BrowserDocument::new(renderer, &html, options)?;
    Ok(Self {
      document,
      interaction: InteractionEngine::new(),
    })
  }

  /// Render a new frame (or reuse cached layout), taking the current interaction state into account.
  pub fn render_frame(&mut self) -> Result<Pixmap> {
    Ok(
      self
        .document
        .render_frame_with_scroll_state_and_interaction_state(Some(
          self.interaction.interaction_state(),
        ))?
        .pixmap,
    )
  }

  /// Convenience wrapper around [`InteractionEngine::key_activate`], using a chrome-local base URL.
  ///
  /// This is primarily intended for unit tests and chrome-frame consumers that do not care about
  /// relative URL resolution.
  pub fn key_activate(&mut self, key: KeyAction) -> (bool, InteractionAction) {
    // Use a stable, parseable base URL so `resolve_url` behaves deterministically.
    const BASE_URL: &str = "chrome://dialog";
    self
      .interaction
      .key_activate(self.document.dom_mut(), key, BASE_URL, BASE_URL)
  }
}

const DIALOG_CSS: &str = r#"
:root {
  color-scheme: light dark;
  --dlg-bg: rgb(255, 255, 255);
  --dlg-text: rgb(15, 23, 42);
  --dlg-border: rgba(15, 23, 42, 0.18);
  --dlg-shadow: 0 18px 60px rgba(0, 0, 0, 0.22);
  --dlg-backdrop: rgba(0, 0, 0, 0.35);
  --dlg-btn-bg: rgba(15, 23, 42, 0.04);
  --dlg-btn-bg-hover: rgba(15, 23, 42, 0.08);
  --dlg-accent: rgb(37, 99, 235);
  --dlg-accent-bg: rgba(37, 99, 235, 0.15);
  --dlg-accent-bg-hover: rgba(37, 99, 235, 0.22);
  --dlg-accent-border: rgba(37, 99, 235, 0.55);
  --dlg-focus: rgba(37, 99, 235, 0.45);
}
@media (prefers-color-scheme: dark) {
  :root {
    --dlg-bg: rgb(14, 18, 28);
    --dlg-text: rgb(229, 231, 235);
    --dlg-border: rgba(255, 255, 255, 0.18);
    --dlg-shadow: 0 18px 60px rgba(0, 0, 0, 0.65);
    --dlg-backdrop: rgba(0, 0, 0, 0.55);
    --dlg-btn-bg: rgba(255, 255, 255, 0.08);
    --dlg-btn-bg-hover: rgba(255, 255, 255, 0.12);
    --dlg-accent: rgb(96, 165, 250);
    --dlg-accent-bg: rgba(96, 165, 250, 0.22);
    --dlg-accent-bg-hover: rgba(96, 165, 250, 0.30);
    --dlg-accent-border: rgba(96, 165, 250, 0.60);
    --dlg-focus: rgba(96, 165, 250, 0.55);
  }
}
html, body {
  height: 100%;
}
body {
  margin: 0;
  font: 14px/1.4 system-ui, -apple-system, Segoe UI, sans-serif;
  color: var(--dlg-text);
}
* {
  box-sizing: border-box;
}
.dialog-backdrop {
  position: fixed;
  inset: 0;
  background: var(--dlg-backdrop);
}
.dialog-root {
  position: fixed;
  inset: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 24px;
}
.dialog-modal {
  width: min(520px, 100%);
  max-width: 100%;
  padding: 18px 18px 16px;
  border-radius: 12px;
  background: var(--dlg-bg);
  border: 1px solid var(--dlg-border);
  box-shadow: var(--dlg-shadow);
}
.dialog-message {
  margin: 0 0 12px;
  white-space: pre-wrap;
}
.dialog-input {
  width: 100%;
  padding: 8px 10px;
  border-radius: 8px;
  border: 1px solid var(--dlg-border);
  background: transparent;
  color: inherit;
  font: inherit;
}
.dialog-input:focus {
  outline: none;
  border-color: var(--dlg-accent-border);
  box-shadow: 0 0 0 3px var(--dlg-focus);
}
.dialog-actions {
  margin-top: 14px;
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.dialog-btn {
  display: inline-block;
  padding: 7px 12px;
  border-radius: 8px;
  border: 1px solid var(--dlg-border);
  background: var(--dlg-btn-bg);
  color: inherit;
  text-decoration: none;
  font: inherit;
  line-height: 1;
  cursor: pointer;
}
button.dialog-btn {
  appearance: none;
}
.dialog-btn:hover {
  background: var(--dlg-btn-bg-hover);
}
.dialog-btn.primary {
  background: var(--dlg-accent-bg);
  border-color: var(--dlg-accent-border);
}
.dialog-btn.primary:hover {
  background: var(--dlg-accent-bg-hover);
}
.dialog-btn:focus {
  outline: none;
  box-shadow: 0 0 0 3px var(--dlg-focus);
}
"#;

fn dialog_html(kind: &DialogKind, message: &str) -> String {
  let safe_message = escape_html(message);

  let inner = match kind {
    DialogKind::Alert => format!(
      r#"<p id="dialog-message" class="dialog-message">{message}</p>
<div class="dialog-actions">
  <a id="dialog-ok" class="dialog-btn primary" href="chrome-dialog:accept">OK</a>
</div>"#,
      message = safe_message
    ),
    DialogKind::Confirm => format!(
      r#"<p id="dialog-message" class="dialog-message">{message}</p>
<div class="dialog-actions">
  <a id="dialog-cancel" class="dialog-btn" href="chrome-dialog:cancel">Cancel</a>
  <a id="dialog-ok" class="dialog-btn primary" href="chrome-dialog:accept">OK</a>
</div>"#,
      message = safe_message
    ),
    DialogKind::Prompt { default } => {
      let safe_default = escape_html(default);
      format!(
        r#"<form id="dialog-form" action="chrome-dialog:accept" method="get">
  <p id="dialog-message" class="dialog-message">{message}</p>
  <input id="dialog-input" class="dialog-input" type="text" name="value" value="{default}" autofocus>
  <div class="dialog-actions">
    <a id="dialog-cancel" class="dialog-btn" href="chrome-dialog:cancel">Cancel</a>
    <button id="dialog-ok" class="dialog-btn primary" type="submit">OK</button>
  </div>
</form>"#,
        message = safe_message,
        default = safe_default
      )
    }
  };

  format!(
    r#"<!doctype html>
<html class="chrome-dialog">
  <head>
    <meta charset="utf-8">
    <title>Dialog</title>
    <style>{css}</style>
  </head>
  <body>
    <div class="dialog-backdrop"></div>
    <div class="dialog-root">
      <div class="dialog-modal" role="dialog" aria-modal="true">
        {inner}
      </div>
    </div>
  </body>
</html>"#,
    css = DIALOG_CSS,
    inner = inner
  )
}

fn escape_html(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  for ch in text.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' => out.push_str("&quot;"),
      '\'' => out.push_str("&#39;"),
      _ => out.push(ch),
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom::{enumerate_dom_ids, DomNode};

  fn find_by_id<'a>(root: &'a DomNode, html_id: &str) -> Option<&'a DomNode> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
      if node.get_attribute_ref("id") == Some(html_id) {
        return Some(node);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  fn node_id(root: &DomNode, html_id: &str) -> usize {
    let ids = enumerate_dom_ids(root);
    let node = find_by_id(root, html_id).expect("node with id");
    ids
      .get(&(node as *const DomNode))
      .copied()
      .expect("preorder id present")
  }

  fn set_test_viewport(doc: &mut BrowserDocument) {
    doc.set_viewport(320, 240);
    doc.set_device_pixel_ratio(1.0);
  }

  #[test]
  fn alert_renders() {
    let mut dialog = DialogDocument::new(DialogKind::Alert, "Hello").expect("dialog");
    set_test_viewport(&mut dialog.document);
    let pixmap = dialog.render_frame().expect("render");
    assert_eq!(pixmap.width(), 320);
    assert_eq!(pixmap.height(), 240);
  }

  #[test]
  fn confirm_ok_cancel_produce_distinct_actions() {
    let mut dialog = DialogDocument::new(DialogKind::Confirm, "Continue?").expect("dialog");
    let dom = dialog.document.dom();
    let ok_id = node_id(dom, "dialog-ok");
    let cancel_id = node_id(dom, "dialog-cancel");

    dialog
      .interaction
      .focus_node_id(dialog.document.dom_mut(), Some(ok_id), true);
    let (_, ok_action) = dialog.key_activate(KeyAction::Enter);
    assert_eq!(
      ok_action,
      InteractionAction::Navigate {
        href: "chrome-dialog:accept".to_string()
      }
    );

    dialog
      .interaction
      .focus_node_id(dialog.document.dom_mut(), Some(cancel_id), true);
    let (_, cancel_action) = dialog.key_activate(KeyAction::Enter);
    assert_eq!(
      cancel_action,
      InteractionAction::Navigate {
        href: "chrome-dialog:cancel".to_string()
      }
    );
  }

  #[test]
  fn prompt_enter_submits_value() {
    let mut dialog = DialogDocument::new(
      DialogKind::Prompt {
        default: String::new(),
      },
      "Name?",
    )
    .expect("dialog");
    let dom = dialog.document.dom();
    let input_id = node_id(dom, "dialog-input");

    dialog
      .interaction
      .focus_node_id(dialog.document.dom_mut(), Some(input_id), true);
    dialog
      .interaction
      .text_input(dialog.document.dom_mut(), "abc");

    let (_, action) = dialog.key_activate(KeyAction::Enter);
    assert_eq!(
      action,
      InteractionAction::Navigate {
        href: "chrome-dialog:accept?value=abc".to_string()
      }
    );
  }
}
