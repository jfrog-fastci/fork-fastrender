use crate::animation::TransitionState;
use crate::dom::DomNode;
use crate::dom2::{Document, RendererDomSnapshot};
use crate::error::{Error, RenderError, RenderStage, Result};
use crate::geometry::{Point, Rect};
use crate::clock::{Clock, RealClock};
use crate::resource::ReferrerPolicy;
use crate::scroll::ScrollState;

use super::{PreparedDocument, PreparedPaintOptions, RenderOptions};
use std::sync::Arc;
use std::time::Duration;

/// Mutable, multi-frame renderer backed by a `dom2` live DOM tree.
///
/// `BrowserDocument2` owns a [`super::FastRender`] instance and a live [`Document`]. DOM mutations
/// invalidate the cached style/layout/paint results, and the next call to
/// [`BrowserDocument2::render_if_needed`] recomputes the pipeline once, coalescing all intermediate
/// changes.
///
/// This type does **not** execute JavaScript and does not include an HTML event loop. For a
/// JS-capable runtime, use [`super::BrowserTab`].
pub struct BrowserDocument2 {
  renderer: super::FastRender,
  dom: Document,
  options: RenderOptions,
  prepared: Option<PreparedDocument>,
  last_dom_mapping: Option<RendererDomSnapshot>,
  last_painted_scroll_state: Option<ScrollState>,
  paint_damage: Option<Rect>,
  last_incremental_paint_report: Option<super::IncrementalPaintReport>,
  animation_clock: Arc<dyn Clock>,
  realtime_animations_enabled: bool,
  animation_timeline_origin: Option<Duration>,
  last_painted_animation_clock: Option<Duration>,
  last_painted_animation_time: Option<f32>,
  animation_state_store: crate::animation::AnimationStateStore,
  style_dirty: bool,
  layout_dirty: bool,
  paint_dirty: bool,
  #[cfg(test)]
  paint_into_resize_count: u64,
}

impl BrowserDocument2 {
  /// Creates a new live `dom2` document from an HTML string using a fresh renderer.
  pub fn from_html(html: &str, options: RenderOptions) -> Result<Self> {
    Self::new(super::FastRender::new()?, html, options)
  }

  /// Creates a new live `dom2` document from an HTML string using the provided renderer.
  pub fn new(renderer: super::FastRender, html: &str, options: RenderOptions) -> Result<Self> {
    // Install a scoped render deadline so HTML parsing honors `RenderOptions::{timeout,cancel_callback}`.
    // This keeps behavior consistent with document preparation and allows callers (e.g. browser UI
    // workers) to cancel expensive HTML parsing when a navigation is superseded.
    let deadline_enabled = options.timeout.is_some() || options.cancel_callback.is_some();
    let dom = if deadline_enabled {
      let deadline = crate::render_control::RenderDeadline::new(
        options.timeout,
        options.cancel_callback.clone(),
      );
      let _guard = crate::render_control::DeadlineGuard::install(Some(&deadline));
      renderer.parse_html(html)?
    } else {
      renderer.parse_html(html)?
    };
    let dom = Document::from_renderer_dom(&dom);
    let paint_damage = {
      let (viewport_w, viewport_h) = options
        .viewport
        .unwrap_or((renderer.default_width, renderer.default_height));
      let scale = options
        .device_pixel_ratio
        .unwrap_or(renderer.device_pixel_ratio)
        .max(f32::EPSILON);
      let device_w = ((viewport_w as f32) * scale).round().max(1.0);
      let device_h = ((viewport_h as f32) * scale).round().max(1.0);
      Some(Rect::from_xywh(0.0, 0.0, device_w, device_h))
    };
    Ok(Self {
      renderer,
      dom,
      options,
      prepared: None,
      last_dom_mapping: None,
      last_painted_scroll_state: None,
      paint_damage,
      last_incremental_paint_report: None,
      animation_clock: Arc::new(RealClock::default()),
      realtime_animations_enabled: false,
      animation_timeline_origin: None,
      last_painted_animation_clock: None,
      last_painted_animation_time: None,
      animation_state_store: crate::animation::AnimationStateStore::default(),
      // First frame needs a full pipeline run.
      style_dirty: true,
      layout_dirty: true,
      paint_dirty: true,
      #[cfg(test)]
      paint_into_resize_count: 0,
    })
  }

