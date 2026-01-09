use crate::error::{Error, RenderError, RenderStage, Result};
use crate::geometry::Point;
use crate::resource::ReferrerPolicy;
use crate::scroll::ScrollState;

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
  style_dirty: bool,
  layout_dirty: bool,
  paint_dirty: bool,
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
      // First frame needs a full pipeline run.
      style_dirty: true,
      layout_dirty: true,
      paint_dirty: true,
    })
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
    let Some(prepared) = self.prepared.as_ref() else {
      return Err(Error::Render(RenderError::InvalidParameters {
        message: "BrowserDocumentDom2 has no cached layout; call render_frame() first".to_string(),
      }));
    };

    let _deadline_guard = deadline
      .map(|deadline| crate::render_control::DeadlineGuard::install(Some(deadline)));
    crate::render_control::check_active(RenderStage::Paint).map_err(Error::Render)?;

    let scroll_state = ScrollState::from_parts(
      Point::new(self.options.scroll_x, self.options.scroll_y),
      self.options.element_scroll_offsets.clone(),
    );
    let frame = prepared.paint_with_options_frame(PreparedPaintOptions {
      scroll: Some(scroll_state),
      viewport: None,
      background: None,
      animation_time: self.options.animation_time,
    })?;

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
  fn is_dirty(&self) -> bool {
    self.style_dirty || self.layout_dirty || self.paint_dirty
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
}
