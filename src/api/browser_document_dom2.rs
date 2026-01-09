use crate::animation::TransitionState;
use crate::error::{Error, RenderError, RenderStage, Result};
use crate::geometry::Point;
use crate::js::clock::{Clock, RealClock};
use crate::resource::ReferrerPolicy;
use crate::scroll::ScrollState;
use std::sync::Arc;
use std::time::Duration;

use super::browser_document::prepare_dom_inner;
use super::{PreparedDocument, PreparedPaintOptions, RenderOptions};

/// Mutable, multi-frame renderer backed by a live `dom2` document.
///
/// `BrowserDocumentDom2` mirrors [`super::BrowserDocument`] but stores a spec-ish mutable
/// [`crate::dom2::Document`] as the authoritative DOM (e.g. for JavaScript). The renderer only
/// snapshots the `dom2` document into the renderer's immutable [`crate::dom::DomNode`] form when a
/// layout recomputation is needed.
pub struct BrowserDocumentDom2 {
  renderer: super::FastRender,
  dom: crate::dom2::Document,
  options: RenderOptions,
  prepared: Option<PreparedDocument>,
  animation_state_store: crate::animation::AnimationStateStore,
  style_dirty: bool,
  layout_dirty: bool,
  paint_dirty: bool,
  realtime_animations_enabled: bool,
  animation_clock: Arc<dyn Clock>,
  animation_timeline_origin: Option<Duration>,
}

impl BrowserDocumentDom2 {
  /// Creates a new live document from an HTML string using a fresh renderer.
  pub fn from_html(html: &str, options: RenderOptions) -> Result<Self> {
    Self::new(super::FastRender::new()?, html, options)
  }

