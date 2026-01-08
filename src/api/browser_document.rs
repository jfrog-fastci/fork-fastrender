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
    Ok(Self {
      renderer,
      dom,
      options,
      prepared: None,
      // First frame needs a full pipeline run.
      style_dirty: true,
      layout_dirty: true,
      paint_dirty: true,
    })
  }

  /// Returns an immutable reference to the live DOM tree.
  pub fn dom(&self) -> &DomNode {
    &self.dom
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
    self.options.scroll_x = scroll_x;
    self.options.scroll_y = scroll_y;
    self.paint_dirty = true;
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
      let prepared = self.prepare_dom_with_options()?;
      self.prepared = Some(prepared);
    }

    let pixmap = self.paint_from_cache()?;

    // Clear flags only when a render was requested due to invalidation.
    if self.is_dirty() {
      self.clear_dirty();
    }

    Ok(pixmap)
  }

  fn paint_from_cache(&self) -> Result<super::Pixmap> {
    let Some(prepared) = &self.prepared else {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocument has no cached layout; call render_frame() first".to_string(),
      }));
    };

    let scroll_state = ScrollState::from_parts(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
    );
    prepared.paint_with_options(PreparedPaintOptions {
      scroll: Some(scroll_state),
      viewport: None,
      background: None,
      animation_time: self.options.animation_time,
    })
  }

  fn prepare_dom_with_options(&mut self) -> Result<PreparedDocument> {
    let options = self.options.clone();
    let dom = &self.dom;
    let renderer = &mut self.renderer;

    let toggles = renderer.resolve_runtime_toggles(&options);
    let _toggles_guard = super::RuntimeTogglesSwap::new(&mut renderer.runtime_toggles, toggles.clone());
    crate::debug::runtime::with_runtime_toggles(toggles, || {
      let trace = super::TraceSession::from_options(Some(&options));
      let trace_handle = trace.handle();
      let _root_span = trace_handle.span("browser_document_prepare", "pipeline");

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
