use crate::dom::DomNode;
use crate::dom2::{Document, RendererDomSnapshot};
use crate::error::{Error, RenderError, RenderStage, Result};
use crate::geometry::Point;
use crate::js::clock::{Clock, RealClock};
use crate::resource::ReferrerPolicy;
use crate::scroll::ScrollState;
use crate::animation::TransitionState;

use super::{PreparedDocument, PreparedPaintOptions, RenderOptions};
use std::sync::Arc;
use std::time::Duration;

/// Mutable, multi-frame renderer backed by a `dom2` live DOM tree.
///
/// `BrowserDocument2` owns a [`super::FastRender`] instance and a live [`Document`]. DOM mutations
/// invalidate the cached style/layout/paint results, and the next call to
/// [`BrowserDocument2::render_if_needed`] recomputes the pipeline once, coalescing all intermediate
/// changes.
pub struct BrowserDocument2 {
  renderer: super::FastRender,
  dom: Document,
  options: RenderOptions,
  prepared: Option<PreparedDocument>,
  last_dom_mapping: Option<RendererDomSnapshot>,
  animation_clock: Arc<dyn Clock>,
  realtime_animations_enabled: bool,
  animation_timeline_origin: Option<Duration>,
  animation_state_store: crate::animation::AnimationStateStore,
  style_dirty: bool,
  layout_dirty: bool,
  paint_dirty: bool,
}

impl BrowserDocument2 {
  /// Creates a new live `dom2` document from an HTML string using a fresh renderer.
  pub fn from_html(html: &str, options: RenderOptions) -> Result<Self> {
    let renderer = super::FastRender::new()?;
    let dom = renderer.parse_html(html)?;
    let dom = Document::from_renderer_dom(&dom);
    Ok(Self {
      renderer,
      dom,
      options,
      prepared: None,
      last_dom_mapping: None,
      animation_clock: Arc::new(RealClock::default()),
      realtime_animations_enabled: false,
      animation_timeline_origin: None,
      animation_state_store: crate::animation::AnimationStateStore::default(),
      // First frame needs a full pipeline run.
      style_dirty: true,
      layout_dirty: true,
      paint_dirty: true,
    })
  }

  /// Fetches and prepares a URL using the internal renderer, replacing the live `dom2` document
  /// in-place.
  pub fn navigate_url(
    &mut self,
    url: &str,
    options: RenderOptions,
  ) -> Result<super::BrowserNavigationReport> {
    let super::PreparedDocumentReport {
      document,
      diagnostics,
      final_url,
      base_url,
    } = self.renderer.prepare_url(url, options.clone())?;

    self.dom = Document::from_renderer_dom(&document.dom);
    self.options = options;
    self.prepared = Some(document);
    self.last_dom_mapping = None;
    self.style_dirty = false;
    self.layout_dirty = false;
    self.paint_dirty = true;
    self.animation_timeline_origin = None;
    self.animation_state_store = crate::animation::AnimationStateStore::default();

    Ok(super::BrowserNavigationReport {
      diagnostics,
      final_url,
      base_url,
    })
  }

  /// Overrides the clock used for real-time animation sampling.
  ///
  /// Changing the clock resets the document timeline origin so the next painted frame samples at
  /// ~0ms.
  pub fn set_animation_clock(&mut self, clock: Arc<dyn Clock>) {
    self.animation_clock = clock;
    self.animation_timeline_origin = None;
    self.animation_state_store = crate::animation::AnimationStateStore::default();
    self.paint_dirty = true;
  }

  /// Enables/disables real-time sampling of time-based animations.
  ///
  /// When enabled and no explicit `RenderOptions.animation_time` override is present, each paint
  /// samples the document timeline based on the configured [`Clock`]. The origin is lazily
  /// initialized so the first frame after enabling starts at ~0ms.
  pub fn set_realtime_animations_enabled(&mut self, enabled: bool) {
    if enabled && !self.realtime_animations_enabled {
      self.animation_timeline_origin = None;
      self.animation_state_store = crate::animation::AnimationStateStore::default();
    }
    if enabled != self.realtime_animations_enabled {
      self.paint_dirty = true;
    }
    self.realtime_animations_enabled = enabled;
  }

  /// Returns an immutable reference to the live `dom2` document.
  pub fn dom(&self) -> &Document {
    &self.dom
  }

  /// Returns a mutable reference to the live `dom2` document, marking the document dirty.
  pub fn dom_mut(&mut self) -> &mut Document {
    self.invalidate_all();
    &mut self.dom
  }