  /// Fetches and prepares a URL using the internal renderer, replacing the live `dom2` document
  /// in-place.
  pub fn navigate_url(
    &mut self,
    url: &str,
    options: RenderOptions,
  ) -> Result<super::BrowserNavigationReport> {
    let prev_document_url = self.renderer.document_url.clone();
    let prev_base_url = self.renderer.base_url.clone();
    let super::PreparedDocumentReport {
      document,
      diagnostics,
      final_url,
      base_url,
    } = match self.renderer.prepare_url(url, options.clone()) {
      Ok(report) => report,
      Err(err) => {
        // Restore URL hints so cancellation/errors don't perturb the currently committed document.
        self.renderer.document_url = prev_document_url;
        match prev_base_url {
          Some(url) => self.renderer.set_base_url(url),
          None => self.renderer.clear_base_url(),
        }
        return Err(err);
      }
    };

    self.dom = Document::from_renderer_dom(&document.dom);
    self.options = options;
    self.prepared = Some(document);
    self.last_dom_mapping = None;
    self.last_painted_scroll_state = None;
    self.last_incremental_paint_report = None;
    self.style_dirty = false;
    self.layout_dirty = false;
    self.paint_dirty = true;
    self.mark_full_paint_damage();
    self.animation_timeline_origin = None;
    self.last_painted_animation_clock = None;
    self.last_painted_animation_time = None;
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
    self.last_painted_animation_clock = None;
    self.last_painted_animation_time = None;
    self.paint_dirty = true;
    self.mark_full_paint_damage();
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
      self.last_painted_animation_clock = None;
      self.last_painted_animation_time = None;
      self.mark_full_paint_damage();
    }
    self.realtime_animations_enabled = enabled;
  }

  pub fn needs_animation_frame(&self) -> bool {
    if self.options.animation_time.is_some() || !self.realtime_animations_enabled {
      return false;
    }
    let Some(last) = self.last_painted_animation_clock else {
      return self.prepared.is_some();
    };
    self.animation_clock.now() != last
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
      self.mark_full_paint_damage();
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

  /// Returns the incremental-paint report recorded for the most recent `*_into` paint entrypoint.
  ///
  /// This is primarily intended for embedders and tests that need visibility into whether partial
  /// repaint / scroll-blit optimizations were used or conservatively disabled.
  pub fn last_incremental_paint_report(&self) -> Option<super::IncrementalPaintReport> {
    self.last_incremental_paint_report
  }

  /// Renders a new frame if anything has been invalidated since the last successful frame.
  ///
  /// Returns `Ok(None)` when no dirty flags are set.
  pub fn render_if_needed(&mut self) -> Result<Option<super::Pixmap>> {
    Ok(
      self
        .render_if_needed_with_deadlines(None)?
        .map(|frame| frame.pixmap),
    )
  }

  /// Renders a new frame if anything has been invalidated since the last successful frame,
  /// applying an optional deadline to the *paint* phase.
  ///
  /// This mirrors [`super::BrowserDocument::render_if_needed_with_deadlines`] for the `dom2`
  /// backed document variant.
  pub fn render_if_needed_with_deadlines(
    &mut self,
    paint_deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<Option<super::PaintedFrame>> {
    if !self.is_dirty() && self.prepared.is_some() && !self.needs_animation_frame() {
      return Ok(None);
    }
    let frame = self.render_frame_with_deadlines(paint_deadline)?;
    Ok(Some(frame))
  }

  /// Renders one frame.
  ///
  /// If the document is dirty, this triggers a full pipeline run. Otherwise, it repaints from
  /// cached layout artifacts.
  pub fn render_frame(&mut self) -> Result<super::Pixmap> {
    Ok(self.render_frame_with_deadlines(None)?.pixmap)
  }

  /// Renders one frame into a caller-supplied pixmap buffer.
  ///
  /// This mirrors [`BrowserDocument2::render_frame`], but writes into `output` so embeddings can
  /// reuse pixel buffers across frames.
  pub fn render_frame_into(&mut self, output: &mut Option<super::Pixmap>) -> Result<ScrollState> {
    self.render_frame_with_deadlines_into(output, None)
  }

  /// Like [`BrowserDocument2::render_frame_into`], but applies an optional deadline to the paint
  /// phase.
  pub fn render_frame_with_deadlines_into(
    &mut self,
    output: &mut Option<super::Pixmap>,
    paint_deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<ScrollState> {
    self.record_incremental_paint_report();
    let frame = self.render_frame_with_deadlines(paint_deadline)?;
    self.copy_frame_into_pixmap(&frame.pixmap, output)?;
    Ok(frame.scroll_state)
  }

  /// Renders a new frame if anything has been invalidated since the last successful frame,
  /// writing pixels into `output` when a repaint occurs.
  ///
  /// Returns `Ok(None)` when no dirty flags are set.
  pub fn render_if_needed_into(
    &mut self,
    output: &mut Option<super::Pixmap>,
  ) -> Result<Option<ScrollState>> {
    self.render_if_needed_with_deadlines_into(output, None)
  }

  /// Like [`BrowserDocument2::render_if_needed_into`], but applies an optional deadline to the
  /// paint phase.
  pub fn render_if_needed_with_deadlines_into(
    &mut self,
    output: &mut Option<super::Pixmap>,
    paint_deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<Option<ScrollState>> {
    let Some(frame) = self.render_if_needed_with_deadlines(paint_deadline)? else {
      return Ok(None);
    };
    self.record_incremental_paint_report();
    self.copy_frame_into_pixmap(&frame.pixmap, output)?;
    Ok(Some(frame.scroll_state))
  }

  /// Renders one frame, applying an optional deadline to the *paint* phase.
  ///
  /// When layout is required, prepare/layout is executed using the currently configured
  /// `RenderOptions::{timeout,cancel_callback}`, then painting proceeds under `paint_deadline`.
  pub fn render_frame_with_deadlines(
    &mut self,
    paint_deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<super::PaintedFrame> {
    // If we haven't rendered before, force a full pipeline run even if the flags were cleared.
    if self.prepared.is_none() {
      self.invalidate_all();
    }

    let resolved_animation_time = self.resolve_animation_time_ms();

    let needs_layout = self.style_dirty || self.layout_dirty;
    if needs_layout {
      let prev_prepared = self.prepared.take();

      let mut snapshot = self.dom.to_renderer_dom_with_mapping();
      self
        .dom
        .project_form_control_state_into_renderer_dom_snapshot(&mut snapshot.dom, &snapshot.mapping);
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
          prepared.fragment_tree.transition_state = Some(Arc::new(transition_state));
        }
      }

      if let Some(prev_prepared) = prev_prepared.as_ref() {
        let old_scroll_state = ScrollState::from_parts_with_deltas(
          Point::new(self.options.scroll_x, self.options.scroll_y),
          self.options.element_scroll_offsets.clone(),
          self.options.scroll_delta,
          self.options.element_scroll_deltas.clone(),
        );
        let anchored = crate::scroll::apply_scroll_anchoring_between_trees(
          prev_prepared.fragment_tree(),
          prepared.fragment_tree(),
          &old_scroll_state,
          prepared.layout_viewport(),
        );
        self.options.scroll_x = anchored.viewport.x;
        self.options.scroll_y = anchored.viewport.y;
        self.options.element_scroll_offsets = anchored.elements.clone();
        self.options.scroll_delta = anchored.viewport_delta;
        self.options.element_scroll_deltas = anchored.elements_delta.clone();
      }

      self.prepared = Some(prepared);
      self.last_dom_mapping = Some(snapshot);
      // We now have fresh style/layout artifacts stored in `self.prepared`, even if the subsequent
      // paint step is cancelled or fails. Clear the layout dirtiness so callers can retry paint
      // from cache without re-running cascade/layout.
      self.style_dirty = false;
      self.layout_dirty = false;
      // Layout changes always require a paint attempt. Keep paint marked dirty so a cancelled paint
      // can be retried.
      self.paint_dirty = true;
    }

    let frame = self.paint_from_cache_frame_with_deadline_and_animation_time(
      paint_deadline,
      resolved_animation_time,
    )?;

    // Clear flags only when a render was requested due to invalidation.
    if self.is_dirty() {
      self.clear_dirty();
    }

    Ok(frame)
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
      self.mark_full_paint_damage();
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
      Some(crate::render_control::DeadlineGuard::install(Some(
        deadline,
      )))
    } else {
      let deadline_enabled =
        self.options.timeout.is_some() || self.options.cancel_callback.is_some();
      deadline_enabled.then(|| {
        let options_deadline = crate::render_control::RenderDeadline::new(
          self.options.timeout,
          self.options.cancel_callback.clone(),
        );
        crate::render_control::DeadlineGuard::install(Some(&options_deadline))
      })
    };
    crate::render_control::check_active(RenderStage::Paint).map_err(Error::Render)?;

    let scroll_state = ScrollState::from_parts_with_deltas(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
      self.options.scroll_delta,
      self.options.element_scroll_deltas.clone(),
    );
    let paint_options = PreparedPaintOptions {
      scroll: Some(scroll_state),
      viewport: None,
      background: None,
      animation_time,
      media_provider: None,
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
    self.options.scroll_delta = frame.scroll_state.viewport_delta;
    self.options.element_scroll_deltas = frame.scroll_state.elements_delta.clone();
    self.last_painted_scroll_state = Some(frame.scroll_state.clone());
    self.paint_damage = None;
    self.paint_dirty = false;
    if self.realtime_animations_enabled && self.options.animation_time.is_none() {
      self.last_painted_animation_clock = Some(self.animation_clock.now());
    } else {
      self.last_painted_animation_clock = None;
    }
    self.last_painted_animation_time = animation_time;

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
    crate::debug::runtime::with_thread_runtime_toggles(toggles, || {
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
      let result = super::browser_document::prepare_dom_inner(
        renderer,
        dom,
        options.clone(),
        trace_handle,
        None,
        None,
      );
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
    self.last_painted_scroll_state = None;
    self.last_incremental_paint_report = None;
    self.mark_full_paint_damage();
  }

  #[inline]
  fn clear_dirty(&mut self) {
    self.style_dirty = false;
    self.layout_dirty = false;
    self.paint_dirty = false;
    self.paint_damage = None;
  }

  #[inline]
  fn is_dirty(&self) -> bool {
    self.style_dirty || self.layout_dirty || self.paint_dirty
  }

  fn mark_full_paint_damage(&mut self) {
    let (viewport_w, viewport_h) = self
      .options
      .viewport
      .unwrap_or((self.renderer.default_width, self.renderer.default_height));
    let scale = self
      .options
      .device_pixel_ratio
      .unwrap_or(self.renderer.device_pixel_ratio)
      .max(f32::EPSILON);
    let device_w = ((viewport_w as f32) * scale).round().max(1.0);
    let device_h = ((viewport_h as f32) * scale).round().max(1.0);
    self.paint_damage = Some(Rect::from_xywh(0.0, 0.0, device_w, device_h));
  }

  fn copy_frame_into_pixmap(
    &mut self,
    source: &super::Pixmap,
    output: &mut Option<super::Pixmap>,
  ) -> Result<()> {
    let expected_w = source.width();
    let expected_h = source.height();

    let needs_resize = match output.as_ref() {
      Some(pixmap) => pixmap.width() != expected_w || pixmap.height() != expected_h,
      None => true,
    };
    if needs_resize {
      #[cfg(test)]
      {
        self.paint_into_resize_count = self.paint_into_resize_count.saturating_add(1);
      }

      *output = Some(
        crate::paint::pixmap::new_pixmap_with_context(
          expected_w,
          expected_h,
          "BrowserDocument2::render_frame_into",
        )
        .map_err(Error::Render)?,
      );
    }

    let Some(target) = output.as_mut() else {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocument2::render_frame_into: missing output pixmap".to_string(),
      }));
    };

    let src = source.data();
    let dst = target.data_mut();
    if src.len() != dst.len() {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: format!(
          "BrowserDocument2::render_frame_into: byte length mismatch (src={} dst={})",
          src.len(),
          dst.len()
        ),
      }));
    }
    dst.copy_from_slice(src);
    Ok(())
  }

  fn record_incremental_paint_report(&mut self) {
    use crate::paint::painter::{paint_backend_from_env, PaintBackend};

    // Incremental paint fast paths (partial repaint + scroll blit) rely on the display-list
    // renderer. If the runtime paint backend is legacy/immediate mode, we must not attempt to blit
    // and then repaint only exposed strips: the legacy backend cannot repaint subregions from a
    // cached display list.
    let backend = if let Some(prepared) = self.prepared.as_ref() {
      crate::debug::runtime::with_thread_runtime_toggles(
        Arc::clone(&prepared.runtime_toggles),
        paint_backend_from_env,
      )
    } else {
      crate::debug::runtime::with_thread_runtime_toggles(
        self.renderer.resolve_runtime_toggles(&self.options),
        paint_backend_from_env,
      )
    };

    self.last_incremental_paint_report = Some(super::IncrementalPaintReport {
      incremental_used: false,
      disabled_reason: (backend != PaintBackend::DisplayList)
        .then_some(super::IncrementalPaintDisabledReason::PaintBackendLegacy),
    });
  }
}

