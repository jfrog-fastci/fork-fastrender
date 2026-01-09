use crate::dom::DomNode;
use crate::dom2::{Document, RendererDomSnapshot};
use crate::error::{Error, RenderError, Result};
use crate::geometry::Point;
use crate::resource::ReferrerPolicy;
use crate::scroll::ScrollState;

use super::{PreparedDocument, PreparedPaintOptions, RenderOptions};

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
      // First frame needs a full pipeline run.
      style_dirty: true,
      layout_dirty: true,
      paint_dirty: true,
    })
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

    let needs_layout = self.style_dirty || self.layout_dirty;
    if needs_layout {
      let snapshot = self.dom.to_renderer_dom_with_mapping();
      let prepared = self.prepare_dom_with_options(&snapshot.dom)?;
      self.prepared = Some(prepared);
      self.last_dom_mapping = Some(snapshot);
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
        message: "BrowserDocument2 has no cached layout; call render_frame() first".to_string(),
      }));
    };

    let scroll_state = ScrollState::from_parts(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
    );
    let deadline_enabled =
      self.options.timeout.is_some() || self.options.cancel_callback.is_some();
    let _deadline_guard = deadline_enabled.then(|| {
      let deadline = crate::render_control::RenderDeadline::new(
        self.options.timeout,
        self.options.cancel_callback.clone(),
      );
      crate::render_control::DeadlineGuard::install(Some(&deadline))
    });
    prepared.paint_with_options(PreparedPaintOptions {
      scroll: Some(scroll_state),
      viewport: None,
      background: None,
      animation_time: self.options.animation_time,
    })
  }

  fn prepare_dom_with_options(&mut self, dom: &DomNode) -> Result<PreparedDocument> {
    let options = self.options.clone();
    let renderer = &mut self.renderer;

    let toggles = renderer.resolve_runtime_toggles(&options);
    let _toggles_guard = super::RuntimeTogglesSwap::new(&mut renderer.runtime_toggles, toggles.clone());
    crate::debug::runtime::with_runtime_toggles(toggles, || {
      let trace = super::TraceSession::from_options(Some(&options));
      let trace_handle = trace.handle();
      let _root_span = trace_handle.span("browser_document2_prepare", "pipeline");

      let shared_diagnostics = renderer
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
      let (prev_self, prev_image, prev_layout_image, prev_font) = renderer.push_resource_context(context);
      let result = super::browser_document::prepare_dom_inner(renderer, dom, options.clone(), trace_handle);
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
