//! Anonymous box creation
//!
//! Implements CSS anonymous box generation rules to ensure the box tree
//! satisfies CSS structural constraints.
//!
//! # CSS Specifications
//!
//! - CSS 2.1 Section 9.2.1.1: Anonymous block boxes
//! - CSS 2.1 Section 9.2.2.1: Anonymous inline boxes
//! - CSS 2.1 Section 17.2.1: Anonymous table boxes
//!
//! # Anonymous Box Types
//!
//! ## Anonymous Block Boxes
//!
//! When a block container has both block-level and inline-level children,
//! the inline-level children are wrapped in anonymous block boxes.
//!
//! ```text
//! Before:                     After:
//! Block                       Block
//! ├── Text "Hello"            ├── AnonymousBlock
//! ├── Block (p)               │   └── Text "Hello"
//! └── Text "World"            ├── Block (p)
//!                             └── AnonymousBlock
//!                                 └── Text "World"
//! ```
//!
//! ## Anonymous Inline Boxes
//!
//! Text that isn't inside an inline element is wrapped in anonymous inline boxes.
//! This ensures all text has a containing inline box.
//!
//! # Usage
//!
//! ```ignore
//! use fastrender::tree::anonymous::AnonymousBoxCreator;
//!
//! let box_tree = generator.generate(&dom)?;
//! let fixed_tree = AnonymousBoxCreator::fixup_tree(box_tree)?;
//! ```

use crate::error::{RenderStage, Result};
use crate::render_control::active_deadline;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::float::{Clear, Float};
use crate::style::position::Position;
use crate::style::types::WhiteSpace;
use crate::style::ComputedStyle;
use crate::tree::box_tree::AnonymousBox;
use crate::tree::box_tree::AnonymousType;
use crate::tree::box_tree::BoxNode;
use crate::tree::box_tree::BoxType;
use crate::tree::box_tree::GeneratedPseudoElement;
use crate::tree::box_tree::InlineBox;
use crate::tree::debug::DebugInfo;
use std::sync::Arc;

/// Creates anonymous boxes in a box tree
///
/// This struct provides static methods to transform a box tree by inserting
/// anonymous boxes where CSS structural rules require them.
///
/// # CSS 2.1 Section 9.2.1.1
///
/// "If a block container box has a block-level box inside it, then we force
/// it to have only block-level boxes inside it."
///
/// This is accomplished by wrapping runs of inline-level content in anonymous
/// block boxes.
pub struct AnonymousBoxCreator;

const ANON_FIXUP_DEADLINE_STRIDE: usize = 256;

enum InlineSplitOutcome {
  Unchanged(BoxNode),
  Split(Vec<BoxNode>),
}

impl AnonymousBoxCreator {
  fn trim_ascii_whitespace(value: &str) -> &str {
    value.trim_matches(|c: char| {
      matches!(
        c,
        '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}'
      )
    })
  }

  /// Fixes up a box tree by inserting anonymous boxes
  ///
  /// This is a post-processing step after initial box generation. It traverses
  /// the tree recursively and inserts anonymous boxes where required by CSS rules.
  ///
  /// # Algorithm
  ///
  /// 1. Recursively process all children first (bottom-up)
  /// 2. Then fix up this node's children based on its type
  /// 3. Block containers get block fixup (wrap inline runs)
  /// 4. Inline containers get inline fixup (wrap text nodes)
  ///
  /// # CSS 2.1 Section 9.2.1.1
  ///
  /// "If a block container box has a block-level box inside it, then we force
  /// it to have only block-level boxes inside it."
  ///
  /// # Examples
  ///
  /// ```
  /// use std::sync::Arc;
  /// use fastrender::{BoxNode, FormattingContextType};
  /// use fastrender::tree::anonymous::AnonymousBoxCreator;
  /// use fastrender::ComputedStyle;
  /// # fn main() -> fastrender::Result<()> {
  ///
  /// let style = Arc::new(ComputedStyle::default());
  /// let text = BoxNode::new_text(style.clone(), "Hello".to_string());
  /// let block = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
  /// let container = BoxNode::new_block(
  ///     style.clone(),
  ///     FormattingContextType::Block,
  ///     vec![text, block],
  /// );
  ///
  /// let fixed = AnonymousBoxCreator::fixup_tree(container)?;
  /// // Text is now wrapped in anonymous block
  /// // fixed.children.len() == 2
  /// // fixed.children[0].is_anonymous()
  /// # Ok(())
  /// # }
  /// ```
  pub fn fixup_tree(box_node: BoxNode) -> Result<BoxNode> {
    let mut deadline_counter = 0usize;
    Self::fixup_tree_with_deadline(box_node, &mut deadline_counter)
  }

  /// Fixes up a box tree while periodically checking the active render deadline.
  pub fn fixup_tree_with_deadline(
    box_node: BoxNode,
    deadline_counter: &mut usize,
  ) -> Result<BoxNode> {
    let deadline = active_deadline();
    let deadline = deadline.as_ref();
    if let Some(deadline) = deadline {
      deadline.check_periodic(
        deadline_counter,
        ANON_FIXUP_DEADLINE_STRIDE,
        RenderStage::BoxTree,
      )?;
    }

    // Bottom-up fixup needs post-order traversal (children first, then parent). The previous
    // implementation used recursion, which can overflow the stack on pathological DOMs with
    // tens of thousands of nodes. Walk the tree iteratively with an explicit stack.
    struct Frame {
      node: *mut BoxNode,
      next_child_idx: usize,
      visited_footnote_body: bool,
    }

    let mut root = box_node;
    let mut stack = vec![Frame {
      node: &mut root as *mut BoxNode,
      next_child_idx: 0,
      visited_footnote_body: false,
    }];

    while let Some(frame) = stack.last_mut() {
      // SAFETY: `BoxNode`s aren't moved while they have active frames on the stack. We only
      // replace a node's `children` Vec after all child frames have been popped, so there are
      // no outstanding pointers into that Vec at mutation time.
      let node = unsafe { &mut *frame.node };

      if !frame.visited_footnote_body {
        frame.visited_footnote_body = true;
        if let Some(body) = node.footnote_body.as_deref_mut() {
          if let Some(deadline) = deadline {
            deadline.check_periodic(
              deadline_counter,
              ANON_FIXUP_DEADLINE_STRIDE,
              RenderStage::BoxTree,
            )?;
          }

          stack.push(Frame {
            node: body as *mut BoxNode,
            next_child_idx: 0,
            visited_footnote_body: false,
          });
          continue;
        }
      }

      if frame.next_child_idx < node.children.len() {
        let idx = frame.next_child_idx;
        frame.next_child_idx += 1;

        if let Some(deadline) = deadline {
          deadline.check_periodic(
            deadline_counter,
            ANON_FIXUP_DEADLINE_STRIDE,
            RenderStage::BoxTree,
          )?;
        }

        let child_ptr = &mut node.children[idx] as *mut BoxNode;
        stack.push(Frame {
          node: child_ptr,
          next_child_idx: 0,
          visited_footnote_body: false,
        });
        continue;
      }

      let children = std::mem::take(&mut node.children);
      let parent_style = node.style.clone();
      node.children = Self::fixup_children(children, &node.box_type, &parent_style);
      stack.pop();
    }

    Ok(root)
  }

  /// Fixes up children based on parent box type
  ///
  /// Dispatches to appropriate fixup method based on whether parent is
  /// a block container or inline container.
  fn fixup_children(
    children: Vec<BoxNode>,
    parent_type: &BoxType,
    parent_style: &Arc<ComputedStyle>,
  ) -> Vec<BoxNode> {
    if children.is_empty() {
      return children;
    }

    match parent_type {
      // Block containers need block fixup (CSS 2.1 §9.2.1.1). However, flex/grid/table formatting
      // contexts are not block formatting contexts; their direct children participate as
      // flex/grid/table items and must not be wrapped into anonymous block runs.
      BoxType::Block(block) => match block.formatting_context {
        FormattingContextType::Flex | FormattingContextType::Grid => {
          Self::fixup_flex_or_grid_children(children, parent_style.as_ref())
        }
        FormattingContextType::Table => children,
        FormattingContextType::Block | FormattingContextType::Inline => {
          Self::fixup_block_children(children, parent_style.as_ref())
        }
      },

      // Inline-level boxes can still establish their own formatting context (e.g. `inline-block`,
      // `inline-flex`). In that case, their children must satisfy the same anonymous-box
      // invariants as a block container with the corresponding formatting context.
      //
      // This matters for common real-world patterns like `<a><div>...</div></a>` inside an
      // inline-block: the inline `<a>` can contain a block-level box (valid HTML, but violates
      // CSS2's "no blocks in inlines" rule). If we treat the inline-block as an inline container
      // here, we won't split that illegal inline, and later layout will overlap subsequent block
      // siblings (Engadget relies on this for feature-card images).
      BoxType::Inline(inline) => match inline.formatting_context {
        None => Self::fixup_inline_children(children, parent_style),
        Some(FormattingContextType::Flex | FormattingContextType::Grid) => {
          Self::fixup_flex_or_grid_children(children, parent_style.as_ref())
        }
        Some(FormattingContextType::Table) => children,
        Some(FormattingContextType::Block | FormattingContextType::Inline) => {
          Self::fixup_block_children(children, parent_style.as_ref())
        }
      },

      // Anonymous block containers also need block fixup
      BoxType::Anonymous(anon)
        if matches!(
          anon.anonymous_type,
          AnonymousType::Block | AnonymousType::FieldsetContent | AnonymousType::TableCell
        ) =>
      {
        Self::fixup_block_children(children, parent_style.as_ref())
      }

      // Anonymous inline containers need inline fixup
      BoxType::Anonymous(anon) if matches!(anon.anonymous_type, AnonymousType::Inline) => {
        Self::fixup_inline_children(children, parent_style)
      }

      // Text boxes, replaced elements, and others don't need fixup
      _ => children,
    }
  }

