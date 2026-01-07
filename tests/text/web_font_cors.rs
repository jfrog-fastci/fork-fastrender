use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fastrender::api::ResourceContext;
use fastrender::css::types::{FontDisplay, FontFaceRule, FontFaceSource};
use fastrender::debug::runtime::{self, RuntimeToggles};
use fastrender::resource::FetchedResource;
use fastrender::text::font_db::{FontDatabase, FontStretch, FontStyle};
use fastrender::text::font_loader::{
  FontContext, FontFetcher, FontLoadStatus, WebFontLoadOptions, WebFontPolicy,
};

fn fixture_font_bytes() -> Option<Vec<u8>> {
  let path = Path::new("tests/fixtures/fonts/DejaVuSans-subset.ttf");
  if !path.exists() {
    return None;
  }
  fs::read(path).ok()
}

fn context_with_fetcher(fetcher: Arc<dyn FontFetcher>) -> Option<(FontContext, String)> {
  let data = fixture_font_bytes()?;
  let mut db = FontDatabase::empty();
  db.load_font_data(data).ok()?;
  let mut ctx = FontContext::with_database_and_fetcher(Arc::new(db), fetcher);
  ctx.set_resource_context(Some(ResourceContext {
    document_url: Some("https://a.test/page".to_string()),
    ..Default::default()
  }));
  let fallback_family = ctx
    .database()
    .first_font()
    .map(|font| font.family.clone())?;
  Some((ctx, fallback_family))
}

#[derive(Clone)]
struct CorsFetcher {
  data: Vec<u8>,
  access_control_allow_origin: Option<String>,
}

impl FontFetcher for CorsFetcher {
  fn fetch(&self, url: &str, _referrer_url: Option<&str>) -> fastrender::Result<FetchedResource> {
    let mut res = FetchedResource::with_final_url(
      self.data.clone(),
      Some("font/ttf".to_string()),
      Some(url.to_string()),
    );
    res.status = Some(200);
    res.access_control_allow_origin = self.access_control_allow_origin.clone();
    Ok(res)
  }
}

#[test]
fn web_font_cors_enforcement_is_runtime_gated() {
  let data = match fixture_font_bytes() {
    Some(bytes) => bytes,
    None => return,
  };

  let face = FontFaceRule {
    family: Some("CorsFace".to_string()),
    sources: vec![FontFaceSource::url("https://b.test/font.ttf".to_string())],
    display: FontDisplay::Block,
    ..Default::default()
  };

  let options = WebFontLoadOptions {
    policy: WebFontPolicy::BlockUntilLoaded {
      timeout: Duration::from_secs(1),
    },
  };

  let run_case =
    |toggle_enabled: bool, acao: Option<&str>, expect_loaded: bool, expect_selectable: bool| {
      let mut raw = HashMap::new();
      raw.insert(
        "FASTR_FETCH_ENFORCE_CORS".to_string(),
        if toggle_enabled {
          "1".to_string()
        } else {
          "0".to_string()
        },
      );
      let _guard = runtime::set_runtime_toggles(Arc::new(RuntimeToggles::from_map(raw)));

      let fetcher: Arc<dyn FontFetcher> = Arc::new(CorsFetcher {
        data: data.clone(),
        access_control_allow_origin: acao.map(|v| v.to_string()),
      });
      let (ctx, fallback_family) =
        context_with_fetcher(fetcher).expect("fixture font should be loadable");

      let report = ctx
        .load_web_fonts_with_options(&[face.clone()], None, None, options)
        .expect("web font load should not hard-fail");

      let status = report
        .events
        .iter()
        .find(|event| event.family == "CorsFace")
        .map(|event| &event.status)
        .expect("expected web font load event");

      if expect_loaded {
        assert!(matches!(status, FontLoadStatus::Loaded));
      } else {
        assert!(matches!(status, FontLoadStatus::Failed { .. }));
      }

      let resolved = ctx
        .get_font_full(
          &["CorsFace".to_string(), fallback_family],
          400,
          FontStyle::Normal,
          FontStretch::Normal,
        )
        .expect("resolve font with fallback");
      if expect_selectable {
        assert_eq!(resolved.family, "CorsFace");
      } else {
        assert_ne!(resolved.family, "CorsFace");
      }
    };

  // Toggle OFF: allow cross-origin fonts without ACAO metadata (status=Loaded).
  run_case(false, None, true, true);
  // Toggle ON: block cross-origin fonts without ACAO metadata (status=Failed).
  run_case(true, None, false, false);
  // Toggle ON: ACAO "*" is permitted.
  run_case(true, Some("*"), true, true);
  // Toggle ON: ACAO matching the document origin is permitted (default-port normalization).
  run_case(true, Some("https://a.test"), true, true);
}
