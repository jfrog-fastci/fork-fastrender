#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::geometry::{Point, Rect};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use fastrender::ui::messages::{
  PointerButton, PointerModifiers, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

fn extract_clipboard_text(messages: Vec<WorkerToUi>) -> Option<String> {
  messages.into_iter().find_map(|msg| match msg {
    WorkerToUi::SetClipboardText { text, .. } => Some(text),
    _ => None,
  })
}

fn find_point_in_text(tree: &FragmentTree, needle: &str) -> Option<Point> {
  fn walk(node: &FragmentNode, offset: Point, needle: &str) -> Option<Point> {
    let rect = Rect::new(offset.translate(node.bounds.origin), node.bounds.size);
    if let FragmentContent::Text { text, is_marker, .. } = &node.content {
      if !*is_marker && text.contains(needle) {
        let x = rect.x() + (rect.width().max(1.0) * 0.5);
        let y = rect.y() + (rect.height().max(1.0) * 0.5);
        return Some(Point::new(x, y));
      }
    }

    let child_offset = rect.origin;
    for child in node.children.iter() {
      if let Some(found) = walk(child, child_offset, needle) {
        return Some(found);
      }
    }
    None
  }

  walk(&tree.root, Point::ZERO, needle).or_else(|| {
    tree
      .additional_fragments
      .iter()
      .find_map(|root| walk(root, Point::ZERO, needle))
  })
}

#[test]
fn ui_document_selection_double_click_selects_word_and_suppresses_link_navigation() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let index_url = site.write(
    "index.html",
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  body { font: 40px/80px monospace; }
  #link { position: absolute; top: 0; left: 10px; }
</style>
<a id="link" href="dest.html">hello</a>
"#,
  );
  let _dest_url = site.write(
    "dest.html",
    r#"<!doctype html><meta charset="utf-8"><p>dest</p>"#,
  );

  let tab_id = TabId::new();
  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    // Load HTML from the fixture string, but keep a `file://` URL so relative href resolution works.
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  body { font: 40px/80px monospace; }
  #link { position: absolute; top: 0; left: 10px; }
</style>
<a id="link" href="dest.html">hello</a>
"#,
    &index_url,
    (320, 140),
    1.0,
  )?;

  // Initial paint ensures layout artifacts exist for selection serialization.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let down_msgs = controller.handle_message(support::pointer_down_with(
    tab_id,
    (20.0, 40.0),
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  assert!(
    down_msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected pointer down to repaint selection state"
  );

  let up_msgs = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (20.0, 40.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  assert!(
    !up_msgs.iter().any(|msg| matches!(
      msg,
      WorkerToUi::NavigationStarted { .. }
        | WorkerToUi::NavigationCommitted { .. }
        | WorkerToUi::NavigationFailed { .. }
    )),
    "double-click selection should suppress link navigation, got messages: {up_msgs:?}"
  );
  assert_eq!(
    controller.current_url(),
    index_url,
    "double-click selection should not update the current URL"
  );

  let copy_msgs = controller.handle_message(UiToWorker::Copy { tab_id })?;
  assert_eq!(
    extract_clipboard_text(copy_msgs).as_deref(),
    Some("hello"),
    "expected double-click word selection to copy just the clicked word"
  );

  Ok(())
}

#[test]
fn ui_document_selection_double_click_selects_word_across_inline_nodes() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId::new();
  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  body { font: 40px/80px monospace; }
  #p { position: absolute; top: 0; left: 10px; margin: 0; }
</style>
<p id="p"><span>he</span><span>llo</span></p>
"#,
    "https://example.invalid/",
    (320, 140),
    1.0,
  )?;

  // Initial paint ensures layout artifacts exist for selection serialization.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  // Double-click on the second span ("llo"). The word is split across two text nodes, so selection
  // should still include the full "hello".
  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    (100.0, 40.0),
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (100.0, 40.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  let copy_msgs = controller.handle_message(UiToWorker::Copy { tab_id })?;
  assert_eq!(
    extract_clipboard_text(copy_msgs).as_deref(),
    Some("hello"),
    "expected double-click selection to span across inline text node boundaries"
  );

  Ok(())
}

