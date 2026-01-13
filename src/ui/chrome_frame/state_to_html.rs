use crate::ui::browser_app::BrowserAppState;
use crate::ui::chrome_dynamic_asset_fetcher::ChromeDynamicAssetFetcher;
use crate::ui::html_escape::escape_html;

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

  let mut out = String::new();
  out.push_str("<!DOCTYPE html>\n");
  out.push_str("<html class=\"chrome-frame\">\n");
  out.push_str("<head>\n");
  out.push_str("  <meta charset=\"utf-8\">\n");
  out.push_str("  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
  out.push_str("  <link rel=\"stylesheet\" href=\"chrome://styles/chrome.css\">\n");
  out.push_str("</head>\n");
  out.push_str("<body>\n");

  // Tab strip.
  out.push_str("  <div class=\"tab-strip\" id=\"tab-strip\">\n");
  for tab in &app.tabs {
    let tab_id = escape_html(&tab.id.0.to_string());
    let title = escape_html(&tab.display_title());
    let favicon_url = ChromeDynamicAssetFetcher::favicon_url(tab.id);
    let is_active = active_tab_id == Some(tab.id);
    out.push_str("    <div class=\"tab");
    if is_active {
      out.push_str(" active");
    }
    out.push_str("\" data-tab-id=\"");
    out.push_str(&tab_id);
    out.push_str("\">\n");

    // "Activate tab" link.
    out.push_str("      <a class=\"tab-activate\" href=\"chrome-action:activate-tab?tab_id=");
    out.push_str(&tab_id);
    out.push_str("\">");
    out.push_str("<img class=\"tab-favicon\" src=\"");
    out.push_str(&favicon_url);
    out.push_str("\" alt=\"\" />");
    out.push_str("<span class=\"tab-title\">");
    out.push_str(&title);
    out.push_str("</span>");
    out.push_str("</a>\n");

    // Close button.
    out.push_str(
      "      <a class=\"tab-close\" aria-label=\"Close tab\" href=\"chrome-action:close-tab?tab_id=",
    );
    out.push_str(&tab_id);
    out.push_str("\">×</a>\n");

    out.push_str("    </div>\n");
  }
  out.push_str("  </div>\n");

  // Toolbar.
  out.push_str("  <div class=\"toolbar\" id=\"toolbar\">\n");

  // Helper for toolbar button links.
  fn push_toolbar_button(out: &mut String, class: &str, label: &str, href: &str, enabled: bool) {
    if enabled {
      out.push_str("    <a class=\"toolbar-button ");
      out.push_str(class);
      out.push_str("\" role=\"button\" href=\"");
      out.push_str(href);
      out.push_str("\">");
      out.push_str(label);
      out.push_str("</a>\n");
    } else {
      out.push_str("    <span class=\"toolbar-button ");
      out.push_str(class);
      out.push_str(" disabled\" role=\"button\" aria-disabled=\"true\">");
      out.push_str(label);
      out.push_str("</span>\n");
    }
  }

  push_toolbar_button(&mut out, "back", "←", "chrome-action:back", can_go_back);
  push_toolbar_button(
    &mut out,
    "forward",
    "→",
    "chrome-action:forward",
    can_go_forward,
  );
  push_toolbar_button(&mut out, "reload", "↻", "chrome-action:reload", !loading);
  push_toolbar_button(&mut out, "stop", "✕", "chrome-action:stop", loading);
  push_toolbar_button(&mut out, "home", "⌂", "chrome-action:home", true);

  // Address bar.
  out.push_str(
    "    <form class=\"address-bar-form\" action=\"chrome-action:navigate\" method=\"get\">\n",
  );
  out.push_str("      <input class=\"address-bar\" name=\"url\" type=\"text\" value=\"");
  out.push_str(&escape_html(&app.chrome.address_bar_text));
  out.push_str("\">\n");
  // A submit button is useful for keyboard-less environments; CSS can hide it if desired.
  out.push_str("      <button class=\"address-bar-submit\" type=\"submit\">Go</button>\n");
  out.push_str("    </form>\n");

  out.push_str("  </div>\n");

  // Content frame placeholder.
  out.push_str("  <div id=\"content-frame\"></div>\n");

  out.push_str("</body>\n");
  out.push_str("</html>\n");
  out
}

#[cfg(test)]
mod tests {
  use super::chrome_frame_html_from_state;
  use crate::ui::browser_app::{BrowserAppState, BrowserTabState};
  use crate::ui::messages::TabId;

  #[test]
  fn chrome_frame_html_contains_tabs_actions_and_escapes_titles() {
    let mut app = BrowserAppState::new();

    let mut tab1 = BrowserTabState::new(TabId(1), "https://example.com/".to_string());
    tab1.title = Some("Rust & <Friends>".to_string());

    let mut tab2 = BrowserTabState::new(TabId(2), "https://example.net/".to_string());
    tab2.title = Some("Tab 2".to_string());
    tab2.can_go_back = true;
    tab2.can_go_forward = false;

    let tab3 = BrowserTabState::new(TabId(3), "about:newtab".to_string());

    app.push_tab(tab1, false);
    app.push_tab(tab2, true);
    app.push_tab(tab3, false);

    let html = chrome_frame_html_from_state(&app);

    // One `.tab` element per tab.
    let tab_count =
      html.matches("<div class=\"tab\"").count() + html.matches("<div class=\"tab active\"").count();
    assert_eq!(tab_count, 3);

    // Active tab marker is present.
    assert!(html.contains("class=\"tab active\" data-tab-id=\"2\""));

    // Action URLs contain expected tab ids.
    assert!(html.contains("chrome-action:activate-tab?tab_id=1"));
    assert!(html.contains("chrome-action:activate-tab?tab_id=2"));
    assert!(html.contains("chrome-action:activate-tab?tab_id=3"));
    assert!(html.contains("chrome-action:close-tab?tab_id=1"));
    assert!(html.contains("chrome-action:close-tab?tab_id=2"));
    assert!(html.contains("chrome-action:close-tab?tab_id=3"));

    // Favicons use stable chrome://favicons/<tab_id>.png URLs (no embedded data URLs).
    assert!(html.contains("chrome://favicons/1.png"));
    assert!(html.contains("chrome://favicons/2.png"));
    assert!(html.contains("chrome://favicons/3.png"));

    // Titles are escaped.
    assert!(html.contains("Rust &amp; &lt;Friends&gt;"));
    assert!(!html.contains("Rust & <Friends>"));
  }
}