  /// Mutates the `dom2` document, marking the document dirty only when `f` reports that it changed
  /// something.
  pub fn mutate_dom<F>(&mut self, f: F) -> bool
  where
    F: FnOnce(&mut Document) -> bool,
  {
    let changed = f(&mut self.dom);
    if changed {
      self.invalidate_all();
    }
    changed
  }

  /// Updates the animation/transition sampling timestamp in milliseconds since document load.
  ///
  /// When the value changes, this marks paint dirty (but does not invalidate style/layout).
  pub fn set_animation_time(&mut self, time_ms: Option<f32>) {
    let sanitized = super::sanitize_animation_time_ms(time_ms);
    if sanitized != self.options.animation_time {
      self.options.animation_time = sanitized;
      self.paint_dirty = true;
    }
  }

  /// Convenience wrapper for [`BrowserDocument2::set_animation_time`] with a concrete timestamp.
  pub fn set_animation_time_ms(&mut self, time_ms: f32) {
    self.set_animation_time(Some(time_ms));
  }

  /// Returns the mapping produced for the most recently rendered snapshot, if available.
  pub fn last_dom_mapping(&self) -> Option<&RendererDomSnapshot> {
    self.last_dom_mapping.as_ref()
  }

  /// Renders a new frame if anything has been invalidated since the last successful frame.
  ///
  /// Returns `Ok(None)` when no dirty flags are set.
  pub fn render_if_needed(&mut self) -> Result<Option<super::Pixmap>> {
    if !self.is_dirty() {
      return Ok(None);
    }
    let pixmap = self.render_frame()?;
    Ok(Some(pixmap))
  }

  /// Renders one frame.
  ///
  /// If the document is dirty, this triggers a full pipeline run. Otherwise, it repaints from
  /// cached layout artifacts.
  pub fn render_frame(&mut self) -> Result<super::Pixmap> {
    // If we haven't rendered before, force a full pipeline run even if the flags were cleared.
    if self.prepared.is_none() {
      self.invalidate_all();
    }

    let resolved_animation_time = self.resolve_animation_time_ms();

    let needs_layout = self.style_dirty || self.layout_dirty;
    if needs_layout {
      let prev_prepared = self.prepared.take();

      let snapshot = self.dom.to_renderer_dom_with_mapping();
      let mut prepared = match self.prepare_dom_with_options(&snapshot.dom) {
        Ok(prepared) => prepared,
        Err(err) => {
          self.prepared = prev_prepared;
          return Err(err);
        }
      };

      let now_ms = resolved_animation_time;
      match now_ms {
        None => {
          prepared.fragment_tree.transition_state = None;
        }
        Some(now_ms) => {
          let prev_state = prev_prepared
            .as_ref()
            .and_then(|prepared| prepared.fragment_tree.transition_state.as_deref());
          let prev_box_tree = prev_prepared.as_ref().map(|prepared| prepared.box_tree());
          let mut transition_state = TransitionState::update_for_style_change(
            prev_state,
            prev_box_tree,
            prepared.box_tree(),
            now_ms,
          );
          transition_state.capture_layout_from_fragment_tree(&prepared.fragment_tree);
          prepared.fragment_tree.transition_state = Some(Box::new(transition_state));
        }
      }

      self.prepared = Some(prepared);
      self.last_dom_mapping = Some(snapshot);
    }

    let frame =
      self.paint_from_cache_frame_with_deadline_and_animation_time(None, resolved_animation_time)?;
    let pixmap = frame.pixmap;

    // Clear flags only when a render was requested due to invalidation.
    if self.is_dirty() {
      self.clear_dirty();
    }

    Ok(pixmap)
  }

  /// Updates the device pixel ratio used for media queries and resolution-dependent resources.
  ///
  /// Non-finite or non-positive values clear the override (falling back to the renderer default).
  /// Changing DPR invalidates layout+paint.
  pub fn set_device_pixel_ratio(&mut self, dpr: f32) {
    let sanitized = super::sanitize_scale(Some(dpr));
    if sanitized != self.options.device_pixel_ratio {
      self.options.device_pixel_ratio = sanitized;
      self.layout_dirty = true;
      self.paint_dirty = true;
    }
  }

  /// Returns true when style/layout must be recomputed before painting.
  pub fn needs_layout(&self) -> bool {
    self.prepared.is_none() || self.style_dirty || self.layout_dirty
  }