  /// Fixes up direct children of flex/grid containers.
  ///
  /// Flex/grid layout items are not subject to the CSS 2.1 anonymous block run wrapping rule,
  /// because each in-flow child participates as a separate flex/grid item.
  ///
  /// However, text nodes *do* generate anonymous flex/grid items. If we leave `BoxType::Text`
  /// nodes directly under the flex/grid container, later layout stages may drop them entirely
  /// because they are not element boxes. Wrap each non-collapsible text node into its own
  /// anonymous block container so it participates as an item, matching browser behavior for
  /// anonymous flex/grid items.
  fn fixup_flex_or_grid_children(
    children: Vec<BoxNode>,
    parent_style: &ComputedStyle,
  ) -> Vec<BoxNode> {
    let Some(first_text_idx) = children
      .iter()
      .position(|child| matches!(child.box_type, BoxType::Text(_)))
    else {
      return children;
    };

    let mut result = Vec::with_capacity(children.len());
    let mut anon_block_style: Option<Arc<ComputedStyle>> = None;

    // Preserve allocation for the common prefix with no text nodes.
    let mut iter = children.into_iter();
    result.extend(iter.by_ref().take(first_text_idx));

    for child in iter {
      if matches!(child.box_type, BoxType::Text(_)) {
        if Self::is_collapsible_whitespace_text_node(&child) {
          continue;
        }

        let style = anon_block_style.get_or_insert_with(|| {
          let mut style = inherited_style(parent_style);
          style.display = Display::Block;
          Arc::new(style)
        });
        result.push(Self::create_anonymous_block(style.clone(), vec![child]));
      } else {
        result.push(child);
      }
    }

    result
  }

  /// Determines whether a child should participate as inline-level content for fixup.
  ///
  /// Replaced elements with inline display behave as inline-level boxes even though
  /// `BoxType::is_inline_level` returns false.
  fn is_inline_level_child(child: &BoxNode) -> bool {
    // Out-of-flow positioned boxes do not participate in anonymous in-flow box fixup.
    // Treat them as neither inline nor block so they remain as siblings in tree order.
    if matches!(child.style.position, Position::Absolute | Position::Fixed) {
      return false;
    }
    // Floats are out-of-flow and should not participate in the anonymous block-run splitting
    // decision (CSS 2.1 §9.2.1.1 only considers in-flow block-level boxes). Treat them as neither
    // inline nor block for the purposes of determining mixed content.
    if child.style.float != Float::None {
      return false;
    }
    match &child.box_type {
      BoxType::Replaced(_) => child.style.display.is_inline_level(),
      _ => child.is_inline_level(),
    }
  }

  /// Returns true if this child should be included in an inline run when splitting mixed inline
  /// and block-level content into anonymous block boxes.
  ///
  /// Unlike `is_inline_level_child`, this includes floating boxes so they stay inside the inline
  /// run where they occur in the source, matching the float insertion rules in CSS 2.1 §9.5.1.
  fn is_inline_run_child(child: &BoxNode) -> bool {
    if matches!(child.style.position, Position::Absolute | Position::Fixed) {
      return false;
    }
    if child.style.float.is_floating() {
      return true;
    }
    match &child.box_type {
      BoxType::Replaced(_) => child.style.display.is_inline_level(),
      _ => child.is_inline_level(),
    }
  }

  /// Determines whether a child should participate as block-level content for fixup.
  ///
  /// Replaced elements with inline display are treated as inline-level and shouldn't
  /// be counted as blocks when deciding anonymous box insertion.
  fn is_block_level_child(child: &BoxNode) -> bool {
    // Out-of-flow positioned boxes do not participate in anonymous in-flow box fixup.
    if matches!(child.style.position, Position::Absolute | Position::Fixed) {
      return false;
    }
    if child.style.float != Float::None {
      return false;
    }
    match &child.box_type {
      BoxType::Replaced(_) => !child.style.display.is_inline_level(),
      _ => child.is_block_level(),
    }
  }

  /// Returns true if an inline box (without its own formatting context) contains
  /// any block-level descendants.
  fn inline_contains_block_descendants(node: &BoxNode) -> bool {
    if node.formatting_context().is_some() {
      return false;
    }

    let mut stack: Vec<&BoxNode> = Vec::new();
    stack.push(node);
    while let Some(current) = stack.pop() {
      for child in &current.children {
        if Self::is_block_level_child(child) {
          return true;
        }
        if Self::is_inline_level_child(child)
          && child.formatting_context().is_none()
          && !child.children.is_empty()
        {
          stack.push(child);
        }
      }
    }

    false
  }

  /// Splits an inline box around any block-level descendants, returning either the original
  /// box (when no split is required) or a list of boxes that should replace the original
  /// inline in its parent.
  ///
  /// Inline fragments preserve the original inline's style/debug info; block descendants
  /// are lifted into the returned list so callers can place them directly in the block
  /// formatting context.
  fn split_inline_with_blocks(inline: BoxNode) -> InlineSplitOutcome {
    // Fast path: nothing under this inline can violate the "no blocks inside inlines" rule.
    // This avoids allocating replacement vectors for the overwhelmingly common case.
    if inline.formatting_context().is_some()
      || inline.children.is_empty()
      || !Self::inline_contains_block_descendants(&inline)
    {
      return InlineSplitOutcome::Unchanged(inline);
    }

    struct Frame {
      node: BoxNode,
      children: std::vec::IntoIter<BoxNode>,
      inline_run: Vec<BoxNode>,
      out: Vec<BoxNode>,
      had_block: bool,
    }

    impl Frame {
      fn new(mut node: BoxNode) -> Self {
        let children_vec = std::mem::take(&mut node.children);
        let inline_run = Vec::with_capacity(children_vec.len());
        let children = children_vec.into_iter();
        Self {
          node,
          children,
          inline_run,
          out: Vec::new(),
          had_block: false,
        }
      }

      fn flush_inline_run(&mut self) {
        if self.inline_run.is_empty() {
          return;
        }
        let fragment = AnonymousBoxCreator::clone_inline_fragment(
          &self.node.style,
          self.node.starting_style.clone(),
          self.node.footnote_body.clone(),
          &self.node.box_type,
          self.node.debug_info.as_ref(),
          self.node.styled_node_id,
          self.node.generated_pseudo,
          std::mem::take(&mut self.inline_run),
        );
        self.out.push(fragment);
      }

      fn push_piece(&mut self, piece: BoxNode) {
        if AnonymousBoxCreator::is_block_level_child(&piece) {
          self.had_block = true;
          self.flush_inline_run();
          self.out.push(piece);
        } else {
          self.inline_run.push(piece);
        }
      }
    }

    let mut stack: Vec<Frame> = Vec::new();
    stack.push(Frame::new(inline));

    loop {
      let Some(top) = stack.last_mut() else {
        // Defensive: `split_inline_level_block_descendants` always pushes an initial frame onto
        // the stack, so reaching an empty stack here indicates a logic error elsewhere. Don't
        // panic; instead, fall back to producing no split pieces.
        return InlineSplitOutcome::Split(Vec::new());
      };

      if let Some(child) = top.children.next() {
        if Self::is_block_level_child(&child) {
          top.had_block = true;
          top.flush_inline_run();
          top.out.push(child);
          continue;
        }

        if Self::is_inline_level_child(&child)
          && child.formatting_context().is_none()
          && !child.children.is_empty()
        {
          stack.push(Frame::new(child));
          continue;
        }

        top.inline_run.push(child);
        continue;
      }

      let Some(mut finished) = stack.pop() else {
        // Defensive: `stack` was non-empty at the top of the loop (we just had a `top` frame),
        // so this should be unreachable. If it happens anyway, avoid panicking and fall back to
        // producing no split pieces.
        return InlineSplitOutcome::Split(Vec::new());
      };
      if finished.had_block {
        finished.flush_inline_run();
        let pieces = finished.out;
        if let Some(parent) = stack.last_mut() {
          for piece in pieces {
            parent.push_piece(piece);
          }
          continue;
        }
        return InlineSplitOutcome::Split(pieces);
      }

      finished.node.children = finished.inline_run;
      let node = finished.node;
      if let Some(parent) = stack.last_mut() {
        parent.inline_run.push(node);
        continue;
      }
      return InlineSplitOutcome::Unchanged(node);
    }
  }

  /// Splits inline-level children that contain block-level descendants so that block
  /// boxes participate directly in the parent's block formatting context.
  fn split_inline_children_with_block_descendants(mut children: Vec<BoxNode>) -> Vec<BoxNode> {
    // Fast path: if no inline-level child contains block descendants, return the original Vec to
    // preserve its allocation.
    let Some(first_split_idx) = children.iter().position(|child| {
      Self::is_inline_level_child(child)
        && child.formatting_context().is_none()
        && Self::inline_contains_block_descendants(child)
    }) else {
      return children;
    };

    let mut result = Vec::with_capacity(children.len());
    result.extend(children.drain(..first_split_idx));

    let mut iter = children.into_iter();
    let Some(first_child) = iter.next() else {
      // Defensive: `first_split_idx` came from `.position(...)` on the original `children`, so it
      // should always point at an in-bounds element. If something goes wrong, preserve the
      // already-drained prefix.
      return result;
    };
    // We already proved the first child needs splitting.
    match Self::split_inline_with_blocks(first_child) {
      InlineSplitOutcome::Unchanged(node) => result.push(node),
      InlineSplitOutcome::Split(pieces) => result.extend(pieces),
    }

    for child in iter {
      if Self::is_inline_level_child(&child)
        && child.formatting_context().is_none()
        && Self::inline_contains_block_descendants(&child)
      {
        match Self::split_inline_with_blocks(child) {
          InlineSplitOutcome::Unchanged(node) => result.push(node),
          InlineSplitOutcome::Split(pieces) => result.extend(pieces),
        }
      } else {
        result.push(child);
      }
    }

    result
  }

