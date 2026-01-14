use fastrender::error::{Error, Result};
use fastrender::js::{RunLimits, RunUntilIdleOutcome};
use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::{BrowserTab, RenderOptions};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone, Copy, Default)]
struct FileOnlyFetcher;

impl ResourceFetcher for FileOnlyFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    let parsed =
      Url::parse(url).map_err(|err| Error::Other(format!("invalid URL {url:?}: {err}")))?;
    if parsed.scheme() != "file" {
      return Err(Error::Other(format!(
        "FileOnlyFetcher only supports file:// URLs; got scheme={} url={url:?}",
        parsed.scheme()
      )));
    }
    let path = parsed
      .to_file_path()
      .map_err(|()| Error::Other(format!("failed to convert file:// URL to path: {url:?}")))?;
    let bytes = std::fs::read(&path).map_err(|err| {
      Error::Other(format!(
        "failed to read file:// resource {}: {err}",
        path.display()
      ))
    })?;
    Ok(FetchedResource::with_final_url(
      bytes,
      None,
      Some(url.to_string()),
    ))
  }
}

fn fixture_path() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/pages/fixtures/media_playback/preload_none.html")
}

#[test]
fn preload_none_does_not_fire_load_events_until_play() -> Result<()> {
  let path = fixture_path();
  let html = std::fs::read_to_string(&path)
    .map_err(|err| Error::Other(format!("failed to read fixture {}: {err}", path.display())))?;
  let document_url = Url::from_file_path(&path)
    .map(|u| u.to_string())
    .map_err(|()| {
      Error::Other(format!(
        "failed to convert {} to file:// URL",
        path.display()
      ))
    })?;

  let options = RenderOptions::new().with_viewport(64, 64);
  let fetcher: Arc<dyn ResourceFetcher> = Arc::new(FileOnlyFetcher);
  let mut tab = BrowserTab::from_html_with_vmjs_and_document_url_and_fetcher(
    &html,
    &document_url,
    options,
    fetcher,
  )?;

  let limits = RunLimits {
    max_tasks: 10_000,
    max_microtasks: 100_000,
    max_wall_time: Some(Duration::from_secs(2)),
  };

  // Let DOMContentLoaded fire and give the host a chance to discover media elements. With
  // `preload="none"`, no load readiness events should fire yet.
  for _ in 0..8 {
    match tab.run_event_loop_until_idle(limits)? {
      RunUntilIdleOutcome::Idle => {}
      RunUntilIdleOutcome::Stopped(_) => {}
    }

    let dom = tab.dom();
    let indicator = dom
      .get_element_by_id("load-indicator")
      .ok_or_else(|| Error::Other("missing #load-indicator in fixture".to_string()))?;
    let class = dom
      .get_attribute(indicator, "class")
      .map_err(|err| Error::Other(err.to_string()))?
      .map(|s| s.to_string());
    if class.as_deref() != Some("indicator") {
      return Err(Error::Other(format!(
        "expected preload_none fixture to keep #load-indicator idle before play(); got class={class:?}"
      )));
    }
  }

  // Trigger playback via click (calls `video.play()` in the fixture).
  let play_button = {
    let dom = tab.dom();
    dom
      .get_element_by_id("play")
      .ok_or_else(|| Error::Other("missing #play in fixture".to_string()))?
  };
  let _default_allowed = tab.dispatch_click_event(play_button)?;

  // After play, the fixture should flip #play-indicator to playing deterministically.
  for _ in 0..8 {
    match tab.run_event_loop_until_idle(limits)? {
      RunUntilIdleOutcome::Idle => {}
      RunUntilIdleOutcome::Stopped(_) => {}
    }

    let dom = tab.dom();
    let indicator = dom
      .get_element_by_id("play-indicator")
      .ok_or_else(|| Error::Other("missing #play-indicator in fixture".to_string()))?;
    let class = dom
      .get_attribute(indicator, "class")
      .map_err(|err| Error::Other(err.to_string()))?
      .map(|s| s.to_string());
    if class.as_deref() == Some("indicator playing") {
      return Ok(());
    }
  }

  let dom = tab.dom();
  let indicator = dom
    .get_element_by_id("play-indicator")
    .ok_or_else(|| Error::Other("missing #play-indicator in fixture".to_string()))?;
  let class = dom
    .get_attribute(indicator, "class")
    .map_err(|err| Error::Other(err.to_string()))?;
  Err(Error::Other(format!(
    "expected preload_none fixture to flip #play-indicator to .playing after play(); got class={class:?}"
  )))
}
