use crate::animation::TransitionState;
use crate::dom::DomNode;
use crate::error::{Error, RenderError, RenderStage, Result};
use crate::geometry::{Point, Size};
use crate::js::clock::{Clock, RealClock};
use crate::resource::ReferrerPolicy;
use crate::scroll::ScrollState;
use crate::tree::box_tree::BoxTree;
use crate::tree::fragment_tree::FragmentTree;
use std::sync::Arc;
use std::time::Duration;

use super::{
  resolve_viewport, LayoutDocumentOptions, PreparedDocument, PreparedPaintOptions, RenderOptions,
};

/// Result of a navigation performed via [`BrowserDocument::navigate_url`].
#[derive(Debug, Clone)]
pub struct BrowserNavigationReport {
  /// Diagnostics captured while fetching resources.
  pub diagnostics: super::RenderDiagnostics,
  /// Final document URL after redirects, when available.
  pub final_url: Option<String>,
  /// Effective base URL used to resolve relative subresources (after `<base href>`).
  pub base_url: Option<String>,
}

/// Mutable, multi-frame renderer that caches the most recent layout result.
///
/// `BrowserDocument` owns a [`super::FastRender`] instance and a live DOM tree. DOM mutations
/// invalidate the cached style/layout/paint results, and the next call to [`BrowserDocument::render_if_needed`]
/// recomputes the pipeline once, coalescing all intermediate changes.
pub struct BrowserDocument {
  renderer: super::FastRender,
  dom: DomNode,
  options: RenderOptions,
  document_url: Option<String>,
  prepared: Option<PreparedDocument>,
  style_dirty: bool,
  layout_dirty: bool,
  paint_dirty: bool,
  realtime_animations_enabled: bool,
  animation_clock: Arc<dyn Clock>,
  animation_timeline_origin: Option<Duration>,
}

impl BrowserDocument {
  /// Creates a new live document from an HTML string using a fresh renderer.
  pub fn from_html(html: &str, options: RenderOptions) -> Result<Self> {
    Self::new(super::FastRender::new()?, html, options)
  }

  /// Creates a new live document from an HTML string using the provided renderer.
  pub fn new(renderer: super::FastRender, html: &str, options: RenderOptions) -> Result<Self> {
    let dom = renderer.parse_html(html)?;
    // Preserve the renderer's initial document URL hint so later `<base href>` mutations do not
    // accidentally change origin/referrer semantics.
    let document_url = renderer.document_url_hint().map(|url| url.to_string());
    Ok(Self {
      renderer,
      dom,
      options,
      document_url,
      prepared: None,
      // First frame needs a full pipeline run.
      style_dirty: true,
      layout_dirty: true,
      paint_dirty: true,
      realtime_animations_enabled: false,
      animation_clock: Arc::new(RealClock::default()),
      animation_timeline_origin: None,
    })
  }

  /// Creates a new live document from a prepared layout result.
  ///
  /// This constructor avoids reparsing or re-running layout before the first paint, while still
  /// allowing callers to mutate the DOM in-place and re-run the pipeline on demand.
  pub fn from_prepared(
    renderer: super::FastRender,
    prepared: PreparedDocument,
    options: RenderOptions,
  ) -> Result<Self> {
    let dom = prepared.dom.clone();
    let document_url = renderer.document_url_hint().map(|url| url.to_string());
    Ok(Self {
      renderer,
      dom,
      options,
      document_url,
      prepared: Some(prepared),
      style_dirty: false,
      layout_dirty: false,
      // First frame still needs a paint.
      paint_dirty: true,
      realtime_animations_enabled: false,
      animation_clock: Arc::new(RealClock::default()),
      animation_timeline_origin: None,
    })
  }

  /// Overrides the clock used to derive the document timeline for real-time animation sampling.
  ///
  /// This resets the timeline origin so the next frame starts at 0ms (when real-time sampling is
  /// enabled).
  pub fn set_animation_clock(&mut self, clock: Arc<dyn Clock>) {
    self.animation_clock = clock;
    self.animation_timeline_origin = None;
  }

  /// Enables/disables real-time animation sampling based on this document's timeline.
  ///
  /// When enabled and `RenderOptions.animation_time` is `None`, each paint call samples CSS
  /// animations/transitions at the time elapsed since the first rendered frame after enabling.
  pub fn set_realtime_animations_enabled(&mut self, enabled: bool) {
    if enabled && !self.realtime_animations_enabled {
      self.realtime_animations_enabled = true;
      self.animation_timeline_origin = None;
    } else if !enabled && self.realtime_animations_enabled {
      self.realtime_animations_enabled = false;
      self.animation_timeline_origin = None;
    }
  }