impl crate::js::DomHost for BrowserDocument2 {
  fn with_dom<R, F>(&self, f: F) -> R
  where
    F: FnOnce(&Document) -> R,
  {
    f(self.dom())
  }

  fn mutate_dom<R, F>(&mut self, f: F) -> R
  where
    F: FnOnce(&mut Document) -> (R, bool),
  {
    let (result, changed) = f(&mut self.dom);
    if changed {
      self.invalidate_all();
    }
    result
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::clock::VirtualClock;

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
    let options = RenderOptions::new()
      .with_viewport(2, 2)
      .with_layout_parallelism(crate::LayoutParallelism::disabled())
      .with_paint_parallelism(crate::PaintParallelism::disabled());
    let mut doc = BrowserDocument2::from_html(fixture_html(), options)?;

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
  fn render_if_needed_rerenders_for_realtime_animation_progress() -> Result<()> {
    let options = RenderOptions::new()
      .with_viewport(2, 2)
      .with_layout_parallelism(crate::LayoutParallelism::disabled())
      .with_paint_parallelism(crate::PaintParallelism::disabled());
    let mut doc = BrowserDocument2::from_html(fixture_html(), options)?;

    let clock = Arc::new(VirtualClock::new());
    doc.set_animation_clock(clock.clone());
    doc.set_realtime_animations_enabled(true);

    let _ = doc.render_frame()?;
    assert!(
      doc.render_if_needed()?.is_none(),
      "expected render_if_needed to return None before time advances"
    );

    clock.advance(Duration::from_millis(500));
    let pixmap = doc
      .render_if_needed()?
      .expect("expected a new frame after advancing the animation clock");
    let (r, g, b, a) = pixel(&pixmap, 0, 0);
    assert!(
      (120..=135).contains(&r),
      "expected ~50% blended red at 500ms, got rgba=({r},{g},{b},{a})"
    );
    assert_eq!((g, b, a), (0, 0, 255));

    Ok(())
  }

  #[test]
  fn realtime_animation_play_state_pauses_and_resumes_across_frames() -> Result<()> {
    let options = RenderOptions::new()
      .with_viewport(2, 2)
      .with_layout_parallelism(crate::LayoutParallelism::disabled())
      .with_paint_parallelism(crate::PaintParallelism::disabled());
    let mut doc = BrowserDocument2::from_html(fixture_html(), options)?;

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

  fn static_fixture_html() -> &'static str {
    r#"
      <style>
        html, body { margin: 0; background: rgb(0, 0, 0); }
        #box { width: 1px; height: 1px; background: rgb(255, 0, 0); }
      </style>
      <div id="box"></div>
    "#
  }

  #[test]
  fn render_frame_into_matches_render_frame_pixels() -> Result<()> {
    let options = RenderOptions::new()
      .with_viewport(2, 2)
      .with_layout_parallelism(crate::LayoutParallelism::disabled())
      .with_paint_parallelism(crate::PaintParallelism::disabled());
    let mut doc = BrowserDocument2::from_html(static_fixture_html(), options)?;

    let expected = doc.render_frame()?;
    let mut output: Option<super::super::Pixmap> = None;
    let _ = doc.render_frame_into(&mut output)?;
    let actual = output.as_ref().expect("render_frame_into output");

    assert_eq!((actual.width(), actual.height()), (expected.width(), expected.height()));
    assert_eq!(actual.data(), expected.data());
    Ok(())
  }

  #[test]
  fn render_frame_into_reuses_buffer_when_size_is_unchanged() -> Result<()> {
    let options = RenderOptions::new()
      .with_viewport(2, 2)
      .with_layout_parallelism(crate::LayoutParallelism::disabled())
      .with_paint_parallelism(crate::PaintParallelism::disabled());
    let mut doc = BrowserDocument2::from_html(static_fixture_html(), options)?;

    let mut output: Option<super::super::Pixmap> = None;
    let _ = doc.render_frame_into(&mut output)?;
    let first = output.as_ref().expect("output pixmap");
    let first_ptr = first.data().as_ptr();
    let first_resizes = doc.paint_into_resize_count;

    let _ = doc.render_frame_into(&mut output)?;
    let second = output.as_ref().expect("output pixmap");
    let second_ptr = second.data().as_ptr();
    let second_resizes = doc.paint_into_resize_count;

    assert_eq!(first_ptr, second_ptr, "expected output pixmap buffer reuse");
    assert_eq!(second_resizes, first_resizes, "expected no output resize");
    Ok(())
  }
}
