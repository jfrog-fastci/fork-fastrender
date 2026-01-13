//! HTML templates for renderer-driven context menus (dogfooding FastRender).
//!
//! The context menu is rendered as trusted HTML/CSS (browser process) and can be interacted with
//! without JavaScript by using `chrome-action:` navigations.

use crate::ui::html_escape::escape_html;

/// A renderer-driven context menu entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
  /// A clickable menu item.
  Item(Item),
  /// A visual separator between groups of actions.
  Separator,
}

/// A clickable item in a renderer-driven context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
  /// Visible label for the menu item.
  pub label: String,
  /// Optional icon URL (typically `chrome://...`).
  pub icon_url: Option<String>,
  /// Whether the item is enabled and should navigate when clicked.
  pub enabled: bool,
  /// Target URL when activated (typically a `chrome-action:*` URL).
  pub href: String,
}

/// Build a standalone context menu HTML document.
///
/// This is intended to be embedded/painted as part of the chrome UI (eventually as an overlay), but
/// it is returned as a full HTML document so it can be parsed and rendered standalone for tests and
/// prototyping.
pub fn context_menu_html(entries: &[Entry]) -> String {
  let mut out = String::with_capacity(256 + entries.len() * 96);

  for entry in entries {
    match entry {
      Entry::Separator => {
        out.push_str("      <div class=\"chrome-context-menu-separator\" role=\"separator\"></div>\n");
      }
      Entry::Item(item) => {
        let label = escape_html(&item.label);
        let icon_html = match item.icon_url.as_deref() {
          Some(url) if !url.is_empty() => format!(
            "<img class=\"chrome-context-menu-icon\" src=\"{}\" alt=\"\">",
            escape_html(url)
          ),
          _ => "<span class=\"chrome-context-menu-icon-spacer\"></span>".to_string(),
        };

        if item.enabled {
          out.push_str(&format!(
            "      <a class=\"chrome-context-menu-item\" role=\"menuitem\" href=\"{}\">{}{}</a>\n",
            escape_html(&item.href),
            icon_html,
            format!("<span class=\"chrome-context-menu-label\">{label}</span>")
          ));
        } else {
          out.push_str(&format!(
            "      <a class=\"chrome-context-menu-item disabled\" role=\"menuitem\" aria-disabled=\"true\" tabindex=\"-1\" href=\"#\">{}{}</a>\n",
            icon_html,
            format!("<span class=\"chrome-context-menu-label\">{label}</span>")
          ));
        }
      }
    }
  }

  format!(
    r#"<!doctype html>
<html class="chrome-context-menu-doc">
  <head>
    <meta charset="utf-8">
    <link rel="stylesheet" href="chrome://styles/chrome.css">
    <title>Context Menu</title>
  </head>
  <body>
    <div class="chrome-context-menu" role="menu" aria-label="Context menu">
{out}    </div>
  </body>
</html>"#
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{BrowserDocument, FastRender, FontConfig, RenderOptions};

  fn count_nodes_with_class(root: &crate::dom::DomNode, class_name: &str) -> usize {
    let mut count = 0usize;
    root.walk_tree(&mut |node| {
      if node.is_element() && node.has_class(class_name) {
        count += 1;
      }
    });
    count
  }

  #[test]
  fn context_menu_template_is_parseable() {
    let entries = vec![
      Entry::Item(Item {
        label: "Copy".to_string(),
        icon_url: None,
        enabled: true,
        href: "chrome-action:copy".to_string(),
      }),
      Entry::Separator,
      Entry::Item(Item {
        label: "Paste".to_string(),
        icon_url: Some("chrome://icons/paste.svg".to_string()),
        enabled: false,
        href: "chrome-action:paste".to_string(),
      }),
    ];

    let html = context_menu_html(&entries);

    // Quick invariant: should link the shared chrome stylesheet.
    assert!(
      html.contains(r#"<link rel="stylesheet" href="chrome://styles/chrome.css">"#),
      "context menu should link chrome://styles/chrome.css"
    );

    // Ensure the template can be parsed by the default convenience constructor.
    let _doc = BrowserDocument::from_html(&html, RenderOptions::default())
      .expect("BrowserDocument::from_html should parse context menu HTML");

    // Parse again with a deterministic renderer configuration so this test does not depend on
    // host-installed system fonts.
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let doc =
      BrowserDocument::new(renderer, &html, RenderOptions::default()).expect("parse context menu");

    assert_eq!(
      count_nodes_with_class(doc.dom(), "chrome-context-menu"),
      1,
      "expected exactly one .chrome-context-menu root"
    );
  }

  #[test]
  fn context_menu_escapes_labels() {
    let label = r#"<Copy & "Paste">"#;
    let entries = vec![Entry::Item(Item {
      label: label.to_string(),
      icon_url: None,
      enabled: true,
      href: "chrome-action:test".to_string(),
    })];
    let html = context_menu_html(&entries);

    assert!(
      html.contains("&lt;Copy &amp; &quot;Paste&quot;&gt;"),
      "expected label to be HTML-escaped, got: {html}"
    );
    assert!(
      !html.contains(label),
      "expected raw label not to appear unescaped in HTML, got: {html}"
    );
  }

  #[test]
  fn context_menu_disabled_entries_render_disabled_class_and_aria() {
    let entries = vec![Entry::Item(Item {
      label: "Disabled".to_string(),
      icon_url: None,
      enabled: false,
      href: "chrome-action:noop".to_string(),
    })];
    let html = context_menu_html(&entries);

    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let doc = BrowserDocument::new(renderer, &html, RenderOptions::default())
      .expect("parse context menu");

    let mut disabled_count = 0usize;
    doc.dom().walk_tree(&mut |node| {
      if !node.is_element() {
        return;
      }
      if !node.has_class("chrome-context-menu-item") {
        return;
      }
      if node.get_attribute_ref("aria-disabled") == Some("true") {
        disabled_count += 1;
        assert!(
          node.has_class("disabled"),
          "disabled items should include the .disabled class"
        );
      }
    });

    assert_eq!(disabled_count, 1, "expected exactly one disabled menu item");
  }
}