  /// Clones an inline box fragment preserving the inline/anonymous inline identity and debug info.
  fn clone_inline_fragment(
    style: &Arc<ComputedStyle>,
    starting_style: Option<Arc<ComputedStyle>>,
    footnote_body: Option<Box<BoxNode>>,
    box_type: &BoxType,
    debug_info: Option<&DebugInfo>,
    styled_node_id: Option<usize>,
    generated_pseudo: Option<GeneratedPseudoElement>,
    children: Vec<BoxNode>,
  ) -> BoxNode {
    let fragment = match box_type {
      BoxType::Inline(inline) => BoxNode {
        style: style.clone(),
        original_display: style.display,
        starting_style: starting_style.clone(),
        box_type: BoxType::Inline(InlineBox {
          formatting_context: inline.formatting_context,
        }),
        children,
        footnote_body: footnote_body.clone(),
        id: 0,
        debug_info: debug_info.cloned(),
        styled_node_id,
        generated_pseudo,
        implicit_anchor_box_id: None,
        form_control: None,
        table_cell_span: None,
        table_column_span: None,
        first_line_style: None,
        first_letter_style: None,
      },
      BoxType::Anonymous(anon) if matches!(anon.anonymous_type, AnonymousType::Inline) => BoxNode {
        style: style.clone(),
        original_display: style.display,
        starting_style: starting_style.clone(),
        box_type: BoxType::Anonymous(anon.clone()),
        children,
        footnote_body: footnote_body.clone(),
        id: 0,
        debug_info: debug_info.cloned(),
        styled_node_id,
        generated_pseudo,
        implicit_anchor_box_id: None,
        form_control: None,
        table_cell_span: None,
        table_column_span: None,
        first_line_style: None,
        first_letter_style: None,
      },
      _ => {
        let mut node = BoxNode::new_inline(style.clone(), children);
        node.debug_info = debug_info.cloned();
        node.styled_node_id = styled_node_id;
        node.generated_pseudo = generated_pseudo;
        node.starting_style = starting_style;
        node.footnote_body = footnote_body;
        node
      }
    };

    fragment
  }

  /// Fixes up children of block containers
  ///
  /// CSS 2.1 Section 9.2.1.1: Block containers can only contain:
  /// - All block-level boxes, OR
  /// - All inline-level boxes (establishes inline formatting context)
  ///
  /// If mixed, we create anonymous block boxes to wrap inline content.
  ///
  /// # Algorithm
  ///
  /// 1. Check if children are all block-level, all inline-level, or mixed
  /// 2. If all block-level: no changes needed
  /// 3. If all inline-level: wrap any bare text in anonymous inline boxes
  /// 4. If mixed: wrap runs of inline content in anonymous block boxes
  fn fixup_block_children(children: Vec<BoxNode>, parent_style: &ComputedStyle) -> Vec<BoxNode> {
    // First, split any inline boxes that illegally contain block-level descendants
    // so the block descendants participate directly in the block formatting context.
    // Fast path: if there are no inline-level children, there can't be any illegal
    // "inline contains block" situations either.
    let mut children = children;
    if !children.iter().any(Self::is_inline_level_child) {
      return children;
    }
    children = Self::split_inline_children_with_block_descendants(children);

    // Determine what kind of content we have after splitting
    let mut has_block = false;
    let mut has_inline = false;
    for child in &children {
      has_block |= Self::is_block_level_child(child);
      has_inline |= Self::is_inline_level_child(child);
      if has_block && has_inline {
        break;
      }
    }

    if has_block && has_inline {
      // Mixed content - wrap inline runs in anonymous blocks
      Self::wrap_inline_runs_in_anonymous_blocks(children, parent_style)
    } else if !has_block && has_inline {
      // All inline content - wrap bare text in anonymous inlines
      // (This maintains proper inline structure)
      Self::wrap_bare_text_in_anonymous_inline(children, parent_style)
    } else {
      // All block or empty - no fixup needed
      children
    }
  }

  /// Fixes up children of inline containers
  ///
  /// Inline boxes should only contain inline-level content.
  /// Bare text nodes are wrapped in anonymous inline boxes.
  ///
  /// CSS 2.1 Section 9.2.2.1: "Any text that is directly contained inside
  /// a block container element (not inside an inline element) must be
  /// treated as an anonymous inline element."
  fn fixup_inline_children(
    children: Vec<BoxNode>,
    parent_style: &Arc<ComputedStyle>,
  ) -> Vec<BoxNode> {
    // Wrap bare text nodes in anonymous inline boxes while reusing the existing Vec allocation.
    let mut children = children;
    let mut inline_style: Option<Arc<ComputedStyle>> = None;

    for child in children.iter_mut() {
      if !matches!(&child.box_type, BoxType::Text(_)) {
        continue;
      }

      // Anonymous inline boxes must inherit only inheritable properties (CSS 2.1 §9.2.2.1).
      //
      // Cloning the full parent style (including padding/border/position/etc) effectively duplicates
      // those non-inherited properties on the anonymous wrapper, which can dramatically distort
      // inline layout (e.g. button text nodes causing repeated padding and unexpected wrapping).
      let style = inline_style.get_or_insert_with(|| {
        // CSS 2.1 §9.2.2.1 anonymous inline boxes: they inherit inheritable properties from the
        // parent element, but otherwise use initial values.
        //
        // Non-inherited box properties like padding/border/positioning must *not* be copied onto
        // the anonymous wrapper. Doing so effectively applies those properties twice when a
        // blockified inline element (e.g. an inline flex item) lays out its inline children,
        // inflating line boxes and the element's used height (notably for inline-flex buttons).
        // https://www.w3.org/TR/CSS21/visuren.html#anonymous
        let mut style = inherited_style(parent_style.as_ref());
        style.display = Display::Inline;
        Arc::new(style)
      });

      Self::wrap_text_in_anonymous_inline_in_place(child, style.clone());
    }

    children
  }

  /// Wraps consecutive inline boxes in anonymous block boxes
  ///
  /// When a block container has mixed block/inline content, this function
  /// groups consecutive inline-level children and wraps each group in an
  /// anonymous block box.
  ///
  /// # Example
  ///
  /// ```text
  /// Input:  [Text, Text, Block, Inline, Block]
  /// Output: [AnonymousBlock[Text, Text], Block, AnonymousBlock[Inline], Block]
  /// ```
  ///
  /// Consecutive inline elements are wrapped together in a single anonymous
  /// block, preserving the document order.
  fn wrap_inline_runs_in_anonymous_blocks(
    children: Vec<BoxNode>,
    parent_style: &ComputedStyle,
  ) -> Vec<BoxNode> {
    let mut result = Vec::with_capacity(children.len());
    let mut inline_run: Vec<BoxNode> = Vec::new();
    let mut anonymous_block_style: Option<Arc<ComputedStyle>> = None;
    let mut flush_inline_run = |result: &mut Vec<BoxNode>, inline_run: &mut Vec<BoxNode>| {
      if inline_run.is_empty() {
        return;
      }
      if inline_run
        .iter()
        .all(Self::is_collapsible_whitespace_text_node)
      {
        inline_run.clear();
        return;
      }
      // Runs that contain only floats + collapsible whitespace (common clearfix pattern) should not
      // be wrapped into an anonymous block box. Doing so makes the floats descendants of the
      // anonymous wrapper, which prevents subsequent clearing blocks/pseudo-elements from clearing
      // them in our layout engine and results in collapsed container heights.
      if inline_run.iter().all(|node| {
        node.style.float.is_floating() || Self::is_collapsible_whitespace_text_node(node)
      }) {
        let inline_run_nodes = std::mem::take(inline_run);
        for node in inline_run_nodes {
          if node.style.float.is_floating() {
            result.push(node);
          }
        }
        return;
      }
      let style = anonymous_block_style.get_or_insert_with(|| {
        let mut style = inherited_style(parent_style);
        style.display = Display::Block;
        Arc::new(style)
      });
      let anon_block = Self::create_anonymous_block(style.clone(), std::mem::take(inline_run));
      result.push(anon_block);
    };

    for child in children {
      if Self::is_inline_run_child(&child) {
        // Accumulate inline boxes
        inline_run.push(child);
      } else {
        // Block box encountered - flush any inline run
        // If the inline run ends with a `<br>` (BoxType::LineBreak) and is immediately followed by
        // a block-level box, browsers do not create an extra empty line box between the inline
        // content and the block. The following block starts at the beginning of the line after
        // the break, so drop exactly one trailing line break in this boundary case.
        //
        // Note: trailing collapsible whitespace text nodes are trimmed first so markup like
        // `text<br>\n  <div>block</div>` is treated the same as `text<br><div>block</div>`.
        if !inline_run.is_empty() {
          while inline_run
            .last()
            .is_some_and(|node| Self::is_collapsible_whitespace_text_node(node))
          {
            inline_run.pop();
          }
          if matches!(
            inline_run.last().map(|n| &n.box_type),
            Some(BoxType::LineBreak(_))
          ) {
            let has_non_break_content = inline_run.iter().any(|node| {
              !matches!(node.box_type, BoxType::LineBreak(_))
                && !Self::is_collapsible_whitespace_text_node(node)
            });
            // Preserve legacy clearing line breaks (`<br clear=...>`) because dropping them would
            // prevent float clearance from being applied (common clearfix pattern).
            let is_clearing = inline_run
              .last()
              .is_some_and(|node| node.style.clear != Clear::None);
            if has_non_break_content && !is_clearing {
              inline_run.pop();
            }
          }
        }
        flush_inline_run(&mut result, &mut inline_run);
        result.push(child);
      }
    }

    // Flush remaining inline run at end
    flush_inline_run(&mut result, &mut inline_run);

    result
  }

  fn is_collapsible_whitespace_text_node(node: &BoxNode) -> bool {
    match &node.box_type {
      BoxType::Text(text_box) => {
        Self::trim_ascii_whitespace(&text_box.text).is_empty()
          && matches!(
            node.style.white_space,
            WhiteSpace::Normal | WhiteSpace::Nowrap
          )
      }
      _ => false,
    }
  }

