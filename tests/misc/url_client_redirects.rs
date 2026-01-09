use fastrender::api::{ClientRedirectKind, FastRender, RenderArtifactRequest, RenderOptions};
use fastrender::dom::{DomNode, DomNodeType};
use fastrender::Result;
use std::fs;
use tempfile::tempdir;
use url::Url;

fn dom_contains_text(node: &DomNode, needle: &str) -> bool {
  match &node.node_type {
    DomNodeType::Text { content } => {
      if content.contains(needle) {
        return true;
      }
    }
    _ => {}
  }

  for child in &node.children {
    if dom_contains_text(child, needle) {
      return true;
    }
  }
  false
}

#[test]
fn render_url_follows_meta_refresh_redirect() -> Result<()> {
  let tmp = tempdir()?;
  let start_dir = tmp.path().join("start");
  let target_dir = tmp.path().join("target");
  fs::create_dir_all(&start_dir)?;
  fs::create_dir_all(&target_dir)?;

  let target_path = target_dir.join("page.html");
  fs::write(
    &target_path,
    "<!doctype html><html><body>TARGET_PAGE</body></html>",
  )?;

  let base_href = Url::from_directory_path(&target_dir).expect("base href url");
  let start_path = start_dir.join("index.html");
  fs::write(
    &start_path,
    format!(
      "<!doctype html><html><head><base href=\"{base}\"><meta http-equiv=\"refresh\" content=\"0; url=page.html\"></head><body>START_PAGE</body></html>",
      base = base_href.as_str()
    ),
  )?;

  let start_url = Url::from_file_path(&start_path)
    .expect("start url")
    .to_string();
  let target_url = Url::from_file_path(&target_path)
    .expect("target url")
    .to_string();

  let mut renderer = FastRender::new()?;
  let options = RenderOptions::new()
    .with_viewport(64, 64)
    .with_follow_client_redirects(true)
    .with_max_client_redirect_hops(3);
  let report = renderer.render_url_with_options_report(
    &start_url,
    options,
    RenderArtifactRequest {
      dom: true,
      ..RenderArtifactRequest::none()
    },
  )?;

  let dom = report.artifacts.dom.expect("expected DOM artifact");
  assert!(
    dom_contains_text(&dom, "TARGET_PAGE"),
    "expected final DOM to come from redirect target"
  );

  assert_eq!(
    report.diagnostics.client_redirects.len(),
    1,
    "expected exactly one redirect hop"
  );
  let hop = &report.diagnostics.client_redirects[0];
  assert_eq!(hop.kind, ClientRedirectKind::MetaRefresh);
  assert_eq!(hop.from_url, start_url);
  assert_eq!(hop.to_url, target_url);
  assert!(
    !report.diagnostics.client_redirect_hop_limit_exhausted,
    "hop budget should not be exhausted"
  );
  Ok(())
}

#[test]
fn render_url_does_not_follow_meta_refresh_redirect_without_flag() -> Result<()> {
  let tmp = tempdir()?;
  let start_dir = tmp.path().join("start");
  let target_dir = tmp.path().join("target");
  fs::create_dir_all(&start_dir)?;
  fs::create_dir_all(&target_dir)?;

  let target_path = target_dir.join("page.html");
  fs::write(
    &target_path,
    "<!doctype html><html><body>TARGET_PAGE</body></html>",
  )?;

  let base_href = Url::from_directory_path(&target_dir).expect("base href url");
  let start_path = start_dir.join("index.html");
  fs::write(
    &start_path,
    format!(
      "<!doctype html><html><head><base href=\"{base}\"><meta http-equiv=\"refresh\" content=\"0; url=page.html\"></head><body>START_PAGE</body></html>",
      base = base_href.as_str()
    ),
  )?;

  let start_url = Url::from_file_path(&start_path)
    .expect("start url")
    .to_string();

  let mut renderer = FastRender::new()?;
  let options = RenderOptions::new().with_viewport(64, 64);
  let report = renderer.render_url_with_options_report(
    &start_url,
    options,
    RenderArtifactRequest {
      dom: true,
      ..RenderArtifactRequest::none()
    },
  )?;

  let dom = report.artifacts.dom.expect("expected DOM artifact");
  assert!(
    dom_contains_text(&dom, "START_PAGE"),
    "expected meta refresh to be ignored when follow_client_redirects is disabled"
  );
  assert_eq!(
    report.diagnostics.client_redirects.len(),
    0,
    "expected no followed redirect hops"
  );
  assert!(
    !report.diagnostics.client_redirect_hop_limit_exhausted,
    "hop budget should not be exhausted"
  );
  Ok(())
}

