use crate::ui::browser_app::BrowserAppState;
use crate::ui::chrome_dynamic_asset_fetcher::ChromeDynamicAssetFetcher;
use crate::ui::html_escape::escape_html;
use crate::ui::{
  ChromeActionUrl, OmniboxAction, OmniboxSearchSource, OmniboxSuggestion, OmniboxSuggestionSource,
  OmniboxUrlSource,
};
use std::fmt::Write;

use super::ids::{
  CHROME_ADDRESS_BAR_ID, CHROME_ADDRESS_FORM_ID, CHROME_CONTENT_FRAME_ID, CHROME_NEW_TAB_ID,
  CHROME_OMNIBOX_POPUP_ID, CHROME_TAB_STRIP_ID, CHROME_TOOLBAR_ID,
};
use super::theme::chrome_theme_css;

fn omnibox_suggestion_type_class(suggestion: &OmniboxSuggestion) -> &'static str {
  match &suggestion.action {
    OmniboxAction::NavigateToUrl => "omnibox-type-url",
    OmniboxAction::Search(_) => "omnibox-type-search",
    OmniboxAction::ActivateTab(_) => "omnibox-type-tab",
  }
}

fn omnibox_suggestion_source_class(suggestion: &OmniboxSuggestion) -> &'static str {
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
    OmniboxAction::NavigateToUrl => suggestion
      .url
      .as_ref()
      .map(|url| ChromeActionUrl::Navigate { url: url.clone() }.to_url_string()),
    // `chrome-action:navigate` is handled like a typed navigation; passing the raw query preserves
    // the same behaviour as pressing Enter in the address bar (search-vs-url resolution happens in
    // the action handler).
    OmniboxAction::Search(query) => {
      Some(ChromeActionUrl::Navigate { url: query.clone() }.to_url_string())
    }
    OmniboxAction::ActivateTab(tab_id) => {
      Some(ChromeActionUrl::ActivateTab { tab_id: *tab_id }.to_url_string())
    }
  }
}

fn push_omnibox_popup_html(out: &mut String, app: &BrowserAppState) {
  let omnibox = &app.chrome.omnibox;
  let show = omnibox.open && !omnibox.suggestions.is_empty();
  if !show {
    out.push_str("      <div id=\"");
    out.push_str(CHROME_OMNIBOX_POPUP_ID);
    out.push_str("\" class=\"omnibox-popup\" role=\"listbox\" aria-label=\"Suggestions\" hidden></div>\n");
    return;
  }

  let selected_idx = omnibox
    .selected
    .filter(|idx| *idx < omnibox.suggestions.len());

  out.push_str("      <div id=\"");
  out.push_str(CHROME_OMNIBOX_POPUP_ID);
  out.push_str("\" class=\"omnibox-popup\" role=\"listbox\" aria-label=\"Suggestions\">\n");

  let setsize = omnibox.suggestions.len();
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
        OmniboxAction::NavigateToUrl => suggestion.url.as_deref(),
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

    let posinset = idx + 1;
    write!(
      out,
      "        <a id=\"{row_id}\" class=\"{classes}\" role=\"option\" aria-selected=\"{aria_selected}\" aria-posinset=\"{posinset}\" aria-setsize=\"{setsize}\" href=\"{safe_href}\"><span class=\"omnibox-icon\" aria-hidden=\"true\"></span><span class=\"omnibox-text\"><span id=\"omnibox-title-{idx}\" class=\"omnibox-title\">{safe_title}</span>"
    )
    .expect("write omnibox suggestion row"); // fastrender-allow-unwrap

    out.push_str("<span id=\"omnibox-url-");
    let _ = write!(out, "{idx}");
    out.push_str("\" class=\"omnibox-url\"");
    if secondary.is_none() {
      out.push_str(" hidden");
    }
    out.push('>');
    if let Some(url) = secondary {
      out.push_str(&escape_html(url));
    }
    out.push_str("</span>");

    out.push_str("</span></a>\n");
  }

  out.push_str("      </div>\n");
}

