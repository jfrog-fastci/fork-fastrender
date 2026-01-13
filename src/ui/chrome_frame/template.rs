//! HTML templates for renderer-driven browser chrome (dogfooding FastRender).
//!
//! The chrome frame is intended to be rendered by FastRender (trusted browser process) and driven
//! without JS for P0/P1 by using `chrome-action:` URLs.

use crate::ui::html_escape::escape_html;
use crate::ui::{BrowserAppState, ChromeActionUrl, OmniboxAction, OmniboxSearchSource, OmniboxSuggestion};
use std::fmt::Write;

fn omnibox_suggestion_type_class(suggestion: &OmniboxSuggestion) -> &'static str {
  match suggestion.action {
    OmniboxAction::NavigateToUrl(_) => "omnibox-type-url",
    OmniboxAction::Search(_) => "omnibox-type-search",
    OmniboxAction::ActivateTab(_) => "omnibox-type-tab",
  }
}

fn omnibox_suggestion_source_class(suggestion: &OmniboxSuggestion) -> &'static str {
  use crate::ui::OmniboxSuggestionSource;
  use crate::ui::OmniboxUrlSource;

  match suggestion.source {
    OmniboxSuggestionSource::Primary => "omnibox-source-primary",
    OmniboxSuggestionSource::Search(OmniboxSearchSource::RemoteSuggest) => {
      "omnibox-source-remote-suggest"
    }
    OmniboxSuggestionSource::Url(OmniboxUrlSource::About) => "omnibox-source-about",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Bookmark) => "omnibox-source-bookmark",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::ClosedTab) => "omnibox-source-closed-tab",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab) => "omnibox-source-open-tab",
    OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited) => "omnibox-source-visited",
  }
}

fn omnibox_suggestion_href(suggestion: &OmniboxSuggestion) -> Option<String> {
  match &suggestion.action {
    OmniboxAction::NavigateToUrl(url) => Some(
      ChromeActionUrl::Navigate {
        url: url.clone(),
      }
      .to_url_string(),
    ),
    // `chrome-action:navigate` is treated as a typed navigation; passing the raw query preserves
    // the same behaviour as pressing Enter in the address bar.
    OmniboxAction::Search(query) => Some(
      ChromeActionUrl::Navigate {
        url: query.clone(),
      }
      .to_url_string(),
    ),
    OmniboxAction::ActivateTab(tab_id) => Some(
      ChromeActionUrl::ActivateTab { tab_id: *tab_id }.to_url_string(),
    ),
  }
}

