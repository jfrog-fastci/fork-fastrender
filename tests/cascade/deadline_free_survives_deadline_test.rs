use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::CssImportLoader;
use fastrender::dom::{DomNode, DomNodeType};
use fastrender::render_control::{DeadlineGuard, RenderDeadline};
use fastrender::style::cascade::{
  apply_style_set_with_media_target_and_imports_cached, apply_styles_with_media_target_and_imports_cached,
};
use fastrender::style::media::MediaContext;
use fastrender::style::style_set::StyleSet;
use std::time::Duration;

struct DummyImportLoader;

impl CssImportLoader for DummyImportLoader {
  fn load(&self, _url: &str) -> fastrender::Result<String> {
    Ok(String::new())
  }
}

fn minimal_dom() -> DomNode {
  DomNode {
    node_type: DomNodeType::Document {
      quirks_mode: selectors::context::QuirksMode::NoQuirks,
      scripting_enabled: true,
    },
    children: Vec::new(),
  }
}

#[test]
fn deadline_free_cascade_helpers_do_not_panic_under_deadline() {
  let dom = minimal_dom();
  let media_ctx = MediaContext::screen(800.0, 600.0);
  let loader = DummyImportLoader;

  // Ensure we exercise import resolution in the style-set helper by including an @import rule.
  let sheet = parse_stylesheet("@import \"https://example.com/import.css\";")
    .expect("stylesheet should parse");
  let style_set = StyleSet::from_document(sheet.clone());

  // Install an already-expired deadline in TLS so any check_active calls fail immediately.
  let deadline = RenderDeadline::new(Some(Duration::from_millis(0)), None);
  let _deadline_guard = DeadlineGuard::install(Some(&deadline));

  let styled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    apply_styles_with_media_target_and_imports_cached(
      &dom,
      &sheet,
      &media_ctx,
      None,
      Some(&loader),
      None,
      None,
      None,
      None,
      None,
    )
  }))
  .expect("expected stylesheet cascade helper to return instead of panicking");
  assert_eq!(styled.node_id, 1);

  let styled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    apply_style_set_with_media_target_and_imports_cached(
      &dom,
      &style_set,
      &media_ctx,
      None,
      Some(&loader),
      None,
      None,
      None,
      None,
      None,
    )
  }))
  .expect("expected style-set cascade helper to return instead of panicking");
  assert_eq!(styled.node_id, 1);
}