  /// Paints the most recently laid-out document without re-running style/layout.
  pub fn paint_from_cache_frame_with_deadline(
    &mut self,
    deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<super::PaintedFrame> {
    let animation_time = self.resolve_animation_time_ms();
    self.paint_from_cache_frame_with_deadline_and_animation_time(deadline, animation_time)
  }

  fn paint_from_cache_frame_with_deadline_and_animation_time(
    &mut self,
    deadline: Option<&crate::render_control::RenderDeadline>,
    animation_time: Option<f32>,
  ) -> Result<super::PaintedFrame> {
    let Some(prepared) = self.prepared.as_ref() else {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocument2 has no cached layout; call render_frame() first".to_string(),
      }));
    };

    let _deadline_guard = if let Some(deadline) = deadline {
      Some(crate::render_control::DeadlineGuard::install(Some(deadline)))
    } else {
      let deadline_enabled = self.options.timeout.is_some() || self.options.cancel_callback.is_some();
      deadline_enabled.then(|| {
        let options_deadline =
          crate::render_control::RenderDeadline::new(self.options.timeout, self.options.cancel_callback.clone());
        crate::render_control::DeadlineGuard::install(Some(&options_deadline))
      })
    };
    crate::render_control::check_active(RenderStage::Paint).map_err(Error::Render)?;

    let scroll_state = ScrollState::from_parts(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
    );
    let paint_options = PreparedPaintOptions {
      scroll: Some(scroll_state),
      viewport: None,
      background: None,
      animation_time,
    };

    let frame = if animation_time.is_some() {
      prepared.paint_with_options_frame_with_animation_state_store(
        paint_options,
        &mut self.animation_state_store,
      )?
    } else {
      prepared.paint_with_options_frame(paint_options)?
    };

    self.options.scroll_x = frame.scroll_state.viewport.x;
    self.options.scroll_y = frame.scroll_state.viewport.y;
    self.options.element_scroll_offsets = frame.scroll_state.elements.clone();
    self.paint_dirty = false;

    Ok(frame)
  }

  pub fn paint_from_cache_with_deadline(
    &mut self,
    deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<super::Pixmap> {
    Ok(self.paint_from_cache_frame_with_deadline(deadline)?.pixmap)
  }

  fn resolve_animation_time_ms(&mut self) -> Option<f32> {
    let manual = super::sanitize_animation_time_ms(self.options.animation_time);
    if manual.is_some() {
      return manual;
    }
    if !self.realtime_animations_enabled {
      return None;
    }

    let now = self.animation_clock.now();
    let origin = *self.animation_timeline_origin.get_or_insert(now);
    let elapsed = now.checked_sub(origin).unwrap_or(Duration::ZERO);
    let time_ms = (elapsed.as_secs_f64() * 1000.0).min(f32::MAX as f64) as f32;
    Some(time_ms)
  }

  fn prepare_dom_with_options(&mut self, dom: &DomNode) -> Result<PreparedDocument> {
    let options = self.options.clone();
    let renderer = &mut self.renderer;

    let toggles = renderer.resolve_runtime_toggles(&options);
    let _toggles_guard =
      super::RuntimeTogglesSwap::new(&mut renderer.runtime_toggles, toggles.clone());
    crate::debug::runtime::with_runtime_toggles(toggles, || {
      let trace = super::TraceSession::from_options(Some(&options));
      let trace_handle = trace.handle();
      let _root_span = trace_handle.span("browser_document2_prepare", "pipeline");

      let shared_diagnostics =
        renderer
          .diagnostics
          .as_ref()
          .map(|diag| super::SharedRenderDiagnostics {
            inner: std::sync::Arc::clone(diag),
          });
      let context = Some(renderer.build_resource_context(
        renderer.document_url_hint(),
        shared_diagnostics,
        ReferrerPolicy::default(),
      ));
      let (prev_self, prev_image, prev_layout_image, prev_font) =
        renderer.push_resource_context(context);
      let result =
        super::browser_document::prepare_dom_inner(renderer, dom, options.clone(), trace_handle);
      renderer.pop_resource_context(prev_self, prev_image, prev_layout_image, prev_font);
      drop(_root_span);
      trace.finalize(result)
    })
  }

  #[inline]
  fn invalidate_all(&mut self) {
    self.style_dirty = true;
    self.layout_dirty = true;
    self.paint_dirty = true;
  }

  #[inline]
  fn clear_dirty(&mut self) {
    self.style_dirty = false;
    self.layout_dirty = false;
    self.paint_dirty = false;
  }

  #[inline]
  fn is_dirty(&self) -> bool {
    self.style_dirty || self.layout_dirty || self.paint_dirty
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::js::clock::VirtualClock;
  use std::sync::Once;

  static INIT_ENV: Once = Once::new();

  fn ensure_test_env() {
    INIT_ENV.call_once(|| {
      // FastRender uses Rayon for parallel layout/paint. Rayon defaults to the host CPU count, which
      // can exceed sandbox thread budgets and cause the global pool init to fail.
      if std::env::var("RAYON_NUM_THREADS").is_err() {
        std::env::set_var("RAYON_NUM_THREADS", "1");
      }
    });
  }

  fn pixel(pixmap: &super::super::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let px = pixmap.pixel(x, y).unwrap();
    (px.red(), px.green(), px.blue(), px.alpha())
  }

  fn fixture_html() -> &'static str {
    r#"
      <style>
        html, body { margin: 0; background: rgb(0, 0, 0); }
        #box {
          width: 1px;
          height: 1px;
          background: rgb(255, 0, 0);
          animation: fade 1000ms linear forwards;
        }
        @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
      </style>
      <div id="box"></div>
    "#
  }

  #[test]
  fn realtime_animation_sampling_progresses_with_virtual_clock() -> Result<()> {
    ensure_test_env();
    let mut doc =
      BrowserDocument2::from_html(fixture_html(), RenderOptions::new().with_viewport(2, 2))?;

    let clock = Arc::new(VirtualClock::new());
    doc.set_animation_clock(clock.clone());
    doc.set_realtime_animations_enabled(true);

    let pixmap_0 = doc.render_frame()?;
    assert_eq!(pixel(&pixmap_0, 0, 0), (0, 0, 0, 255));

    clock.advance(Duration::from_millis(500));
    let pixmap_500 = doc.render_frame()?;
    let (r, g, b, a) = pixel(&pixmap_500, 0, 0);
    assert!(
      (120..=135).contains(&r),
      "expected ~50% blended red at 500ms, got rgba=({r},{g},{b},{a})"
    );
    assert_eq!((g, b, a), (0, 0, 255));

    Ok(())
  }

  #[test]
  fn realtime_animation_play_state_pauses_and_resumes_across_frames() -> Result<()> {
    ensure_test_env();
    let mut doc =
      BrowserDocument2::from_html(fixture_html(), RenderOptions::new().with_viewport(2, 2))?;

    let clock = Arc::new(VirtualClock::new());
    doc.set_animation_clock(clock.clone());
    doc.set_realtime_animations_enabled(true);

    let box_id = doc
      .dom_mut()
      .query_selector("#box", None)
      .unwrap()
      .expect("fixture box element");

    let _ = doc.render_frame()?;

    clock.advance(Duration::from_millis(500));
    let pixmap_mid = doc.render_frame()?;
    let (mid_r, mid_g, mid_b, mid_a) = pixel(&pixmap_mid, 0, 0);
    assert!(
      (120..=135).contains(&mid_r),
      "expected ~50% blended red at 500ms, got rgba=({mid_r},{mid_g},{mid_b},{mid_a})"
    );

    {
      let dom = doc.dom_mut();
      dom
        .set_attribute(box_id, "style", "animation-play-state: paused;")
        .unwrap();
    }

    let pixmap_paused = doc.render_frame()?;
    let (paused_r, paused_g, paused_b, paused_a) = pixel(&pixmap_paused, 0, 0);
    assert!(
      (120..=135).contains(&paused_r),
      "expected paused animation to hold at ~50% opacity, got rgba=({paused_r},{paused_g},{paused_b},{paused_a})"
    );

    clock.advance(Duration::from_millis(500));
    let pixmap_paused_late = doc.render_frame()?;
    let (paused_r, paused_g, paused_b, paused_a) = pixel(&pixmap_paused_late, 0, 0);
    assert!(
      (120..=135).contains(&paused_r),
      "expected paused animation to hold at ~50% opacity, got rgba=({paused_r},{paused_g},{paused_b},{paused_a})"
    );

    {
      let dom = doc.dom_mut();
      dom
        .set_attribute(box_id, "style", "animation-play-state: running;")
        .unwrap();
    }

    let pixmap_resumed = doc.render_frame()?;
    let (resumed_r, resumed_g, resumed_b, resumed_a) = pixel(&pixmap_resumed, 0, 0);
    assert!(
      (120..=135).contains(&resumed_r),
      "expected resumed animation to continue from ~50% opacity, got rgba=({resumed_r},{resumed_g},{resumed_b},{resumed_a})"
    );

    clock.advance(Duration::from_millis(500));
    let pixmap_end = doc.render_frame()?;
    assert_eq!(pixel(&pixmap_end, 0, 0), (255, 0, 0, 255));

    Ok(())
  }
}