  /// Updates the renderer's document/base URL hints for the current navigation.
  ///
  /// - `document_url` is used for referrer/origin semantics (after redirects).
  /// - `base_url` is used for resolving relative URLs (and can be overridden by `<base href>`).
  pub fn set_navigation_urls(&mut self, document_url: Option<String>, base_url: Option<String>) {
    // Normalize empty/whitespace-only inputs so we avoid unnecessary state churn.
    let document_url =
      document_url.and_then(|url| (!super::trim_ascii_whitespace(&url).is_empty()).then_some(url));
    let base_url = match base_url {
      Some(url) if !super::trim_ascii_whitespace(&url).is_empty() => Some(url),
      _ => None,
    };

    if self.renderer.document_url != document_url {
      match document_url {
        Some(url) => self.renderer.set_document_url(url),
        None => self.renderer.clear_document_url(),
      }
    }
    if self.renderer.base_url != base_url {
      match base_url {
        Some(url) => self.renderer.set_base_url(url),
        None => self.renderer.clear_base_url(),
      }
    }
  }

  /// Fetches and prepares a URL using the internal renderer, replacing the live document in-place.
  ///
  /// This enables UIs to keep a long-lived `BrowserDocument` (and its internal caches) across
  /// navigations without constructing a new [`super::FastRender`] instance per load.
  pub fn navigate_url(
    &mut self,
    url: &str,
    options: RenderOptions,
  ) -> Result<BrowserNavigationReport> {
    let super::PreparedDocumentReport {
      document,
      diagnostics,
      final_url,
      base_url,
    } = self.renderer.prepare_url(url, options.clone())?;

    // Keep the renderer's URL hints consistent with the navigation result. (This is typically a
    // no-op because `prepare_url` already updates the hints, but it ensures callers that manually
    // tweaked them don't drift out of sync.)
    self.set_navigation_urls(final_url.clone(), base_url.clone());

    // Preserve the post-navigation document URL hint so later `<base href>` changes do not alter
    // origin/referrer semantics for subsequent resource fetches.
    let stable_document_url = final_url
      .clone()
      .or_else(|| self.renderer.document_url_hint().map(|url| url.to_string()));
    self.set_document_url(stable_document_url);

    // Swap the live DOM while retaining the renderer instance and its caches.
    self.reset_with_prepared(document, options);

    Ok(BrowserNavigationReport {
      diagnostics,
      final_url,
      base_url,
    })
  }

  /// Replaces the live DOM, clears any cached preparation state, and marks the document dirty.
  pub fn reset_with_dom(&mut self, dom: DomNode, options: RenderOptions) {
    self.dom = dom;
    self.options = options;
    self.prepared = None;
    self.animation_timeline_origin = None;
    self.invalidate_all();
  }

  /// Replaces the live DOM with a prepared document's DOM and installs the prepared cache.
  ///
  /// The next `render_if_needed` call will paint using the prepared layout without re-running
  /// cascade/layout.
  pub fn reset_with_prepared(&mut self, prepared: PreparedDocument, options: RenderOptions) {
    self.dom = prepared.dom.clone();
    self.options = options;
    self.prepared = Some(prepared);
    self.style_dirty = false;
    self.layout_dirty = false;
    self.paint_dirty = true;
    self.animation_timeline_origin = None;
  }

  /// Parses HTML using the internal renderer and resets the document state.
  pub fn reset_with_html(&mut self, html: &str, options: RenderOptions) -> Result<()> {
    let dom = self.renderer.parse_html(html)?;
    self.reset_with_dom(dom, options);
    Ok(())
  }

