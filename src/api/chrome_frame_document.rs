//! Standalone chrome-frame helper built on [`BrowserDocument`].
//!
//! This type exists in the public `api` surface for embeddings that want to render *custom* chrome
//! HTML/CSS and still get correct interaction behaviour (notably `:hover` clearing via
//! [`ChromeFrameDocument::pointer_leave`]).
//!
//! For the *renderer-chrome* runtime (the browser UI rendered by FastRender itself, driven by
//! `BrowserAppState` + `chrome-action:` URLs), prefer [`crate::chrome_frame::ChromeFrameDocument`]
//! (re-exported as [`crate::ui::ChromeFrameDocument`]).

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

  /// Returns `true` when the most recently prepared fragment tree contains any time-based effects
  /// (currently CSS animations/transitions) that require periodic ticking.
  pub fn wants_ticks(&self) -> bool {
    crate::document_ticks::browser_document_wants_ticks(&self.document)
  }

  /// Advance the chrome document's animation timeline.
  ///
  /// - When `now_ms` is `Some(t)`, CSS animations/transitions are sampled at `t` milliseconds since
  ///   load by calling [`BrowserDocument::set_animation_time_ms`]. This only invalidates paint, so
  ///   the next render can repaint from cached layout artifacts.
  /// - When `now_ms` is `None`, real-time animation sampling is enabled and callers should only
  ///   repaint when [`BrowserDocument::needs_animation_frame`] reports that the animation clock has
  ///   advanced.
  ///
  /// Returns `true` when callers should render a new frame.
  pub fn tick(&mut self, now_ms: Option<f32>) -> bool {
    match now_ms {
      Some(ms) => {
        self.document.set_animation_time_ms(ms);
        true
      }
      None => {
        // Ensure explicit timelines are cleared so real-time sampling is active.
        self.document.set_animation_time(None);
        self.document.set_realtime_animations_enabled(true);
        self.document.needs_animation_frame()
      }
    }
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
  use std::collections::hash_map::DefaultHasher;
  use std::hash::{Hash, Hasher};

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

  fn pixmap_hash(pixmap: &Pixmap) -> u64 {
    let mut hasher = DefaultHasher::new();
    pixmap.width().hash(&mut hasher);
    pixmap.height().hash(&mut hasher);
    pixmap.data().hash(&mut hasher);
    hasher.finish()
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

  #[test]
  fn chrome_frame_tick_advances_css_keyframes_animation() -> Result<()> {
    let _lock = crate::testing::global_test_lock();

    let html = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <style>
      html, body { margin: 0; padding: 0; }
      #box {
        width: 32px;
        height: 32px;
        background: rgb(255, 0, 0);
        animation: bg 1000ms linear infinite;
      }
      @keyframes bg {
        from { background: rgb(255, 0, 0); }
        to   { background: rgb(0, 0, 255); }
      }
    </style>
  </head>
  <body><div id="box"></div></body>
</html>"#;

    let options = RenderOptions::new().with_viewport(32, 32);
    let mut doc = ChromeFrameDocument::from_html(html, options)?;

    assert!(
      !doc.wants_ticks(),
      "expected wants_ticks to be false before first render (no prepared fragment tree)"
    );

    // Prime layout/paint cache so wants_ticks inspects the prepared fragment tree.
    doc.render_frame()?;
    assert!(
      doc.wants_ticks(),
      "expected wants_ticks to be true after first render for document with @keyframes"
    );

    assert!(doc.tick(Some(0.0)), "tick(Some) should request a repaint");
    let first = doc
      .render_if_needed()?
      .expect("expected repaint after tick(Some(0.0))");
    let first_hash = pixmap_hash(&first);

    assert!(doc.tick(Some(500.0)), "tick(Some) should request a repaint");
    let second = doc
      .render_if_needed()?
      .expect("expected repaint after tick(Some(500.0))");
    let second_hash = pixmap_hash(&second);

    assert_ne!(
      first_hash, second_hash,
      "expected keyframes animation sampling to change rendered output between two times"
    );

    Ok(())
  }
}