  /// Creates a new live document from an HTML string using the provided renderer.
  pub fn new(renderer: super::FastRender, html: &str, options: RenderOptions) -> Result<Self> {
    let dom = renderer.parse_html(html)?;
    let dom = crate::dom2::Document::from_renderer_dom(&dom);
    Ok(Self {
      renderer,
      dom,
      options,
      prepared: None,
      animation_state_store: crate::animation::AnimationStateStore::new(),
      // First frame needs a full pipeline run.
      style_dirty: true,
      layout_dirty: true,
      paint_dirty: true,
      realtime_animations_enabled: false,
      animation_clock: Arc::new(RealClock::default()),
      animation_timeline_origin: None,
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

    self.dom = crate::dom2::Document::from_renderer_dom(&document.dom);
    self.options = options;
    self.prepared = Some(document);
    self.animation_state_store = crate::animation::AnimationStateStore::new();
    self.style_dirty = false;
    self.layout_dirty = false;
    self.paint_dirty = true;
    self.animation_timeline_origin = None;

    Ok(super::BrowserNavigationReport {
      diagnostics,
      final_url,
      base_url,
    })
  }

  /// Overrides the clock used to derive the document timeline for real-time animation sampling.
  ///
  /// This resets the timeline origin so the next frame starts at 0ms (when real-time sampling is
  /// enabled).
  pub fn set_animation_clock(&mut self, clock: Arc<dyn Clock>) {
    self.animation_clock = clock;
    self.animation_timeline_origin = None;
    self.animation_state_store = crate::animation::AnimationStateStore::new();
  }

  /// Enables/disables real-time animation sampling based on this document's timeline.
  ///
  /// When enabled and `RenderOptions.animation_time` is `None`, each paint call samples CSS
  /// animations/transitions at the time elapsed since the first rendered frame after enabling.
  pub fn set_realtime_animations_enabled(&mut self, enabled: bool) {
    if enabled && !self.realtime_animations_enabled {
      self.realtime_animations_enabled = true;
      self.animation_timeline_origin = None;
      self.animation_state_store = crate::animation::AnimationStateStore::new();
    } else if !enabled && self.realtime_animations_enabled {
      self.realtime_animations_enabled = false;
      self.animation_timeline_origin = None;
      self.animation_state_store = crate::animation::AnimationStateStore::new();
    }
  }

  /// Returns an immutable reference to the live `dom2` document.
  pub fn dom(&self) -> &crate::dom2::Document {
    &self.dom
  }

  /// Returns a mutable reference to the live `dom2` document, marking the document dirty.
  ///
  /// MVP invalidation: any mutation to the DOM is treated as a full structural/style change.
  pub fn dom_mut(&mut self) -> &mut crate::dom2::Document {
    self.invalidate_all();
    &mut self.dom
  }

  /// Mutates the DOM tree, marking the document dirty only when `f` reports that it changed
  /// something.
  ///
  /// MVP invalidation: any mutation to the DOM is treated as a full structural/style change.
  pub fn mutate_dom<F>(&mut self, f: F) -> bool
  where
    F: FnOnce(&mut crate::dom2::Document) -> bool,
  {
    let changed = f(&mut self.dom);
    if changed {
      self.invalidate_all();
    }
    changed
  }

  /// Updates the viewport size (in CSS px), marking layout+paint dirty.
  pub fn set_viewport(&mut self, width: u32, height: u32) {
    self.options.viewport = Some((width, height));
    self.layout_dirty = true;
    self.paint_dirty = true;
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

  /// Updates the viewport scroll offset (in CSS px), marking paint dirty.
  pub fn set_scroll(&mut self, scroll_x: f32, scroll_y: f32) {
    self.options.scroll_x = scroll_x;
    self.options.scroll_y = scroll_y;
    self.paint_dirty = true;
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

  /// Convenience wrapper for [`BrowserDocumentDom2::set_animation_time`] with a concrete timestamp.
  pub fn set_animation_time_ms(&mut self, time_ms: f32) {
    self.set_animation_time(Some(time_ms));
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

    let needs_layout = self.style_dirty || self.layout_dirty;
    if needs_layout {
      let prev_prepared = self.prepared.take();
      let mut prepared = match self.prepare_dom_with_options() {
        Ok(prepared) => prepared,
        Err(err) => {
          self.prepared = prev_prepared;
          return Err(err);
        }
      };

      let now_ms = super::sanitize_animation_time_ms(self.animation_time_for_paint());
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
    }

    let pixmap = self.paint_from_cache_with_deadline(None)?;

    // Clear flags only when a render was requested due to invalidation.
    if self.is_dirty() {
      self.clear_dirty();
    }

    Ok(pixmap)
  }

  /// Paints the most recently laid-out document without re-running style/layout.
  ///
  /// This mirrors [`super::BrowserDocument::paint_from_cache_frame_with_deadline`] but operates on
  /// the `dom2`-backed document. Callers should check [`BrowserDocumentDom2::needs_layout`] first
  /// and fall back to [`BrowserDocumentDom2::render_frame`] when layout is required.
  pub fn paint_from_cache_frame_with_deadline(
    &mut self,
    deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<super::PaintedFrame> {
    if self.prepared.is_none() {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocumentDom2 has no cached layout; call render_frame() first".to_string(),
      }));
    };
    let animation_time = self.animation_time_for_paint();
    let prepared = self
      .prepared
      .as_ref()
      .expect("prepared checked by early return");

    // Prefer an explicitly provided deadline; otherwise fall back to this document's configured
    // `RenderOptions::{timeout,cancel_callback}`.
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
    // Perform an early cancellation check so callers can deterministically abort repaints without
    // relying on deep paint loops to periodically poll deadlines.
    crate::render_control::check_active(RenderStage::Paint).map_err(Error::Render)?;

    let scroll_state = ScrollState::from_parts(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
    );
    let frame = prepared.paint_with_options_frame_with_animation_state_store(
      PreparedPaintOptions {
        scroll: Some(scroll_state),
        viewport: None,
        background: None,
        animation_time,
      },
      &mut self.animation_state_store,
    )?;

    // Keep our internal scroll model synchronized with any adjustments made during painting (e.g.
    // scroll snap/clamp). This must not mark the document dirty because the frame we just painted
    // already reflects this state.
    self.options.scroll_x = frame.scroll_state.viewport.x;
    self.options.scroll_y = frame.scroll_state.viewport.y;
    self.options.element_scroll_offsets = frame.scroll_state.elements.clone();

    // A successful paint always satisfies any outstanding paint invalidation, but must not clear
    // pending style/layout dirtiness.
    self.paint_dirty = false;
    Ok(frame)
  }

  pub fn paint_from_cache_with_deadline(
    &mut self,
    deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<super::Pixmap> {
    Ok(self.paint_from_cache_frame_with_deadline(deadline)?.pixmap)
  }

  fn animation_time_for_paint(&mut self) -> Option<f32> {
    if self.options.animation_time.is_some() {
      return self.options.animation_time;
    }

    if !self.realtime_animations_enabled {
      return None;
    }

    let now = self.animation_clock.now();
    let Some(origin) = self.animation_timeline_origin else {
      self.animation_timeline_origin = Some(now);
      return Some(0.0);
    };
    let elapsed = now.checked_sub(origin).unwrap_or(Duration::ZERO);
    let time_ms = elapsed.as_secs_f64() * 1000.0;
    Some((time_ms.min(f32::MAX as f64)) as f32)
  }

  fn prepare_dom_with_options(&mut self) -> Result<PreparedDocument> {
    let options = self.options.clone();
    let renderer_dom = self.dom.to_renderer_dom();
    let renderer_dom_ref = &renderer_dom;
    let renderer = &mut self.renderer;

    let toggles = renderer.resolve_runtime_toggles(&options);
    let _toggles_guard =
      super::RuntimeTogglesSwap::new(&mut renderer.runtime_toggles, toggles.clone());
    crate::debug::runtime::with_runtime_toggles(toggles, || {
      let trace = super::TraceSession::from_options(Some(&options));
      let trace_handle = trace.handle();
      let _root_span = trace_handle.span("browser_document_dom2_prepare", "pipeline");

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
      let result = prepare_dom_inner(renderer, renderer_dom_ref, options.clone(), trace_handle);
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
  pub fn is_dirty(&self) -> bool {
    self.style_dirty || self.layout_dirty || self.paint_dirty
  }
}

impl crate::js::DomHost for BrowserDocumentDom2 {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&crate::dom2::Document) -> R,
  {
    f(self.dom())
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut crate::dom2::Document) -> (R, bool),
  {
    let mut out: Option<R> = None;
    let _changed = BrowserDocumentDom2::mutate_dom(self, |dom| {
      let (result, changed) = f(dom);
      out = Some(result);
      changed
    });
    out.expect("DomHost::mutate_dom closure did not set a result")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn renderer_for_tests() -> super::super::FastRender {
    super::super::FastRender::builder()
      .font_sources(crate::text::font_db::FontConfig::bundled_only())
      .build()
      .expect("renderer")
  }

  fn assert_channel_close(actual: u8, expected: u8, tolerance: u8) {
    let diff = actual.abs_diff(expected);
    assert!(
      diff <= tolerance,
      "expected channel {expected}±{tolerance}, got {actual} (diff={diff})"
    );
  }

  fn assert_rgb_close(color: tiny_skia::PremultipliedColorU8, expected: u8, tolerance: u8) {
    assert_channel_close(color.red(), expected, tolerance);
    assert_channel_close(color.green(), expected, tolerance);
    assert_channel_close(color.blue(), expected, tolerance);
    assert_eq!(color.alpha(), 255);
  }

  fn first_text_node_id(doc: &crate::dom2::Document) -> Option<crate::dom2::NodeId> {
    let mut stack = vec![doc.root()];
    while let Some(id) = stack.pop() {
      let node = doc.node(id);
      if matches!(node.kind, crate::dom2::NodeKind::Text { .. }) {
        return Some(id);
      }
      for &child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    None
  }

  #[test]
  fn render_if_needed_returns_none_when_clean() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");

    assert!(doc.render_if_needed().unwrap().is_some());
    assert!(doc.render_if_needed().unwrap().is_none());
  }

  #[test]
  fn multiple_dom_mutations_coalesce_into_single_layout() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");

    // First render clears dirty flags.
    doc.render_frame().unwrap();

    {
      let dom = doc.dom_mut();
      if let Some(text_id) = first_text_node_id(dom) {
        let node = dom.node_mut(text_id);
        if let crate::dom2::NodeKind::Text { content } = &mut node.kind {
          content.clear();
          content.push_str("first");
        }
      }
    }

    {
      let dom = doc.dom_mut();
      if let Some(text_id) = first_text_node_id(dom) {
        let node = dom.node_mut(text_id);
        if let crate::dom2::NodeKind::Text { content } = &mut node.kind {
          content.clear();
          content.push_str("second");
        }
      }
    }

    assert!(doc.render_if_needed().unwrap().is_some());
    assert!(doc.render_if_needed().unwrap().is_none());
  }

  #[test]
  fn mutate_dom_false_does_not_invalidate() {
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(
      renderer,
      "<!doctype html><html><body><div>Hello</div></body></html>",
      RenderOptions::new().with_viewport(32, 32),
    )
    .expect("document");

    doc.render_frame().unwrap();

    let changed = doc.mutate_dom(|_dom| false);
    assert!(!changed);
    assert!(doc.render_if_needed().unwrap().is_none());
  }

  #[test]
  fn realtime_animations_sample_document_timeline_when_enabled() -> Result<()> {
    let renderer = renderer_for_tests();
    let html = r#"
      <style>
        html, body { margin: 0; background: white; }
        #box {
          width: 10px;
          height: 10px;
          background: black;
          animation: fade 1000ms linear forwards;
        }
        @keyframes fade {
          from { opacity: 0; }
          to { opacity: 1; }
        }
      </style>
      <div id="box"></div>
    "#;
    let mut doc = BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(20, 20))?;

    let clock = Arc::new(crate::js::clock::VirtualClock::new());
    doc.set_animation_clock(clock.clone());
    doc.set_realtime_animations_enabled(true);

    let pixmap0 = doc.render_frame()?;
    let c0 = pixmap0.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c0, 255, 0);

    clock.advance(Duration::from_millis(500));
    let sampled = doc.animation_time_for_paint().expect("animation time");
    assert!(
      (sampled - 500.0).abs() <= 0.1,
      "expected document timeline ~500ms, got {sampled}"
    );

    let pixmap1 = doc.render_frame()?;
    let c1 = pixmap1.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c1, 128, 8);

    // Explicit per-render timestamps always override the real-time document timeline.
    doc.set_animation_time_ms(1000.0);
    let pixmap_override = doc.render_frame()?;
    let c2 = pixmap_override.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c2, 0, 0);

    Ok(())
  }

