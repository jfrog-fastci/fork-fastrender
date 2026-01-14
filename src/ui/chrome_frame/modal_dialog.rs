//! HTML templates for renderer-driven modal dialogs (alert/confirm/prompt).
//!
//! Like the rest of the renderer-chrome templates, these dialogs are intended to be driven without
//! JavaScript (at least initially) by emitting `chrome-dialog:` navigations for accept/cancel.

use crate::ui::html_escape::escape_html;

/// Kind of modal dialog being rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalDialogKind {
  Alert,
  Confirm,
  Prompt,
}

/// Semantic action taken by clicking a modal button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalDialogButtonAction {
  Accept,
  Cancel,
}

impl ModalDialogButtonAction {
  pub fn chrome_dialog_url(self) -> &'static str {
    match self {
      Self::Accept => "chrome-dialog:accept",
      Self::Cancel => "chrome-dialog:cancel",
    }
  }
}

/// A clickable button rendered in the modal footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalDialogButton {
  pub label: String,
  pub action: ModalDialogButtonAction,
  /// Whether this button is the default/primary action.
  pub primary: bool,
}

impl ModalDialogButton {
  pub fn accept(label: impl Into<String>) -> Self {
    Self {
      label: label.into(),
      action: ModalDialogButtonAction::Accept,
      primary: true,
    }
  }

  pub fn cancel(label: impl Into<String>) -> Self {
    Self {
      label: label.into(),
      action: ModalDialogButtonAction::Cancel,
      primary: false,
    }
  }
}

/// Prompt-specific input field configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptField {
  /// Name of the `<input>` field. Used when submitting a `chrome-dialog:` navigation.
  pub name: String,
  pub value: String,
  pub placeholder: Option<String>,
}

impl PromptField {
  pub fn new(name: impl Into<String>) -> Self {
    Self {
      name: name.into(),
      value: String::new(),
      placeholder: None,
    }
  }
}

/// Model describing a modal dialog to be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
  pub kind: ModalDialogKind,
  pub title: String,
  pub body: String,
  pub buttons: Vec<ModalDialogButton>,
  pub prompt: Option<PromptField>,
}

impl Model {
  pub fn alert(title: impl Into<String>, body: impl Into<String>) -> Self {
    Self {
      kind: ModalDialogKind::Alert,
      title: title.into(),
      body: body.into(),
      buttons: vec![ModalDialogButton::accept("OK")],
      prompt: None,
    }
  }

  pub fn confirm(title: impl Into<String>, body: impl Into<String>) -> Self {
    Self {
      kind: ModalDialogKind::Confirm,
      title: title.into(),
      body: body.into(),
      buttons: vec![
        ModalDialogButton::cancel("Cancel"),
        ModalDialogButton::accept("OK"),
      ],
      prompt: None,
    }
  }

  pub fn prompt(title: impl Into<String>, body: impl Into<String>, prompt: PromptField) -> Self {
    Self {
      kind: ModalDialogKind::Prompt,
      title: title.into(),
      body: body.into(),
      buttons: vec![
        ModalDialogButton::cancel("Cancel"),
        ModalDialogButton::accept("OK"),
      ],
      prompt: Some(prompt),
    }
  }
}