#[test]
fn render_url_client_redirect_hop_limit_stops_loop() -> Result<()> {
  let tmp = tempdir()?;

  let a_path = tmp.path().join("a.html");
  let b_path = tmp.path().join("b.html");
  let c_path = tmp.path().join("c.html");
  let d_path = tmp.path().join("d.html");

  fs::write(
    &a_path,
    "<!doctype html><meta http-equiv=\"refresh\" content=\"0; url=b.html\"><body>PAGE_A</body>",
  )?;
  fs::write(
    &b_path,
    "<!doctype html><meta http-equiv=\"refresh\" content=\"0; url=c.html\"><body>PAGE_B</body>",
  )?;
  fs::write(
    &c_path,
    "<!doctype html><meta http-equiv=\"refresh\" content=\"0; url=d.html\"><body>PAGE_C</body>",
  )?;
  fs::write(
    &d_path,
    "<!doctype html><meta http-equiv=\"refresh\" content=\"0; url=a.html\"><body>PAGE_D</body>",
  )?;

  let a_url = Url::from_file_path(&a_path).expect("a url").to_string();

  let mut renderer = FastRender::new()?;
  let options = RenderOptions::new()
    .with_viewport(64, 64)
    .with_follow_client_redirects(true)
    .with_max_client_redirect_hops(2);
  let report = renderer.render_url_with_options_report(
    &a_url,
    options,
    RenderArtifactRequest {
      dom: true,
      ..RenderArtifactRequest::none()
    },
  )?;

  let dom = report.artifacts.dom.expect("expected DOM artifact");
  assert!(
    dom_contains_text(&dom, "PAGE_C"),
    "expected DOM after hop budget to reflect the last followed document"
  );

  assert_eq!(
    report.diagnostics.client_redirects.len(),
    2,
    "expected two followed redirect hops"
  );
  assert!(
    report.diagnostics.client_redirect_hop_limit_exhausted,
    "expected hop budget exhaustion to be recorded"
  );
  Ok(())
}

#[test]
fn render_url_ignores_non_immediate_meta_refresh_delay() -> Result<()> {
  let tmp = tempdir()?;
  let start_dir = tmp.path().join("start");
  let target_dir = tmp.path().join("target");
  fs::create_dir_all(&start_dir)?;
  fs::create_dir_all(&target_dir)?;

  let target_path = target_dir.join("page.html");
  fs::write(
    &target_path,
    "<!doctype html><html><body>TARGET_PAGE</body></html>",
  )?;

  let base_href = Url::from_directory_path(&target_dir).expect("base href url");
  let start_path = start_dir.join("index.html");
  fs::write(
    &start_path,
    format!(
      "<!doctype html><html><head><base href=\"{base}\"><meta http-equiv=\"refresh\" content=\"5; url=page.html\"></head><body>START_PAGE</body></html>",
      base = base_href.as_str()
    ),
  )?;

  let start_url = Url::from_file_path(&start_path)
    .expect("start url")
    .to_string();

  let mut renderer = FastRender::new()?;
  let options = RenderOptions::new()
    .with_viewport(64, 64)
    .with_follow_client_redirects(true)
    .with_max_client_redirect_hops(3);
  let report = renderer.render_url_with_options_report(
    &start_url,
    options,
    RenderArtifactRequest {
      dom: true,
      ..RenderArtifactRequest::none()
    },
  )?;

  let dom = report.artifacts.dom.expect("expected DOM artifact");
  assert!(
    dom_contains_text(&dom, "START_PAGE"),
    "expected non-immediate meta refresh to be ignored"
  );
  assert_eq!(
    report.diagnostics.client_redirects.len(),
    0,
    "expected no followed redirect hops"
  );
  assert!(
    !report.diagnostics.client_redirect_hop_limit_exhausted,
    "hop budget should not be exhausted"
  );
  Ok(())
}

#[test]
fn render_url_follows_meta_refresh_without_delay_token() -> Result<()> {
  let tmp = tempdir()?;
  let start_dir = tmp.path().join("start");
  let target_dir = tmp.path().join("target");
  fs::create_dir_all(&start_dir)?;
  fs::create_dir_all(&target_dir)?;

  let target_path = target_dir.join("page.html");
  fs::write(
    &target_path,
    "<!doctype html><html><body>TARGET_PAGE</body></html>",
  )?;

  let base_href = Url::from_directory_path(&target_dir).expect("base href url");
  let start_path = start_dir.join("index.html");
  fs::write(
    &start_path,
    format!(
      "<!doctype html><html><head><base href=\"{base}\"><meta http-equiv=\"refresh\" content=\"url=page.html\"></head><body>START_PAGE</body></html>",
      base = base_href.as_str()
    ),
  )?;

  let start_url = Url::from_file_path(&start_path)
    .expect("start url")
    .to_string();
  let target_url = Url::from_file_path(&target_path)
    .expect("target url")
    .to_string();

  let mut renderer = FastRender::new()?;
  let options = RenderOptions::new()
    .with_viewport(64, 64)
    .with_follow_client_redirects(true)
    .with_max_client_redirect_hops(3);
  let report = renderer.render_url_with_options_report(
    &start_url,
    options,
    RenderArtifactRequest {
      dom: true,
      ..RenderArtifactRequest::none()
    },
  )?;

  let dom = report.artifacts.dom.expect("expected DOM artifact");
  assert!(
    dom_contains_text(&dom, "TARGET_PAGE"),
    "expected meta refresh without delay token to be treated as immediate"
  );
  assert_eq!(
    report.diagnostics.client_redirects.len(),
    1,
    "expected exactly one redirect hop"
  );
  let hop = &report.diagnostics.client_redirects[0];
  assert_eq!(hop.kind, ClientRedirectKind::MetaRefresh);
  assert_eq!(hop.from_url, start_url);
  assert_eq!(hop.to_url, target_url);
  assert!(
    !report.diagnostics.client_redirect_hop_limit_exhausted,
    "hop budget should not be exhausted"
  );
  Ok(())
}