  /// Navigates this document to a URL using the internal renderer and installs the prepared cache.
  ///
  /// This is intended for browser-UI integrations that want to keep a stable renderer instance per
  /// tab (sharing caches/fetcher) while swapping out the live DOM on navigation.
  ///
  /// Returns `(committed_url, base_url)` where:
  /// - `committed_url` is the final URL after redirects when available (falls back to the input).
  /// - `base_url` is the effective base used for resolving relative URLs (falls back to
  ///   `committed_url` when absent).
  pub fn navigate_url_with_options(
    &mut self,
    url: &str,
    options: RenderOptions,
  ) -> Result<(String, String)> {
    let report = self.renderer.prepare_url(url, options.clone())?;

    let committed_url = report.final_url.clone().unwrap_or_else(|| url.to_string());
    let base_url = report
      .base_url
      .clone()
      .filter(|base| !super::trim_ascii_whitespace(base).is_empty())
      .unwrap_or_else(|| committed_url.clone());

    // Update our stable document URL hint (used for origin/referrer semantics) and the renderer's
    // navigation URL hints for relative URL resolution.
    self.document_url = (!super::trim_ascii_whitespace(&committed_url).is_empty()).then_some(committed_url.clone());
    self.set_navigation_urls(Some(committed_url.clone()), Some(base_url.clone()));

    // Install the prepared layout result and mark paint dirty so the next render call produces a
    // frame without re-running layout.
    self.reset_with_prepared(report.document, options);

    Ok((committed_url, base_url))
  }

  /// Returns an immutable reference to the live DOM tree.
  pub fn dom(&self) -> &DomNode {
    &self.dom
  }

  /// Returns the cached prepared document, if available.
  pub fn prepared(&self) -> Option<&PreparedDocument> {
    self.prepared.as_ref()
  }

  /// Returns a mutable reference to the cached prepared document, if available.
  pub fn prepared_mut(&mut self) -> Option<&mut PreparedDocument> {
    self.prepared.as_mut()
  }

  /// Updates the document URL used for origin/referrer policy decisions.
  ///
  /// This is intentionally distinct from the effective base URL derived from `<base href>`, which
  /// is allowed to change as the DOM mutates.
  pub fn set_document_url(&mut self, url: Option<String>) {
    let sanitized =
      url.and_then(|url| (!super::trim_ascii_whitespace(&url).is_empty()).then_some(url));
    if sanitized != self.document_url {
      self.document_url = sanitized;
      self.invalidate_all();
    }
  }

  /// Updates the document URL used for origin/referrer policy decisions **without** invalidating
  /// cached style/layout/paint state.
  ///
  /// This is intended for same-document fragment navigations (e.g. `#target`) where the document
  /// identity has not changed and the UI wants to reuse the cached layout artifacts for scrolling.
  ///
  /// Note: since this does not mark the document dirty, `:target` styling will only update once a
  /// later operation triggers style/layout invalidation.
  pub fn set_document_url_without_invalidation(&mut self, url: Option<String>) {
    let sanitized =
      url.and_then(|url| (!super::trim_ascii_whitespace(&url).is_empty()).then_some(url));
    if sanitized != self.document_url {
      // Keep the renderer's internal document URL hint in sync so any subsequent fetches or
      // pipeline re-runs use the updated value.
      match sanitized.as_deref() {
        Some(url) => self.renderer.set_document_url(url.to_string()),
        None => self.renderer.clear_document_url(),
      }
      self.document_url = sanitized;
    }
  }

  /// Returns the document URL used for origin/referrer policy decisions.
  pub fn document_url(&self) -> Option<&str> {
    self.document_url.as_deref()
  }

  /// Returns the effective base URL used for resolving relative links, reflecting `<base href>`
  /// after the most recent prepare/layout pass.
  pub fn base_url(&self) -> Option<&str> {
    self.renderer.base_url.as_deref()
  }

  /// Returns a mutable reference to the live DOM tree, marking the document dirty.
  pub fn dom_mut(&mut self) -> &mut DomNode {
    self.invalidate_all();
    &mut self.dom
  }

  /// Mutates the DOM tree, marking the document dirty only when `f` reports that it changed
  /// something.
  pub fn mutate_dom<F>(&mut self, f: F) -> bool
  where
    F: FnOnce(&mut DomNode) -> bool,
  {
    let changed = f(&mut self.dom);
    if changed {
      self.invalidate_all();
    }
    changed
  }

