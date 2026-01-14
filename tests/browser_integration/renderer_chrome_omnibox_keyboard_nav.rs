#![cfg(feature = "browser_ui")]

use fastrender::dom::DomNode;
use fastrender::ui::about_pages;
use fastrender::ui::omnibox::{build_omnibox_suggestions_default_limit, OmniboxAction, OmniboxContext};
use fastrender::ui::omnibox_nav::OmniboxNavKey;
use fastrender::ui::messages::{RepaintReason, UiToWorker};
use fastrender::ui::{BrowserAppState, BrowserTabState, ChromeAction, ChromeFrameDocument, TabId};

use super::support;
use super::worker_harness::WorkerHarness;

fn seed_omnibox_suggestions(app: &mut BrowserAppState) {
  let input = app.chrome.address_bar_text.clone();
  let ctx = OmniboxContext {
    open_tabs: &app.tabs,
    closed_tabs: &app.closed_tabs,
    visited: &app.visited,
    active_tab_id: app.active_tab_id(),
    bookmarks: None,
    remote_search_suggest: Some(&app.chrome.remote_search_cache),
  };
  app.chrome.omnibox.suggestions = build_omnibox_suggestions_default_limit(&ctx, &input);
  app.chrome.omnibox.open = !app.chrome.omnibox.suggestions.is_empty();
  app.chrome.omnibox.selected = None;
  app.chrome.omnibox.original_input = None;
  app.chrome.omnibox.last_built_for_input = input;
  app.chrome.omnibox.last_built_remote_fetched_at = app.chrome.remote_search_cache.fetched_at;
}

