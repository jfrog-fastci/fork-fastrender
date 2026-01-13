//! Browser integration coverage for the experimental renderer-chrome address bar.
//!
//! This test is headless: it does not create a window or initialize wgpu/winit/egui.

use crate::browser_integration::support;

#[test]
fn renderer_chrome_address_bar_typing_updates_browser_state() {
  let _lock = crate::browser_integration::stage_listener_test_lock();

  let renderer = support::deterministic_renderer();

  let mut browser_state = fastrender::ui::BrowserAppState::new_with_initial_tab("about:newtab".to_string());
  let mut chrome =
    fastrender::ui::ChromeFrameDocument::new_with_renderer(renderer, (480, 80), 1.0).expect("chrome frame");

  // Initial state → DOM sync (navigation commit / tab switch should reflect in DOM).
  fastrender::ui::sync_browser_state_to_chrome_frame(&mut browser_state, &mut chrome);
  assert_eq!(
    chrome.address_bar_value(),
    browser_state.chrome.address_bar_text,
    "expected state → DOM sync to set address bar value"
  );

  // Focus + type in the DOM, then apply DOM → state events.
  for event in chrome.focus_address_bar() {
    fastrender::ui::apply_chrome_frame_event(&mut browser_state, event);
  }
  for event in chrome.text_input("cats") {
    fastrender::ui::apply_chrome_frame_event(&mut browser_state, event);
  }

  assert_eq!(browser_state.chrome.address_bar_text, "cats");
  assert!(browser_state.chrome.address_bar_has_focus);
  assert!(browser_state.chrome.address_bar_editing);

  // This mirrors the windowed browser's search-suggest request gate:
  // it only fires while focused + editing and the input resolves to a Search query.
  let resolution = fastrender::ui::resolve_omnibox_input(&browser_state.chrome.address_bar_text)
    .expect("resolve omnibox input");
  match resolution {
    fastrender::ui::OmniboxInputResolution::Search { query, .. } => {
      assert_eq!(query, "cats");
    }
    other => panic!("expected Search resolution for typed query, got {other:?}"),
  }

  // State → DOM sync should keep the DOM input value coherent (no clobber while editing).
  fastrender::ui::sync_browser_state_to_chrome_frame(&mut browser_state, &mut chrome);
  assert_eq!(chrome.address_bar_value(), "cats");
}

