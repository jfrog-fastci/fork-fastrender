//! HTML generator for experimental renderer-chrome UI.
//!
//! This is an early prototype used to render browser chrome (tabs/address bar) with FastRender
//! itself. It intentionally keeps data payloads (notably favicons) out of the HTML by using stable
//! `chrome://` URLs served by [`crate::ui::ChromeDynamicAssetFetcher`].

use crate::ui::browser_app::BrowserAppState;
use crate::ui::chrome_dynamic_asset_fetcher::ChromeDynamicAssetFetcher;
use crate::ui::html_escape::escape_html;

/// Generate a minimal HTML tab strip.
///
/// The structure is intentionally simple for now:
/// - Each tab has `data-tab-id`.
/// - Favicons are referenced via a stable `chrome://favicon/<tab_id>` URL.
pub fn tab_strip_html(app: &BrowserAppState) -> String {
  let active = app.active_tab_id();
  let mut html = String::new();
  html.push_str("<div class=\"tab-strip\">");
  for tab in &app.tabs {
    let mut class = "tab";
    if active == Some(tab.id) {
      class = "tab active";
    }
    let title = escape_html(&tab.display_title());
    let favicon_url = ChromeDynamicAssetFetcher::favicon_url(tab.id);
    html.push_str(&format!(
      "<div class=\"{class}\" data-tab-id=\"{}\"><img class=\"tab-favicon\" src=\"{favicon_url}\" alt=\"\" /><span class=\"tab-title\">{title}</span></div>",
      tab.id.0
    ));
  }
  html.push_str("</div>");
  html
}