  /// Mutates the DOM tree while granting access to the cached layout artifacts.
  ///
  /// This is primarily intended for UI interaction/hit-testing layers that need to consult the
  /// last computed layout (box + fragment trees) while mutating the live DOM.
  ///
  /// The closure returns `(changed, output)` where `changed` indicates whether the DOM mutation
  /// should invalidate cached style/layout/paint state.
  ///
  /// # Errors
  ///
  /// Returns an error when the document has no cached layout yet (i.e. `render_frame()` has not
  /// been called). Call `render_frame()` first to populate the layout cache.
  pub fn mutate_dom_with_layout_artifacts<F, R>(&mut self, f: F) -> Result<R>
  where
    F: FnOnce(&mut DomNode, &BoxTree, &FragmentTree) -> (bool, R),
  {
    let Some(prepared) = self.prepared.as_ref() else {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocument has no cached layout; call render_frame() first".to_string(),
      }));
    };

    let (changed, out) = f(&mut self.dom, prepared.box_tree(), prepared.fragment_tree());
    if changed {
      self.invalidate_all();
    }
    Ok(out)
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
    if self.options.scroll_x != scroll_x || self.options.scroll_y != scroll_y {
      self.options.scroll_x = scroll_x;
      self.options.scroll_y = scroll_y;
      self.paint_dirty = true;
    }
  }

  /// Updates (or clears) the animation/transition sampling timestamp.
  ///
  /// When set to `None`, time-based animations resolve to a deterministic settled state.
  ///
  /// When the value changes, this marks paint dirty (but does not invalidate style/layout).
  pub fn set_animation_time(&mut self, time_ms: Option<f32>) {
    let sanitized = super::sanitize_animation_time_ms(time_ms);
    if sanitized != self.options.animation_time {
      self.options.animation_time = sanitized;
      self.paint_dirty = true;
    }
  }

  /// Updates the animation/transition sampling timestamp in milliseconds since load.
  ///
  /// Unlike DOM mutations, updating time only marks the paint stage dirty. This allows callers to
  /// advance the animation clock and request a new frame without rerunning cascade/layout.
  pub fn set_animation_time_ms(&mut self, time_ms: f32) {
    self.set_animation_time(Some(time_ms));
  }

  /// Updates the full scroll state (viewport + element scroll offsets), marking paint dirty.
  pub fn set_scroll_state(&mut self, state: ScrollState) {
    let ScrollState { viewport, elements } = state;
    let changed = self.options.scroll_x != viewport.x
      || self.options.scroll_y != viewport.y
      || self.options.element_scroll_offsets != elements;
    if changed {
      self.options.scroll_x = viewport.x;
      self.options.scroll_y = viewport.y;
      self.options.element_scroll_offsets = elements;
      self.paint_dirty = true;
    }
  }

  /// Updates the cooperative cancellation callback used during prepare/layout.
  ///
  /// This is a control knob (e.g. for UI-level cancellation) and does not mark the document dirty.
  pub fn set_cancel_callback(
    &mut self,
    cb: Option<std::sync::Arc<crate::render_control::CancelCallback>>,
  ) {
    self.options.cancel_callback = cb;
  }

  /// Updates the hard timeout used during prepare/layout.
  ///
  /// This is a control knob (e.g. for UI-level cancellation) and does not mark the document dirty.
  pub fn set_timeout(&mut self, timeout: Option<std::time::Duration>) {
    self.options.timeout = timeout;
  }

  /// Returns the current scroll state used by this document.
  pub fn scroll_state(&self) -> ScrollState {
    ScrollState::from_parts(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
    )
  }

  /// Applies a scroll wheel delta at a point in viewport coordinates.
  ///
  /// This updates both viewport scroll and element scroll container offsets (e.g. `<select size>`
  /// listboxes) using the cached layout's fragment tree.
  pub fn wheel_scroll_at_viewport_point(
    &mut self,
    viewport_point_css: crate::geometry::Point,
    delta_css: (f32, f32),
  ) -> crate::Result<bool> {
    let prepared = self.prepared.as_ref().ok_or_else(|| {
      Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocument has no cached layout; call render_frame() first".to_string(),
      })
    })?;

    let current_scroll_state = self.scroll_state();
    let page_point_css = viewport_point_css.translate(current_scroll_state.viewport);
    let (delta_x, delta_y) = delta_css;
    let next = crate::interaction::scroll_wheel::apply_wheel_scroll_at_point(
      prepared.fragment_tree(),
      &current_scroll_state,
      prepared.layout_viewport(),
      page_point_css,
      crate::interaction::scroll_wheel::ScrollWheelInput { delta_x, delta_y },
    );

    if next != current_scroll_state {
      self.set_scroll_state(next);
      Ok(true)
    } else {
      Ok(false)
    }
  }

  /// Renders a new frame if anything has been invalidated since the last successful frame.
  ///
  /// Returns `Ok(None)` when no dirty flags are set.
  pub fn render_if_needed(&mut self) -> Result<Option<super::Pixmap>> {
    Ok(
      self
        .render_if_needed_with_scroll_state()?
        .map(|frame| frame.pixmap),
    )
  }

  /// Renders a new frame if anything has been invalidated since the last successful frame,
  /// returning the pixmap plus the effective scroll state used during painting.
  pub fn render_if_needed_with_scroll_state(&mut self) -> Result<Option<super::PaintedFrame>> {
    if !self.is_dirty() {
      return Ok(None);
    }
    let frame = self.render_frame_with_scroll_state()?;
    Ok(Some(frame))
  }

  /// Renders one frame.
  ///
  /// If the document is dirty, this triggers a full pipeline run. Otherwise, it repaints from
  /// cached layout artifacts.
  pub fn render_frame(&mut self) -> Result<super::Pixmap> {
    Ok(self.render_frame_with_scroll_state()?.pixmap)
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

    let frame = self.paint_from_cache_frame_with_deadline(paint_deadline)?;

    // Clear flags only when a render was requested due to invalidation.
    if self.is_dirty() {
      self.clear_dirty();
    }

    Ok(frame)
  }

  /// Renders one frame, returning the pixmap plus the effective scroll state used during painting.
  ///
  /// If the document is dirty, this triggers a full pipeline run. Otherwise, it repaints from
  /// cached layout artifacts.
  pub fn render_frame_with_scroll_state(&mut self) -> Result<super::PaintedFrame> {
    self.render_frame_with_deadlines(None)
  }

  /// Paints the most recently laid-out document without re-running style/layout.
  ///
  /// This is primarily intended for UI-driven repaints (scrolling, hit-testing highlights, etc)
  /// where the caller wants to provide a [`crate::render_control::RenderDeadline`] for cooperative
  /// cancellation. Callers should check [`BrowserDocument::needs_layout`] first and fall back to
  /// [`BrowserDocument::render_frame_with_scroll_state`] when layout is required.
  pub fn paint_from_cache_frame_with_deadline(
    &mut self,
    deadline: Option<&crate::render_control::RenderDeadline>,
  ) -> Result<super::PaintedFrame> {
    if self.prepared.is_none() {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocument has no cached layout; call render_frame() first".to_string(),
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

    let scroll_state = self.scroll_state();
    let frame = prepared.paint_with_options_frame(PreparedPaintOptions {
      scroll: Some(scroll_state),
      viewport: None,
      background: None,
      animation_time,
    })?;

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
    let dom = &self.dom;
    let document_url = self.document_url.clone();
    let renderer = &mut self.renderer;

    let toggles = renderer.resolve_runtime_toggles(&options);
    let _toggles_guard =
      super::RuntimeTogglesSwap::new(&mut renderer.runtime_toggles, toggles.clone());
    crate::debug::runtime::with_runtime_toggles(toggles, || {
      let trace = super::TraceSession::from_options(Some(&options));
      let trace_handle = trace.handle();
      let _root_span = trace_handle.span("browser_document_prepare", "pipeline");

      // Ensure the resource context sees the stable document URL hint, rather than a potentially
      // mutable `<base href>` override stored in `renderer.base_url`.
      if let Some(url) = document_url.as_deref() {
        renderer.set_document_url(url.to_string());
      } else {
        renderer.clear_document_url();
      }

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
      let result = prepare_dom_inner(renderer, dom, options.clone(), trace_handle);
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

pub(super) fn prepare_dom_inner(
  renderer: &mut super::FastRender,
  dom: &DomNode,
  options: RenderOptions,
  trace: &crate::debug::trace::TraceHandle,
) -> Result<PreparedDocument> {
  let (width, height) = options
    .viewport
    .unwrap_or((renderer.default_width, renderer.default_height));
  if width == 0 || height == 0 {
    return Err(Error::Render(RenderError::InvalidParameters {
      message: format!("Invalid dimensions: width={width}, height={height}"),
    }));
  }

  let deadline =
    crate::render_control::RenderDeadline::new(options.timeout, options.cancel_callback.clone());
  let _deadline_guard = crate::render_control::DeadlineGuard::install(Some(&deadline));

  // Ensure cooperative cancellation is observable even if the subsequent stage preamble (base URL
  // / referrer policy extraction) finishes quickly without checking the deadline.
  crate::render_control::check_active(RenderStage::DomParse).map_err(Error::Render)?;

  renderer.update_base_url_from_dom(dom);
  if let Some(policy) = crate::html::referrer_policy::extract_referrer_policy_with_deadline(dom)? {
    let needs_update = renderer
      .resource_context
      .as_ref()
      .is_some_and(|ctx| ctx.referrer_policy != policy);
    if needs_update {
      if let Some(mut ctx) = renderer.resource_context.clone() {
        ctx.referrer_policy = policy;
        // Propagate the updated policy to all caches/fetchers that hold a copy of the current
        // resource context.
        renderer.push_resource_context(Some(ctx));
      }
    }
  }

  let requested_viewport = Size::new(width as f32, height as f32);
  let base_dpr = options
    .device_pixel_ratio
    .unwrap_or(renderer.device_pixel_ratio);
  let meta_viewport = if renderer.apply_meta_viewport {
    crate::html::viewport::extract_viewport_with_deadline(dom)?
  } else {
    None
  };
  let resolved_viewport = resolve_viewport(requested_viewport, base_dpr, meta_viewport.as_ref());
  let layout_width = resolved_viewport.layout_viewport.width.max(1.0).round() as u32;
  let layout_height = resolved_viewport.layout_viewport.height.max(1.0).round() as u32;
  let paint_parallelism = renderer.resolve_paint_parallelism(&options);
  let layout_parallelism = renderer.resolve_layout_parallelism(&options);

  let previous_dpr = renderer.device_pixel_ratio;
  let artifacts_result = (|| -> Result<super::LayoutArtifacts> {
    renderer.device_pixel_ratio = resolved_viewport.device_pixel_ratio;
    renderer.pending_device_size = Some(resolved_viewport.visual_viewport);
    renderer.layout_document_for_media_with_artifacts(
      dom,
      layout_width,
      layout_height,
      options.media_type,
      LayoutDocumentOptions {
        page_stacking: super::PageStacking::Stacked { gap: 0.0 },
        animation_time: options.animation_time,
      },
      Point::new(options.scroll_x, options.scroll_y),
      Some(&deadline),
      options.stage_mem_budget_bytes,
      trace,
      layout_parallelism,
      None,
    )
  })();

  renderer.device_pixel_ratio = previous_dpr;
  renderer.pending_device_size = None;
  let artifacts = artifacts_result?;

  let layout_viewport = artifacts.fragment_tree.viewport_size();
  Ok(PreparedDocument {
    dom: artifacts.dom,
    stylesheet: artifacts.stylesheet,
    styled_tree: artifacts.styled_tree,
    box_tree: artifacts.box_tree,
    fragment_tree: artifacts.fragment_tree,
    layout_viewport,
    visual_viewport: resolved_viewport.visual_viewport,
    device_pixel_ratio: resolved_viewport.device_pixel_ratio,
    page_zoom: resolved_viewport.zoom,
    background_color: renderer.background_color,
    default_scroll: ScrollState::from_parts(
      Point::new(options.scroll_x, options.scroll_y),
      options.element_scroll_offsets.clone(),
    ),
    animation_time: options.animation_time,
    font_context: renderer.font_context.clone(),
    image_cache: renderer.image_cache.clone(),
    max_iframe_depth: renderer.max_iframe_depth,
    paint_parallelism,
    runtime_toggles: std::sync::Arc::clone(&renderer.runtime_toggles),
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::render_control::{push_stage_listener, RenderDeadline, StageHeartbeat};
  use crate::text::font_db::FontConfig;
  use std::time::Duration;
  use std::sync::{Arc, Mutex};
  use tiny_skia::PremultipliedColorU8;

  fn capture_stages<T>(f: impl FnOnce() -> Result<T>) -> Result<Vec<StageHeartbeat>> {
    let stages: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
    let stages_for_listener = Arc::clone(&stages);
    let _guard = push_stage_listener(Some(Arc::new(move |stage| {
      stages_for_listener.lock().unwrap().push(stage);
    })));
    let _ = f()?;
    let captured = stages.lock().unwrap().clone();
    Ok(captured)
  }

  #[test]
  fn reset_with_prepared_skips_layout_on_first_paint() -> Result<()> {
    let mut renderer = super::super::FastRender::new()?;
    let options = RenderOptions::default().with_viewport(64, 64);
    let prepared = renderer.prepare_html("<div>hi</div>", options.clone())?;

    let mut document = BrowserDocument::from_prepared(renderer, prepared, options.clone())?;
    let stages = capture_stages(|| document.render_if_needed().map(|_| ()))?;

    assert!(
      !stages.contains(&StageHeartbeat::Layout),
      "expected no layout stage; got {stages:?}"
    );
    assert!(
      stages.contains(&StageHeartbeat::PaintBuild)
        || stages.contains(&StageHeartbeat::PaintRasterize),
      "expected paint stage heartbeats; got {stages:?}"
    );
    Ok(())
  }

  #[test]
  fn reset_with_html_clears_prepared_and_triggers_layout() -> Result<()> {
    let mut renderer = super::super::FastRender::new()?;
    let options = RenderOptions::default().with_viewport(64, 64);
    let prepared = renderer.prepare_html("<div>old</div>", options.clone())?;

    let mut document = BrowserDocument::from_prepared(renderer, prepared, options.clone())?;
    document.reset_with_html("<div>new</div>", options.clone())?;

    let stages = capture_stages(|| document.render_if_needed().map(|_| ()))?;
    assert!(
      stages.contains(&StageHeartbeat::Layout),
      "expected layout stage after reset_with_html; got {stages:?}"
    );
    Ok(())
  }

  #[test]
  fn non_ascii_whitespace_set_navigation_urls_does_not_trim_nbsp_base_url() -> Result<()> {
    let mut document = BrowserDocument::from_html("<div>hi</div>", RenderOptions::default())?;
    let nbsp = "\u{00A0}".to_string();
    document.set_navigation_urls(None, Some(nbsp.clone()));
    assert_eq!(document.renderer.base_url.as_deref(), Some(nbsp.as_str()));
    Ok(())
  }

  #[test]
  fn set_device_pixel_ratio_triggers_layout() -> Result<()> {
    let mut document =
      BrowserDocument::from_html("<div>hi</div>", RenderOptions::default().with_viewport(32, 32))?;
    document.render_frame()?;

    document.set_device_pixel_ratio(2.0);
    let stages = capture_stages(|| document.render_if_needed().map(|_| ()))?;

    assert!(
      stages.contains(&StageHeartbeat::Layout),
      "expected layout stage after DPR change; got {stages:?}"
    );
    Ok(())
  }

  #[test]
  fn needs_layout_transitions() -> Result<()> {
    let mut document =
      BrowserDocument::from_html("<div>hi</div>", RenderOptions::default().with_viewport(32, 32))?;

    assert!(document.needs_layout(), "expected needs_layout before first render");
    document.render_frame()?;
    assert!(
      !document.needs_layout(),
      "expected needs_layout to be false after first render"
    );

    document.set_device_pixel_ratio(2.0);
    assert!(
      document.needs_layout(),
      "expected needs_layout to be true after DPR change"
    );
    Ok(())
  }

  #[test]
  fn paint_from_cache_frame_with_deadline_can_cancel() -> Result<()> {
    let mut document =
      BrowserDocument::from_html("<div>hi</div>", RenderOptions::default().with_viewport(32, 32))?;
    document.render_frame()?;

    let cancel: Arc<crate::render_control::CancelCallback> = Arc::new(|| true);
    let deadline = RenderDeadline::new(None, Some(cancel));
    let err = match document.paint_from_cache_frame_with_deadline(Some(&deadline)) {
      Ok(_) => panic!("expected paint to be cancelled"),
      Err(err) => err,
    };
    assert!(
      matches!(err, Error::Render(RenderError::Timeout { stage: RenderStage::Paint, .. })),
      "expected paint timeout error, got {err:?}"
    );
    Ok(())
  }

  #[test]
  fn render_frame_with_deadlines_cancels_layout_via_cancel_callback() -> Result<()> {
    let options = RenderOptions::default().with_viewport(32, 32);
    let mut document = BrowserDocument::from_html("<div>hi</div>", options)?;

    let cb: Arc<crate::render_control::CancelCallback> = Arc::new(|| true);
    document.set_cancel_callback(Some(cb));

    let err = match document.render_frame_with_deadlines(None) {
      Ok(_) => panic!("expected cancellation error"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::DomParse);
      }
      other => panic!("expected RenderError::Timeout; got {other:?}"),
    }
    Ok(())
  }

  #[test]
  fn render_frame_with_deadlines_cancels_paint_via_paint_deadline() -> Result<()> {
    let options = RenderOptions::default().with_viewport(32, 32);
    let mut document = BrowserDocument::from_html("<div>hi</div>", options)?;

    // Prime the layout cache.
    let _ = document.render_frame_with_deadlines(None)?;

    // Mark paint dirty.
    document.set_scroll(1.0, 0.0);

    let cb: Arc<crate::render_control::CancelCallback> = Arc::new(|| true);
    let paint_deadline = RenderDeadline::new(None, Some(cb));
    let err = match document.render_frame_with_deadlines(Some(&paint_deadline)) {
      Ok(_) => panic!("expected cancellation error"),
      Err(err) => err,
    };
    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => {
        assert_eq!(stage, RenderStage::Paint);
      }
      other => panic!("expected RenderError::Timeout; got {other:?}"),
    }
    Ok(())
  }

  #[test]
  fn base_url_reflects_base_href_after_prepare() -> Result<()> {
    fn set_base_href(dom: &mut DomNode, href: &str) -> bool {
      let mut stack = vec![dom];
      while let Some(node) = stack.pop() {
        if let crate::dom::DomNodeType::Element {
          tag_name,
          attributes,
          ..
        } = &mut node.node_type
        {
          if tag_name.eq_ignore_ascii_case("base") {
            if let Some((_, value)) = attributes
              .iter_mut()
              .find(|(name, _)| name.eq_ignore_ascii_case("href"))
            {
              *value = href.to_string();
            } else {
              attributes.push(("href".to_string(), href.to_string()));
            }
            return true;
          }
        }
        for child in node.children.iter_mut() {
          stack.push(child);
        }
      }
      false
    }

    let options = RenderOptions::default().with_viewport(32, 32);
    let html = r#"<!doctype html><html><head><base href="https://example.com/base/"></head><body><a href="x">x</a></body></html>"#;
    let mut document = BrowserDocument::from_html(html, options)?;
    let _ = document.render_frame_with_deadlines(None)?;
    assert_eq!(document.base_url(), Some("https://example.com/base/"));

    let changed = document.mutate_dom(|dom| set_base_href(dom, "https://example.com/next/"));
    assert!(changed, "expected DOM mutation to update <base href>");

    let _ = document.render_frame_with_deadlines(None)?;
    assert_eq!(document.base_url(), Some("https://example.com/next/"));
    Ok(())
  }

  #[test]
  fn navigate_url_updates_live_document_in_place() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let file_path = temp_dir.path().join("index.html");
    std::fs::write(
      &file_path,
      "<!doctype html><html><head><title>New Title</title></head><body>hello</body></html>",
    )?;
    let file_url = url::Url::from_file_path(&file_path)
      .expect("file url")
      .to_string();

    let renderer = super::super::FastRender::builder()
      .font_sources(crate::text::font_db::FontConfig::bundled_only())
      .build()?;

    let mut document = BrowserDocument::new(
      renderer,
      "<!doctype html><html><head><title>Old Title</title></head><body>old</body></html>",
      RenderOptions::new().with_viewport(64, 64),
    )?;

    let report = document.navigate_url(&file_url, RenderOptions::new().with_viewport(64, 64))?;

    // Ensure we can paint the newly navigated document from the prepared cache.
    document.render_frame()?;
    assert_eq!(
      crate::html::title::find_document_title(document.dom()),
      Some("New Title".to_string())
    );

    assert_eq!(report.final_url.as_deref(), Some(file_url.as_str()));
    assert_eq!(report.base_url.as_deref(), Some(file_url.as_str()));
    assert_eq!(document.document_url(), Some(file_url.as_str()));
    Ok(())
  }

  fn renderer_for_tests() -> super::super::FastRender {
    super::super::FastRender::builder()
      .font_sources(FontConfig::bundled_only())
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

  fn assert_rgb_close(color: PremultipliedColorU8, expected: u8, tolerance: u8) {
    assert_channel_close(color.red(), expected, tolerance);
    assert_channel_close(color.green(), expected, tolerance);
    assert_channel_close(color.blue(), expected, tolerance);
    assert_eq!(color.alpha(), 255);
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
    let mut document =
      BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(20, 20))?;

    let clock = Arc::new(crate::js::clock::VirtualClock::new());
    document.set_animation_clock(clock.clone());
    document.set_realtime_animations_enabled(true);

    let pixmap0 = document.render_frame()?;
    let c0 = pixmap0.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c0, 255, 0);

    clock.advance(Duration::from_millis(500));
    let pixmap1 = document.render_frame()?;
    let c1 = pixmap1.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c1, 128, 8);

    // Explicit per-render timestamps always override the real-time document timeline.
    document.set_animation_time_ms(1000.0);
    let pixmap_override = document.render_frame()?;
    let c2 = pixmap_override.pixel(5, 5).expect("pixel 5,5");
    assert_rgb_close(c2, 0, 0);

    Ok(())
  }
}