/// Build a renderer-chrome modal dialog HTML document.
///
/// The output is a standalone, parseable HTML document that links to `chrome://styles/chrome.css`.
#[must_use]
pub fn modal_dialog_html(model: &Model) -> String {
  let safe_title = escape_html(&model.title);
  let safe_body = escape_html(&model.body);

  let prompt_html = match model.kind {
    ModalDialogKind::Prompt => {
      let prompt = model.prompt.as_ref();
      let name = prompt.map(|p| p.name.as_str()).unwrap_or("value");
      let value = prompt.map(|p| p.value.as_str()).unwrap_or("");
      let placeholder = prompt.and_then(|p| p.placeholder.as_deref());

      let safe_name = escape_html(name);
      let safe_value = escape_html(value);
      let placeholder_attr = placeholder.map(|p| format!(r#" placeholder="{}""#, escape_html(p)));

      format!(
        r#"<div class="chrome-modal-input-row">
          <input class="chrome-modal-input" type="text" name="{safe_name}" value="{safe_value}" aria-labelledby="chrome-modal-body"{placeholder_attr} autofocus>
        </div>"#,
        placeholder_attr = placeholder_attr.unwrap_or_default()
      )
    }
    _ => String::new(),
  };

  let mut buttons_html = String::new();
  for button in &model.buttons {
    let class = if button.primary {
      "chrome-modal-button chrome-modal-button-primary"
    } else {
      "chrome-modal-button"
    };
    let label = escape_html(&button.label);
    buttons_html.push_str(&format!(
      r#"<button type="submit" class="{class}" formaction="{action}">{label}</button>"#,
      class = class,
      action = button.action.chrome_dialog_url(),
      label = label
    ));
  }

  // Keep the template JS-free: the chrome host should intercept `chrome-dialog:` navigations and
  // dispatch them to the appropriate modal result handlers.
  format!(
    r#"<!doctype html>
<html class="chrome-modal-document">
  <head>
    <meta charset="utf-8">
    <link rel="stylesheet" href="chrome://styles/chrome.css">
    <title>{safe_title}</title>
  </head>
  <body>
    <div class="chrome-modal-backdrop">
      <form
        class="chrome-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="chrome-modal-title"
        aria-describedby="chrome-modal-body"
        action="chrome-dialog:accept"
        method="get"
      >
        <div class="chrome-modal-header">
          <div id="chrome-modal-title" class="chrome-modal-title">{safe_title}</div>
        </div>
        <div id="chrome-modal-body" class="chrome-modal-body">{safe_body}</div>
        {prompt_html}
        <div class="chrome-modal-buttons">{buttons_html}</div>
      </form>
    </div>
  </body>
</html>"#,
    safe_title = safe_title,
    safe_body = safe_body,
    prompt_html = prompt_html,
    buttons_html = buttons_html
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{BrowserDocument, FastRender, FontConfig, RenderOptions};

  fn count_elements_with_tag(root: &crate::dom::DomNode, tag: &str) -> usize {
    let mut count = 0usize;
    root.walk_tree(&mut |node| {
      if matches!(node.tag_name(), Some(name) if name.eq_ignore_ascii_case(tag)) {
        count += 1;
      }
    });
    count
  }

  fn count_nodes_with_class(root: &crate::dom::DomNode, class: &str) -> usize {
    let mut count = 0usize;
    root.walk_tree(&mut |node| {
      if node.is_element() && node.has_class(class) {
        count += 1;
      }
    });
    count
  }

  fn count_buttons_with_formaction(root: &crate::dom::DomNode, url: &str) -> usize {
    let mut count = 0usize;
    root.walk_tree(&mut |node| {
      if !matches!(node.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("button")) {
        return;
      }
      if node
        .is_element()
        .then(|| node.get_attribute_ref("formaction"))
        .flatten()
        .is_some_and(|value| value == url)
      {
        count += 1;
      }
    });
    count
  }

  fn parse_with_deterministic_fonts(html: &str) -> BrowserDocument {
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    BrowserDocument::new(renderer, html, RenderOptions::default()).expect("parse modal dialog HTML")
  }

  #[test]
  fn modal_dialog_template_is_parseable() {
    let model = Model::alert("Hello", "Body text");
    let html = modal_dialog_html(&model);

    let _doc = BrowserDocument::from_html(&html, RenderOptions::default())
      .expect("BrowserDocument::from_html should parse modal dialog HTML");
    let doc = parse_with_deterministic_fonts(&html);

    assert_eq!(
      count_nodes_with_class(doc.dom(), "chrome-modal-backdrop"),
      1,
      "expected exactly one .chrome-modal-backdrop"
    );
    assert_eq!(
      count_nodes_with_class(doc.dom(), "chrome-modal"),
      1,
      "expected exactly one .chrome-modal"
    );
  }

  #[test]
  fn modal_dialog_escapes_title_and_body() {
    let model = Model::alert(r#"<script id="evil">"#, r#"hello & <b>world</b>"#);
    let html = modal_dialog_html(&model);

    assert!(
      html.contains("&lt;script id=&quot;evil&quot;&gt;"),
      "expected title to be escaped"
    );
    assert!(
      html.contains("hello &amp; &lt;b&gt;world&lt;/b&gt;"),
      "expected body to be escaped"
    );

    let doc = parse_with_deterministic_fonts(&html);
    assert_eq!(
      count_elements_with_tag(doc.dom(), "script"),
      0,
      "escaped title/body should not create <script> elements"
    );
  }

  #[test]
  fn confirm_dialog_includes_accept_and_cancel_buttons() {
    let model = Model::confirm("Confirm", "Are you sure?");
    let html = modal_dialog_html(&model);
    let doc = parse_with_deterministic_fonts(&html);

    assert_eq!(
      count_buttons_with_formaction(doc.dom(), "chrome-dialog:accept"),
      1,
      "confirm dialog should include exactly one accept button"
    );
    assert_eq!(
      count_buttons_with_formaction(doc.dom(), "chrome-dialog:cancel"),
      1,
      "confirm dialog should include exactly one cancel button"
    );
  }

  #[test]
  fn prompt_dialog_labels_input_from_body_text() {
    let model = Model::prompt("Prompt", "Enter value", PromptField::new("value"));
    let html = modal_dialog_html(&model);
    assert!(
      html.contains(r#"aria-labelledby="chrome-modal-body""#),
      "expected prompt input to be labelled by the dialog body"
    );
  }
}