fn omnibox_popup_html(app: &BrowserAppState) -> String {
  let omnibox = &app.chrome.omnibox;
  if !omnibox.open || omnibox.suggestions.is_empty() {
    return String::new();
  }

  let selected_idx = omnibox
    .selected
    .filter(|idx| *idx < omnibox.suggestions.len());

  let mut out = String::new();
  out.push_str(r#"<div id="omnibox-popup" class="omnibox-popup" role="listbox">"#);

  for (idx, suggestion) in omnibox.suggestions.iter().enumerate() {
    let href = omnibox_suggestion_href(suggestion).unwrap_or_default();

    let mut classes = String::new();
    classes.push_str("omnibox-suggestion");
    classes.push(' ');
    classes.push_str(omnibox_suggestion_type_class(suggestion));
    classes.push(' ');
    classes.push_str(omnibox_suggestion_source_class(suggestion));
    if selected_idx == Some(idx) {
      classes.push_str(" selected");
    }

    let aria_selected = if selected_idx == Some(idx) {
      "true"
    } else {
      "false"
    };

    let title = suggestion
      .title
      .as_deref()
      .map(str::trim)
      .filter(|t| !t.is_empty())
      .or_else(|| match &suggestion.action {
        OmniboxAction::NavigateToUrl(url) => Some(url.as_str()),
        OmniboxAction::Search(query) => Some(query.as_str()),
        OmniboxAction::ActivateTab(_) => suggestion.url.as_deref(),
      })
      .unwrap_or_default();

    let mut secondary = suggestion
      .url
      .as_deref()
      .map(str::trim)
      .filter(|u| !u.is_empty());
    if secondary.is_some_and(|u| u == title) {
      secondary = None;
    }

    let safe_href = escape_html(&href);
    let safe_title = escape_html(title);

    let row_id = format!("omnibox-suggestion-{idx}");

    write!(
      &mut out,
      r#"<a id="{row_id}" class="{classes}" role="option" aria-selected="{aria_selected}" href="{safe_href}"><span class="omnibox-icon" aria-hidden="true"></span><span class="omnibox-text"><span class="omnibox-title">{safe_title}</span>"#
    )
    .expect("write omnibox suggestion html");

    if let Some(url) = secondary {
      let safe_url = escape_html(url);
      write!(
        &mut out,
        r#"<span class="omnibox-url">{safe_url}</span>"#
      )
      .expect("write omnibox suggestion url");
    }

    out.push_str("</span></a>");
  }

  out.push_str("</div>");
  out
}

/// Build the chrome frame HTML document.
///
/// The document is expected to be served at a `chrome://` URL (or similar trusted internal origin)
/// so linked resources like `chrome://styles/chrome.css` can be resolved by the embedding browser.
pub fn chrome_frame_html(app: &BrowserAppState) -> String {
  let current_url = app.chrome.address_bar_text.as_str();
  let safe_current_url = escape_html(current_url);
  let omnibox_popup = omnibox_popup_html(app);

  // Keep this template intentionally minimal. It should remain JS-free so the chrome can be driven
  // via simple `chrome-action:` navigations while JS support is still being brought up.
  format!(
    r#"<!doctype html>
<html class="chrome-frame">
  <head>
    <meta charset="utf-8">
    <link rel="stylesheet" href="chrome://styles/chrome.css">
    <title>FastRender</title>
  </head>
  <body>
    <div id="top-toolbar" class="toolbar" role="toolbar" aria-label="Browser toolbar">
      <div class="toolbar-buttons">
        <a class="toolbar-button" href="chrome-action:back" aria-label="Back">Back</a>
        <a class="toolbar-button" href="chrome-action:forward" aria-label="Forward">Forward</a>
        <a class="toolbar-button" href="chrome-action:reload" aria-label="Reload">Reload</a>
      </div>
      <div class="address-bar-wrap">
        <form class="address-bar" action="chrome-action:navigate" method="get" autocomplete="off">
          <input
            class="address-input"
            name="url"
            type="text"
            value="{safe_current_url}"
            placeholder="Enter URL"
            aria-label="Address bar"
          >
        </form>
        {omnibox_popup}
      </div>
    </div>

    <div id="content-frame" class="content-frame"></div>
  </body>
</html>"#
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::{OmniboxAction, OmniboxSuggestion, OmniboxSuggestionSource, OmniboxUrlSource};
  use crate::{BrowserDocument, FastRender, FontConfig, RenderOptions};

  fn count_nodes_with_id(root: &crate::dom::DomNode, id: &str) -> usize {
    let mut count = 0usize;
    root.walk_tree(&mut |node| {
      if node
        .is_element()
        .then(|| node.get_attribute_ref("id"))
        .flatten()
        .is_some_and(|value| value == id)
      {
        count += 1;
      }
    });
    count
  }

  #[test]
  fn chrome_frame_template_is_complete_and_parseable() {
    let app = BrowserAppState::new_with_initial_tab("https://example.invalid/".to_string());
    let html = chrome_frame_html(&app);

    // Static string invariants.
    assert!(
      html.contains(r#"<link rel="stylesheet" href="chrome://styles/chrome.css">"#),
      "chrome frame should link chrome://styles/chrome.css"
    );
    assert!(
      html.contains(r#"href="chrome-action:back""#)
        && html.contains(r#"href="chrome-action:forward""#)
        && html.contains(r#"href="chrome-action:reload""#),
      "chrome frame should include Back/Forward/Reload chrome-action links"
    );
    assert!(
      html.contains(r#"<form class="address-bar" action="chrome-action:navigate" method="get""#),
      "chrome frame should include address bar form using chrome-action:navigate"
    );
    assert!(
      html.contains(r#"name="url""#),
      "chrome frame address bar should include an <input name=\"url\">"
    );
    assert!(
      html.contains(r#"id="content-frame""#),
      "chrome frame should include #content-frame placeholder"
    );

    // Ensure the template can be parsed by the default convenience constructor.
    let _doc = BrowserDocument::from_html(&html, RenderOptions::default())
      .expect("BrowserDocument::from_html should parse chrome frame HTML");

    // Parse again with a deterministic renderer configuration so this test does not depend on
    // host-installed system fonts.
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let doc =
      BrowserDocument::new(renderer, &html, RenderOptions::default()).expect("parse chrome frame");

    assert_eq!(
      count_nodes_with_id(doc.dom(), "content-frame"),
      1,
      "#content-frame should exist exactly once"
    );
  }

  #[test]
  fn chrome_frame_template_renders_omnibox_suggestions_dropdown() {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.invalid/".to_string());
    app.chrome.omnibox.open = true;
    app.chrome.omnibox.selected = Some(1);
    app.chrome.omnibox.suggestions = vec![
      OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl("https://example.com/".to_string()),
        title: Some("Example".to_string()),
        url: Some("https://example.com/".to_string()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
      },
      OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl("https://rust-lang.org/?a=1&b=2".to_string()),
        title: Some("Rust".to_string()),
        url: Some("https://rust-lang.org/?a=1&b=2".to_string()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
      },
      OmniboxSuggestion {
        action: OmniboxAction::ActivateTab(crate::ui::TabId(77)),
        title: Some("Open tab".to_string()),
        url: Some("https://open-tab.example/".to_string()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
      },
      OmniboxSuggestion {
        action: OmniboxAction::Search("cats & dogs".to_string()),
        title: Some("cats & dogs".to_string()),
        url: None,
        source: OmniboxSuggestionSource::Search(crate::ui::OmniboxSearchSource::RemoteSuggest),
      },
    ];

    let html = chrome_frame_html(&app);
    assert!(
      html.contains(r#"id="omnibox-popup""#) && html.contains(r#"role="listbox""#),
      "expected omnibox dropdown element in chrome frame HTML"
    );

    let expected_href_0 = format!(
      r#"href="{}""#,
      crate::ui::ChromeActionUrl::Navigate {
        url: "https://example.com/".to_string()
      }
      .to_url_string()
    );
    assert!(
      html.contains(&expected_href_0),
      "expected first suggestion href, got html: {html}"
    );

    let expected_href_1 = format!(
      r#"href="{}""#,
      crate::ui::ChromeActionUrl::Navigate {
        url: "https://rust-lang.org/?a=1&b=2".to_string()
      }
      .to_url_string()
    );
    assert!(
      html.contains(&expected_href_1),
      "expected second suggestion href, got html: {html}"
    );

    assert!(
      html.contains(r#"href="chrome-action:activate-tab?tab=77""#),
      "expected activate-tab href for open-tab suggestion, got html: {html}"
    );

    let expected_search_href = format!(
      r#"href="{}""#,
      crate::ui::ChromeActionUrl::Navigate {
        url: "cats & dogs".to_string()
      }
      .to_url_string()
    );
    assert!(
      html.contains(&expected_search_href),
      "expected search suggestion href, got html: {html}"
    );

    assert!(
      html.contains(r#"id="omnibox-suggestion-0""#) && html.contains(r#"id="omnibox-suggestion-1""#),
      "expected stable ids for suggestion rows"
    );

    assert!(
      html.contains(r#"id="omnibox-suggestion-1" class="omnibox-suggestion"#)
        && html.contains(r#"selected" role="option" aria-selected="true""#),
      "expected selected suggestion row to be marked selected"
    );

    // Ensure the template remains parseable by BrowserDocument.
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let _doc = BrowserDocument::new(renderer, &html, RenderOptions::default())
      .expect("parse chrome frame with omnibox dropdown");
  }
}
