use crate::dom::DomNode;
use crate::error::{Error, RenderError, Result};
use crate::geometry::{Point, Size};
use crate::resource::ReferrerPolicy;
use crate::scroll::ScrollState;

use super::{
  resolve_viewport, LayoutDocumentOptions, PreparedDocument, PreparedPaintOptions, RenderOptions,
};

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
    })
  }

  /// Updates the renderer's document/base URL hints for the current navigation.
  ///
  /// - `document_url` is used for referrer/origin semantics (after redirects).
  /// - `base_url` is used for resolving relative URLs (and can be overridden by `<base href>`).
  pub fn set_navigation_urls(&mut self, document_url: Option<String>, base_url: Option<String>) {
    match document_url {
      Some(url) => self.renderer.set_document_url(url),
      None => self.renderer.clear_document_url(),
    }
    match base_url {
      Some(url) if !super::trim_ascii_whitespace(&url).is_empty() => self.renderer.set_base_url(url),
      _ => self.renderer.clear_base_url(),
    }
  }

  /// Replaces the live DOM, clears any cached preparation state, and marks the document dirty.
  pub fn reset_with_dom(&mut self, dom: DomNode, options: RenderOptions) {
    self.dom = dom;
    self.options = options;
    self.prepared = None;
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
  }

  /// Parses HTML using the internal renderer and resets the document state.
  pub fn reset_with_html(&mut self, html: &str, options: RenderOptions) -> Result<()> {
    let dom = self.renderer.parse_html(html)?;
    self.reset_with_dom(dom, options);
    Ok(())
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
    let sanitized = url.and_then(|url| (!super::trim_ascii_whitespace(&url).is_empty()).then_some(url));
    if sanitized != self.document_url {
      self.document_url = sanitized;
      self.invalidate_all();
    }
  }

  /// Returns the document URL used for origin/referrer policy decisions.
  pub fn document_url(&self) -> Option<&str> {
    self.document_url.as_deref()
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

  /// Updates the viewport size (in CSS px), marking layout+paint dirty.
  pub fn set_viewport(&mut self, width: u32, height: u32) {
    self.options.viewport = Some((width, height));
    self.layout_dirty = true;
    self.paint_dirty = true;
  }

  /// Updates the viewport scroll offset (in CSS px), marking paint dirty.
  pub fn set_scroll(&mut self, scroll_x: f32, scroll_y: f32) {
    if self.options.scroll_x != scroll_x || self.options.scroll_y != scroll_y {
      self.options.scroll_x = scroll_x;
      self.options.scroll_y = scroll_y;
      self.paint_dirty = true;
    }
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

  /// Returns the current scroll state used by this document.
  pub fn scroll_state(&self) -> ScrollState {
    ScrollState::from_parts(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
    )
  }

  /// Renders a new frame if anything has been invalidated since the last successful frame.
  ///
  /// Returns `Ok(None)` when no dirty flags are set.
  pub fn render_if_needed(&mut self) -> Result<Option<super::Pixmap>> {
    Ok(self
      .render_if_needed_with_scroll_state()?
      .map(|frame| frame.pixmap))
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

  /// Renders one frame, returning the pixmap plus the effective scroll state used during painting.
  ///
  /// If the document is dirty, this triggers a full pipeline run. Otherwise, it repaints from
  /// cached layout artifacts.
  pub fn render_frame_with_scroll_state(&mut self) -> Result<super::PaintedFrame> {
    // If we haven't rendered before, force a full pipeline run even if the flags were cleared.
    if self.prepared.is_none() {
      self.invalidate_all();
    }

    let needs_layout = self.style_dirty || self.layout_dirty;
    if needs_layout {
      let prepared = self.prepare_dom_with_options()?;
      self.prepared = Some(prepared);
    }

    let frame = self.paint_from_cache_frame()?;
    // Keep our internal scroll model synchronized with any adjustments made during painting (e.g.
    // scroll snap/clamp). This must not mark the document dirty because the frame we just painted
    // already reflects this state.
    self.options.scroll_x = frame.scroll_state.viewport.x;
    self.options.scroll_y = frame.scroll_state.viewport.y;
    self.options.element_scroll_offsets = frame.scroll_state.elements.clone();

    // Clear flags only when a render was requested due to invalidation.
    if self.is_dirty() {
      self.clear_dirty();
    }

    Ok(frame)
  }

  fn paint_from_cache_frame(&self) -> Result<super::PaintedFrame> {
    let Some(prepared) = &self.prepared else {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocument has no cached layout; call render_frame() first".to_string(),
      }));
    };

    let scroll_state = self.scroll_state();
    prepared.paint_with_options_frame(PreparedPaintOptions {
      scroll: Some(scroll_state),
      viewport: None,
      background: None,
      animation_time: self.options.animation_time,
    })
  }

  fn prepare_dom_with_options(&mut self) -> Result<PreparedDocument> {
    let options = self.options.clone();
    let dom = &self.dom;
    let document_url = self.document_url.clone();
    let renderer = &mut self.renderer;

    let toggles = renderer.resolve_runtime_toggles(&options);
    let _toggles_guard = super::RuntimeTogglesSwap::new(&mut renderer.runtime_toggles, toggles.clone());
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

      let shared_diagnostics = renderer.diagnostics.as_ref().map(|diag| super::SharedRenderDiagnostics {
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

  let deadline = crate::render_control::RenderDeadline::new(options.timeout, options.cancel_callback.clone());
  let _deadline_guard = crate::render_control::DeadlineGuard::install(Some(&deadline));

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
  let base_dpr = options.device_pixel_ratio.unwrap_or(renderer.device_pixel_ratio);
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
  use crate::render_control::{push_stage_listener, StageHeartbeat};
  use std::sync::{Arc, Mutex};

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
}