  fn pixel_gray(pixmap: &super::super::Pixmap) -> u8 {
    let px = pixmap.pixel(0, 0).expect("pixel in bounds");
    assert_eq!(px.alpha(), 255, "expected opaque pixel");
    assert_eq!(px.red(), px.green(), "expected grayscale pixel");
    assert_eq!(px.red(), px.blue(), "expected grayscale pixel");
    px.red()
  }

  fn assert_pixel_gray_approx(pixmap: &super::super::Pixmap, expected: u8, tolerance: u8) {
    let actual = pixel_gray(pixmap);
    let delta = actual.abs_diff(expected);
    assert!(
      delta <= tolerance,
      "expected gray ≈{expected}±{tolerance}, got {actual} (Δ={delta})"
    );
  }

  #[test]
  fn realtime_animations_pause_and_resume_across_frames() -> Result<()> {
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; background: white; }
            #a { width: 1px; height: 1px; background: black; animation: fade 1s linear forwards; }
            @keyframes fade { from { opacity: 0; } to { opacity: 1; } }
          </style>
        </head>
        <body>
          <div id="a"></div>
        </body>
      </html>
    "#;
    let renderer = renderer_for_tests();
    let mut doc = BrowserDocumentDom2::new(renderer, html, RenderOptions::new().with_viewport(2, 2))?;

