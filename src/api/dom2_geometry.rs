use crate::dom2;
use crate::geometry::{Point, Rect, Size};
use crate::scroll::ScrollState;
use crate::tree::box_tree::BoxNode;
use crate::tree::fragment_tree::ScrollbarReservation;

use super::{FastRender, PreparedDocument};

/// Scroll- and sticky-aware DOM2 geometry queries.
///
/// This context is intended for DOM geometry APIs that need to mirror how the renderer positions
/// fragments for a given scroll state (e.g. `IntersectionObserver` root rect calculations or
/// `ResizeObserver` box metrics).
///
/// Coordinates returned by `*_in_viewport` helpers are in CSS pixels, relative to the top-left of
/// the layout viewport (i.e. translated by `-scroll_state.viewport`).
pub struct Dom2GeometryContext<'a> {
  prepared: &'a PreparedDocument,
  dom_mapping: &'a dom2::RendererDomMapping,
  fragment_tree: crate::tree::fragment_tree::FragmentTree,
  viewport_scroll: Point,
  viewport_size: Size,
}

impl<'a> Dom2GeometryContext<'a> {
  pub fn new(
    renderer: &FastRender,
    prepared: &'a PreparedDocument,
    dom_mapping: &'a dom2::RendererDomMapping,
    scroll_state: ScrollState,
  ) -> Self {
    let mut fragment_tree = prepared.fragment_tree().clone();

    // Mirror the paint pipeline: apply scroll snap before sticky offsets so sticky positioning is
    // computed against the snapped scroll state.
    let scroll_state = crate::scroll::resolve_effective_scroll_state_for_paint_mut(
      &mut fragment_tree,
      scroll_state,
      prepared.layout_viewport(),
    );

    renderer.apply_sticky_offsets_to_tree_with_scroll_state(&mut fragment_tree, &scroll_state);
    crate::scroll::apply_scroll_offsets(&mut fragment_tree, &scroll_state);
    crate::scroll::apply_viewport_scroll_cancel(&mut fragment_tree, &scroll_state);

    Self {
      prepared,
      dom_mapping,
      fragment_tree,
      viewport_scroll: scroll_state.viewport,
      // Use the visual viewport when resolving viewport-relative lengths, mirroring paint.
      viewport_size: prepared.visual_viewport(),
    }
  }

  fn styled_node_id_for_dom2_node(&self, node: dom2::NodeId) -> Option<usize> {
    self.dom_mapping.preorder_for_node_id(node)
  }

  fn boxes_for_styled_node_id(&self, styled_node_id: usize) -> Vec<&BoxNode> {
    let mut out = Vec::new();
    let mut stack: Vec<&BoxNode> = vec![&self.prepared.box_tree().root];

    while let Some(node) = stack.pop() {
      if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
        out.push(node);
      }

      // Mirror `BoxTree::assign_box_ids` traversal ordering.
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push(body);
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }

    out
  }

  fn translate_to_viewport(&self, rect: Rect) -> Rect {
    rect.translate(Point::new(-self.viewport_scroll.x, -self.viewport_scroll.y))
  }

  pub fn border_box_in_viewport(&self, node: dom2::NodeId) -> Option<Rect> {
    let styled_node_id = self.styled_node_id_for_dom2_node(node)?;
    let boxes = self.boxes_for_styled_node_id(styled_node_id);

    let mut out: Option<Rect> = None;
    for box_node in boxes {
      let Some(border_box_page) =
        crate::interaction::absolute_bounds_for_box_id(&self.fragment_tree, box_node.id)
      else {
        continue;
      };
      let border_box = self.translate_to_viewport(border_box_page);
      out = Some(match out {
        Some(existing) => existing.union(border_box),
        None => border_box,
      });
    }

    out
  }

  pub fn padding_box_in_viewport(&self, node: dom2::NodeId) -> Option<Rect> {
    let styled_node_id = self.styled_node_id_for_dom2_node(node)?;
    let boxes = self.boxes_for_styled_node_id(styled_node_id);

    let mut out: Option<Rect> = None;
    for box_node in boxes {
      let Some(border_box_page) =
        crate::interaction::absolute_bounds_for_box_id(&self.fragment_tree, box_node.id)
      else {
        continue;
      };
      let border_box = self.translate_to_viewport(border_box_page);
      let padding_box = crate::interaction::padding_rect_for_border_rect(
        border_box,
        &box_node.style,
        self.viewport_size,
      );
      out = Some(match out {
        Some(existing) => existing.union(padding_box),
        None => padding_box,
      });
    }

    out
  }

  pub fn content_box_in_viewport(&self, node: dom2::NodeId) -> Option<Rect> {
    let styled_node_id = self.styled_node_id_for_dom2_node(node)?;
    let boxes = self.boxes_for_styled_node_id(styled_node_id);

    let mut out: Option<Rect> = None;
    for box_node in boxes {
      let Some(border_box_page) =
        crate::interaction::absolute_bounds_for_box_id(&self.fragment_tree, box_node.id)
      else {
        continue;
      };
      let border_box = self.translate_to_viewport(border_box_page);
      let content_box = crate::interaction::content_rect_for_border_rect(
        border_box,
        &box_node.style,
        self.viewport_size,
      );
      out = Some(match out {
        Some(existing) => existing.union(content_box),
        None => content_box,
      });
    }

    out
  }

  /// Returns the scrollport/client rect (padding box minus reserved scrollbar gutters) in viewport
  /// coordinates.
  ///
  /// This should be preferred over `padding_box_in_viewport` when computing observer roots and other
  /// "visible scrollport" geometry, because it accounts for classic (non-overlay) scrollbars / `scrollbar-gutter: stable`
  /// reservations captured during layout.
  pub fn scrollport_box_in_viewport(&self, node: dom2::NodeId) -> Option<Rect> {
    let padding_box = self.padding_box_in_viewport(node)?;
    let styled_node_id = self.styled_node_id_for_dom2_node(node)?;
    let boxes = self.boxes_for_styled_node_id(styled_node_id);

    // Prefer the principal box id (first in pre-order) when looking up scrollbar gutters.
    let mut reservation = ScrollbarReservation::default();
    for box_node in boxes {
      if let Some(found) =
        crate::interaction::scrollbar_reservation_for_box_id(&self.fragment_tree, box_node.id)
      {
        reservation = found;
        break;
      }
    }

    Some(crate::interaction::scrollport_rect_for_padding_rect(
      padding_box,
      reservation,
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::{FastRenderConfig, FontConfig, LayoutParallelism, PaintParallelism, RenderOptions, RuntimeToggles};
  use crate::dom2::Document;

  fn test_renderer() -> FastRender {
    let config = FastRenderConfig::default()
      .with_runtime_toggles(RuntimeToggles::default())
      .with_font_sources(FontConfig::bundled_only())
      .with_paint_parallelism(PaintParallelism::disabled())
      .with_layout_parallelism(LayoutParallelism::disabled());
    FastRender::with_config(config).expect("renderer")
  }

  fn mapping_for_prepared(prepared: &PreparedDocument) -> (Document, dom2::RendererDomMapping) {
    let doc = Document::from_renderer_dom(prepared.dom());
    let snapshot = doc.to_renderer_dom_with_mapping();
    (doc, snapshot.mapping)
  }

  #[test]
  fn viewport_fixed_position_cancels_viewport_scroll() {
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #fixed { position: fixed; left: 20px; top: 10px; width: 30px; height: 40px; }
            #spacer { height: 2000px; }
          </style>
        </head>
        <body>
          <div id="fixed"></div>
          <div id="spacer"></div>
        </body>
      </html>"#;

    let mut renderer = test_renderer();
    let prepared = renderer
      .prepare_html(html, RenderOptions::new().with_viewport(200, 100))
      .expect("prepare_html");

    let (doc, mapping) = mapping_for_prepared(&prepared);
    let fixed = doc.get_element_by_id("fixed").expect("fixed element");

    let scroll_state = ScrollState::with_viewport(Point::new(0.0, 50.0));
    let ctx = Dom2GeometryContext::new(&renderer, &prepared, &mapping, scroll_state);

    let rect = ctx.border_box_in_viewport(fixed).expect("fixed rect");
    assert_eq!(rect, Rect::from_xywh(20.0, 10.0, 30.0, 40.0));
  }

  #[test]
  fn fixed_inside_fixed_containing_block_does_not_cancel_viewport_scroll() {
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #container { transform: translateX(0px); }
            #fixed { position: fixed; left: 20px; top: 10px; width: 30px; height: 40px; }
            #spacer { height: 2000px; }
          </style>
        </head>
        <body>
          <div id="container">
            <div id="fixed"></div>
          </div>
          <div id="spacer"></div>
        </body>
      </html>"#;

    let mut renderer = test_renderer();
    let prepared = renderer
      .prepare_html(html, RenderOptions::new().with_viewport(200, 100))
      .expect("prepare_html");

    let (doc, mapping) = mapping_for_prepared(&prepared);
    let fixed = doc.get_element_by_id("fixed").expect("fixed element");

    let scroll_state = ScrollState::with_viewport(Point::new(0.0, 50.0));
    let ctx = Dom2GeometryContext::new(&renderer, &prepared, &mapping, scroll_state);

    let rect = ctx.border_box_in_viewport(fixed).expect("fixed rect");
    assert_eq!(rect, Rect::from_xywh(20.0, -40.0, 30.0, 40.0));
  }

  #[test]
  fn nested_fixed_does_not_double_cancel_viewport_scroll() {
    let html = r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #outer { position: fixed; left: 20px; top: 10px; width: 100px; height: 100px; }
            #inner { position: fixed; left: 5px; top: 6px; width: 30px; height: 40px; }
            #spacer { height: 2000px; }
          </style>
        </head>
        <body>
          <div id="outer">
            <div id="inner"></div>
          </div>
          <div id="spacer"></div>
        </body>
      </html>"#;

    let mut renderer = test_renderer();
    let prepared = renderer
      .prepare_html(html, RenderOptions::new().with_viewport(200, 100))
      .expect("prepare_html");

    let (doc, mapping) = mapping_for_prepared(&prepared);
    let outer = doc.get_element_by_id("outer").expect("outer element");
    let inner = doc.get_element_by_id("inner").expect("inner element");

    let scroll_state = ScrollState::with_viewport(Point::new(0.0, 50.0));
    let ctx = Dom2GeometryContext::new(&renderer, &prepared, &mapping, scroll_state);

    let outer_rect = ctx.border_box_in_viewport(outer).expect("outer rect");
    assert_eq!(outer_rect, Rect::from_xywh(20.0, 10.0, 100.0, 100.0));

    let inner_rect = ctx.border_box_in_viewport(inner).expect("inner rect");
    assert_eq!(inner_rect, Rect::from_xywh(5.0, 6.0, 30.0, 40.0));
  }
}