  /// Wraps bare text nodes in anonymous inline boxes
  ///
  /// CSS 2.1 Section 9.2.2.1: Text directly inside a block container
  /// that isn't inside an inline element gets an anonymous inline box.
  fn wrap_bare_text_in_anonymous_inline(
    children: Vec<BoxNode>,
    parent_style: &ComputedStyle,
  ) -> Vec<BoxNode> {
    let mut inherited_inline_style: Option<Arc<ComputedStyle>> = None;
    let mut children = children;
    for child in children.iter_mut() {
      if !matches!(&child.box_type, BoxType::Text(_)) {
        continue;
      }
      // Wrap text in anonymous inline.
      let style = inherited_inline_style.get_or_insert_with(|| {
        let mut inherited = inherited_style(parent_style);
        inherited.display = Display::Inline;
        Arc::new(inherited)
      });
      Self::wrap_text_in_anonymous_inline_in_place(child, style.clone());
    }

    children
  }

  fn wrap_text_in_anonymous_inline_in_place(node: &mut BoxNode, wrapper_style: Arc<ComputedStyle>) {
    if !matches!(&node.box_type, BoxType::Text(_)) {
      return;
    }

    let wrapper_box_type = BoxType::Anonymous(AnonymousBox {
      anonymous_type: AnonymousType::Inline,
    });

    let wrapper_original_display = wrapper_style.display;
    let text_node = BoxNode {
      style: std::mem::replace(&mut node.style, wrapper_style),
      original_display: std::mem::replace(&mut node.original_display, wrapper_original_display),
      starting_style: node.starting_style.take(),
      box_type: std::mem::replace(&mut node.box_type, wrapper_box_type),
      children: std::mem::take(&mut node.children),
      footnote_body: node.footnote_body.take(),
      id: std::mem::replace(&mut node.id, 0),
      debug_info: node.debug_info.take(),
      styled_node_id: node.styled_node_id.take(),
      generated_pseudo: node.generated_pseudo.take(),
      implicit_anchor_box_id: std::mem::take(&mut node.implicit_anchor_box_id),
      form_control: node.form_control.take(),
      table_cell_span: node.table_cell_span.take(),
      table_column_span: node.table_column_span.take(),
      first_line_style: node.first_line_style.take(),
      first_letter_style: node.first_letter_style.take(),
    };

    node.children = vec![text_node];
  }

  /// Creates an anonymous block box
  ///
  /// Anonymous block boxes are generated to satisfy the CSS constraint that
  /// block containers must contain either all block-level or all inline-level
  /// children.
  ///
  /// # Style Inheritance
  ///
  /// Anonymous boxes inherit computed style from their containing block.
  /// Currently uses default style as a placeholder - proper inheritance
  /// should be handled during style computation.
  pub fn create_anonymous_block(style: Arc<ComputedStyle>, children: Vec<BoxNode>) -> BoxNode {
    let style = if style.display == Display::Block {
      style
    } else {
      let mut style = (*style).clone();
      style.display = Display::Block;
      Arc::new(style)
    };
    let original_display = style.display;
    BoxNode {
      style,
      original_display,
      starting_style: None,
      box_type: BoxType::Anonymous(AnonymousBox {
        anonymous_type: AnonymousType::Block,
      }),
      children,
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      implicit_anchor_box_id: None,
      form_control: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Creates an anonymous inline box
  ///
  /// Anonymous inline boxes wrap text that isn't inside an inline element.
  ///
  /// CSS 2.1 Section 9.2.2.1: "Any text that is directly contained inside
  /// a block container element (not inside an inline element) must be
  /// treated as an anonymous inline element."
  pub fn create_anonymous_inline(style: Arc<ComputedStyle>, children: Vec<BoxNode>) -> BoxNode {
    let style = if style.display == Display::Inline {
      style
    } else {
      let mut style = (*style).clone();
      style.display = Display::Inline;
      Arc::new(style)
    };
    let original_display = style.display;
    BoxNode {
      style,
      original_display,
      starting_style: None,
      box_type: BoxType::Anonymous(AnonymousBox {
        anonymous_type: AnonymousType::Inline,
      }),
      children,
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      implicit_anchor_box_id: None,
      form_control: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Creates an anonymous table row box
  ///
  /// Used when table cells appear outside of table rows.
  pub fn create_anonymous_table_row(style: Arc<ComputedStyle>, children: Vec<BoxNode>) -> BoxNode {
    let original_display = style.display;
    BoxNode {
      style,
      original_display,
      starting_style: None,
      box_type: BoxType::Anonymous(AnonymousBox {
        anonymous_type: AnonymousType::TableRow,
      }),
      children,
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      implicit_anchor_box_id: None,
      form_control: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Creates an anonymous table cell box
  ///
  /// Used when non-table content appears inside table rows.
  pub fn create_anonymous_table_cell(style: Arc<ComputedStyle>, children: Vec<BoxNode>) -> BoxNode {
    let original_display = style.display;
    BoxNode {
      style,
      original_display,
      starting_style: None,
      box_type: BoxType::Anonymous(AnonymousBox {
        anonymous_type: AnonymousType::TableCell,
      }),
      children,
      footnote_body: None,
      id: 0,
      debug_info: None,
      styled_node_id: None,
      generated_pseudo: None,
      implicit_anchor_box_id: None,
      form_control: None,
      table_cell_span: None,
      table_column_span: None,
      first_line_style: None,
      first_letter_style: None,
    }
  }

  /// Checks if a list of children contains mixed block/inline content
  ///
  /// Returns true if children contain both block-level and inline-level boxes.
  pub fn has_mixed_content(children: &[BoxNode]) -> bool {
    let has_block = children.iter().any(Self::is_block_level_child);
    let has_inline = children.iter().any(Self::is_inline_level_child);
    has_block && has_inline
  }

  /// Checks if all children are block-level
  pub fn all_block_level(children: &[BoxNode]) -> bool {
    children.iter().all(Self::is_block_level_child)
  }

  /// Checks if all children are inline-level
  pub fn all_inline_level(children: &[BoxNode]) -> bool {
    children.iter().all(Self::is_inline_level_child)
  }

  /// Counts the number of anonymous boxes that would be created
  ///
  /// Useful for debugging and testing to verify fixup behavior.
  pub fn count_anonymous_boxes(node: &BoxNode) -> usize {
    let self_count = usize::from(node.is_anonymous());
    let children_count: usize = node.children.iter().map(Self::count_anonymous_boxes).sum();
    self_count + children_count
  }
}

/// Build a style that inherits inheritable properties from the parent while leaving
/// non-inherited properties at their initial values.
pub(crate) fn inherited_style(parent: &ComputedStyle) -> ComputedStyle {
  let mut style = ComputedStyle::default();
  // Typography / text
  style.font_family = parent.font_family.clone();
  style.font_size = parent.font_size;
  style.root_font_size = parent.root_font_size;
  style.font_weight = parent.font_weight;
  style.font_style = parent.font_style;
  style.font_variant = parent.font_variant;
  style.font_variant_caps = parent.font_variant_caps;
  style.font_variant_alternates = parent.font_variant_alternates.clone();
  style.font_variant_numeric = parent.font_variant_numeric;
  style.font_variant_east_asian = parent.font_variant_east_asian;
  style.font_variant_ligatures = parent.font_variant_ligatures;
  style.font_variant_position = parent.font_variant_position;
  style.font_size_adjust = parent.font_size_adjust;
  style.font_synthesis = parent.font_synthesis;
  style.font_feature_settings = parent.font_feature_settings.clone();
  style.font_optical_sizing = parent.font_optical_sizing;
  style.font_variation_settings = parent.font_variation_settings.clone();
  style.font_language_override = parent.font_language_override.clone();
  style.font_variant_emoji = parent.font_variant_emoji;
  style.font_stretch = parent.font_stretch;
  style.font_kerning = parent.font_kerning;
  style.line_height = parent.line_height.clone();
  style.direction = parent.direction;
  style.unicode_bidi = parent.unicode_bidi;
  style.text_align = parent.text_align;
  style.text_align_last = parent.text_align_last;
  style.text_justify = parent.text_justify;
  style.hanging_punctuation = parent.hanging_punctuation;
  style.text_rendering = parent.text_rendering;
  style.allow_subpixel_aa = parent.allow_subpixel_aa;
  style.font_smoothing = parent.font_smoothing;
  style.text_indent = parent.text_indent;
  style.text_wrap = parent.text_wrap;
  style.text_decoration_skip_box = parent.text_decoration_skip_box;
  style.text_decoration_skip_spaces = parent.text_decoration_skip_spaces;
  style.text_decoration_skip_ink = parent.text_decoration_skip_ink;
  style.text_shadow = parent.text_shadow.clone();
  style.text_underline_offset = parent.text_underline_offset;
  style.text_underline_position = parent.text_underline_position;
  style.text_emphasis_style = parent.text_emphasis_style.clone();
  style.text_emphasis_color = parent.text_emphasis_color;
  style.text_emphasis_position = parent.text_emphasis_position;
  style.text_emphasis_skip = parent.text_emphasis_skip;
  style.text_transform = parent.text_transform;
  style.text_combine_upright = parent.text_combine_upright;
  style.text_orientation = parent.text_orientation;
  style.writing_mode = parent.writing_mode;
  style.letter_spacing = parent.letter_spacing;
  style.word_spacing = parent.word_spacing;
  style.line_padding = parent.line_padding;
  style.text_spacing_trim = parent.text_spacing_trim;
  style.text_autospace = parent.text_autospace;
  style.justify_items = parent.justify_items;
  style.visibility = parent.visibility;
  style.visibility_is_inherited = true;
  style.white_space = parent.white_space;
  style.line_break = parent.line_break;
  style.tab_size = parent.tab_size;
  style.caption_side = parent.caption_side;
  style.empty_cells = parent.empty_cells;
  style.hyphens = parent.hyphens;
  style.hyphenate_character = parent.hyphenate_character.clone();
  style.word_break = parent.word_break;
  style.overflow_wrap = parent.overflow_wrap;
  style.language = parent.language.clone();
  style.list_style_type = parent.list_style_type.clone();
  style.list_style_position = parent.list_style_position;
  style.list_style_image = parent.list_style_image.clone();
  style.quotes = parent.quotes.clone();
  style.cursor = parent.cursor;
  style.cursor_images = parent.cursor_images.clone();
  style.interpolate_size = parent.interpolate_size;
  style.color = parent.color;
  style.color_is_inherited = true;
  style.custom_property_registry = parent.custom_property_registry.clone();
  style.custom_properties = parent.custom_properties.clone();
  style
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::style::color::Rgba;
  use crate::style::display::{Display, FormattingContextType};
  use crate::style::float::{Clear, Float};
  use crate::style::position::Position;
  use crate::style::types::BorderStyle;
  use crate::style::values::Length;
  use crate::tree::box_tree::MarkerContent;
  use crate::tree::table_fixup::TableStructureFixer;

  fn default_style() -> Arc<ComputedStyle> {
    Arc::new(ComputedStyle::default())
  }

  fn style_with_display(display: Display) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.display = display;
    Arc::new(style)
  }

  fn fixup_tree(node: BoxNode) -> BoxNode {
    super::AnonymousBoxCreator::fixup_tree(node).expect("anonymous fixup")
  }

  fn subtree_contains_text(node: &BoxNode, needle: &str) -> bool {
    if let Some(text) = node.text() {
      if text == needle {
        return true;
      }
    }
    node
      .children
      .iter()
      .any(|child| subtree_contains_text(child, needle))
  }

  #[test]
  fn anonymous_inline_wrappers_inside_inline_containers_do_not_duplicate_box_model() {
    // `fixup_inline_children` wraps bare text nodes in anonymous inline boxes so they participate in
    // the inline formatting context as an element box. Those anonymous boxes must not copy
    // non-inherited properties like padding/border/position from the parent inline container; doing
    // so effectively duplicates the parent's box model for every text node, which can drastically
    // distort layout (e.g. inline-block buttons with whitespace children).
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::InlineBlock;
    parent_style.position = Position::Relative;
    parent_style.padding_left = Length::px(10.0);
    parent_style.padding_right = Length::px(10.0);
    parent_style.padding_top = Length::px(5.0);
    parent_style.padding_bottom = Length::px(5.0);
    parent_style.border_left_width = Length::px(1.0);
    parent_style.border_right_width = Length::px(1.0);
    parent_style.border_top_width = Length::px(1.0);
    parent_style.border_bottom_width = Length::px(1.0);

    let text = BoxNode::new_text(default_style(), "hello".to_string());
    let inline_block = BoxNode::new_inline_block(
      Arc::new(parent_style),
      FormattingContextType::Block,
      vec![text],
    );

    let fixed = fixup_tree(inline_block);
    assert_eq!(fixed.children.len(), 1);
    let wrapper = &fixed.children[0];
    assert!(
      matches!(
        wrapper.box_type,
        BoxType::Anonymous(AnonymousBox {
          anonymous_type: AnonymousType::Inline
        })
      ),
      "expected text to be wrapped in an anonymous inline box"
    );
    assert_eq!(wrapper.style.display, Display::Inline);
    assert_eq!(wrapper.style.position, Position::Static);
    assert_eq!(wrapper.style.padding_left, Length::px(0.0));
    assert_eq!(wrapper.style.padding_right, Length::px(0.0));
    assert_eq!(wrapper.style.padding_top, Length::px(0.0));
    assert_eq!(wrapper.style.padding_bottom, Length::px(0.0));
    assert_eq!(wrapper.style.border_left_width, Length::px(0.0));
    assert_eq!(wrapper.style.border_right_width, Length::px(0.0));
    assert_eq!(wrapper.style.border_top_width, Length::px(0.0));
    assert_eq!(wrapper.style.border_bottom_width, Length::px(0.0));
  }

  #[test]
  fn non_ascii_whitespace_collapsible_whitespace_text_node_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let text = BoxNode::new_text(default_style(), nbsp.to_string());
    assert!(
      !AnonymousBoxCreator::is_collapsible_whitespace_text_node(&text),
      "NBSP must not be treated as collapsible whitespace"
    );
  }

  #[test]
  fn anonymous_fixup_traverses_footnote_body() {
    let style = default_style();
    let text = BoxNode::new_text(style.clone(), "Footnote".to_string());
    let block = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    let body = BoxNode::new_block(
      style.clone(),
      FormattingContextType::Block,
      vec![text, block],
    );

    let mut call = BoxNode::new_inline(style.clone(), vec![]);
    call.footnote_body = Some(Box::new(body));
    let root = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![call]);

    let fixed = fixup_tree(root);
    let call = fixed.children.first().expect("call box");
    let body = call.footnote_body.as_deref().expect("footnote body");

    assert_eq!(body.children.len(), 2);
    assert!(
      body.children[0].is_anonymous() && body.children[0].is_block_level(),
      "expected anonymous block wrapper in footnote body after fixup"
    );
    assert!(
      body.children[1].is_block_level(),
      "expected original block-level child to remain after fixup"
    );
  }

  #[test]
  fn test_fixup_deep_tree_stack_safe() {
    // This used to be recursive (both anonymous fixup and table fixup). Deep trees triggered
    // stack overflows in real-world pages; keep this test large enough to fail with recursion.
    let style = default_style();
    let mut node = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    for _ in 0..30_000 {
      node = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![node]);
    }

    let fixed = AnonymousBoxCreator::fixup_tree(node).expect("anonymous fixup");
    let fixed = TableStructureFixer::fixup_tree_internals(fixed).expect("table fixup");
    assert!(fixed.is_block_level());
    drop(fixed);
  }

  #[test]
  fn test_split_inline_with_blocks_stack_safe() {
    // Deeply nested inlines containing a block used to recurse both in the "contains block"
    // detection and in the splitting logic.
    let style = default_style();
    let block = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    let mut inline = BoxNode::new_inline(style.clone(), vec![block]);
    for _ in 0..20_000 {
      inline = BoxNode::new_inline(style.clone(), vec![inline]);
    }

    let root = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![inline]);
    let fixed = fixup_tree(root);
    assert_eq!(fixed.children.len(), 1);
    assert!(fixed.children[0].is_block_level());
  }

  #[test]
  fn inline_block_children_are_fixed_up_as_a_block_container() {
    // Inline-level boxes like inline-block establish a formatting context for their children.
    // Anonymous box fixup must treat them like block containers, otherwise we miss required
    // fixups such as splitting illegal "inline contains block" descendants.
    //
    // This pattern is extremely common in the wild (e.g. `<a><div>...</div></a>` inside an
    // inline-block wrapper).
    let style = default_style();
    let block_child = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    let inline_with_block = BoxNode::new_inline(style.clone(), vec![block_child]);

    let sibling_block = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);

    let mut inline_block_style = (*style).clone();
    inline_block_style.display = Display::InlineBlock;
    let inline_block = BoxNode::new_inline_block(
      Arc::new(inline_block_style),
      FormattingContextType::Block,
      vec![inline_with_block, sibling_block],
    );

    let fixed = fixup_tree(inline_block);
    assert_eq!(fixed.children.len(), 2);
    assert!(
      fixed.children.iter().all(|child| child.is_block_level()),
      "expected inline-block fixup to lift block descendants out of inline children"
    );
  }