/// Generate a deterministic chrome-frame HTML document from the browser UI state.
///
/// This is intentionally JS-free: user interactions are encoded as navigations to `chrome-action:`
/// URLs (links + form submissions), allowing the embedding to intercept and dispatch them.
pub fn chrome_frame_html_from_state(app: &BrowserAppState) -> String {
  let active_tab_id = app.active_tab_id();
  let active_tab = app.active_tab();

  let (can_go_back, can_go_forward, loading) = match active_tab {
    Some(tab) => (tab.can_go_back, tab.can_go_forward, tab.loading),
    None => (false, false, false),
  };

  let omnibox_open = app.chrome.omnibox.open && !app.chrome.omnibox.suggestions.is_empty();
  let omnibox_selected_idx = app
    .chrome
    .omnibox
    .selected
    .filter(|idx| *idx < app.chrome.omnibox.suggestions.len());

  let mut out = String::new();
  out.push_str("<!DOCTYPE html>\n");
  out.push_str("<html class=\"chrome-frame\">\n");
  out.push_str("<head>\n");
  out.push_str("  <meta charset=\"utf-8\">\n");
  out.push_str("  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
  out.push_str("  <style id=\"chrome-theme\">");
  out.push_str(&chrome_theme_css(&app.appearance));
  out.push_str("</style>\n");
  out.push_str("  <link rel=\"stylesheet\" href=\"chrome://styles/chrome.css\">\n");
  out.push_str("</head>\n");
  out.push_str("<body>\n");

  // Tab strip.
  out.push_str("  <div class=\"tab-strip\" id=\"");
  out.push_str(CHROME_TAB_STRIP_ID);
  out.push_str("\">\n");
  out.push_str("    <div class=\"tab-strip-tabs\" role=\"tablist\" aria-label=\"Tabs\">\n");
  let tab_setsize = app.tabs.len();
  for (tab_idx, tab) in app.tabs.iter().enumerate() {
    let tab_id = escape_html(&tab.id.0.to_string());
    let title = escape_html(&tab.display_title());
    let favicon_url = ChromeDynamicAssetFetcher::favicon_url(tab.id);
    let is_active = active_tab_id == Some(tab.id);
    let aria_selected = if is_active { "true" } else { "false" };
    let aria_posinset = tab_idx + 1;
    let activate_href =
      escape_html(&ChromeActionUrl::ActivateTab { tab_id: tab.id }.to_url_string());
    let close_href = escape_html(&ChromeActionUrl::CloseTab { tab_id: tab.id }.to_url_string());
    out.push_str("      <div class=\"tab");
    if is_active {
      out.push_str(" active");
    }
    out.push_str("\" id=\"tab-");
    out.push_str(&tab_id);
    out.push_str("\" data-tab-id=\"");
    out.push_str(&tab_id);
    out.push_str("\">\n");

    // "Activate tab" link.
    out.push_str("        <a id=\"tab-activate-");
    out.push_str(&tab_id);
    out.push_str("\" class=\"tab-activate\" role=\"tab\" aria-selected=\"");
    out.push_str(aria_selected);
    let _ = write!(
      out,
      "\" aria-posinset=\"{aria_posinset}\" aria-setsize=\"{tab_setsize}\" href=\"{activate_href}",
    );
    out.push_str("\">");
    out.push_str("<img class=\"tab-favicon\" src=\"");
    out.push_str(&favicon_url);
    out.push_str("\" alt=\"\" />");
    out.push_str("<span id=\"tab-title-");
    out.push_str(&tab_id);
    out.push_str("\" class=\"tab-title\">");
    out.push_str(&title);
    out.push_str("</span>");
    out.push_str("</a>\n");

    // Close button.
    out.push_str("        <a id=\"tab-close-");
    out.push_str(&tab_id);
    out.push_str("\" class=\"tab-close\" role=\"button\" aria-label=\"Close tab: ");
    out.push_str(&title);
    out.push_str("\" href=\"");
    out.push_str(&close_href);
    out.push_str("\">×</a>\n");

    out.push_str("      </div>\n");
  }
  out.push_str("    </div>\n");
  out.push_str(
    "    <a id=\"",
  );
  out.push_str(CHROME_NEW_TAB_ID);
  out.push_str(
    "\" class=\"tab tab-new\" role=\"button\" aria-label=\"New tab\" href=\"chrome-action:new-tab\">+</a>\n",
  );
  out.push_str("  </div>\n");

  // Toolbar.
  out.push_str("  <div class=\"toolbar\" id=\"");
  out.push_str(CHROME_TOOLBAR_ID);
  out.push_str("\" role=\"toolbar\" aria-label=\"Browser controls\">\n");

  // Helper for toolbar button links.
  fn push_toolbar_button(
    out: &mut String,
    id: &str,
    class: &str,
    label: &str,
    aria_label: &str,
    href: &str,
    enabled: bool,
  ) {
    out.push_str("    <a id=\"");
    out.push_str(id);
    out.push_str("\" class=\"toolbar-button ");
    out.push_str(class);
    if !enabled {
      out.push_str(" disabled");
      out.push_str("\" role=\"button\" aria-label=\"");
      out.push_str(aria_label);
      out.push_str("\" aria-disabled=\"true\">");
      out.push_str(label);
      out.push_str("</a>\n");
      return;
    }
    out.push_str("\" role=\"button\" aria-label=\"");
    out.push_str(aria_label);
    out.push_str("\" href=\"");
    out.push_str(href);
    out.push_str("\">");
    out.push_str(label);
    out.push_str("</a>\n");
  }

  let back_href = escape_html(&ChromeActionUrl::Back.to_url_string());
  let forward_href = escape_html(&ChromeActionUrl::Forward.to_url_string());
  let reload_href = escape_html(&ChromeActionUrl::Reload.to_url_string());
  let stop_loading_href = escape_html(&ChromeActionUrl::StopLoading.to_url_string());
  let home_href = escape_html(&ChromeActionUrl::Home.to_url_string());

  push_toolbar_button(
    &mut out,
    "toolbar-back",
    "back",
    "←",
    "Back",
    &back_href,
    can_go_back,
  );
  push_toolbar_button(
    &mut out,
    "toolbar-forward",
    "forward",
    "→",
    "Forward",
    &forward_href,
    can_go_forward,
  );
  push_toolbar_button(
    &mut out,
    "toolbar-reload",
    "reload",
    "↻",
    "Reload",
    &reload_href,
    !loading,
  );
  push_toolbar_button(
    &mut out,
    "toolbar-stop",
    "stop",
    "✕",
    "Stop loading",
    &stop_loading_href,
    loading,
  );
  push_toolbar_button(
    &mut out,
    "toolbar-home",
    "home",
    "⌂",
    "Home",
    &home_href,
    true,
  );

  // Address bar.
  out.push_str("    <div id=\"address-bar-wrap\" class=\"address-bar-wrap\">\n");
  out.push_str("      <form id=\"");
  out.push_str(CHROME_ADDRESS_FORM_ID);
  out.push_str(
    "\" class=\"address-bar-form\" action=\"chrome-action:navigate\" method=\"get\" autocomplete=\"off\">\n",
  );
  out.push_str("        <input id=\"");
  out.push_str(CHROME_ADDRESS_BAR_ID);
  out.push_str("\" class=\"address-input\" name=\"url\" type=\"text\" value=\"");
  out.push_str(&escape_html(&app.chrome.address_bar_text));
  out.push_str("\" aria-label=\"Address bar\" role=\"combobox\" aria-autocomplete=\"list\" aria-haspopup=\"listbox\" aria-controls=\"");
  out.push_str(CHROME_OMNIBOX_POPUP_ID);
  out.push_str("\" aria-expanded=\"");
  out.push_str(if omnibox_open { "true" } else { "false" });
  out.push('"');
  if omnibox_open {
    if let Some(selected_idx) = omnibox_selected_idx {
      let _ = write!(
        out,
        " aria-activedescendant=\"omnibox-suggestion-{selected_idx}\""
      );
    }
  }
  out.push_str(">\n");
  // A submit button is useful for keyboard-less environments; CSS can hide it if desired.
  out.push_str("        <button class=\"address-bar-submit\" type=\"submit\">Go</button>\n");
  out.push_str("      </form>\n");

  push_omnibox_popup_html(&mut out, app);

  out.push_str("    </div>\n");

  out.push_str("  </div>\n");

  // Content frame placeholder.
  //
  // Mark this as the main landmark so assistive tech can jump directly to the page region (the
  // actual page content is composited by the host, but this establishes the chrome/content split in
  // the renderer's accessibility tree).
  out.push_str("  <div id=\"");
  out.push_str(CHROME_CONTENT_FRAME_ID);
  out.push_str("\" class=\"content-frame\" role=\"main\" aria-label=\"Page\"></div>\n");

  out.push_str("</body>\n");
  out.push_str("</html>\n");
  out
}

#[cfg(test)]
mod tests {
  use super::chrome_frame_html_from_state;
  use crate::ui::browser_app::{BrowserAppState, BrowserTabState};
  use crate::ui::messages::TabId;
  use crate::ui::theme_parsing::BrowserTheme;
  use crate::ui::{OmniboxAction, OmniboxSuggestion, OmniboxSuggestionSource, OmniboxUrlSource};
  use crate::{BrowserDocument, FastRender, FontConfig, RenderOptions};

  #[test]
  fn chrome_frame_html_contains_tabs_actions_and_escapes_titles() {
    let mut app = BrowserAppState::new();

    let mut tab1 = BrowserTabState::new(TabId(1), "https://example.com/".to_string());
    tab1.title = Some("Rust & <Friends>".to_string());

    let mut tab2 = BrowserTabState::new(TabId(2), "https://example.net/".to_string());
    tab2.title = Some("Tab 2".to_string());
    tab2.can_go_back = true;
    tab2.can_go_forward = false;
    tab2.loading = true;

    let tab3 = BrowserTabState::new(TabId(3), "about:newtab".to_string());

    app.push_tab(tab1, false);
    app.push_tab(tab2, true);
    app.push_tab(tab3, false);

    let html = chrome_frame_html_from_state(&app);

    // One `.tab` element per tab.
    let tab_count = html.matches("<div class=\"tab\"").count()
      + html.matches("<div class=\"tab active\"").count();
    assert_eq!(tab_count, 3);

    // Active tab marker is present.
    assert!(html.contains("class=\"tab active\" id=\"tab-2\" data-tab-id=\"2\""));

    // Stable element ids exist for hit-testing/geometry queries.
    assert!(html.contains("id=\"tab-1\" data-tab-id=\"1\""));
    assert!(html.contains("id=\"tab-2\" data-tab-id=\"2\""));
    assert!(html.contains("id=\"tab-3\" data-tab-id=\"3\""));

    // Action URLs contain expected tab ids.
    assert!(html.contains("chrome-action:activate-tab?tab=1"));
    assert!(html.contains("chrome-action:activate-tab?tab=2"));
    assert!(html.contains("chrome-action:activate-tab?tab=3"));
    assert!(html.contains("chrome-action:close-tab?tab=1"));
    assert!(html.contains("chrome-action:close-tab?tab=2"));
    assert!(html.contains("chrome-action:close-tab?tab=3"));

    // Favicons use stable chrome://favicon/<tab_id> URLs (no embedded data URLs).
    assert!(html.contains("chrome://favicon/1"));
    assert!(html.contains("chrome://favicon/2"));
    assert!(html.contains("chrome://favicon/3"));

    // Titles are escaped.
    assert!(html.contains("Rust &amp; &lt;Friends&gt;"));
    assert!(!html.contains("Rust & <Friends>"));

    // Stop-loading uses the canonical chrome-action name (not `stop`).
    assert!(html.contains("chrome-action:stop-loading"));

    // Tab strip uses ARIA tab roles so the FastRender accessibility tree can expose tab semantics
    // (useful when chrome is rendered by FastRender itself).
    assert!(html.contains("id=\"tab-strip\""));
    assert!(html.contains("class=\"tab-strip-tabs\" role=\"tablist\" aria-label=\"Tabs\""));
    assert_eq!(html.matches("role=\"tab\"").count(), 3);
    assert_eq!(html.matches("role=\"tab\" aria-selected=\"true\"").count(), 1);
    assert!(
      html.contains(
        r#"aria-posinset="2" aria-setsize="3" href="chrome-action:activate-tab?tab=2""#
      ),
      "expected tab posinset/setsize attributes"
    );

    // Close buttons should include the tab title so the label is unique and meaningful.
    assert!(html.contains("aria-label=\"Close tab: Rust &amp; &lt;Friends&gt;\""));
    assert!(html.contains("role=\"button\" aria-label=\"Close tab: Rust &amp; &lt;Friends&gt;\""));

    // New-tab button should be present and wired to chrome-action:new-tab.
    assert!(html.contains("id=\"new-tab\""));
    assert!(html.contains("aria-label=\"New tab\""));
    assert!(html.contains("href=\"chrome-action:new-tab\""));

    // Toolbar buttons should have meaningful accessible labels; the visual glyphs are not good
    // spoken names ("←" etc).
    assert!(html.contains("id=\"toolbar\""));
    assert!(html.contains("role=\"toolbar\""));
    assert!(html.contains("aria-label=\"Browser controls\""));
    assert!(html.contains("id=\"toolbar-back\""));
    assert!(html.contains("aria-label=\"Back\""));
    assert!(html.contains("id=\"toolbar-forward\""));
    assert!(html.contains("aria-label=\"Forward\""));
    assert!(html.contains("id=\"toolbar-reload\""));
    assert!(html.contains("aria-label=\"Reload\""));
    assert!(html.contains("id=\"toolbar-stop\""));
    assert!(html.contains("aria-label=\"Stop loading\""));
    assert!(html.contains("id=\"toolbar-home\""));
    assert!(html.contains("aria-label=\"Home\""));

    // The chrome frame should include the content-frame placeholder exactly once so the host can
    // reliably target it for compositing/sync.
    assert_eq!(
      html.matches(r#"id="content-frame""#).count(),
      1,
      "expected exactly one #content-frame placeholder"
    );
    assert!(
      html.contains(r#"id="content-frame" class="content-frame" role="main""#),
      "expected #content-frame to be marked as the main landmark"
    );

    // Address bar uses combobox semantics (collapsed when no omnibox popup is open).
    assert!(html.contains("id=\"address-bar\""));
    assert!(html.contains("role=\"combobox\""));
    assert!(html.contains("aria-expanded=\"false\""));
    assert!(html.contains("aria-controls=\"omnibox-popup\""));
    assert!(!html.contains("aria-activedescendant=\"omnibox-suggestion-"));
  }

  #[test]
  fn chrome_frame_html_renders_omnibox_popup_with_clickable_suggestions() {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.invalid/".to_string());
    app.chrome.omnibox.open = true;
    app.chrome.omnibox.selected = Some(1);
    app.chrome.omnibox.suggestions = vec![
      OmniboxSuggestion {
        action: OmniboxAction::NavigateToUrl,
        title: Some("Example <Title>".to_string()),
        url: Some("https://example.com/".to_string()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
      },
      OmniboxSuggestion {
        action: OmniboxAction::ActivateTab(TabId(42)),
        title: Some("Tab".to_string()),
        url: Some("https://tab.example/".to_string()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
      },
      OmniboxSuggestion {
        action: OmniboxAction::ActivateTab(TabId(42)),
        title: Some("Switch to tab".to_string()),
        url: Some("https://tab.example/".to_string()),
        source: OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
      },
      OmniboxSuggestion {
        action: OmniboxAction::Search("cats & dogs".to_string()),
        title: Some("cats & dogs".to_string()),
        url: None,
        source: OmniboxSuggestionSource::Search(crate::ui::OmniboxSearchSource::RemoteSuggest),
      },
    ];

    let html = chrome_frame_html_from_state(&app);
    assert!(
      html.contains(r#"id="omnibox-popup""#) && html.contains(r#"role="listbox""#),
      "expected omnibox popup element in HTML"
    );
    assert!(html.contains(r#"aria-label="Suggestions""#));
    assert!(html.contains(r#"aria-controls="omnibox-popup""#));
    assert!(html.contains(r#"aria-activedescendant="omnibox-suggestion-1""#));

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
      crate::ui::ChromeActionUrl::ActivateTab { tab_id: TabId(42) }.to_url_string()
    );
    assert!(
      html.contains(&expected_href_1),
      "expected activate-tab suggestion href, got html: {html}"
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

    // Selected row uses the `.selected` class and aria-selected.
    assert!(
      html.contains(r#"id="omnibox-suggestion-1""#)
        && html.contains(r#"aria-selected="true""#)
        && html.contains(r#"aria-posinset="2""#)
        && html.contains(r#"aria-setsize="4""#)
        && html.contains(
          r#"class="omnibox-suggestion omnibox-type-tab omnibox-source-open-tab selected""#
        ),
      "expected selected row markup, got html: {html}"
    );

    // Titles are escaped.
    assert!(html.contains("Example &lt;Title&gt;"));
    assert!(!html.contains("Example <Title>"));

    // Ensure the HTML remains parseable by BrowserDocument.
    let renderer = FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("build deterministic renderer");
    let _doc = BrowserDocument::new(renderer, &html, RenderOptions::default())
      .expect("parse chrome frame HTML with omnibox popup");
  }

  #[test]
  fn chrome_frame_html_emits_theme_css_variables_from_accent() {
    let mut app = BrowserAppState::new_with_initial_tab("https://example.invalid/".to_string());
    app.appearance.theme = BrowserTheme::Light;
    app.appearance.accent_color = Some("#ff00ff".to_string());

    let html = chrome_frame_html_from_state(&app);
    for needle in [
      "--chrome-accent: rgb(255, 0, 255);",
      "--chrome-accent-bg: rgba(255, 0, 255, 0.18);",
      "--chrome-accent-border: rgba(255, 0, 255, 0.55);",
      "--chrome-focus-ring: rgba(255, 0, 255, 0.65);",
    ] {
      assert!(
        html.contains(needle),
        "expected chrome frame HTML to include themed CSS variable {needle:?}, got: {html}"
      );
    }
  }
}