#[test]
fn ui_document_selection_triple_click_selects_paragraph_or_line() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId::new();
  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  body { font: 40px/80px monospace; }
  #p { position: absolute; top: 0; left: 10px; margin: 0; }
</style>
<p id="p">alpha <span>beta</span></p>
"#,
    "https://example.invalid/",
    (420, 140),
    1.0,
  )?;

  // Initial paint ensures layout artifacts exist for selection serialization.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    (40.0, 40.0),
    PointerButton::Primary,
    PointerModifiers::NONE,
    3,
  ))?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (40.0, 40.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  let copy_msgs = controller.handle_message(UiToWorker::Copy { tab_id })?;
  assert_eq!(
    extract_clipboard_text(copy_msgs).as_deref(),
    Some("alpha beta"),
    "expected triple-click selection to copy the full paragraph/line"
  );

  Ok(())
}

#[test]
fn ui_document_selection_double_click_in_list_does_not_copy_stray_markers_outside_range() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId::new();
  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    r#"<!doctype html><meta charset="utf-8">
<style>body{font:40px/80px monospace}</style>
<ul><li>AAAA</li><li>BBBB</li></ul>
"#,
    "https://example.invalid/",
    (500, 240),
    1.0,
  )?;

  // Initial paint ensures layout artifacts exist for selection serialization.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let prepared = controller
    .document()
    .prepared()
    .expect("expected prepared document after initial repaint");
  let click_point = find_point_in_text(prepared.fragment_tree(), "BBBB")
    .expect("expected to find a non-marker text fragment containing BBBB");

  let scroll = controller.scroll_state().viewport;
  let click_css = (click_point.x - scroll.x, click_point.y - scroll.y);

  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    click_css,
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    click_css,
    PointerButton::Primary,
  ))?;

  let copy_msgs = controller.handle_message(UiToWorker::Copy { tab_id })?;
  assert_eq!(
    extract_clipboard_text(copy_msgs).as_deref(),
    Some("BBBB"),
    "expected range selection inside a list item to copy only the selected text (no stray bullets)"
  );

  Ok(())
}

#[test]
fn ui_document_selection_double_click_drag_preserves_initial_word() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId::new();
  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  body { font: 40px/80px monospace; }
  #p { position: absolute; top: 0; left: 10px; margin: 0; }
