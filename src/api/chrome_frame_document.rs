use crate::geometry::Point;
use crate::interaction::{InteractionEngine, InteractionState};
use crate::scroll::ScrollState;
use crate::Result;

use super::{BrowserDocument, FastRender, Pixmap, RenderOptions};

/// A small helper around [`BrowserDocument`] for rendering "browser chrome" HTML/CSS.
///
/// The chrome UI is typically composited alongside page content. When the pointer leaves the chrome
/// region, embeddings must clear internal pointer state so `:hover` does not remain stuck.
pub struct ChromeFrameDocument {
  document: BrowserDocument,
  interaction: InteractionEngine,
}

impl ChromeFrameDocument {
  /// Creates a new chrome document from an HTML string using a fresh renderer.
  pub fn from_html(html: &str, options: RenderOptions) -> Result<Self> {
    Self::new(FastRender::new()?, html, options)
  }

  /// Creates a new chrome document from an HTML string using the provided renderer.
  pub fn new(renderer: FastRender, html: &str, options: RenderOptions) -> Result<Self> {
    Ok(Self {
      document: BrowserDocument::new(renderer, html, options)?,
      interaction: InteractionEngine::new(),
    })
  }

  pub fn document(&self) -> &BrowserDocument {
    &self.document
  }

  pub fn document_mut(&mut self) -> &mut BrowserDocument {
    &mut self.document
  }

  pub fn interaction_state(&self) -> &InteractionState {
    self.interaction.interaction_state()
  }

  /// Render a new frame if the document or interaction state has changed.
  pub fn render_if_needed(&mut self) -> Result<Option<Pixmap>> {
    Ok(
      self
        .document
        .render_if_needed_with_scroll_state_and_interaction_state(Some(
          self.interaction.interaction_state(),
        ))?
        .map(|frame| frame.pixmap),
    )
  }

  /// Render a new frame, even if nothing is marked dirty.
  pub fn render_frame(&mut self) -> Result<Pixmap> {
    Ok(
      self
        .document
        .render_frame_with_scroll_state_and_interaction_state(Some(
          self.interaction.interaction_state(),
        ))?
        .pixmap,
    )
  }

  /// Update hover state for a pointer move in viewport CSS pixels.
  ///
  /// This is a convenience wrapper around [`InteractionEngine::pointer_move`]. The document must
  /// have been rendered at least once before calling this (so cached layout artifacts exist). When
  /// the document has not been rendered yet, this method will render the first frame implicitly.
  pub fn pointer_move(&mut self, pos_css: (f32, f32)) -> Result<bool> {
    // Support the same sentinel used by the windowed browser integration: a negative coordinate
    // means "pointer left this frame".
    if pos_css.0 < 0.0 || pos_css.1 < 0.0 {
      return Ok(self.pointer_leave());
    }

    if self.document.prepared().is_none() {
      // Populate the layout cache so we can hit-test.
      let _ = self.render_frame()?;
    }

    let ChromeFrameDocument {
      document,
      interaction,
    } = self;

    let scroll: ScrollState = document.scroll_state();
    let viewport_point = Point::new(pos_css.0, pos_css.1);

    let hit_tree =
      (scroll.viewport != Point::ZERO || !scroll.elements.is_empty())
        .then(|| document.prepared().map(|prepared| prepared.fragment_tree_for_geometry(&scroll)))
        .flatten();
    document.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let fragment_tree = hit_tree.as_ref().unwrap_or(fragment_tree);
      let changed = interaction.pointer_move(dom, box_tree, fragment_tree, &scroll, viewport_point);
      (changed, changed)
    })
  }

  /// Clear hover/active pointer state (equivalent to a `pointerleave` event).
  ///
  /// This should be called by embeddings when the cursor leaves the chrome region so `:hover`
  /// styles/cursor state do not remain stuck while the pointer is over other composited content.
  ///
  /// Returns `true` when the interaction state changed (i.e. a rerender is required to clear
  /// hover/active styling).
  pub fn pointer_leave(&mut self) -> bool {
    let state = self.interaction.interaction_state();
    let had_hover = !state.hover_chain().is_empty();
    let had_active = !state.active_chain().is_empty();
    if !had_hover && !had_active {
      // Still clear internal drag state so future interactions don't carry stale capture state, but
      // avoid forcing a rerender when no pseudo-class-visible state changed.
      self.interaction.clear_pointer_state_without_dom();
      return false;
    }

    self.interaction.clear_pointer_state_without_dom();
    true
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rgba_at(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
    let width = pixmap.width();
    let height = pixmap.height();
    assert!(
      x < width && y < height,
      "rgba_at out of bounds: requested ({x}, {y}) in {width}x{height} pixmap"
    );
    let idx = (y as usize * width as usize + x as usize) * 4;
    let data = pixmap.data();
    [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
  }

  #[test]
  fn pointer_leave_clears_hover_state() -> Result<()> {
    let _lock = crate::testing::global_test_lock();

    let html = r##"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #link {
              position: absolute;
              left: 0;
              top: 0;
              width: 40px;
              height: 40px;
              background: rgb(0, 0, 0);
            }
            #link:hover { background: rgb(0, 0, 255); }
          </style>
        </head>
        <body>
          <a id="link" href="#"></a>
        </body>
      </html>
    "##;

    let options = RenderOptions::new().with_viewport(60, 60);
    let mut doc = ChromeFrameDocument::from_html(html, options)?;

    // Initial state: not hovered.
    let initial = doc.render_frame()?;
    assert_eq!(
      rgba_at(&initial, 10, 10),
      [0, 0, 0, 255],
      "expected initial background to be black"
    );
    assert!(
      doc.interaction_state().hover_chain().is_empty(),
      "expected initial hover chain to be empty"
    );

    // Hover the link.
    assert!(
      doc.pointer_move((10.0, 10.0))?,
      "expected pointer_move to update hover state"
    );
    assert!(
      !doc.interaction_state().hover_chain().is_empty(),
      "expected hover chain to be non-empty after pointer_move"
    );
    let hovered = doc
      .render_if_needed()?
      .expect("expected a rerender for hover state change");
    assert_eq!(
      rgba_at(&hovered, 10, 10),
      [0, 0, 255, 255],
      "expected :hover rule to apply after pointer_move"
    );

    // Leaving chrome clears hover.
    assert!(
      doc.pointer_leave(),
      "expected pointer_leave to report a hover/active state change"
    );
    assert!(
      doc.interaction_state().hover_chain().is_empty(),
      "expected hover chain to be empty after pointer_leave"
    );
    let cleared = doc
      .render_if_needed()?
      .expect("expected a rerender for hover state clear");
    assert_eq!(
      rgba_at(&cleared, 10, 10),
      [0, 0, 0, 255],
      "expected :hover rule to clear after pointer_leave"
    );

    Ok(())
  }
}
