use std::sync::Arc;

use fastrender::dom::{enumerate_dom_ids, DomNode};
use fastrender::interaction::{absolute_bounds_for_box_id, InteractionAction, InteractionEngine};
use fastrender::ui::browser_app::BrowserAppState;
use fastrender::ui::chrome_action_url::ChromeActionUrl;
use fastrender::ui::chrome_assets::ChromeAssetsFetcher;
use fastrender::ui::chrome_frame::chrome_frame_html_from_state;
use fastrender::ui::omnibox::{
  OmniboxAction, OmniboxSuggestion, OmniboxSuggestionSource, OmniboxUrlSource,
};
use fastrender::ui::{PointerButton, PointerModifiers, TabId};
use fastrender::{BoxNode, BoxTree, FastRender, FontConfig, Point, RenderOptions, Result};

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

fn find_box_id_for_styled_node_id(box_tree: &BoxTree, styled_node_id: usize) -> Option<usize> {
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if node.styled_node_id == Some(styled_node_id) {
      return Some(node.id);
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn clicking_omnibox_suggestion_emits_chrome_action_url() -> Result<()> {
  let mut app = BrowserAppState::new();
  app.chrome.omnibox.open = true;
  app.chrome.omnibox.selected = Some(0);
  app.chrome.omnibox.suggestions = vec![OmniboxSuggestion {
    action: OmniboxAction::NavigateToUrl,
    title: Some("Example".to_string()),
    url: Some("https://example.com/".to_string()),
    source: OmniboxSuggestionSource::Url(OmniboxUrlSource::Visited),
  }];

  let html = chrome_frame_html_from_state(&app);

  let renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .fetcher(Arc::new(ChromeAssetsFetcher::new()))
    .build()
    .expect("build renderer");

  let options = RenderOptions::new().with_viewport(600, 300);
  let mut doc = fastrender::BrowserDocument::new(renderer, &html, options)?;
  doc.render_frame_with_scroll_state()?;

  // Find the DOM node id for the first suggestion.
  let node =
    find_by_id(doc.dom(), "omnibox-suggestion-0").expect("omnibox-suggestion-0 should exist");
  let ids = enumerate_dom_ids(doc.dom());
  let node_id = *ids
    .get(&(node as *const DomNode))
    .expect("expected omnibox item to have a DOM id");

  // Find a box corresponding to that styled node and compute its bounds.
  let prepared = doc.prepared().expect("expected cached layout artifacts");
  let box_id = find_box_id_for_styled_node_id(prepared.box_tree(), node_id)
    .expect("expected omnibox item to produce at least one box");
  let rect = absolute_bounds_for_box_id(prepared.fragment_tree(), box_id).expect("box bounds");
  let click_point = Point::new(
    rect.x() + rect.width() * 0.5,
    rect.y() + rect.height() * 0.5,
  );

  let scroll_state = doc.scroll_state();
  let mut engine = InteractionEngine::new();
  let action = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let mut changed = engine.pointer_down(dom, box_tree, fragment_tree, &scroll_state, click_point);
    let (changed_up, action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_point,
      PointerButton::Primary,
      PointerModifiers::NONE,
      false,
      "chrome://frame",
      "chrome://frame",
    );
    changed |= changed_up;
    (changed, action)
  })?;

  let href = match action {
    InteractionAction::Navigate { href } => href,
    other => panic!("expected Navigate action, got {other:?}"),
  };
  let parsed = ChromeActionUrl::parse(&href).expect("parse chrome-action href");
  assert_eq!(
    parsed,
    ChromeActionUrl::Navigate {
      url: "https://example.com/".to_string()
    }
  );

  Ok(())
}

#[test]
fn omnibox_activate_tab_suggestion_has_expected_action_url() {
  let mut app = BrowserAppState::new();
  app.chrome.omnibox.open = true;
  app.chrome.omnibox.selected = Some(0);
  app.chrome.omnibox.suggestions = vec![OmniboxSuggestion {
    action: OmniboxAction::ActivateTab(TabId(42)),
    title: Some("Tab".to_string()),
    url: Some("https://tab.example/".to_string()),
    source: OmniboxSuggestionSource::Url(OmniboxUrlSource::OpenTab),
  }];

  let html = chrome_frame_html_from_state(&app);
  let expected_href = ChromeActionUrl::ActivateTab { tab_id: TabId(42) }.to_url_string();
  assert!(
    html.contains(&format!("href=\"{expected_href}\"")),
    "unexpected html: {html}"
  );
}