    let clock = Arc::new(crate::js::clock::VirtualClock::new());
    doc.set_animation_clock(clock.clone());
    doc.set_realtime_animations_enabled(true);

    // t=0ms: opacity=0 => background shines through.
    let frame0 = doc.render_frame()?;
    assert_eq!(pixel_gray(&frame0), 255);

    // t=600ms: opacity=0.6 => ~40% white.
    clock.advance(Duration::from_millis(600));
    let frame600 = doc.render_frame()?;
    assert_pixel_gray_approx(&frame600, 102, 4);

    // Pause at t=600ms.
    let node = crate::dom2::get_element_by_id(doc.dom(), "a").expect("#a element");
    doc
      .dom_mut()
      .set_attribute(node, "style", "animation-play-state: paused;")
      .expect("set_attribute");
    let paused600 = doc.render_frame()?;
    assert_pixel_gray_approx(&paused600, 102, 4);

    // Advance time while paused; output should remain frozen.
    clock.advance(Duration::from_millis(300));
    let paused900 = doc.render_frame()?;
    assert_pixel_gray_approx(&paused900, 102, 4);

    // Resume at t=900ms (without advancing time).
    let node = crate::dom2::get_element_by_id(doc.dom(), "a").expect("#a element");
    doc
      .dom_mut()
      .set_attribute(node, "style", "animation-play-state: running;")
      .expect("set_attribute");
    let resumed900 = doc.render_frame()?;
    assert_pixel_gray_approx(&resumed900, 102, 4);

    // t=1000ms: animation should have progressed to 700ms of active time (0.7 opacity).
    clock.advance(Duration::from_millis(100));
    let frame1000 = doc.render_frame()?;
    assert_pixel_gray_approx(&frame1000, 77, 5);

    Ok(())
  }
}