#[test]
fn render_url_follows_js_location_redirect_when_enabled() -> Result<()> {
  let tmp = tempdir()?;
  let start_dir = tmp.path().join("start");
  let target_dir = tmp.path().join("target");
  fs::create_dir_all(&start_dir)?;
  fs::create_dir_all(&target_dir)?;

  let target_path = target_dir.join("page.html");
  fs::write(
    &target_path,
    "<!doctype html><html><body>TARGET_PAGE</body></html>",
  )?;

  let base_href = Url::from_directory_path(&target_dir).expect("base href url");
  let start_path = start_dir.join("index.html");
  fs::write(
    &start_path,
    format!(
      "<!doctype html><html><head><base href=\"{base}\"><script>location.replace('page.html')</script></head><body>START_PAGE</body></html>",
      base = base_href.as_str()
    ),
  )?;

  let start_url = Url::from_file_path(&start_path)
    .expect("start url")
    .to_string();
  let target_url = Url::from_file_path(&target_path)
    .expect("target url")
    .to_string();

  let mut renderer = FastRender::new()?;
  let options = RenderOptions::new()
    .with_viewport(64, 64)
    .with_follow_client_redirects(true)
    .with_follow_js_location_redirects(true)
    .with_max_client_redirect_hops(3);
  let report = renderer.render_url_with_options_report(
    &start_url,
    options,
    RenderArtifactRequest {
      dom: true,
      ..RenderArtifactRequest::none()
    },
  )?;

  let dom = report.artifacts.dom.expect("expected DOM artifact");
  assert!(
    dom_contains_text(&dom, "TARGET_PAGE"),
    "expected final DOM to come from redirect target"
  );

  assert_eq!(
    report.diagnostics.client_redirects.len(),
    1,
    "expected exactly one redirect hop"
  );
  let hop = &report.diagnostics.client_redirects[0];
  assert_eq!(hop.kind, ClientRedirectKind::JsLocation);
  assert_eq!(hop.from_url, start_url);
  assert_eq!(hop.to_url, target_url);
  assert!(
    !report.diagnostics.client_redirect_hop_limit_exhausted,
    "hop budget should not be exhausted"
  );
  Ok(())
}

#[test]
fn render_url_does_not_follow_js_location_redirect_without_flag() -> Result<()> {
  let tmp = tempdir()?;
  let start_dir = tmp.path().join("start");
  let target_dir = tmp.path().join("target");
  fs::create_dir_all(&start_dir)?;
  fs::create_dir_all(&target_dir)?;

  let target_path = target_dir.join("page.html");
  fs::write(
    &target_path,
    "<!doctype html><html><body>TARGET_PAGE</body></html>",
  )?;

  let base_href = Url::from_directory_path(&target_dir).expect("base href url");
  let start_path = start_dir.join("index.html");
  fs::write(
    &start_path,
    format!(
      "<!doctype html><html><head><base href=\"{base}\"><script>location.replace('page.html')</script></head><body>START_PAGE</body></html>",
      base = base_href.as_str()
    ),
  )?;

  let start_url = Url::from_file_path(&start_path)
    .expect("start url")
    .to_string();

  let mut renderer = FastRender::new()?;
  let options = RenderOptions::new()
    .with_viewport(64, 64)
    .with_follow_client_redirects(true)
    .with_max_client_redirect_hops(3);
  let report = renderer.render_url_with_options_report(
    &start_url,
    options,
    RenderArtifactRequest {
      dom: true,
      ..RenderArtifactRequest::none()
    },
  )?;

  let dom = report.artifacts.dom.expect("expected DOM artifact");
  assert!(
    dom_contains_text(&dom, "START_PAGE"),
    "expected DOM to remain on the initial document when JS redirects are disabled"
  );
  assert_eq!(
    report.diagnostics.client_redirects.len(),
    0,
    "expected no followed redirect hops"
  );
  assert!(
    !report.diagnostics.client_redirect_hop_limit_exhausted,
    "hop budget should not be exhausted"
  );
  Ok(())
}