fn selected_suggestion_index_from_dom(dom: &DomNode) -> Option<usize> {
  let mut stack: Vec<&DomNode> = vec![dom];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("aria-selected") == Some("true") {
      if let Some(id) = node.get_attribute_ref("id") {
        if let Some(idx) = id.strip_prefix("omnibox-suggestion-") {
          if let Ok(parsed) = idx.parse::<usize>() {
            return Some(parsed);
          }
        }
      }
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn fill_text_for_suggestion(suggestion: &fastrender::ui::OmniboxSuggestion) -> Option<&str> {
  match &suggestion.action {
    OmniboxAction::NavigateToUrl => suggestion.url.as_deref(),
    OmniboxAction::ActivateTab(_) => suggestion.url.as_deref(),
    OmniboxAction::Search(query) => Some(query),
  }
}

#[test]
fn renderer_chrome_omnibox_arrow_nav_and_escape_update_state_and_dom() {
  let mut app = BrowserAppState::new();
  let tab_a = TabId::new();
  app.push_tab(BrowserTabState::new(tab_a, about_pages::ABOUT_NEWTAB.to_string()), true);

  app.chrome.address_bar_text = "about:h".to_string();
  app.chrome.address_bar_has_focus = true;
  app.chrome.address_bar_editing = true;
  seed_omnibox_suggestions(&mut app);
  assert!(app.chrome.omnibox.open, "expected seeded omnibox to be open");
  assert!(
    app.chrome.omnibox.suggestions.len() >= 2,
    "expected at least 2 deterministic suggestions, got {:?}",
    app.chrome.omnibox.suggestions
  );

  let renderer = support::deterministic_renderer();
  let mut doc =
    ChromeFrameDocument::new_with_renderer(renderer, (320, 200), 1.0).expect("chrome doc");
  doc.sync_state(&app);

  assert_eq!(selected_suggestion_index_from_dom(doc.dom()), None);

  // ArrowDown selects the first suggestion and captures original input.
  doc.handle_address_bar_key(&mut app, OmniboxNavKey::ArrowDown);
  assert_eq!(app.chrome.omnibox.selected, Some(0));
  assert_eq!(selected_suggestion_index_from_dom(doc.dom()), Some(0));
  assert_eq!(
    app.chrome.omnibox.original_input.as_deref(),
    Some("about:h"),
    "expected original input to be captured on first selection"
  );

  // ArrowDown again moves selection and fills address bar text.
  doc.handle_address_bar_key(&mut app, OmniboxNavKey::ArrowDown);
  assert_eq!(app.chrome.omnibox.selected, Some(1));
  assert_eq!(selected_suggestion_index_from_dom(doc.dom()), Some(1));
  let expected_fill = fill_text_for_suggestion(&app.chrome.omnibox.suggestions[1])
    .expect("expected suggestion fill text")
    .to_string();
  assert_eq!(app.chrome.address_bar_text, expected_fill);

  // ArrowUp moves back to the previous suggestion.
  doc.handle_address_bar_key(&mut app, OmniboxNavKey::ArrowUp);
  assert_eq!(app.chrome.omnibox.selected, Some(0));
  assert_eq!(selected_suggestion_index_from_dom(doc.dom()), Some(0));

  // Escape closes the dropdown and restores the original input.
  doc.handle_address_bar_key(&mut app, OmniboxNavKey::Escape);
  assert!(!app.chrome.omnibox.open);
  assert_eq!(app.chrome.omnibox.selected, None);
  assert_eq!(app.chrome.omnibox.original_input, None);
  assert_eq!(app.chrome.address_bar_text, "about:h");
  assert_eq!(selected_suggestion_index_from_dom(doc.dom()), None);

  // ArrowDown should reopen the dropdown after dismissal, matching browser omnibox UX.
  doc.handle_address_bar_key(&mut app, OmniboxNavKey::ArrowDown);
  assert!(app.chrome.omnibox.open);
  assert_eq!(app.chrome.omnibox.selected, Some(0));
  assert_eq!(selected_suggestion_index_from_dom(doc.dom()), Some(0));
}

#[test]
fn renderer_chrome_omnibox_enter_accepts_selected_suggestion() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();

  // Spawn a real UI worker so we can assert that accepting an omnibox suggestion triggers the same
  // SetActiveTab + repaint messages the windowed browser would send.
  let worker = WorkerHarness::spawn();
  let tab_a = TabId::new();
  let tab_b = TabId::new();

  // Create both tabs in the worker so SetActiveTab/Repaint are meaningful.
  worker.send(support::create_tab_msg(
    tab_a,
    Some(about_pages::ABOUT_NEWTAB.to_string()),
  ));
  worker.send(support::viewport_changed_msg(tab_a, (320, 200), 1.0));
  let _ = worker.wait_for_frame(tab_a, support::DEFAULT_TIMEOUT);

  worker.send(support::create_tab_msg(
    tab_b,
    Some(about_pages::ABOUT_HISTORY.to_string()),
  ));
  worker.send(support::viewport_changed_msg(tab_b, (320, 200), 1.0));
  let _ = worker.wait_for_frame(tab_b, support::DEFAULT_TIMEOUT);

  // Mirror a minimal UI-side BrowserAppState so the omnibox suggestion engine sees the two tabs.
  let mut app = BrowserAppState::new();
  app.push_tab(BrowserTabState::new(tab_a, about_pages::ABOUT_NEWTAB.to_string()), true);
  app.push_tab(
    BrowserTabState::new(tab_b, about_pages::ABOUT_HISTORY.to_string()),
    false,
  );

  app.chrome.address_bar_text = "about:h".to_string();
  app.chrome.address_bar_has_focus = true;
  app.chrome.address_bar_editing = true;
  seed_omnibox_suggestions(&mut app);

  let activate_idx = app
    .chrome
    .omnibox
    .suggestions
    .iter()
    .position(|s| matches!(s.action, OmniboxAction::ActivateTab(_)))
    .expect("expected an ActivateTab suggestion for the non-active tab");

  let renderer = support::deterministic_renderer();
  let mut doc =
    ChromeFrameDocument::new_with_renderer(renderer, (320, 200), 1.0).expect("chrome doc");
  doc.sync_state(&app);

  // Move selection to the activate-tab suggestion.
  for _ in 0..=activate_idx {
    doc.handle_address_bar_key(&mut app, OmniboxNavKey::ArrowDown);
  }
  assert_eq!(app.chrome.omnibox.selected, Some(activate_idx));
  assert_eq!(
    selected_suggestion_index_from_dom(doc.dom()),
    Some(activate_idx),
    "expected DOM highlight to track selected index"
  );

  let action = doc
    .handle_address_bar_key(&mut app, OmniboxNavKey::Enter)
    .expect("expected Enter to accept the selected suggestion");

  // Apply the emitted chrome actions as the browser front-end would.
  if let ChromeAction::ActivateTab(id) = action {
    assert_eq!(id, tab_b);
    assert!(
      app.set_active_tab(id),
      "expected ActivateTab to update BrowserAppState"
    );
    worker.send(UiToWorker::SetActiveTab { tab_id: id });
    worker.send(UiToWorker::RequestRepaint {
      tab_id: id,
      reason: RepaintReason::Explicit,
    });

    // The repaint should result in a FrameReady for the activated tab.
    let _ = worker.wait_for_frame(id, support::DEFAULT_TIMEOUT);
  }

  assert_eq!(app.active_tab_id(), Some(tab_b));
  assert!(!app.chrome.address_bar_editing);
  assert!(!app.chrome.address_bar_has_focus);
  assert!(!app.chrome.omnibox.open);
  assert_eq!(app.chrome.omnibox.selected, None);
  assert_eq!(selected_suggestion_index_from_dom(doc.dom()), None);
}