</style>
<p id="p">hello world</p>
"#,
    "https://example.invalid/",
    (420, 140),
    1.0,
  )?;

  // Initial paint ensures layout artifacts exist for selection serialization.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  // Double-click on "world", then drag left into "hello".
  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    (240.0, 40.0),
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  let _ = controller.handle_message(UiToWorker::PointerMove {
    tab_id,
    pos_css: (20.0, 40.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (20.0, 40.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  let copy_msgs = controller.handle_message(UiToWorker::Copy { tab_id })?;
  assert_eq!(
    extract_clipboard_text(copy_msgs).as_deref(),
    Some("hello world"),
    "expected double-click drag selection to preserve the initial word"
  );

  Ok(())
}

#[test]
fn ui_document_selection_copy_word_after_br_has_no_leading_newline() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId::new();
  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
</style>
<p style="position: absolute; top: 0; left: 10px; margin: 0; font: 40px/80px monospace"><br>hello</p>
"#,
    "https://example.invalid/",
    (320, 220),
    1.0,
  )?;

  // Initial paint ensures layout artifacts exist for selection serialization.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  // Double-click on "hello" (second line, after the leading <br>).
  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    (20.0, 120.0),
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (20.0, 120.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  let copy_msgs = controller.handle_message(UiToWorker::Copy { tab_id })?;
  assert_eq!(
    extract_clipboard_text(copy_msgs).as_deref(),
    Some("hello"),
    "expected copying a word selection after <br> to omit the leading newline"
  );

  Ok(())
}

#[test]
fn ui_document_selection_copy_word_in_second_table_cell_has_no_leading_tab() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId::new();
  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  table { border-collapse: collapse; border-spacing: 0; }
  td { padding: 0; width: 200px; }
</style>
<table style="position: absolute; top: 0; left: 10px; font: 40px/80px monospace">
  <tr><td>A</td><td>B</td></tr>
</table>
"#,
    "https://example.invalid/",
    (520, 140),
    1.0,
  )?;

  // Initial paint ensures layout artifacts exist for selection serialization.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  // Double-click on "B" in the second <td>.
  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    (230.0, 40.0),
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (230.0, 40.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  let copy_msgs = controller.handle_message(UiToWorker::Copy { tab_id })?;
  assert_eq!(
    extract_clipboard_text(copy_msgs).as_deref(),
    Some("B"),
    "expected copying a word selection in the second table cell to omit the leading tab"
  );

  Ok(())
}

#[test]
fn ui_document_selection_double_click_drag_extends_selection_by_whole_words() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId::new();
  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  body { font: 40px/80px monospace; }
  #p { position: absolute; top: 0; left: 10px; margin: 0; }
</style>
<p id="p"><span>AAAAA</span> <span>BBBBB</span> <span>CCCCC</span></p>
"#,
    "https://example.invalid/",
    (520, 140),
    1.0,
  )?;

  // Initial paint ensures layout artifacts exist for selection serialization.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let prepared = controller
    .document()
    .prepared()
    .expect("expected prepared document after initial repaint");
  let click_point =
    find_point_in_text(prepared.fragment_tree(), "BBBBB").expect("expected point in BBBBB");
  let drag_point =
    find_point_in_text(prepared.fragment_tree(), "CCCCC").expect("expected point in CCCCC");

  let scroll = controller.scroll_state().viewport;
  let click_css = (click_point.x - scroll.x, click_point.y - scroll.y);
  let drag_css = (drag_point.x - scroll.x, drag_point.y - scroll.y);

  // Double-click BBBBB, then drag into CCCCC (but not to its word boundary). The selection should
  // extend to include the whole target word.
  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    click_css,
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  let _ = controller.handle_message(support::pointer_move(
    tab_id,
    drag_css,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    drag_css,
    PointerButton::Primary,
  ))?;

  let copy_msgs = controller.handle_message(UiToWorker::Copy { tab_id })?;
  assert_eq!(
    extract_clipboard_text(copy_msgs).as_deref(),
    Some("BBBBB CCCCC"),
    "expected double-click drag to extend by whole words"
  );

  Ok(())
}

#[test]
fn ui_document_selection_triple_click_drag_extends_selection_by_blocks() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId::new();
  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    r#"<!doctype html><meta charset="utf-8">
<style>
  html, body { margin: 0; padding: 0; background: #fff; }
  body { font: 40px/80px monospace; }
  p { margin: 0; }
</style>
<p>FIRST</p>
<p>SECOND</p>
"#,
    "https://example.invalid/",
    (520, 240),
    1.0,
  )?;

  // Initial paint ensures layout artifacts exist for selection serialization.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let prepared = controller
    .document()
    .prepared()
    .expect("expected prepared document after initial repaint");
  let first_point =
    find_point_in_text(prepared.fragment_tree(), "FIRST").expect("expected point in FIRST");
  let second_point =
    find_point_in_text(prepared.fragment_tree(), "SECOND").expect("expected point in SECOND");

  let scroll = controller.scroll_state().viewport;
  let first_css = (first_point.x - scroll.x, first_point.y - scroll.y);
  let second_css = (second_point.x - scroll.x, second_point.y - scroll.y);

  // Triple-click FIRST, then drag into SECOND. The selection should extend by whole blocks (native
  // behaviour for paragraph selection).
  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    first_css,
    PointerButton::Primary,
    PointerModifiers::NONE,
    3,
  ))?;
  let _ = controller.handle_message(support::pointer_move(
    tab_id,
    second_css,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    second_css,
    PointerButton::Primary,
  ))?;

  let copy_msgs = controller.handle_message(UiToWorker::Copy { tab_id })?;
  assert_eq!(
    extract_clipboard_text(copy_msgs).as_deref(),
    Some("FIRST\nSECOND"),
    "expected triple-click drag to extend by whole blocks"
  );

  Ok(())
}