  #[test]
  fn anonymous_inline_wrapper_does_not_copy_parent_padding() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::InlineFlex;
    parent_style.position = Position::Relative;
    parent_style.padding_top = Length::px(8.0);
    parent_style.padding_right = Length::px(8.0);
    parent_style.padding_bottom = Length::px(8.0);
    parent_style.padding_left = Length::px(8.0);
    let parent_style = Arc::new(parent_style);

    let text = BoxNode::new_text(default_style(), "Platform".to_string());
    let inline_flex =
      BoxNode::new_inline_block(parent_style, FormattingContextType::Flex, vec![text]);
    let fixed = AnonymousBoxCreator::fixup_tree(inline_flex).expect("anonymous fixup");

    let wrapper = fixed.children.first().expect("wrapped text");
    assert!(
      wrapper.is_anonymous(),
      "expected text node to be wrapped in an anonymous inline box"
    );
    assert_eq!(wrapper.style.display, Display::Inline);
    assert_eq!(wrapper.style.position, Position::Static);
    assert_eq!(wrapper.style.padding_top, Length::px(0.0));
    assert_eq!(wrapper.style.padding_right, Length::px(0.0));
    assert_eq!(wrapper.style.padding_bottom, Length::px(0.0));
    assert_eq!(wrapper.style.padding_left, Length::px(0.0));
  }

  #[test]
  fn test_empty_container_no_crash() {
    let empty_block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let fixed = fixup_tree(empty_block);
    assert_eq!(fixed.children.len(), 0);
    assert!(!fixed.is_anonymous());
  }

  #[test]
  fn test_all_block_children_no_change() {
    let child1 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let child2 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let child3 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![child1, child2, child3],
    );

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 3);
    for child in &fixed.children {
      assert!(!child.is_anonymous());
      assert!(child.is_block_level());
    }
  }

  #[test]
  fn test_single_text_in_block_wrapped() {
    let text = BoxNode::new_text(default_style(), "Hello".to_string());

    let container = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![text]);

    let fixed = fixup_tree(container);

    // Text should be wrapped in anonymous inline
    assert_eq!(fixed.children.len(), 1);
    assert!(fixed.children[0].is_anonymous());
    assert!(fixed.children[0].is_inline_level());
    assert_eq!(fixed.children[0].children.len(), 1);
    assert!(fixed.children[0].children[0].is_text());
  }

  #[test]
  fn anonymous_inline_wrapper_does_not_inherit_position() {
    let mut inline_style = (*default_style()).clone();
    inline_style.display = Display::Inline;
    inline_style.position = Position::Absolute;
    let inline_style = Arc::new(inline_style);

    let text = BoxNode::new_text(default_style(), "Hello".to_string());
    let inline = BoxNode::new_inline(inline_style, vec![text]);
    let root = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![inline]);

    let fixed = fixup_tree(root);
    let fixed_inline = fixed.children.first().expect("inline box");
    assert_eq!(fixed_inline.children.len(), 1);

    let anonymous = fixed_inline.children.first().expect("anonymous wrapper");
    assert!(
      matches!(
        anonymous.box_type,
        BoxType::Anonymous(AnonymousBox {
          anonymous_type: AnonymousType::Inline,
        })
      ),
      "expected text to be wrapped in an anonymous inline box"
    );
    assert_eq!(
      anonymous.style.position,
      Position::Static,
      "anonymous inline wrappers should not inherit `position` from parent inline boxes"
    );
  }

  #[test]
  fn marker_boxes_are_not_wrapped_in_anonymous_inlines() {
    let marker = BoxNode::new_marker(default_style(), MarkerContent::Text("•".to_string()));
    let text = BoxNode::new_text(default_style(), "Hello".to_string());

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![marker, text],
    );

    let fixed = fixup_tree(container);
    assert_eq!(fixed.children.len(), 2);

    assert!(
      matches!(&fixed.children[0].box_type, BoxType::Marker(_)),
      "marker should remain a direct child (not wrapped in anonymous inline)"
    );

    let wrapper = &fixed.children[1];
    assert!(
      wrapper.is_anonymous() && wrapper.is_inline_level(),
      "text should still be wrapped in an anonymous inline"
    );
    assert_eq!(wrapper.children.len(), 1);
    assert!(matches!(&wrapper.children[0].box_type, BoxType::Text(_)));
  }

  #[test]
  fn test_mixed_content_text_block_text() {
    let text1 = BoxNode::new_text(default_style(), "Text 1".to_string());
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let text2 = BoxNode::new_text(default_style(), "Text 2".to_string());

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text1, block, text2],
    );

    let fixed = fixup_tree(container);

    // Should have 3 children: anon block, block, anon block
    assert_eq!(fixed.children.len(), 3);

    // First child: anonymous block wrapping text1
    assert!(fixed.children[0].is_anonymous());
    assert!(fixed.children[0].is_block_level());
    assert_eq!(fixed.children[0].children.len(), 1);
    assert!(fixed.children[0].children[0].is_text());

    // Second child: original block
    assert!(!fixed.children[1].is_anonymous());
    assert!(fixed.children[1].is_block_level());

    // Third child: anonymous block wrapping text2
    assert!(fixed.children[2].is_anonymous());
    assert!(fixed.children[2].is_block_level());
    assert_eq!(fixed.children[2].children.len(), 1);
    assert!(fixed.children[2].children[0].is_text());
  }

  #[test]
  fn test_consecutive_inlines_grouped_in_single_anon_block() {
    let inline1 = BoxNode::new_inline(default_style(), vec![]);
    let inline2 = BoxNode::new_inline(default_style(), vec![]);
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let inline3 = BoxNode::new_inline(default_style(), vec![]);

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![inline1, inline2, block, inline3],
    );

    let fixed = fixup_tree(container);

    // Should have 3 children: anon[inline1, inline2], block, anon[inline3]
    assert_eq!(fixed.children.len(), 3);

    // First anonymous block contains 2 inlines
    assert!(fixed.children[0].is_anonymous());
    assert_eq!(fixed.children[0].children.len(), 2);

    // Second is the original block
    assert!(!fixed.children[1].is_anonymous());

    // Third anonymous block contains 1 inline
    assert!(fixed.children[2].is_anonymous());
    assert_eq!(fixed.children[2].children.len(), 1);
  }

  #[test]
  fn grid_containers_do_not_wrap_inline_runs_in_anonymous_blocks() {
    let mut grid_style = ComputedStyle::default();
    grid_style.display = Display::Grid;
    let grid_style = Arc::new(grid_style);

    let inline1 = BoxNode::new_inline(default_style(), vec![]);
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let inline2 = BoxNode::new_inline(default_style(), vec![]);

    let container = BoxNode::new_block(
      grid_style,
      FormattingContextType::Grid,
      vec![inline1, block, inline2],
    );

    let fixed = fixup_tree(container);
    assert_eq!(fixed.children.len(), 3);
    assert!(
      fixed.children.iter().all(|child| !child.is_anonymous()),
      "grid items must remain direct children; CSS2 anonymous block fixup does not apply to grid containers"
    );
  }

  #[test]
  fn out_of_flow_positioned_children_are_not_wrapped_into_anonymous_inline_runs() {
    let style = default_style();
    let text = BoxNode::new_text(style.clone(), "Hello".to_string());

    let mut abs_style = (*style).clone();
    abs_style.display = Display::Inline;
    abs_style.position = crate::style::position::Position::Absolute;
    let abs_child = BoxNode::new_inline(Arc::new(abs_style), vec![]);

    let block = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      style.clone(),
      FormattingContextType::Block,
      vec![text, abs_child, block],
    );

    let fixed = fixup_tree(container);
    assert_eq!(fixed.children.len(), 3);
    assert!(
      fixed.children[0].is_anonymous(),
      "expected anonymous block for inline run"
    );
    assert!(
      matches!(
        fixed.children[1].style.position,
        crate::style::position::Position::Absolute
      ),
      "expected positioned child to remain a direct sibling"
    );
    assert!(!fixed.children[2].is_anonymous());
  }

  #[test]
  fn test_whitespace_only_inline_runs_dropped_in_mixed_content() {
    let whitespace1 = BoxNode::new_text(default_style(), "   ".to_string());
    let block1 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let whitespace2 = BoxNode::new_text(default_style(), "\n  ".to_string());
    let block2 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![whitespace1, block1, whitespace2, block2],
    );

    let fixed = fixup_tree(container);
    assert_eq!(fixed.children.len(), 2);
    assert!(fixed.children.iter().all(|child| child.is_block_level()));
    assert!(fixed.children.iter().all(|child| !child.is_anonymous()));
  }

  #[test]
  fn float_only_inline_runs_are_not_wrapped_into_anonymous_blocks() {
    // Floated children are treated as part of an inline run for source-order purposes, but if a
    // run contains *only* floats + collapsible whitespace, we must not wrap them into an anonymous
    // block. Otherwise clearing elements (including clearfix pseudo-elements) become unable to
    // clear the floats.
    let whitespace1 = BoxNode::new_text(default_style(), "   ".to_string());

    let mut float_style_1 = ComputedStyle::default();
    float_style_1.display = Display::Block;
    float_style_1.float = Float::Left;
    let float1 = BoxNode::new_block(
      Arc::new(float_style_1),
      FormattingContextType::Block,
      vec![],
    );

    let whitespace2 = BoxNode::new_text(default_style(), "\n  ".to_string());

    let mut float_style_2 = ComputedStyle::default();
    float_style_2.display = Display::Block;
    float_style_2.float = Float::Right;
    let float2 = BoxNode::new_block(
      Arc::new(float_style_2),
      FormattingContextType::Block,
      vec![],
    );

    let whitespace3 = BoxNode::new_text(default_style(), " ".to_string());

    let mut clearer_style = ComputedStyle::default();
    clearer_style.display = Display::Block;
    clearer_style.clear = Clear::Both;
    let clearer = BoxNode::new_block(
      Arc::new(clearer_style),
      FormattingContextType::Block,
      vec![],
    );

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![
        whitespace1,
        float1,
        whitespace2,
        float2,
        whitespace3,
        clearer,
      ],
    );

    let fixed = fixup_tree(container);
    assert_eq!(fixed.children.len(), 3);
    assert!(fixed.children.iter().all(|child| !child.is_anonymous()));
    assert!(fixed.children[0].style.float.is_floating());
    assert!(fixed.children[1].style.float.is_floating());
    assert_eq!(fixed.children[2].style.clear, Clear::Both);
  }

  #[test]
  fn test_nested_fixup_two_levels() {
    // Create nested structure that needs fixup at multiple levels
    let inner_text = BoxNode::new_text(default_style(), "Inner".to_string());
    let inner_block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let inner_container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![inner_text, inner_block],
    );

    let outer_text = BoxNode::new_text(default_style(), "Outer".to_string());

    let outer_container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![outer_text, inner_container],
    );

    let fixed = fixup_tree(outer_container);

    // Outer level should have 2 children (anon block, block)
    assert_eq!(fixed.children.len(), 2);
    assert!(fixed.children[0].is_anonymous());
    assert!(!fixed.children[1].is_anonymous());

    // Inner level should also be fixed
    let inner = &fixed.children[1];
    assert_eq!(inner.children.len(), 2);
    assert!(inner.children[0].is_anonymous());
    assert!(!inner.children[1].is_anonymous());
  }

  #[test]
  fn test_all_inline_children_text_gets_wrapped() {
    let inline = BoxNode::new_inline(default_style(), vec![]);
    let text = BoxNode::new_text(default_style(), "Text".to_string());

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![inline, text],
    );

    let fixed = fixup_tree(container);

    // All inline content - text should be wrapped in anonymous inline
    assert_eq!(fixed.children.len(), 2);

    // First is inline (unchanged)
    assert!(!fixed.children[0].is_anonymous());
    assert!(fixed.children[0].is_inline_level());

    // Second is text wrapped in anonymous inline
    assert!(fixed.children[1].is_anonymous());
    assert!(fixed.children[1].is_inline_level());
  }

  #[test]
  fn test_text_in_inline_container_wrapped() {
    let text = BoxNode::new_text(default_style(), "Text".to_string());

    let inline = BoxNode::new_inline(default_style(), vec![text]);

    let fixed = fixup_tree(inline);

    // Text inside inline should be wrapped in anonymous inline
    assert_eq!(fixed.children.len(), 1);
    assert!(fixed.children[0].is_anonymous());
    assert!(fixed.children[0].is_inline_level());
  }

  #[test]
  fn test_has_mixed_content() {
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let inline = BoxNode::new_inline(default_style(), vec![]);

    let mixed = vec![block.clone(), inline.clone()];
    let all_block = vec![block.clone(), block.clone()];
    let all_inline = vec![inline.clone(), inline.clone()];

    assert!(AnonymousBoxCreator::has_mixed_content(&mixed));
    assert!(!AnonymousBoxCreator::has_mixed_content(&all_block));
    assert!(!AnonymousBoxCreator::has_mixed_content(&all_inline));
  }

  #[test]
  fn test_count_anonymous_boxes() {
    let text1 = BoxNode::new_text(default_style(), "Text 1".to_string());
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let text2 = BoxNode::new_text(default_style(), "Text 2".to_string());

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text1, block, text2],
    );

    let fixed = fixup_tree(container);

    // Should have 2 anonymous blocks
    assert_eq!(AnonymousBoxCreator::count_anonymous_boxes(&fixed), 2);
  }

  #[test]
  fn test_text_at_start_and_end() {
    let text1 = BoxNode::new_text(default_style(), "Start".to_string());
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let text2 = BoxNode::new_text(default_style(), "End".to_string());

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text1, block, text2],
    );

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 3);
    assert!(fixed.children[0].is_anonymous());
    assert!(!fixed.children[1].is_anonymous());
    assert!(fixed.children[2].is_anonymous());
  }

  #[test]
  fn test_deeply_nested_structure() {
    // Create: block > block > block > (text, block)
    let text = BoxNode::new_text(default_style(), "Deep".to_string());
    let inner_block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let level3 = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text, inner_block],
    );

    let level2 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![level3]);

    let level1 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![level2]);

    let fixed = fixup_tree(level1);

    // Navigate to level 3 and verify it was fixed
    let level3_fixed = &fixed.children[0].children[0];
    assert_eq!(level3_fixed.children.len(), 2);
    assert!(level3_fixed.children[0].is_anonymous());
    assert!(!level3_fixed.children[1].is_anonymous());
  }

  #[test]
  fn test_child_wrappers_inherit_from_their_actual_parent_style() {
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.color = Rgba::RED;
    let root_style = Arc::new(root_style);

    let mut inner_style = ComputedStyle::default();
    inner_style.display = Display::Block;
    inner_style.color = Rgba::BLUE;
    let inner_style = Arc::new(inner_style);

    let text = BoxNode::new_text(inner_style.clone(), "Hello".to_string());
    let inner = BoxNode::new_block(inner_style, FormattingContextType::Block, vec![text]);
    let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![inner]);

    let fixed = fixup_tree(root);

    let inner_fixed = &fixed.children[0];
    assert_eq!(inner_fixed.children.len(), 1);
    let wrapper = &inner_fixed.children[0];
    assert!(wrapper.is_anonymous());
    assert!(wrapper.is_inline_level());
    assert_eq!(wrapper.style.color, Rgba::BLUE);
  }

  #[test]
  fn test_inline_text_wrappers_do_not_leak_non_inherited_styles_from_ancestors() {
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    root_style.position = Position::Absolute;
    let root_style = Arc::new(root_style);

    let mut inline_style = ComputedStyle::default();
    inline_style.display = Display::Inline;
    inline_style.position = Position::Static;
    let inline_style = Arc::new(inline_style);

    let text = BoxNode::new_text(inline_style.clone(), "Hello".to_string());
    let inline = BoxNode::new_inline(inline_style.clone(), vec![text]);
    let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![inline]);

    let fixed = fixup_tree(root);

    let inline_fixed = &fixed.children[0];
    assert_eq!(inline_fixed.children.len(), 1);
    let wrapper = &inline_fixed.children[0];
    assert!(wrapper.is_anonymous());
    assert_eq!(wrapper.style.display, Display::Inline);
    assert_eq!(wrapper.style.position, Position::Static);
  }

  #[test]
  fn anonymous_inline_text_wrappers_do_not_copy_padding_or_border_from_parent() {
    let mut root_style = ComputedStyle::default();
    root_style.display = Display::Block;
    let root_style = Arc::new(root_style);

    let mut inline_style = ComputedStyle::default();
    inline_style.display = Display::Inline;
    inline_style.padding_top = Length::px(10.0);
    inline_style.padding_right = Length::px(13.0);
    inline_style.padding_bottom = Length::px(11.0);
    inline_style.padding_left = Length::px(12.0);
    inline_style.border_top_width = Length::px(1.0);
    inline_style.border_right_width = Length::px(1.0);
    inline_style.border_bottom_width = Length::px(1.0);
    inline_style.border_left_width = Length::px(1.0);
    inline_style.border_top_style = BorderStyle::Solid;
    inline_style.border_right_style = BorderStyle::Solid;
    inline_style.border_bottom_style = BorderStyle::Solid;
    inline_style.border_left_style = BorderStyle::Solid;
    inline_style.background_color = Rgba::RED;
    let inline_style = Arc::new(inline_style);

    let text = BoxNode::new_text(inline_style.clone(), "Hello".to_string());
    let inline = BoxNode::new_inline(inline_style, vec![text]);
    let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![inline]);

    let fixed = fixup_tree(root);
    let inline_fixed = &fixed.children[0];
    assert_eq!(inline_fixed.children.len(), 1);

    let wrapper = &inline_fixed.children[0];
    assert!(wrapper.is_anonymous() && wrapper.is_inline_level());
    assert_eq!(wrapper.style.display, Display::Inline);
    assert_eq!(wrapper.style.padding_top, Length::px(0.0));
    assert_eq!(wrapper.style.padding_right, Length::px(0.0));
    assert_eq!(wrapper.style.padding_bottom, Length::px(0.0));
    assert_eq!(wrapper.style.padding_left, Length::px(0.0));
    assert_eq!(wrapper.style.border_top_width, Length::px(0.0));
    assert_eq!(wrapper.style.border_right_width, Length::px(0.0));
    assert_eq!(wrapper.style.border_bottom_width, Length::px(0.0));
    assert_eq!(wrapper.style.border_left_width, Length::px(0.0));
    assert_eq!(wrapper.style.background_color, Rgba::TRANSPARENT);
  }

  #[test]
  fn inline_text_wrappers_do_not_copy_position_from_positioned_inline_parent() {
    let mut inline_style = ComputedStyle::default();
    inline_style.display = Display::Inline;
    inline_style.position = Position::Absolute;
    let inline_style = Arc::new(inline_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    text_style.position = Position::Static;
    let text_style = Arc::new(text_style);

    let text = BoxNode::new_text(text_style, "Hello".to_string());
    let inline = BoxNode::new_inline(inline_style, vec![text]);

    let fixed = fixup_tree(inline);
    assert_eq!(fixed.children.len(), 1);

    let wrapper = &fixed.children[0];
    assert!(
      wrapper.is_anonymous() && wrapper.is_inline_level(),
      "expected anonymous inline wrapper for text child"
    );
    assert_eq!(wrapper.style.display, Display::Inline);
    assert_eq!(
      wrapper.style.position,
      Position::Static,
      "anonymous text wrappers must not become out-of-flow when their inline parent is positioned"
    );
    assert_eq!(wrapper.children.len(), 1);
    assert!(matches!(&wrapper.children[0].box_type, BoxType::Text(_)));
  }

  #[test]
  fn inline_text_wrappers_do_not_copy_padding_or_borders_from_parent_inline() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Inline;
    parent_style.position = Position::Absolute;
    parent_style.padding_top = crate::style::values::Length::px(10.0);
    parent_style.padding_bottom = crate::style::values::Length::px(8.0);
    parent_style.border_bottom_width = crate::style::values::Length::px(4.0);
    parent_style.color = Rgba::BLUE;
    let parent_style = Arc::new(parent_style);

    let text = BoxNode::new_text(parent_style.clone(), "Hello".to_string());
    let inline = BoxNode::new_inline(parent_style, vec![text]);
    let fixed = fixup_tree(inline);

    assert_eq!(fixed.children.len(), 1);
    let wrapper = &fixed.children[0];
    assert!(wrapper.is_anonymous());
    assert_eq!(wrapper.style.display, Display::Inline);
    // Anonymous inline wrappers must not inherit non-inherited properties from the parent inline.
    assert_eq!(
      wrapper.style.padding_top,
      crate::style::values::Length::px(0.0)
    );
    assert_eq!(
      wrapper.style.border_bottom_width,
      crate::style::values::Length::px(0.0)
    );
    // But inherited properties like `color` should propagate.
    assert_eq!(wrapper.style.color, Rgba::BLUE);
    // Position is non-inherited; wrapper should be static even if parent is positioned.
    assert_eq!(wrapper.style.position, Position::Static);
  }

  #[test]
  fn anonymous_inline_wrappers_do_not_copy_float_from_parent_inline_box() {
    let mut inline_block_style = ComputedStyle::default();
    inline_block_style.display = Display::InlineBlock;
    inline_block_style.float = Float::Left;
    let inline_block_style = Arc::new(inline_block_style);

    let text = BoxNode::new_text(default_style(), "Hello".to_string());
    let inline_block =
      BoxNode::new_inline_block(inline_block_style, FormattingContextType::Block, vec![text]);
    let root = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![inline_block],
    );

    let fixed = fixup_tree(root);

    let inline_fixed = &fixed.children[0];
    assert_eq!(inline_fixed.children.len(), 1);
    let wrapper = &inline_fixed.children[0];
    assert!(wrapper.is_anonymous());
    assert_eq!(wrapper.style.float, Float::None);
  }

  #[test]
  fn test_single_block_child_no_change() {
    let child = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![child]);

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 1);
    assert!(!fixed.children[0].is_anonymous());
    assert!(fixed.children[0].is_block_level());
  }

  #[test]
  fn test_multiple_text_nodes_each_wrapped() {
    let text1 = BoxNode::new_text(default_style(), "Hello".to_string());
    let text2 = BoxNode::new_text(default_style(), "World".to_string());

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text1, text2],
    );

    let fixed = fixup_tree(container);

    // Each text should be wrapped in its own anonymous inline
    assert_eq!(fixed.children.len(), 2);
    assert!(fixed.children[0].is_anonymous());
    assert!(fixed.children[0].is_inline_level());
    assert!(fixed.children[1].is_anonymous());
    assert!(fixed.children[1].is_inline_level());
  }

  #[test]
  fn test_text_before_block_wrapped_in_anonymous_block() {
    let text = BoxNode::new_text(default_style(), "Before".to_string());
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text, block],
    );

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 2);
    assert!(fixed.children[0].is_anonymous());
    assert!(fixed.children[0].is_block_level());
    assert!(!fixed.children[1].is_anonymous());
    assert!(fixed.children[1].is_block_level());
  }

  #[test]
  fn test_text_after_block_wrapped_in_anonymous_block() {
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let text = BoxNode::new_text(default_style(), "After".to_string());

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![block, text],
    );

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 2);
    assert!(!fixed.children[0].is_anonymous());
    assert!(fixed.children[0].is_block_level());
    assert!(fixed.children[1].is_anonymous());
    assert!(fixed.children[1].is_block_level());
  }

  #[test]
  fn test_text_between_blocks_wrapped() {
    let block1 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let text = BoxNode::new_text(default_style(), "Middle".to_string());
    let block2 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![block1, text, block2],
    );

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 3);
    assert!(!fixed.children[0].is_anonymous());
    assert!(fixed.children[1].is_anonymous());
    assert!(!fixed.children[2].is_anonymous());
  }

  #[test]
  fn test_text_and_inline_grouped_together() {
    let text = BoxNode::new_text(default_style(), "Text".to_string());
    let inline = BoxNode::new_inline(default_style(), vec![]);
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text, inline, block],
    );

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 2);
    assert!(fixed.children[0].is_anonymous());
    assert_eq!(fixed.children[0].children.len(), 2);
  }

  #[test]
  fn test_alternating_block_inline_pattern() {
    let block1 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let inline1 = BoxNode::new_inline(default_style(), vec![]);
    let block2 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let inline2 = BoxNode::new_inline(default_style(), vec![]);
    let block3 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![block1, inline1, block2, inline2, block3],
    );

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 5);
    assert!(!fixed.children[0].is_anonymous());
    assert!(fixed.children[1].is_anonymous());
    assert!(!fixed.children[2].is_anonymous());
    assert!(fixed.children[3].is_anonymous());
    assert!(!fixed.children[4].is_anonymous());
  }

  #[test]
  fn test_nested_inline_in_block() {
    let text = BoxNode::new_text(default_style(), "Nested text".to_string());
    let inline = BoxNode::new_inline(default_style(), vec![text]);

    let container = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![inline]);

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 1);
    let inline_fixed = &fixed.children[0];
    assert_eq!(inline_fixed.children.len(), 1);
    assert!(inline_fixed.children[0].is_anonymous());
  }

  #[test]
  fn test_inline_with_block_descendant_is_split_into_block_flow() {
    let style = default_style();
    let text_before = BoxNode::new_text(style.clone(), "Before".to_string());
    let text_after = BoxNode::new_text(style.clone(), "After".to_string());
    let block = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    let inline = BoxNode::new_inline(style.clone(), vec![text_before, block.clone(), text_after]);

    let container = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![inline]);
    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 3);
    assert!(fixed.children[0].is_anonymous());
    assert!(fixed.children[1].is_block_level());
    assert!(fixed.children[2].is_anonymous());

    assert!(subtree_contains_text(&fixed.children[0], "Before"));
    assert!(subtree_contains_text(&fixed.children[2], "After"));

    // Inline fragments should preserve the inline's style.
    assert!(!fixed.children[0].children.is_empty());
    assert!(Arc::ptr_eq(&fixed.children[0].children[0].style, &style));
  }

  #[test]
  fn test_inline_with_block_descendant_after_leading_block_preserves_siblings() {
    // Exercise the `first_split_idx > 0` path in `split_inline_children_with_block_descendants`.
    // Previously this function used `.expect()` to assume the split index was always in-bounds.
    let style = default_style();
    let leading_block = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    let trailing_block = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);

    let text_before = BoxNode::new_text(style.clone(), "Before".to_string());
    let text_after = BoxNode::new_text(style.clone(), "After".to_string());
    let middle_block = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    let inline = BoxNode::new_inline(
      style.clone(),
      vec![text_before, middle_block, text_after],
    );

    let container = BoxNode::new_block(
      style,
      FormattingContextType::Block,
      vec![leading_block, inline, trailing_block],
    );
    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 5);
    assert!(fixed.children[0].is_block_level() && !fixed.children[0].is_anonymous());
    assert!(fixed.children[1].is_anonymous());
    assert!(fixed.children[2].is_block_level());
    assert!(fixed.children[3].is_anonymous());
    assert!(fixed.children[4].is_block_level() && !fixed.children[4].is_anonymous());

    assert!(subtree_contains_text(&fixed.children[1], "Before"));
    assert!(subtree_contains_text(&fixed.children[3], "After"));
  }

  #[test]
  fn test_nested_inline_with_block_descendant_splits_at_outer() {
    let style = default_style();
    let before = BoxNode::new_text(style.clone(), "Before".to_string());
    let block = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    let inner_inline = BoxNode::new_inline(style.clone(), vec![before, block.clone()]);
    let after = BoxNode::new_text(style.clone(), "After".to_string());
    let outer_inline = BoxNode::new_inline(style.clone(), vec![inner_inline, after]);

    let container = BoxNode::new_block(
      style.clone(),
      FormattingContextType::Block,
      vec![outer_inline],
    );
    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 3);
    assert!(fixed.children[0].is_anonymous());
    assert!(fixed.children[1].is_block_level());
    assert!(fixed.children[2].is_anonymous());

    assert!(subtree_contains_text(&fixed.children[0], "Before"));
    assert!(subtree_contains_text(&fixed.children[2], "After"));
  }

  #[test]
  fn test_inline_block_with_block_children_is_not_split() {
    let inline_block_style = style_with_display(Display::InlineBlock);
    let block_child = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let inline_block = BoxNode::new_inline_block(
      inline_block_style.clone(),
      FormattingContextType::Block,
      vec![block_child],
    );

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![inline_block],
    );
    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 1);
    assert!(fixed.children[0].is_inline_level());
    assert_eq!(fixed.children[0].children.len(), 1);
  }

  #[test]
  fn test_multiple_inlines_no_block_mixing() {
    let inline1 = BoxNode::new_inline(default_style(), vec![]);
    let inline2 = BoxNode::new_inline(default_style(), vec![]);
    let inline3 = BoxNode::new_inline(default_style(), vec![]);

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![inline1, inline2, inline3],
    );

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 3);
    for child in &fixed.children {
      assert!(!child.is_anonymous());
      assert!(child.is_inline_level());
    }
  }

  #[test]
  fn test_float_does_not_force_anonymous_block_wrapping() {
    let text = BoxNode::new_text(default_style(), "Hello".to_string());

    let mut float_style = ComputedStyle::default();
    float_style.display = Display::Block;
    float_style.float = Float::Right;
    let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text, float_box],
    );

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 2);
    assert!(
      matches!(
        fixed.children[0].box_type,
        BoxType::Anonymous(ref anon) if anon.anonymous_type == AnonymousType::Inline
      ),
      "expected first child to be an anonymous inline, got {:?}",
      fixed.children[0].box_type
    );
    assert!(subtree_contains_text(&fixed.children[0], "Hello"));
    assert!(!fixed.children[1].is_anonymous());
  }

  #[test]
  fn test_all_block_level() {
    let block1 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let block2 = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    let inline = BoxNode::new_inline(default_style(), vec![]);

    let all_blocks = vec![block1.clone(), block2.clone()];
    let mixed = vec![block1, inline];

    assert!(AnonymousBoxCreator::all_block_level(&all_blocks));
    assert!(!AnonymousBoxCreator::all_block_level(&mixed));
  }

  #[test]
  fn test_all_inline_level() {
    let inline1 = BoxNode::new_inline(default_style(), vec![]);
    let inline2 = BoxNode::new_inline(default_style(), vec![]);
    let text = BoxNode::new_text(default_style(), "text".to_string());
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let all_inline = vec![inline1.clone(), inline2.clone(), text];
    let mixed = vec![inline1, block];

    assert!(AnonymousBoxCreator::all_inline_level(&all_inline));
    assert!(!AnonymousBoxCreator::all_inline_level(&mixed));
  }

  #[test]
  fn test_single_inline_child_not_wrapped_in_anon_block() {
    let inline = BoxNode::new_inline(default_style(), vec![]);

    let container = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![inline]);

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 1);
    assert!(!fixed.children[0].is_anonymous());
  }

  #[test]
  fn test_empty_text_node() {
    let text = BoxNode::new_text(default_style(), "".to_string());

    let container = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![text]);

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 1);
    assert!(fixed.children[0].is_anonymous());
  }

  #[test]
  fn test_whitespace_only_text_node() {
    let text = BoxNode::new_text(default_style(), "   \n\t  ".to_string());

    let container = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![text]);

    let fixed = fixup_tree(container);

    assert_eq!(fixed.children.len(), 1);
    assert!(fixed.children[0].is_anonymous());
  }

  #[test]
  fn test_anonymous_type_is_block() {
    let text = BoxNode::new_text(default_style(), "Text".to_string());
    let block = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);

    let container = BoxNode::new_block(
      default_style(),
      FormattingContextType::Block,
      vec![text, block],
    );

    let fixed = fixup_tree(container);

    match &fixed.children[0].box_type {
      BoxType::Anonymous(anon) => {
        assert_eq!(anon.anonymous_type, AnonymousType::Block);
      }
      _ => panic!("Expected anonymous box"),
    }
  }

  #[test]
  fn test_anonymous_inline_type() {
    let text = BoxNode::new_text(default_style(), "Text".to_string());

    let container = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![text]);

    let fixed = fixup_tree(container);

    match &fixed.children[0].box_type {
      BoxType::Anonymous(anon) => {
        assert_eq!(anon.anonymous_type, AnonymousType::Inline);
      }
      _ => panic!("Expected anonymous box"),
    }
  }
}
