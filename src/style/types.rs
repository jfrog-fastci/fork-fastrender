//! Style type definitions
//!
//! This module contains all the enum types used in computed styles.
//! These types represent CSS property values that can be applied to elements.

use crate::css::types::ColorStop;
use crate::css::types::RadialGradientShape;
use crate::css::types::RadialGradientSize;
use crate::style::color::Rgba;
use crate::style::values::{CalcSizeExprId, Length};
pub use crate::text::hyphenation::HyphensMode;
use cssparser::{Parser, ParserInput, Token};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Text direction
///
/// CSS: `direction`
/// Reference: CSS Writing Modes Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
  Ltr,
  Rtl,
}

/// Controls bidi embedding/override behavior
///
/// CSS: `unicode-bidi`
/// Reference: CSS Writing Modes Level 3, CSS2.1 9.10
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnicodeBidi {
  Normal,
  Embed,
  BidiOverride,
  Isolate,
  IsolateOverride,
  Plaintext,
}

/// Overflow behavior for content that exceeds container bounds
///
/// CSS: `overflow-x`, `overflow-y`, `overflow`
/// Reference: CSS Overflow Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
  Visible,
  Hidden,
  Scroll,
  Auto,
  Clip,
}

impl Overflow {
  pub fn parse(keyword: &str) -> Option<Self> {
    if keyword.eq_ignore_ascii_case("visible") {
      Some(Self::Visible)
    } else if keyword.eq_ignore_ascii_case("hidden") {
      Some(Self::Hidden)
    } else if keyword.eq_ignore_ascii_case("scroll") {
      Some(Self::Scroll)
    } else if keyword.eq_ignore_ascii_case("auto") {
      Some(Self::Auto)
    } else if keyword.eq_ignore_ascii_case("overlay") {
      Some(Self::Auto)
    } else if keyword.eq_ignore_ascii_case("clip") {
      Some(Self::Clip)
    } else {
      None
    }
  }
}

/// Box keywords accepted by `overflow-clip-margin`.
///
/// CSS Overflow 3 defines `<<visual-box>>` as a subset of the box model boxes
/// that can be used as a reference edge for expanding clip bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualBox {
  BorderBox,
  PaddingBox,
  ContentBox,
}

impl VisualBox {
  pub fn parse(keyword: &str) -> Option<Self> {
    if keyword.eq_ignore_ascii_case("border-box") {
      Some(Self::BorderBox)
    } else if keyword.eq_ignore_ascii_case("padding-box") {
      Some(Self::PaddingBox)
    } else if keyword.eq_ignore_ascii_case("content-box") {
      Some(Self::ContentBox)
    } else {
      None
    }
  }
}

/// Computed value for `overflow-clip-margin`.
///
/// The property allows expanding the clip edge used by `overflow: clip` beyond the
/// chosen reference box edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverflowClipMargin {
  pub visual_box: VisualBox,
  pub margin: Length,
}

impl Default for OverflowClipMargin {
  fn default() -> Self {
    Self {
      visual_box: VisualBox::PaddingBox,
      margin: Length::px(0.0),
    }
  }
}

/// Determines which box the width/height properties apply to.
///
/// CSS: `box-sizing`
/// Reference: CSS Box Sizing Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxSizing {
  ContentBox,
  BorderBox,
}

/// How backgrounds/borders behave when a box is fragmented.
///
/// CSS: `box-decoration-break`
/// Reference: CSS Fragmentation Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxDecorationBreak {
  Slice,
  Clone,
}

/// Legacy axis orientation used by the 2009 flexbox draft (`display: -webkit-box`).
///
/// This is primarily encountered alongside `-webkit-line-clamp` patterns, where WebKit requires
/// `-webkit-box-orient: vertical` for multi-line clamping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebkitBoxOrient {
  Horizontal,
  Vertical,
}

impl Default for WebkitBoxOrient {
  fn default() -> Self {
    Self::Horizontal
  }
}

/// Legacy axis direction used by the 2009 flexbox draft (`display: -webkit-box`).
///
/// Used together with [`WebkitBoxOrient`] to derive an equivalent modern `flex-direction` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebkitBoxDirection {
  Normal,
  Reverse,
}

impl Default for WebkitBoxDirection {
  fn default() -> Self {
    Self::Normal
  }
}

/// Container type for container queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerType {
  Normal,
  Size,
  InlineSize,
  ScrollState,
  SizeScrollState,
  InlineSizeScrollState,
}

impl ContainerType {
  #[inline]
  pub fn is_normal(self) -> bool {
    matches!(self, Self::Normal)
  }

  /// Returns true when this element establishes a size query container.
  #[inline]
  pub fn supports_size(self) -> bool {
    matches!(self, Self::Size | Self::SizeScrollState)
  }

  /// Returns true when this element establishes an inline-size query container.
  #[inline]
  pub fn supports_inline_size(self) -> bool {
    matches!(self, Self::InlineSize | Self::InlineSizeScrollState)
  }

  /// Returns true when this element establishes either kind of size query container.
  #[inline]
  pub fn supports_size_queries(self) -> bool {
    self.supports_size() || self.supports_inline_size()
  }

  /// Returns true when this element establishes a scroll-state query container.
  #[inline]
  pub fn supports_scroll_state(self) -> bool {
    matches!(
      self,
      Self::ScrollState | Self::SizeScrollState | Self::InlineSizeScrollState
    )
  }

  /// Returns true if this container type should create a stacking context.
  ///
  /// FastRender uses the `container-type` property as a stacking-context trigger for size
  /// containers (aligning with browser behavior for size queries). Scroll-state-only containers
  /// do not create stacking contexts.
  #[inline]
  pub fn creates_stacking_context(self) -> bool {
    self.supports_size_queries()
  }

  /// Parse a `container-type` value.
  ///
  /// Spec: CSS Conditional Rules Level 5
  /// Grammar: `normal | [ [ size | inline-size ] || scroll-state ]`
  pub fn parse(raw: &str) -> Option<Self> {
    let mut input = ParserInput::new(raw);
    let mut parser = Parser::new(&mut input);
    Self::parse_from_parser(&mut parser)
  }

  pub(crate) fn parse_from_parser<'i, 't>(parser: &mut Parser<'i, 't>) -> Option<Self> {
    let mut saw_normal = false;
    let mut saw_size = false;
    let mut saw_inline_size = false;
    let mut saw_scroll_state = false;
    let mut saw_any = false;

    while let Ok(token) = parser.next_including_whitespace_and_comments() {
      match token {
        Token::WhiteSpace(_) | Token::Comment(_) => continue,
        Token::Ident(ident) => {
          saw_any = true;
          let ident = ident.as_ref();

          if ident.eq_ignore_ascii_case("normal") {
            // `normal` cannot be combined with other keywords, and duplicates are invalid.
            if saw_normal || saw_size || saw_inline_size || saw_scroll_state {
              return None;
            }
            saw_normal = true;
            continue;
          }

          if ident.eq_ignore_ascii_case("size") {
            // `size` and `inline-size` are mutually exclusive, and duplicates are invalid.
            if saw_size || saw_normal || saw_inline_size {
              return None;
            }
            saw_size = true;
            continue;
          }

          if ident.eq_ignore_ascii_case("inline-size") {
            if saw_inline_size || saw_normal || saw_size {
              return None;
            }
            saw_inline_size = true;
            continue;
          }

          if ident.eq_ignore_ascii_case("scroll-state") {
            if saw_scroll_state || saw_normal {
              return None;
            }
            saw_scroll_state = true;
            continue;
          }

          return None;
        }
        _ => return None,
      }
    }

    if !saw_any {
      return None;
    }

    match (saw_normal, saw_size, saw_inline_size, saw_scroll_state) {
      (true, false, false, false) => Some(Self::Normal),
      (false, true, false, false) => Some(Self::Size),
      (false, false, true, false) => Some(Self::InlineSize),
      (false, false, false, true) => Some(Self::ScrollState),
      (false, true, false, true) => Some(Self::SizeScrollState),
      (false, false, true, true) => Some(Self::InlineSizeScrollState),
      _ => None,
    }
  }
}

/// Controls whether size keywords like `auto` participate in interpolation.
///
/// CSS: `interpolate-size`
/// Spec: CSS Values and Units Level 5
///
/// This property is currently stored for feature-query correctness and future animation support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpolateSize {
  /// Only numeric `<length-percentage>` values interpolate.
  NumericOnly,
  /// Allows interpolation between keyword sizes and numeric sizes where supported.
  AllowKeywords,
}

impl Default for InterpolateSize {
  fn default() -> Self {
    Self::NumericOnly
  }
}

impl InterpolateSize {
  pub fn parse(keyword: &str) -> Option<Self> {
    if keyword.eq_ignore_ascii_case("numeric-only") {
      Some(Self::NumericOnly)
    } else if keyword.eq_ignore_ascii_case("allow-keywords") {
      Some(Self::AllowKeywords)
    } else {
      None
    }
  }
}

/// Border collapsing model for tables
///
/// CSS 2.1 §17.6.1: initial value is `separate`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderCollapse {
  Separate,
  Collapse,
}

/// Whether borders/backgrounds are drawn for empty table cells.
///
/// CSS 2.1 §17.6.1: initial value is `show`, applies to table cells and inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyCells {
  Show,
  Hide,
}

/// Caption placement relative to the table box.
///
/// CSS 2.1 §17.4: initial value is `top`, applies to table captions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionSide {
  Top,
  Bottom,
}

/// Table layout algorithm selection
///
/// CSS: `table-layout`
/// Reference: CSS 2.1 §17.5.2
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableLayout {
  Auto,
  Fixed,
}

/// Border line style
///
/// CSS: `border-style`, `border-*-style`
/// Reference: CSS Backgrounds and Borders Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderStyle {
  None,
  Hidden,
  Solid,
  Dashed,
  Dotted,
  Double,
  Groove,
  Ridge,
  Inset,
  Outset,
}

/// Per-corner border radii for the border-radius property.
///
/// Each corner stores independent horizontal (x) and vertical (y) radii.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BorderCornerRadius {
  pub x: Length,
  pub y: Length,
}

impl BorderCornerRadius {
  pub fn uniform(radius: Length) -> Self {
    Self {
      x: radius,
      y: radius,
    }
  }

  pub fn zero() -> Self {
    Self {
      x: Length::px(0.0),
      y: Length::px(0.0),
    }
  }
}

impl Default for BorderCornerRadius {
  fn default() -> Self {
    Self::zero()
  }
}

/// Border image repeat modes per axis
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderImageRepeat {
  Stretch,
  Repeat,
  Round,
  Space,
}

/// Mask compositing mode per layer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskMode {
  MatchSource,
  Alpha,
  Luminance,
}

/// Mask border mode (`mask-border-mode`).
///
/// This controls how mask values are derived from the source image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskBorderMode {
  MatchSource,
  Alpha,
  Luminance,
}

/// Reference box for mask positioning (mask-origin)
pub type MaskOrigin = BackgroundBox;

/// Area the mask is clipped to (mask-clip)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskClip {
  BorderBox,
  PaddingBox,
  ContentBox,
  Text,
  NoClip,
}

/// Compositing operator between mask layers (mask-composite)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskComposite {
  Add,
  Subtract,
  Intersect,
  Exclude,
}

/// Border image source
#[derive(Debug, Clone, PartialEq)]
pub enum BorderImageSource {
  None,
  Image(Box<BackgroundImage>),
}

/// Border image slice value (number or percentage)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BorderImageSliceValue {
  Number(f32),
  Percentage(f32),
}

/// Border image slice data
#[derive(Debug, Clone, PartialEq)]
pub struct BorderImageSlice {
  pub top: BorderImageSliceValue,
  pub right: BorderImageSliceValue,
  pub bottom: BorderImageSliceValue,
  pub left: BorderImageSliceValue,
  pub fill: bool,
}

impl Default for BorderImageSlice {
  fn default() -> Self {
    Self {
      // https://www.w3.org/TR/css-backgrounds-3/#the-border-image-slice
      // Initial value is `100%`, not `100`.
      top: BorderImageSliceValue::Percentage(100.0),
      right: BorderImageSliceValue::Percentage(100.0),
      bottom: BorderImageSliceValue::Percentage(100.0),
      left: BorderImageSliceValue::Percentage(100.0),
      fill: false,
    }
  }
}

/// Border image width value
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BorderImageWidthValue {
  Auto,
  Number(f32),
  Length(Length),
  Percentage(f32),
}

/// Border image widths per side
#[derive(Debug, Clone, PartialEq)]
pub struct BorderImageWidth {
  pub top: BorderImageWidthValue,
  pub right: BorderImageWidthValue,
  pub bottom: BorderImageWidthValue,
  pub left: BorderImageWidthValue,
}

impl Default for BorderImageWidth {
  fn default() -> Self {
    Self {
      // https://www.w3.org/TR/css-backgrounds-3/#border-image-width
      // Initial value is `1` (a multiplier of the corresponding border-width), not `auto`.
      top: BorderImageWidthValue::Number(1.0),
      right: BorderImageWidthValue::Number(1.0),
      bottom: BorderImageWidthValue::Number(1.0),
      left: BorderImageWidthValue::Number(1.0),
    }
  }
}

/// Border image outset value
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BorderImageOutsetValue {
  Number(f32),
  Length(Length),
}

/// Border image outsets per side
#[derive(Debug, Clone, PartialEq)]
pub struct BorderImageOutset {
  pub top: BorderImageOutsetValue,
  pub right: BorderImageOutsetValue,
  pub bottom: BorderImageOutsetValue,
  pub left: BorderImageOutsetValue,
}

impl Default for BorderImageOutset {
  fn default() -> Self {
    Self {
      top: BorderImageOutsetValue::Number(0.0),
      right: BorderImageOutsetValue::Number(0.0),
      bottom: BorderImageOutsetValue::Number(0.0),
      left: BorderImageOutsetValue::Number(0.0),
    }
  }
}

/// Complete border image data
#[derive(Debug, Clone, PartialEq)]
pub struct BorderImage {
  pub source: BorderImageSource,
  pub slice: BorderImageSlice,
  pub width: BorderImageWidth,
  pub outset: BorderImageOutset,
  pub repeat: (BorderImageRepeat, BorderImageRepeat),
}

impl Default for BorderImage {
  fn default() -> Self {
    Self {
      source: BorderImageSource::None,
      slice: BorderImageSlice::default(),
      width: BorderImageWidth::default(),
      outset: BorderImageOutset::default(),
      repeat: (BorderImageRepeat::Stretch, BorderImageRepeat::Stretch),
    }
  }
}

/// Mask border image data (`mask-border-*` longhands).
///
/// CSS Masking defines a "mask border image" with the same 9-slice tiling model as `border-image`,
/// but with different initial values (notably `slice: 0` and `width: auto`).
#[derive(Debug, Clone, PartialEq)]
pub struct MaskBorder {
  pub source: BorderImageSource,
  pub slice: BorderImageSlice,
  pub width: BorderImageWidth,
  pub outset: BorderImageOutset,
  pub repeat: (BorderImageRepeat, BorderImageRepeat),
  pub mode: MaskBorderMode,
}

impl MaskBorder {
  #[inline]
  pub fn is_active(&self) -> bool {
    matches!(self.source, BorderImageSource::Image(_))
  }
}

impl Default for MaskBorder {
  fn default() -> Self {
    // https://www.w3.org/TR/css-masking-1/#the-mask-border
    Self {
      source: BorderImageSource::None,
      slice: BorderImageSlice {
        top: BorderImageSliceValue::Number(0.0),
        right: BorderImageSliceValue::Number(0.0),
        bottom: BorderImageSliceValue::Number(0.0),
        left: BorderImageSliceValue::Number(0.0),
        fill: false,
      },
      width: BorderImageWidth {
        top: BorderImageWidthValue::Auto,
        right: BorderImageWidthValue::Auto,
        bottom: BorderImageWidthValue::Auto,
        left: BorderImageWidthValue::Auto,
      },
      outset: BorderImageOutset::default(),
      repeat: (BorderImageRepeat::Stretch, BorderImageRepeat::Stretch),
      mode: MaskBorderMode::MatchSource,
    }
  }
}

/// Outline line style
///
/// CSS: `outline-style`
/// Reference: CSS Basic User Interface Level 4 (outline)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineStyle {
  None,
  Hidden,
  Solid,
  Dashed,
  Dotted,
  Double,
  Groove,
  Ridge,
  Inset,
  Outset,
  Auto,
}

impl OutlineStyle {
  /// Returns true if the outline would paint (non-none/hidden)
  pub fn paints(self) -> bool {
    !matches!(self, OutlineStyle::None | OutlineStyle::Hidden)
  }

  /// Converts to the closest border style for painting
  pub fn to_border_style(self) -> BorderStyle {
    match self {
      OutlineStyle::None => BorderStyle::None,
      OutlineStyle::Hidden => BorderStyle::Hidden,
      OutlineStyle::Solid => BorderStyle::Solid,
      OutlineStyle::Dashed => BorderStyle::Dashed,
      OutlineStyle::Dotted => BorderStyle::Dotted,
      OutlineStyle::Double => BorderStyle::Double,
      OutlineStyle::Groove => BorderStyle::Groove,
      OutlineStyle::Ridge => BorderStyle::Ridge,
      OutlineStyle::Inset => BorderStyle::Inset,
      OutlineStyle::Outset => BorderStyle::Outset,
      OutlineStyle::Auto => BorderStyle::Solid,
    }
  }
}

/// Outline color value (can reference currentColor or invert)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutlineColor {
  CurrentColor,
  Color(crate::style::color::Rgba),
  Invert,
}

impl OutlineColor {
  /// Resolves the outline color to an RGBA and whether it should invert destination pixels.
  pub fn resolve(
    self,
    current_color: crate::style::color::Rgba,
  ) -> (crate::style::color::Rgba, bool) {
    match self {
      OutlineColor::CurrentColor => (current_color, false),
      OutlineColor::Color(c) => (c, false),
      OutlineColor::Invert => (crate::style::color::Rgba::WHITE, true),
    }
  }
}

/// Flex container main axis direction
///
/// CSS: `flex-direction`
/// Reference: CSS Flexible Box Layout Module Level 1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDirection {
  Row,
  RowReverse,
  Column,
  ColumnReverse,
}

/// Flex item wrapping behavior
///
/// CSS: `flex-wrap`
/// Reference: CSS Flexible Box Layout Module Level 1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexWrap {
  NoWrap,
  Wrap,
  WrapReverse,
}

/// How multi-column content is balanced across fragmentainers
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnFill {
  Auto,
  Balance,
  BalanceAll,
}

impl Default for ColumnFill {
  fn default() -> Self {
    ColumnFill::Balance
  }
}

/// Whether an element spans across all columns within a multicol container
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnSpan {
  None,
  All,
}

impl Default for ColumnSpan {
  fn default() -> Self {
    ColumnSpan::None
  }
}

/// How replaced content is resized within its box
///
/// CSS: `object-fit`
/// Reference: CSS Images Module Level 4
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFit {
  Fill,
  Contain,
  Cover,
  None,
  ScaleDown,
}

/// Image scaling quality hint
///
/// CSS: `image-rendering`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRendering {
  Auto,
  Smooth,
  CrispEdges,
  Pixelated,
}

/// URL-backed image reference used by computed style values.
///
/// This is an alias for [`BackgroundImageUrl`], which stores an optional `override_resolution`
/// (dppx) used for density-aware resources selected by `image-set()` or `srcset`.
pub type UrlImage = BackgroundImageUrl;

/// Preferred resolution for raster images
///
/// CSS: `image-resolution`
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageResolution {
  /// Whether to prefer the image's own resolution (metadata) when present.
  pub from_image: bool,
  /// Explicit resolution in image pixels per CSS px (dppx). None when the author
  /// omitted a resolution value.
  pub specified: Option<f32>,
  /// Whether to snap the resolution so image pixels map to an integer number of
  /// device pixels.
  pub snap: bool,
}

impl Default for ImageResolution {
  fn default() -> Self {
    Self {
      from_image: false,
      specified: None,
      snap: false,
    }
  }
}

impl ImageResolution {
  /// Computes the used image resolution in dppx given optional resource metadata and device DPR.
  ///
  /// `override_resolution` comes from the chosen resource (e.g. srcset density or image-set
  /// selection). `metadata_resolution` is extracted from the resource itself (e.g. EXIF DPI) and
  /// is only honored when `from_image` is set.
  pub fn used_resolution(
    self,
    override_resolution: Option<f32>,
    metadata_resolution: Option<f32>,
    device_pixel_ratio: f32,
  ) -> f32 {
    let sanitize = |v: Option<f32>| v.filter(|v| v.is_finite() && *v > 0.0);

    let override_resolution = sanitize(override_resolution);
    let metadata_resolution = sanitize(metadata_resolution);
    let specified = sanitize(self.specified);

    let mut resolved = if self.from_image {
      override_resolution
        .or(metadata_resolution)
        .or(specified)
        .unwrap_or(1.0)
    } else {
      specified.or(override_resolution).unwrap_or(1.0)
    };
    if self.snap {
      resolved = snap_resolution(resolved, device_pixel_ratio);
    }
    resolved
  }
}

fn snap_resolution(resolution: f32, device_pixel_ratio: f32) -> f32 {
  if !resolution.is_finite() || resolution <= 0.0 {
    return 1.0;
  }
  if !device_pixel_ratio.is_finite() || device_pixel_ratio <= 0.0 {
    return resolution;
  }

  // device pixels per image pixel = device_dppx / resolution.
  let device_per_image = device_pixel_ratio / resolution;
  let snapped_pixels = device_per_image.round().max(1.0);
  device_pixel_ratio / snapped_pixels
}

/// Orientation applied to decoded images
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrientationTransform {
  /// Number of quarter turns clockwise (0–3)
  pub quarter_turns: u8,
  /// Whether to flip horizontally after rotation
  pub flip_x: bool,
}

impl OrientationTransform {
  pub const IDENTITY: Self = Self {
    quarter_turns: 0,
    flip_x: false,
  };

  /// Returns the oriented dimensions for a given image size.
  pub fn oriented_dimensions(self, width: u32, height: u32) -> (u32, u32) {
    if self.quarter_turns % 2 == 1 {
      (height, width)
    } else {
      (width, height)
    }
  }

  /// Whether the orientation swaps the x/y axes.
  pub fn swaps_axes(self) -> bool {
    self.quarter_turns % 2 == 1
  }
}

/// CSS `image-orientation`
///
/// Reference: CSS Images Module Level 3 §5.1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageOrientation {
  FromImage,
  None,
  Angle { quarter_turns: u8, flip: bool },
}

impl Default for ImageOrientation {
  fn default() -> Self {
    ImageOrientation::FromImage
  }
}

impl ImageOrientation {
  /// Compute the effective transform for a given image, considering whether the
  /// image is decorative (background/border) or content.
  pub fn resolve(
    self,
    metadata: Option<OrientationTransform>,
    decorative: bool,
  ) -> OrientationTransform {
    match self {
      ImageOrientation::None => OrientationTransform::IDENTITY,
      ImageOrientation::FromImage => metadata.unwrap_or(OrientationTransform::IDENTITY),
      ImageOrientation::Angle {
        quarter_turns,
        flip,
      } => {
        if decorative {
          metadata.unwrap_or(OrientationTransform::IDENTITY)
        } else {
          OrientationTransform {
            quarter_turns: quarter_turns % 4,
            flip_x: flip,
          }
        }
      }
    }
  }
}

/// Computed aspect-ratio value
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AspectRatio {
  Auto,
  Ratio(f32),
  AutoRatio(f32),
}

/// Logical alignment for object-position
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionKeyword {
  Start,
  Center,
  End,
}

/// Position component for object positioning
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PositionComponent {
  Keyword(PositionKeyword),
  Length(Length),
  Percentage(f32),
}

/// Object position along x/y
///
/// CSS: `object-position`
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectPosition {
  pub x: PositionComponent,
  pub y: PositionComponent,
}

/// Writing mode for block/inline axis orientation
///
/// CSS: `writing-mode`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WritingMode {
  HorizontalTb,
  VerticalRl,
  VerticalLr,
  SidewaysRl,
  SidewaysLr,
}

/// Mix-blend mode values
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixBlendMode {
  Normal,
  Multiply,
  Screen,
  Overlay,
  Darken,
  Lighten,
  ColorDodge,
  ColorBurn,
  HardLight,
  SoftLight,
  Difference,
  Exclusion,
  Hue,
  Saturation,
  Color,
  Luminosity,
  PlusLighter,
  PlusDarker,
  HueHsv,
  SaturationHsv,
  ColorHsv,
  LuminosityHsv,
  HueOklch,
  ChromaOklch,
  ColorOklch,
  LuminosityOklch,
}

/// Isolation for stacking contexts
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
  Auto,
  Isolate,
}

/// Authored color scheme preferences (CSS `color-scheme`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColorSchemeEntry {
  Light,
  Dark,
  Custom(String),
}

/// Computed value for `color-scheme`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColorSchemePreference {
  /// UA defaults (`color-scheme: normal`)
  Normal,
  /// Explicit list of supported schemes, with optional `only` flag
  Supported {
    schemes: Vec<ColorSchemeEntry>,
    only: bool,
  },
}

impl Default for ColorSchemePreference {
  fn default() -> Self {
    ColorSchemePreference::Normal
  }
}

/// Computed value for the CSS Color HDR 1 `dynamic-range-limit` property.
///
/// FastRender currently renders into SDR, but still parses and cascades this property so
/// `@supports (dynamic-range-limit: ...)` and inheritance behave deterministically.
#[derive(Debug, Clone, PartialEq)]
pub enum DynamicRangeLimit {
  Standard,
  Constrained,
  NoLimit,
  Mix(Vec<DynamicRangeLimitMixComponent>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DynamicRangeLimitMixComponent {
  pub value: Box<DynamicRangeLimit>,
  pub percentage: f32,
}

impl Default for DynamicRangeLimit {
  fn default() -> Self {
    DynamicRangeLimit::NoLimit
  }
}

impl DynamicRangeLimit {
  pub fn parse(raw: &str) -> Option<Self> {
    let mut input = ParserInput::new(raw);
    let mut parser = Parser::new(&mut input);
    parser.skip_whitespace();
    if parser.is_exhausted() {
      return None;
    }
    let value = parse_dynamic_range_limit_value(&mut parser).ok()?;
    parser.skip_whitespace();
    if !parser.is_exhausted() {
      return None;
    }
    Some(value)
  }
}

fn parse_dynamic_range_limit_value<'i, 't>(
  parser: &mut Parser<'i, 't>,
) -> Result<DynamicRangeLimit, cssparser::ParseError<'i, ()>> {
  parser.skip_whitespace();
  let token = parser.next()?;
  match token {
    Token::Ident(ident) => {
      if ident.eq_ignore_ascii_case("standard") {
        Ok(DynamicRangeLimit::Standard)
      } else if ident.eq_ignore_ascii_case("constrained")
        || ident.eq_ignore_ascii_case("constrained-high")
      {
        Ok(DynamicRangeLimit::Constrained)
      } else if ident.eq_ignore_ascii_case("no-limit") || ident.eq_ignore_ascii_case("high") {
        Ok(DynamicRangeLimit::NoLimit)
      } else {
        Err(parser.new_custom_error::<(), ()>(()))
      }
    }
    Token::Function(name) => {
      if !name.eq_ignore_ascii_case("dynamic-range-limit-mix") {
        return Err(parser.new_custom_error::<(), ()>(()));
      }
      let components = parser.parse_nested_block(parse_dynamic_range_limit_mix)?;
      Ok(DynamicRangeLimit::Mix(components))
    }
    _ => Err(parser.new_custom_error::<(), ()>(())),
  }
}

fn parse_dynamic_range_limit_mix<'i, 't>(
  parser: &mut Parser<'i, 't>,
) -> Result<Vec<DynamicRangeLimitMixComponent>, cssparser::ParseError<'i, ()>> {
  parser.skip_whitespace();
  let components = parser.parse_comma_separated(parse_dynamic_range_limit_mix_component)?;
  if components.len() < 2 {
    return Err(parser.new_custom_error::<(), ()>(()));
  }
  Ok(components)
}

fn parse_dynamic_range_limit_mix_component<'i, 't>(
  parser: &mut Parser<'i, 't>,
) -> Result<DynamicRangeLimitMixComponent, cssparser::ParseError<'i, ()>> {
  let mut value: Option<DynamicRangeLimit> = None;
  let mut percentage: Option<f32> = None;

  for _ in 0..2 {
    parser.skip_whitespace();
    if value.is_none() {
      if let Ok(parsed) = parser.try_parse(parse_dynamic_range_limit_value) {
        value = Some(parsed);
        continue;
      }
    }
    if percentage.is_none() {
      if let Ok(parsed) = parser.try_parse(parse_dynamic_range_limit_mix_percentage) {
        percentage = Some(parsed);
        continue;
      }
    }
    return Err(parser.new_custom_error::<(), ()>(()));
  }

  let value = value.ok_or_else(|| parser.new_custom_error::<(), ()>(()))?;
  let percentage = percentage.ok_or_else(|| parser.new_custom_error::<(), ()>(()))?;
  Ok(DynamicRangeLimitMixComponent {
    value: Box::new(value),
    percentage,
  })
}

fn parse_dynamic_range_limit_mix_percentage<'i, 't>(
  parser: &mut Parser<'i, 't>,
) -> Result<f32, cssparser::ParseError<'i, ()>> {
  let token = parser.next()?;
  match token {
    Token::Percentage { unit_value, .. } => {
      let value = (unit_value * 100.0).clamp(0.0, 100.0);
      if value.is_finite() {
        Ok(value)
      } else {
        Err(parser.new_custom_error::<(), ()>(()))
      }
    }
    _ => Err(parser.new_custom_error::<(), ()>(())),
  }
}

/// CSS `forced-color-adjust`
///
/// Reference: CSS Color Adjustment Module Level 1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForcedColorAdjust {
  Auto,
  None,
  PreserveParentColor,
}

impl Default for ForcedColorAdjust {
  fn default() -> Self {
    ForcedColorAdjust::Auto
  }
}

/// CSS `print-color-adjust` (and deprecated shorthand `color-adjust`).
///
/// Reference: CSS Color Adjustment Module Level 1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintColorAdjust {
  Economy,
  Exact,
}

impl Default for PrintColorAdjust {
  fn default() -> Self {
    PrintColorAdjust::Economy
  }
}

/// Computed caret color (`caret-color`)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CaretColor {
  Auto,
  Color(Rgba),
}

impl Default for CaretColor {
  fn default() -> Self {
    CaretColor::Auto
  }
}

/// Computed accent color (`accent-color`)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AccentColor {
  Auto,
  Color(Rgba),
}

impl Default for AccentColor {
  fn default() -> Self {
    AccentColor::Auto
  }
}

/// SVG paint property value that can be a color, `none`, or a paint server reference (`url(...)`).
///
/// Used for core SVG presentation properties like `fill` and `stroke`.
#[derive(Debug, Clone, PartialEq)]
pub enum ColorOrNone {
  Color(Rgba),
  /// The `currentColor` keyword.
  ///
  /// This is resolved later using the element's computed `color` property. Keeping this
  /// representation avoids order-dependence when `fill`/`stroke` and `color` are declared in the
  /// same rule/style attribute.
  CurrentColor,
  None,
  /// Raw `url(...)` value contents (without the `url()` wrapper), e.g. `"#grad"`.
  Url(Arc<str>),
}

/// SVG property value that can be a URL reference (`url(...)`) or `none`.
///
/// Used for marker presentation properties (`marker-start`, `marker-mid`, `marker-end`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SvgUrlOrNone {
  None,
  Url(Arc<str>),
}

/// SVG length list items that accept either a `<length>` or a unitless number.
///
/// Used for SVG presentation properties like `stroke-width` and `stroke-dasharray`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LengthOrNumber {
  Length(Length),
  Number(f32),
}

/// SVG `stroke-linecap` presentation property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrokeLinecap {
  Butt,
  Round,
  Square,
}

/// SVG `stroke-linejoin` presentation property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrokeLinejoin {
  Miter,
  Round,
  Bevel,
}

/// SVG `stroke-dasharray` presentation property.
#[derive(Debug, Clone, PartialEq)]
pub enum StrokeDasharray {
  None,
  Values(Arc<[LengthOrNumber]>),
}

/// SVG `text-anchor` property.
///
/// This is used to control how SVG `<text>` positions its rendered glyphs relative
/// to the `x` coordinate (start/middle/end).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvgTextAnchor {
  Start,
  Middle,
  End,
}

/// SVG `dominant-baseline` property.
///
/// Controls which baseline table is used to align glyphs within SVG text layout.
/// This affects both `<text>` and inline `<tspan>` positioning when serialized to
/// resvg/usvg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvgDominantBaseline {
  Auto,
  TextBottom,
  Alphabetic,
  Ideographic,
  Middle,
  Central,
  Mathematical,
  Hanging,
  TextTop,
}

impl SvgDominantBaseline {
  pub fn parse(keyword: &str) -> Option<Self> {
    Self::parse_keyword(keyword)
  }

  pub fn parse_keyword(keyword: &str) -> Option<Self> {
    if keyword.eq_ignore_ascii_case("auto") {
      Some(Self::Auto)
    } else if keyword.eq_ignore_ascii_case("text-bottom") {
      Some(Self::TextBottom)
    } else if keyword.eq_ignore_ascii_case("alphabetic") {
      Some(Self::Alphabetic)
    } else if keyword.eq_ignore_ascii_case("ideographic") {
      Some(Self::Ideographic)
    } else if keyword.eq_ignore_ascii_case("middle") {
      Some(Self::Middle)
    } else if keyword.eq_ignore_ascii_case("central") {
      Some(Self::Central)
    } else if keyword.eq_ignore_ascii_case("mathematical") {
      Some(Self::Mathematical)
    } else if keyword.eq_ignore_ascii_case("hanging") {
      Some(Self::Hanging)
    } else if keyword.eq_ignore_ascii_case("text-top") {
      Some(Self::TextTop)
    } else {
      None
    }
  }

  pub fn as_css_str(self) -> &'static str {
    match self {
      Self::Auto => "auto",
      Self::TextBottom => "text-bottom",
      Self::Alphabetic => "alphabetic",
      Self::Ideographic => "ideographic",
      Self::Middle => "middle",
      Self::Central => "central",
      Self::Mathematical => "mathematical",
      Self::Hanging => "hanging",
      Self::TextTop => "text-top",
    }
  }
}

/// SVG `baseline-shift` property.
///
/// This is a legacy SVG property used to shift glyphs relative to the dominant
/// baseline (e.g. `sub`/`super` or a length/percentage).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SvgBaselineShift {
  Baseline,
  Sub,
  Super,
  Length(Length),
}

impl SvgBaselineShift {
  pub fn parse_keyword(keyword: &str) -> Option<Self> {
    if keyword.eq_ignore_ascii_case("baseline") {
      Some(Self::Baseline)
    } else if keyword.eq_ignore_ascii_case("sub") {
      Some(Self::Sub)
    } else if keyword.eq_ignore_ascii_case("super") {
      Some(Self::Super)
    } else {
      None
    }
  }
}

/// SVG `shape-rendering` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvgShapeRendering {
  Auto,
  OptimizeSpeed,
  CrispEdges,
  GeometricPrecision,
}

impl SvgShapeRendering {
  pub fn parse_keyword(value: &str) -> Option<Self> {
    if value.eq_ignore_ascii_case("auto") {
      Some(Self::Auto)
    } else if value.eq_ignore_ascii_case("optimizespeed") {
      Some(Self::OptimizeSpeed)
    } else if value.eq_ignore_ascii_case("crispedges") {
      Some(Self::CrispEdges)
    } else if value.eq_ignore_ascii_case("geometricprecision") {
      Some(Self::GeometricPrecision)
    } else {
      None
    }
  }

  pub fn as_css_str(self) -> &'static str {
    match self {
      Self::Auto => "auto",
      Self::OptimizeSpeed => "optimizeSpeed",
      Self::CrispEdges => "crispEdges",
      Self::GeometricPrecision => "geometricPrecision",
    }
  }
}

/// SVG `vector-effect` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvgVectorEffect {
  None,
  NonScalingStroke,
}

impl SvgVectorEffect {
  pub fn parse_keyword(value: &str) -> Option<Self> {
    if value.eq_ignore_ascii_case("none") {
      Some(Self::None)
    } else if value.eq_ignore_ascii_case("non-scaling-stroke") {
      Some(Self::NonScalingStroke)
    } else {
      None
    }
  }

  pub fn as_css_str(self) -> &'static str {
    match self {
      Self::None => "none",
      Self::NonScalingStroke => "non-scaling-stroke",
    }
  }
}

/// SVG `color-rendering` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvgColorRendering {
  Auto,
  OptimizeSpeed,
  OptimizeQuality,
}

impl SvgColorRendering {
  pub fn parse_keyword(value: &str) -> Option<Self> {
    if value.eq_ignore_ascii_case("auto") {
      Some(Self::Auto)
    } else if value.eq_ignore_ascii_case("optimizespeed") {
      Some(Self::OptimizeSpeed)
    } else if value.eq_ignore_ascii_case("optimizequality") {
      Some(Self::OptimizeQuality)
    } else {
      None
    }
  }

  pub fn as_css_str(self) -> &'static str {
    match self {
      Self::Auto => "auto",
      Self::OptimizeSpeed => "optimizeSpeed",
      Self::OptimizeQuality => "optimizeQuality",
    }
  }
}

/// SVG `color-interpolation` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvgColorInterpolation {
  Auto,
  SRgb,
  LinearRgb,
}

impl SvgColorInterpolation {
  pub fn parse_keyword(value: &str) -> Option<Self> {
    if value.eq_ignore_ascii_case("auto") {
      Some(Self::Auto)
    } else if value.eq_ignore_ascii_case("srgb") {
      Some(Self::SRgb)
    } else if value.eq_ignore_ascii_case("linearrgb") {
      Some(Self::LinearRgb)
    } else {
      None
    }
  }

  pub fn as_css_str(self) -> &'static str {
    match self {
      Self::Auto => "auto",
      Self::SRgb => "sRGB",
      Self::LinearRgb => "linearRGB",
    }
  }
}

/// SVG `color-interpolation-filters` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvgColorInterpolationFilters {
  Auto,
  SRgb,
  LinearRgb,
}

impl SvgColorInterpolationFilters {
  pub fn parse_keyword(value: &str) -> Option<Self> {
    if value.eq_ignore_ascii_case("auto") {
      Some(Self::Auto)
    } else if value.eq_ignore_ascii_case("srgb") {
      Some(Self::SRgb)
    } else if value.eq_ignore_ascii_case("linearrgb") {
      Some(Self::LinearRgb)
    } else {
      None
    }
  }

  pub fn as_css_str(self) -> &'static str {
    match self {
      Self::Auto => "auto",
      Self::SRgb => "sRGB",
      Self::LinearRgb => "linearRGB",
    }
  }
}

/// SVG `mask-type` property (for `<mask>` elements).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvgMaskType {
  Luminance,
  Alpha,
}

impl SvgMaskType {
  pub fn parse_keyword(value: &str) -> Option<Self> {
    if value.eq_ignore_ascii_case("luminance") {
      Some(Self::Luminance)
    } else if value.eq_ignore_ascii_case("alpha") {
      Some(Self::Alpha)
    } else {
      None
    }
  }

  pub fn as_css_str(self) -> &'static str {
    match self {
      Self::Luminance => "luminance",
      Self::Alpha => "alpha",
    }
  }
}

/// Computed value for `appearance`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Appearance {
  Auto,
  None,
  Keyword(String),
}

impl Default for Appearance {
  fn default() -> Self {
    Appearance::Auto
  }
}

/// Scroll-behavior property
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollBehavior {
  Auto,
  Smooth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchAction {
  pub auto: bool,
  pub none: bool,
  pub pan_x: bool,
  pub pan_y: bool,
  pub pan_left: bool,
  pub pan_right: bool,
  pub pan_up: bool,
  pub pan_down: bool,
  pub pinch_zoom: bool,
  pub manipulation: bool,
}

impl TouchAction {
  pub fn auto() -> Self {
    Self {
      auto: true,
      none: false,
      pan_x: false,
      pan_y: false,
      pan_left: false,
      pan_right: false,
      pan_up: false,
      pan_down: false,
      pinch_zoom: false,
      manipulation: false,
    }
  }

  pub fn none() -> Self {
    Self {
      auto: false,
      none: true,
      pan_x: false,
      pan_y: false,
      pan_left: false,
      pan_right: false,
      pan_up: false,
      pan_down: false,
      pinch_zoom: false,
      manipulation: false,
    }
  }

  pub fn empty() -> Self {
    Self {
      auto: false,
      none: false,
      pan_x: false,
      pan_y: false,
      pan_left: false,
      pan_right: false,
      pan_up: false,
      pan_down: false,
      pinch_zoom: false,
      manipulation: false,
    }
  }
}

/// Scrollbar width preference (UI hint; currently unused in layout)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarWidth {
  Auto,
  Thin,
  None,
}

/// Scrollbar color preference (UI hint; currently unused in layout)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollbarColor {
  Auto,
  Dark,
  Light,
  Colors { thumb: Rgba, track: Rgba },
}

/// Scroll snap type strictness
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSnapStrictness {
  Proximity,
  Mandatory,
}

impl Default for ScrollSnapStrictness {
  fn default() -> Self {
    ScrollSnapStrictness::Proximity
  }
}

/// Scroll snap axis selection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSnapAxis {
  None,
  X,
  Y,
  Block,
  Inline,
  Both,
}

impl Default for ScrollSnapAxis {
  fn default() -> Self {
    ScrollSnapAxis::None
  }
}

/// CSS `scroll-snap-type`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollSnapType {
  pub axis: ScrollSnapAxis,
  pub strictness: ScrollSnapStrictness,
}

impl Default for ScrollSnapType {
  fn default() -> Self {
    ScrollSnapType {
      axis: ScrollSnapAxis::None,
      strictness: ScrollSnapStrictness::Proximity,
    }
  }
}

/// Scroll snap alignment per axis
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSnapAlign {
  None,
  Start,
  End,
  Center,
}

impl Default for ScrollSnapAlign {
  fn default() -> Self {
    ScrollSnapAlign::None
  }
}

/// CSS `scroll-snap-align`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollSnapAlignments {
  pub inline: ScrollSnapAlign,
  pub block: ScrollSnapAlign,
}

impl Default for ScrollSnapAlignments {
  fn default() -> Self {
    ScrollSnapAlignments {
      inline: ScrollSnapAlign::None,
      block: ScrollSnapAlign::None,
    }
  }
}

/// CSS `scroll-snap-stop`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSnapStop {
  Normal,
  Always,
}

impl Default for ScrollSnapStop {
  fn default() -> Self {
    ScrollSnapStop::Normal
  }
}

/// Axis selection for scroll/view timelines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineAxis {
  Block,
  Inline,
  X,
  Y,
}

impl Default for TimelineAxis {
  fn default() -> Self {
    TimelineAxis::Block
  }
}

/// CSS `timeline-scope`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineScopeProperty {
  None,
  All,
  Names(Vec<String>),
}

impl Default for TimelineScopeProperty {
  fn default() -> Self {
    TimelineScopeProperty::None
  }
}

/// Offset used to start or end a scroll timeline.
#[derive(Debug, Clone, PartialEq)]
pub enum TimelineOffset {
  /// Automatic offset (0 for start, scroll range for end)
  Auto,
  /// Explicit length/percentage value
  Length(crate::style::values::Length),
  /// Percentage expressed directly (0-100)
  Percentage(f32),
}

impl Default for TimelineOffset {
  fn default() -> Self {
    TimelineOffset::Auto
  }
}

/// A scroll-driven timeline definition.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScrollTimeline {
  /// Optional timeline name.
  pub name: Option<String>,
  /// Axis that drives the timeline (block/inline/x/y).
  pub axis: TimelineAxis,
  /// Offset where the timeline starts.
  pub start: TimelineOffset,
  /// Offset where the timeline ends.
  pub end: TimelineOffset,
}

/// Scroller selection for the `scroll()` functional timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollTimelineScroller {
  Root,
  Nearest,
  SelfElement,
}

impl Default for ScrollTimelineScroller {
  fn default() -> Self {
    ScrollTimelineScroller::Nearest
  }
}

/// An anonymous scroll timeline produced by `scroll(...)` in `animation-timeline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollFunctionTimeline {
  pub scroller: ScrollTimelineScroller,
  pub axis: TimelineAxis,
}

impl Default for ScrollFunctionTimeline {
  fn default() -> Self {
    Self {
      scroller: ScrollTimelineScroller::default(),
      axis: TimelineAxis::default(),
    }
  }
}

/// Optional inset offsets for view timelines.
///
/// The inset values are stored as length-percentage values.
///
/// `None` represents the `auto` keyword (use the scroll container's `scroll-padding`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ViewTimelineInset {
  pub start: Option<Length>,
  pub end: Option<Length>,
}

/// An anonymous view timeline produced by `view(...)` in `animation-timeline`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewFunctionTimeline {
  pub scroller: ScrollTimelineScroller,
  pub axis: TimelineAxis,
  pub inset: Option<ViewTimelineInset>,
}

impl Default for ViewFunctionTimeline {
  fn default() -> Self {
    Self {
      scroller: ScrollTimelineScroller::default(),
      axis: TimelineAxis::default(),
      inset: None,
    }
  }
}

/// A view-driven timeline tied to a target element.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewTimeline {
  /// Optional timeline name.
  pub name: Option<String>,
  /// Axis used for visibility tracking.
  pub axis: TimelineAxis,
  /// Optional inset offsets.
  pub inset: Option<ViewTimelineInset>,
}

impl Default for ViewTimeline {
  fn default() -> Self {
    Self {
      name: None,
      axis: TimelineAxis::Block,
      inset: None,
    }
  }
}

/// Reference to a timeline used by an animation.
#[derive(Debug, Clone, PartialEq)]
pub enum AnimationTimeline {
  /// Default time-based timeline.
  Auto,
  /// No timeline; animation disabled.
  None,
  /// Named timeline reference.
  Named(String),
  /// Anonymous scroll timeline (`scroll(...)`).
  Scroll(ScrollFunctionTimeline),
  /// Anonymous view timeline (`view(...)`).
  View(ViewFunctionTimeline),
}

impl Default for AnimationTimeline {
  fn default() -> Self {
    AnimationTimeline::Auto
  }
}

/// Phases available for view timelines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewTimelinePhase {
  Entry,
  EntryCrossing,
  Contain,
  Cover,
  Exit,
  ExitCrossing,
}

/// Offset for animation-range.
#[derive(Debug, Clone, PartialEq)]
pub enum RangeOffset {
  /// Position expressed as normalized progress (0-1) on the timeline.
  Progress(f32),
  /// Position expressed as a length-percentage measured from the start of the timeline.
  Length(Length),
  /// Position based on a view-timeline phase plus optional adjustment.
  ///
  /// The adjustment is a length-percentage resolved against the length of the named timeline
  /// range.
  View(ViewTimelinePhase, Length),
}

impl Default for RangeOffset {
  fn default() -> Self {
    RangeOffset::Progress(0.0)
  }
}

/// Start/end offsets for an animation on a timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimationRange {
  pub start: RangeOffset,
  pub end: RangeOffset,
}

impl Default for AnimationRange {
  fn default() -> Self {
    AnimationRange {
      start: RangeOffset::Progress(0.0),
      end: RangeOffset::Progress(1.0),
    }
  }
}

/// CSS `animation-direction`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationDirection {
  Normal,
  Reverse,
  Alternate,
  AlternateReverse,
}

impl Default for AnimationDirection {
  fn default() -> Self {
    AnimationDirection::Normal
  }
}

/// CSS `animation-fill-mode`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationFillMode {
  None,
  Forwards,
  Backwards,
  Both,
}

impl Default for AnimationFillMode {
  fn default() -> Self {
    AnimationFillMode::None
  }
}

/// CSS `animation-composition`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationComposition {
  Replace,
  Add,
  Accumulate,
}

impl Default for AnimationComposition {
  fn default() -> Self {
    AnimationComposition::Replace
  }
}

/// CSS `animation-play-state`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationPlayState {
  Running,
  Paused,
}

impl Default for AnimationPlayState {
  fn default() -> Self {
    AnimationPlayState::Running
  }
}

/// CSS `animation-iteration-count`
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnimationIterationCount {
  Count(f32),
  Infinite,
}

impl Default for AnimationIterationCount {
  fn default() -> Self {
    AnimationIterationCount::Count(1.0)
  }
}

impl AnimationIterationCount {
  pub fn as_f32(self) -> f32 {
    match self {
      AnimationIterationCount::Count(v) => v,
      AnimationIterationCount::Infinite => f32::INFINITY,
    }
  }
}

/// Property list entry for CSS transitions.
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionProperty {
  All,
  None,
  Name(String),
}

/// Controls whether discrete transitions are allowed to run.
///
/// CSS: `transition-behavior` (CSS Transitions Level 2)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionBehavior {
  /// Default: discrete transitions are suppressed.
  Normal,
  /// Allow discrete transitions to run (switching at the 50% midpoint).
  AllowDiscrete,
}

impl Default for TransitionBehavior {
  fn default() -> Self {
    Self::Normal
  }
}

/// Step timing-function position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepPosition {
  Start,
  End,
  JumpNone,
  JumpBoth,
}

/// Stop in a `linear()` easing function.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearStop {
  /// Input progress in the range [0, 1] (though authored percentages may exceed this).
  pub input: f32,
  /// Output progress at this stop (can overshoot).
  pub output: f32,
}

/// Timing function used by CSS transitions.
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionTimingFunction {
  Ease,
  Linear,
  EaseIn,
  EaseOut,
  EaseInOut,
  CubicBezier(f32, f32, f32, f32),
  Steps(u32, StepPosition),
  /// CSS Easing Level 2 `linear()` function, represented as a list of piecewise-linear stops.
  LinearFunction(Vec<LinearStop>),
}

impl TransitionTimingFunction {
  pub fn value_at(&self, progress: f32) -> f32 {
    let t = progress.clamp(0.0, 1.0);
    match self {
      TransitionTimingFunction::Linear => t,
      TransitionTimingFunction::Ease => cubic_bezier_value(0.25, 0.1, 0.25, 1.0, t),
      TransitionTimingFunction::EaseIn => cubic_bezier_value(0.42, 0.0, 1.0, 1.0, t),
      TransitionTimingFunction::EaseOut => cubic_bezier_value(0.0, 0.0, 0.58, 1.0, t),
      TransitionTimingFunction::EaseInOut => cubic_bezier_value(0.42, 0.0, 0.58, 1.0, t),
      TransitionTimingFunction::CubicBezier(x1, y1, x2, y2) => {
        cubic_bezier_value(*x1, *y1, *x2, *y2, t)
      }
      TransitionTimingFunction::Steps(steps, position) => {
        let steps = (*steps).max(1);
        let steps_f = steps as f32;
        match position {
          StepPosition::Start => ((t * steps_f).ceil() / steps_f).clamp(0.0, 1.0),
          StepPosition::End => ((t * steps_f).floor() / steps_f).clamp(0.0, 1.0),
          StepPosition::JumpNone => {
            if steps <= 1 {
              if t >= 1.0 {
                1.0
              } else {
                0.0
              }
            } else {
              let denom = (steps - 1) as f32;
              let idx = (t * steps_f).floor().min((steps - 1) as f32);
              (idx / denom).clamp(0.0, 1.0)
            }
          }
          StepPosition::JumpBoth => {
            let denom = (steps + 1) as f32;
            if t <= 0.0 {
              0.0
            } else {
              let idx = (t * steps_f).floor() + 1.0;
              (idx / denom).clamp(0.0, 1.0)
            }
          }
        }
      }
      TransitionTimingFunction::LinearFunction(stops) => linear_function_value(stops, t),
    }
  }
}

fn cubic_bezier_value(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
  // CSS cubic-bezier() is defined as y(x), not y(t). We must first solve x(t) = progress,
  // then evaluate y(t). This matches browser behavior and is required for animation/transition
  // sampling at intermediate times.
  //
  // We use Newton–Raphson iteration (fast convergence for typical curves) and fall back to
  // bisection on [0, 1] (guaranteed for monotonic x, which CSS enforces via x1/x2 ∈ [0, 1]).
  if t <= 0.0 {
    return 0.0;
  }
  if t >= 1.0 {
    return 1.0;
  }

  // Use f64 internally to reduce precision loss during the root finding iterations.
  let progress = t as f64;
  let x1 = x1 as f64;
  let y1 = y1 as f64;
  let x2 = x2 as f64;
  let y2 = y2 as f64;

  // Cubic Bézier coefficients for P0=0, P1=p1, P2=p2, P3=1:
  // B(t) = ((a * t + b) * t + c) * t
  // with:
  //   a = 1 - 3*p2 + 3*p1
  //   b = 3*p2 - 6*p1
  //   c = 3*p1
  let ax = 1.0 - 3.0 * x2 + 3.0 * x1;
  let bx = 3.0 * x2 - 6.0 * x1;
  let cx = 3.0 * x1;
  let ay = 1.0 - 3.0 * y2 + 3.0 * y1;
  let by = 3.0 * y2 - 6.0 * y1;
  let cy = 3.0 * y1;

  let sample_x = |t: f64| ((ax * t + bx) * t + cx) * t;
  let sample_y = |t: f64| ((ay * t + by) * t + cy) * t;
  let sample_dx = |t: f64| (3.0 * ax * t + 2.0 * bx) * t + cx;

  // 1) Newton–Raphson solve
  let mut curve_t = progress;
  for _ in 0..8 {
    let x = sample_x(curve_t) - progress;
    if x.abs() < 1e-7 {
      return sample_y(curve_t) as f32;
    }
    let dx = sample_dx(curve_t);
    // If the slope is too small, Newton steps become unstable.
    if dx.abs() < 1e-7 {
      break;
    }
    let next_t = curve_t - x / dx;
    if !(0.0..=1.0).contains(&next_t) {
      break;
    }
    curve_t = next_t;
  }

  // 2) Bisection fallback (monotonic x)
  let mut lo = 0.0f64;
  let mut hi = 1.0f64;
  curve_t = progress;
  for _ in 0..30 {
    let x = sample_x(curve_t);
    let diff = x - progress;
    if diff.abs() < 1e-7 {
      break;
    }
    if diff < 0.0 {
      lo = curve_t;
    } else {
      hi = curve_t;
    }
    curve_t = (lo + hi) * 0.5;
  }

  sample_y(curve_t) as f32
}

#[cfg(test)]
mod tests {
  use super::*;

  fn assert_approx(actual: f32, expected: f32) {
    let epsilon = 1e-4;
    assert!(
      (actual - expected).abs() <= epsilon,
      "expected {expected}, got {actual}"
    );
  }

  #[test]
  fn cubic_bezier_timing_function_matches_known_values() {
    // Edge cases: all cubic-bezier timing functions should map 0 -> 0 and 1 -> 1.
    assert_eq!(TransitionTimingFunction::Ease.value_at(0.0), 0.0);
    assert_eq!(TransitionTimingFunction::Ease.value_at(1.0), 1.0);

    assert_approx(TransitionTimingFunction::Ease.value_at(0.5), 0.8024034);
    assert_approx(TransitionTimingFunction::EaseIn.value_at(0.5), 0.3153568);
    assert_approx(TransitionTimingFunction::EaseOut.value_at(0.5), 0.6846432);

    assert_approx(
      TransitionTimingFunction::EaseInOut.value_at(0.25),
      0.12916193,
    );
    assert_approx(
      TransitionTimingFunction::EaseInOut.value_at(0.75),
      0.87083807,
    );

    let custom = TransitionTimingFunction::CubicBezier(0.65, 0.0, 0.35, 1.0);
    assert_eq!(custom.value_at(0.0), 0.0);
    assert_eq!(custom.value_at(1.0), 1.0);
    assert_approx(custom.value_at(0.25), 0.07079670);
    assert_approx(custom.value_at(0.75), 0.92920330);
  }
}

fn linear_function_value(stops: &[LinearStop], t: f32) -> f32 {
  if stops.len() < 2 {
    return t;
  }
  let mut prev = stops[0];
  if t <= prev.input {
    return prev.output;
  }
  for &next in &stops[1..] {
    if t <= next.input {
      let denom = next.input - prev.input;
      if denom.abs() <= f32::EPSILON {
        return next.output;
      }
      let alpha = ((t - prev.input) / denom).clamp(0.0, 1.0);
      return prev.output + (next.output - prev.output) * alpha;
    }
    prev = next;
  }
  prev.output
}

/// CSS `scrollbar-gutter`
///
/// Controls whether scroll containers reserve space for scrollbars, and whether
/// gutters appear on both edges or only the inline end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGutter {
  /// Reserve scrollbar space even when scrollbars are not currently showing
  pub stable: bool,
  /// Place gutters on both inline edges instead of only the inline end
  pub both_edges: bool,
}

impl Default for ScrollbarGutter {
  fn default() -> Self {
    ScrollbarGutter {
      stable: false,
      both_edges: false,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserSelect {
  Auto,
  Text,
  None,
  All,
  Contain,
}

impl Default for UserSelect {
  fn default() -> Self {
    UserSelect::Auto
  }
}

impl Default for ScrollBehavior {
  fn default() -> Self {
    ScrollBehavior::Auto
  }
}

impl Default for ScrollbarColor {
  fn default() -> Self {
    ScrollbarColor::Auto
  }
}

/// overscroll-behavior values
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverscrollBehavior {
  Auto,
  Contain,
  None,
}

impl Default for OverscrollBehavior {
  fn default() -> Self {
    OverscrollBehavior::Auto
  }
}

/// Fragmentation break opportunities between boxes (page/column breaks)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakBetween {
  /// Follow normal fragmentainer breaking rules
  Auto,
  /// Avoid breaking before/after the element
  Avoid,
  /// Avoid breaking across pages but allow column breaks.
  AvoidPage,
  /// Avoid breaking across columns but allow page breaks.
  AvoidColumn,
  /// Force a fragment break
  Always,
  /// Force a column break
  Column,
  /// Force a page break
  Page,
  /// Force a page break and start the next page on the left/verso side.
  Left,
  /// Force a page break and start the next page on the right/recto side.
  Right,
  /// Force a page break and start the next page on the right/recto side.
  Recto,
  /// Force a page break and start the next page on the left/verso side.
  Verso,
}

impl Default for BreakBetween {
  fn default() -> Self {
    BreakBetween::Auto
  }
}

/// Fragmentation control within an element's contents
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakInside {
  /// Allow breaking inside the element
  Auto,
  /// Avoid breaking inside the element
  Avoid,
  /// Avoid breaking the element across pages.
  AvoidPage,
  /// Avoid breaking the element across columns.
  AvoidColumn,
}

impl Default for BreakInside {
  fn default() -> Self {
    BreakInside::Auto
  }
}

/// CSS GCPM `footnote-policy` property.
///
/// Controls whether a footnote body may be placed on a later page than its reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FootnotePolicy {
  /// UA may keep the reference on the current page and place the body later.
  Auto,
  /// Keep the reference line with the footnote body (legacy FastRender behavior).
  Line,
  /// Keep the reference block with the footnote body.
  Block,
}

impl Default for FootnotePolicy {
  fn default() -> Self {
    // FastRender historically behaved like `footnote-policy: line`; keep that as the initial value
    // so existing paged-media tests don't change unless the author opts into `auto`.
    FootnotePolicy::Line
  }
}

/// CSS GCPM `footnote-display` property.
///
/// Controls how `float: footnote` bodies are placed inside the per-page footnote area.
///
/// Spec (CSS GCPM 3): <https://drafts.csswg.org/css-gcpm-3/#footnote-display>
///
/// - Applies to: elements
/// - Inherited: no
/// - Initial: `block`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FootnoteDisplay {
  /// Place the footnote element as a block (stacked vertically).
  Block,
  /// Place the footnote element as an inline element (allow multiple per line).
  Inline,
  /// UA-controlled block/inline choice; initial implementation treats this as `inline`.
  Compact,
}

impl Default for FootnoteDisplay {
  fn default() -> Self {
    FootnoteDisplay::Block
  }
}

/// CSS `resize` property
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resize {
  None,
  Both,
  Horizontal,
  Vertical,
  Block,
  Inline,
}

impl Default for Resize {
  fn default() -> Self {
    Resize::None
  }
}

/// CSS `field-sizing` property (CSS UI Level 4).
///
/// Determines whether form controls use legacy fixed intrinsic sizing (`fixed`) or size to their
/// current text contents (`content`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSizing {
  Fixed,
  Content,
}

impl Default for FieldSizing {
  fn default() -> Self {
    FieldSizing::Fixed
  }
}

/// CSS `pointer-events`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerEvents {
  Auto,
  None,
  VisiblePainted,
  VisibleFill,
  VisibleStroke,
  Visible,
  Painted,
  Fill,
  Stroke,
  All,
}

impl Default for PointerEvents {
  fn default() -> Self {
    PointerEvents::Auto
  }
}

/// Cursor keywords (fallbacks for custom cursor images)
///
/// CSS UI Level 4 cursor values (subset relevant to rendering hints)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorKeyword {
  Auto,
  Default,
  None,
  ContextMenu,
  Help,
  Pointer,
  Progress,
  Wait,
  Cell,
  Crosshair,
  Text,
  VerticalText,
  Alias,
  Copy,
  Move,
  NoDrop,
  NotAllowed,
  Grab,
  Grabbing,
  AllScroll,
  ColResize,
  RowResize,
  NResize,
  SResize,
  EResize,
  WResize,
  NeResize,
  NwResize,
  SeResize,
  SwResize,
  EwResize,
  NsResize,
  NeswResize,
  NwseResize,
  ZoomIn,
  ZoomOut,
}

impl Default for CursorKeyword {
  fn default() -> Self {
    CursorKeyword::Auto
  }
}

/// A custom cursor image with an optional hotspot (x, y) in CSS pixels
#[derive(Debug, Clone, PartialEq)]
pub struct CursorImage {
  pub url: UrlImage,
  pub hotspot: Option<(f32, f32)>,
}

/// CSS will-change hints
///
/// CSS: `will-change`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WillChange {
  /// Default value – no proactive optimizations
  Auto,
  /// Explicit list of features the author expects to change
  Hints(Vec<WillChangeHint>),
}

impl Default for WillChange {
  fn default() -> Self {
    Self::Auto
  }
}

/// Individual will-change hints
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WillChangeHint {
  ScrollPosition,
  Contents,
  /// A property name (lowercased)
  Property(String),
}

impl WillChange {
  /// Returns true if the hint set should proactively create a stacking context.
  ///
  /// CSS Will Change: "will-change set to a property that would create a stacking context
  /// when not at its initial value" requires creating a stacking context up-front.
  pub fn creates_stacking_context(&self) -> bool {
    match self {
      WillChange::Auto => false,
      WillChange::Hints(hints) => hints.iter().any(WillChangeHint::creates_stacking_context),
    }
  }

  /// Returns true if the hint set should proactively establish a Backdrop Root.
  ///
  /// Filter Effects Level 2 defines *Backdrop Roots* to scope the backdrop image used by
  /// `backdrop-filter` and `mix-blend-mode`. Backdrop roots are a strict subset of stacking context
  /// triggers; notably `will-change: transform` should not establish a backdrop root.
  pub fn establishes_backdrop_root(&self) -> bool {
    match self {
      WillChange::Auto => false,
      WillChange::Hints(hints) => hints.iter().any(WillChangeHint::establishes_backdrop_root),
    }
  }
}

impl WillChangeHint {
  fn creates_stacking_context(&self) -> bool {
    match self {
      WillChangeHint::ScrollPosition | WillChangeHint::Contents => true,
      WillChangeHint::Property(name) => matches!(
        name.as_str(),
        // Properties that create stacking contexts when non-initial
        "transform"
          | "translate"
          | "rotate"
          | "scale"
          | "opacity"
          | "filter"
          | "backdrop-filter"
          | "perspective"
          | "clip-path"
          | "mask"
          | "mask-image"
          | "mask-border"
          | "mask-border-source"
          | "mix-blend-mode"
          | "isolation"
          | "contain"
          | "container-type"
      ),
    }
  }

  fn establishes_backdrop_root(&self) -> bool {
    match self {
      // These hints exist for performance and do not map to any single property that establishes a
      // Backdrop Root.
      WillChangeHint::ScrollPosition | WillChangeHint::Contents => false,
      WillChangeHint::Property(name) => matches!(
        name.as_str(),
        "filter"
          | "opacity"
          | "mask"
          | "mask-image"
          | "mask-border"
          | "mask-border-source"
          | "clip-path"
          | "backdrop-filter"
          | "mix-blend-mode"
      ),
    }
  }
}

/// CSS containment model
///
/// CSS: `contain`
/// Reference: CSS Containment Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Containment {
  pub size: bool,
  pub inline_size: bool,
  pub layout: bool,
  pub style: bool,
  pub paint: bool,
}

impl Containment {
  pub const fn none() -> Self {
    Self {
      size: false,
      inline_size: false,
      layout: false,
      style: false,
      paint: false,
    }
  }

  pub const fn strict() -> Self {
    Self {
      size: true,
      inline_size: false,
      layout: true,
      style: true,
      paint: true,
    }
  }

  pub const fn content() -> Self {
    Self {
      size: false,
      inline_size: false,
      layout: true,
      style: true,
      paint: true,
    }
  }

  #[allow(clippy::fn_params_excessive_bools)]
  pub fn with_flags(size: bool, inline_size: bool, layout: bool, style: bool, paint: bool) -> Self {
    Self {
      size,
      inline_size,
      layout,
      style,
      paint,
    }
  }

  pub fn creates_stacking_context(&self) -> bool {
    self.paint
  }

  /// Returns true when the inline axis should ignore descendant contributions for intrinsic sizing.
  ///
  /// Layout containment implies inline-size containment per the CSS Containment spec, so treat it
  /// the same as explicit size/inline-size containment when measuring preferred widths.
  pub fn isolates_inline_size(&self) -> bool {
    self.size || self.inline_size || self.layout
  }

  /// Returns true when block-size should not be derived from descendants.
  ///
  /// Only full size containment (or strict containment which includes size) applies here; inline
  /// size containment is limited to the inline axis.
  pub fn isolates_block_size(&self) -> bool {
    self.size
  }

  /// Returns true when paint containment rules apply.
  pub fn isolates_paint(&self) -> bool {
    self.paint
  }
}

/// Controls whether an element's contents are rendered.
///
/// CSS: `content-visibility`
/// Reference: CSS Containment Module Level 3 / CSS Content Visibility Module Level 1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentVisibility {
  Visible,
  Hidden,
  Auto,
}

impl Default for ContentVisibility {
  fn default() -> Self {
    Self::Visible
  }
}

/// Intrinsic sizing keywords accepted by the size properties.
///
/// Used by `width`/`height`/`min-*`/`max-*` when authored as `min-content`,
/// `max-content`, or `fit-content(...)` instead of a length/percentage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IntrinsicSizeKeyword {
  MinContent,
  MaxContent,
  FillAvailable,
  /// Represents `fit-content` and `fit-content(<length-percentage>)`.
  FitContent {
    limit: Option<Length>,
  },
  /// Represents `calc-size(<basis>, <calc-sum>)` (CSS Values 5).
  CalcSize(CalcSize),
}

/// The `<basis>` argument to `calc-size()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CalcSizeBasis {
  Auto,
  MinContent,
  MaxContent,
  FillAvailable,
  FitContent { limit: Option<Length> },
  Length(Length),
}

/// Parsed `calc-size(<basis>, <calc-sum>)` value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalcSize {
  pub basis: CalcSizeBasis,
  pub expr: CalcSizeExprId,
}

impl IntrinsicSizeKeyword {
  pub fn has_percentage(&self) -> bool {
    match self {
      Self::FitContent { limit: Some(limit) } => limit.has_percentage(),
      Self::FillAvailable => false,
      Self::CalcSize(calc) => {
        let basis_has_percentage = match calc.basis {
          CalcSizeBasis::FitContent { limit: Some(limit) } => limit.has_percentage(),
          CalcSizeBasis::Length(len) => len.has_percentage(),
          _ => false,
        };
        basis_has_percentage || crate::style::values::calc_size_expr_has_percentage(calc.expr)
      }
      _ => false,
    }
  }

  pub fn fit_content_limit(&self) -> Option<Length> {
    match self {
      Self::FitContent { limit } => *limit,
      _ => None,
    }
  }
}

impl Eq for IntrinsicSizeKeyword {}

impl Hash for IntrinsicSizeKeyword {
  fn hash<H: Hasher>(&self, state: &mut H) {
    std::mem::discriminant(self).hash(state);
    match self {
      IntrinsicSizeKeyword::FitContent { limit } => match limit {
        Some(len) => {
          1u8.hash(state);
          hash_length_for_intrinsic_size_keyword(len, state);
        }
        None => 0u8.hash(state),
      },
      IntrinsicSizeKeyword::CalcSize(calc) => {
        std::mem::discriminant(&calc.basis).hash(state);
        match calc.basis {
          CalcSizeBasis::FitContent { limit } => match limit {
            Some(len) => {
              1u8.hash(state);
              hash_length_for_intrinsic_size_keyword(&len, state);
            }
            None => 0u8.hash(state),
          },
          CalcSizeBasis::Length(len) => {
            2u8.hash(state);
            hash_length_for_intrinsic_size_keyword(&len, state);
          }
          _ => {}
        }
        calc.expr.hash(state);
      }
      IntrinsicSizeKeyword::FillAvailable => {}
      _ => {}
    }
  }
}

fn f32_to_canonical_bits_for_intrinsic_size_keyword(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

fn hash_length_for_intrinsic_size_keyword<H: Hasher>(len: &Length, state: &mut H) {
  len.unit.hash(state);
  f32_to_canonical_bits_for_intrinsic_size_keyword(len.value).hash(state);
  match &len.calc {
    Some(calc) => {
      1u8.hash(state);
      match calc {
        crate::style::values::LengthCalc::Linear(calc) => {
          0u8.hash(state);
          let terms = calc.terms();
          (terms.len() as u8).hash(state);
          for term in terms {
            term.unit.hash(state);
            f32_to_canonical_bits_for_intrinsic_size_keyword(term.value).hash(state);
          }
        }
        crate::style::values::LengthCalc::Expr(id) => {
          1u8.hash(state);
          id.hash(state);
        }
      }
    }
    None => 0u8.hash(state),
  }
}

/// Intrinsic size fallback for an axis when an element's contents are skipped.
///
/// CSS: `contain-intrinsic-*`
/// Reference: CSS Containment Module Level 3 / CSS Content Visibility Module Level 1
///
/// This engine supports a subset of the grammar used on real pages:
/// - `none`
/// - `<length-percentage>`
/// - `auto`
/// - `auto <length-percentage>`
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContainIntrinsicSizeAxis {
  /// Whether the axis uses the `auto` keyword semantics.
  pub auto: bool,
  /// Optional fallback length (e.g. `contain-intrinsic-size: auto 100px`).
  pub length: Option<Length>,
}

impl ContainIntrinsicSizeAxis {
  pub const fn none() -> Self {
    Self {
      auto: false,
      length: None,
    }
  }

  pub const fn auto() -> Self {
    Self {
      auto: true,
      length: None,
    }
  }
}

impl Default for ContainIntrinsicSizeAxis {
  fn default() -> Self {
    // The initial value is `auto` (with no explicit fallback length).
    Self::auto()
  }
}

impl Default for Containment {
  fn default() -> Self {
    Self::none()
  }
}

/// Color value that can defer to currentcolor
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterColor {
  CurrentColor,
  Color(Rgba),
}

/// Shadow parameters for drop-shadow()
#[derive(Debug, Clone, PartialEq)]
pub struct FilterShadow {
  pub offset_x: Length,
  pub offset_y: Length,
  pub blur_radius: Length,
  pub spread: Length,
  pub color: FilterColor,
}

/// CSS filter() functions
#[derive(Debug, Clone, PartialEq)]
pub enum FilterFunction {
  Blur(Length),
  Brightness(f32),
  Contrast(f32),
  Grayscale(f32),
  Sepia(f32),
  Saturate(f32),
  HueRotate(f32), // degrees
  Invert(f32),
  Opacity(f32),
  DropShadow(Box<FilterShadow>),
  Url(String),
}

/// Transform origin for x/y axes
///
/// CSS: `transform-origin`
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransformOrigin {
  pub x: Length,
  pub y: Length,
  /// Optional z offset for 3D transforms (`transform-origin` third component).
  ///
  /// CSS defines this as a `<length>` (no percentages). We keep it as a `Length` so
  /// computed styles can round-trip authored values; callers should treat
  /// non-length units conservatively.
  pub z: Length,
}

/// A position used by the `offset-path: path(...)` function.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionPosition {
  pub x: Length,
  pub y: Length,
}

/// A simplified path command list used by `offset-path: path(...)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MotionPathCommand {
  MoveTo(MotionPosition),
  LineTo(MotionPosition),
  ClosePath,
}

/// Parameters for the `ray()` function used by `offset-path`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray {
  /// Angle in degrees.
  pub angle: f32,
  /// Optional ray length.
  pub length: Option<Length>,
  /// Whether to clamp the ray within the element bounds.
  pub contain: bool,
}

/// Computed value for `offset-path` (CSS Motion Path Module).
#[derive(Debug, Clone, PartialEq)]
pub enum OffsetPath {
  None,
  Ray(Ray),
  Path(Vec<MotionPathCommand>),
  BasicShape(Box<BasicShape>),
}

/// Computed value for `offset-rotate` (CSS Motion Path Module).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OffsetRotate {
  Auto { angle: f32, reverse: bool },
  Angle(f32),
}

/// Computed value for `offset-anchor` (CSS Motion Path Module).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OffsetAnchor {
  Auto,
  Position { x: Length, y: Length },
}

/// Main axis alignment for flex items
///
/// CSS: `justify-content`
/// Reference: CSS Flexible Box Layout Module Level 1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JustifyContent {
  /// CSS initial value (`normal`).
  ///
  /// This resolves at used-value time depending on the layout model:
  /// - Flex containers: behaves like `flex-start`.
  /// - Grid containers: behaves like `stretch`.
  ///
  /// Ref: <https://www.w3.org/TR/css-align-3/#valdef-justify-content-normal>
  Normal,
  Start,
  End,
  FlexStart,
  FlexEnd,
  Center,
  Stretch,
  SpaceBetween,
  SpaceAround,
  SpaceEvenly,
}

/// Cross axis alignment for flex items
///
/// CSS: `align-items`
/// Reference: CSS Flexible Box Layout Module Level 1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignItems {
  Start,
  End,
  SelfStart,
  SelfEnd,
  FlexStart,
  FlexEnd,
  Center,
  /// CSS Anchor Positioning `anchor-center` alignment value.
  ///
  /// Per spec, behaves like `center` unless the element is being aligned against a default anchor
  /// (e.g. an absolutely positioned box with `position-area`).
  AnchorCenter,
  Baseline,
  Stretch,
}

/// Multi-line cross axis alignment
///
/// CSS: `align-content`
/// Reference: CSS Flexible Box Layout Module Level 1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignContent {
  Start,
  End,
  FlexStart,
  FlexEnd,
  Center,
  SpaceBetween,
  SpaceEvenly,
  SpaceAround,
  Stretch,
}

/// Flex item initial main size
///
/// CSS: `flex-basis`
/// Reference: CSS Flexible Box Layout Module Level 1
#[derive(Debug, Clone, PartialEq)]
pub enum FlexBasis {
  Auto,
  Content,
  Length(Length),
}

/// Grid track size specification
///
/// CSS: `grid-template-columns`, `grid-template-rows`
/// Reference: CSS Grid Layout Module Level 1
#[derive(Debug, Clone, PartialEq)]
pub enum GridTrack {
  Length(Length),
  Fr(f32),
  Auto,
  MinContent,
  MaxContent,
  FitContent(Length),
  MinMax(Box<GridTrack>, Box<GridTrack>),
  RepeatAutoFill {
    tracks: Vec<GridTrack>,
    line_names: Vec<Vec<String>>,
  },
  RepeatAutoFit {
    tracks: Vec<GridTrack>,
    line_names: Vec<Vec<String>>,
  },
}

/// Auto-placement direction and density for implicit grid items
///
/// CSS: `grid-auto-flow`
/// Reference: CSS Grid Layout Module Level 1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridAutoFlow {
  Row,
  Column,
  RowDense,
  ColumnDense,
}

/// Font weight
///
/// CSS: `font-weight`
/// Reference: CSS Fonts Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontWeight {
  Normal,
  Bold,
  Bolder,
  Lighter,
  Number(u16),
}

impl FontWeight {
  /// Clamp a numeric weight to the CSS valid range (1-1000)
  fn clamp(weight: u16) -> u16 {
    weight.clamp(1, 1000)
  }

  /// Resolve the `bolder` keyword relative to a parent weight using the CSS Fonts Level 4 table.
  ///
  /// See: https://www.w3.org/TR/css-fonts-4/#relative-weights
  fn relative_bolder(parent_weight: u16) -> u16 {
    let w = Self::clamp(parent_weight);
    if w < 100 {
      400
    } else if w < 350 {
      400
    } else if w < 550 {
      700
    } else if w < 750 {
      900
    } else if w < 900 {
      900
    } else {
      w
    }
  }

  /// Resolve the `lighter` keyword relative to a parent weight using the CSS Fonts Level 4 table.
  ///
  /// See: https://www.w3.org/TR/css-fonts-4/#relative-weights
  fn relative_lighter(parent_weight: u16) -> u16 {
    let w = Self::clamp(parent_weight);
    if w < 100 {
      w
    } else if w < 350 {
      100
    } else if w < 550 {
      100
    } else if w < 750 {
      400
    } else if w < 900 {
      700
    } else {
      700
    }
  }

  /// Resolve relative keywords (bolder/lighter) against the parent weight and clamp numeric values.
  pub(crate) fn resolve_relative(self, parent_weight: u16) -> Self {
    match self {
      FontWeight::Bolder => FontWeight::Number(Self::relative_bolder(parent_weight)),
      FontWeight::Lighter => FontWeight::Number(Self::relative_lighter(parent_weight)),
      FontWeight::Number(n) => FontWeight::Number(Self::clamp(n)),
      other => other,
    }
  }

  /// Converts font weight to numeric u16 value (1-1000)
  pub fn to_u16(self) -> u16 {
    match self {
      FontWeight::Normal => 400,
      FontWeight::Bold => 700,
      FontWeight::Bolder => Self::relative_bolder(400),
      FontWeight::Lighter => Self::relative_lighter(400),
      FontWeight::Number(n) => Self::clamp(n),
    }
  }
}

/// Font style (normal, italic, oblique)
///
/// CSS: `font-style`
/// Reference: CSS Fonts Module Level 3
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontStyle {
  Normal,
  Italic,
  /// Oblique may carry an optional angle (deg)
  Oblique(Option<f32>),
}

impl FontStyle {
  pub fn is_italic(self) -> bool {
    matches!(self, FontStyle::Italic)
  }

  pub fn is_oblique(self) -> bool {
    matches!(self, FontStyle::Oblique(_))
  }

  pub fn oblique_angle(self) -> Option<f32> {
    match self {
      FontStyle::Oblique(angle) => angle,
      _ => None,
    }
  }
}

/// Font variant
///
/// CSS: `font-variant`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontVariant {
  Normal,
  SmallCaps,
}

/// Caps variants (font-variant-caps)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontVariantCaps {
  Normal,
  SmallCaps,
  AllSmallCaps,
  PetiteCaps,
  AllPetiteCaps,
  Unicase,
  TitlingCaps,
}

impl Default for FontVariantCaps {
  fn default() -> Self {
    FontVariantCaps::Normal
  }
}

/// A numeric or named argument to a `font-variant-alternates` function (e.g. `styleset(1)` or
/// `styleset(disambiguation)`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FontVariantAlternateValue {
  Number(u8),
  Name(String),
}

/// Alternates (`font-variant-alternates`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontVariantAlternates {
  pub historical_forms: bool,
  pub stylistic: Option<FontVariantAlternateValue>,
  pub stylesets: Vec<FontVariantAlternateValue>,
  pub character_variants: Vec<FontVariantAlternateValue>,
  pub swash: Option<FontVariantAlternateValue>,
  pub ornaments: Option<FontVariantAlternateValue>,
  pub annotation: Option<FontVariantAlternateValue>,
}

impl Default for FontVariantAlternates {
  fn default() -> Self {
    Self {
      historical_forms: false,
      stylistic: None,
      stylesets: Vec::new(),
      character_variants: Vec::new(),
      swash: None,
      ornaments: None,
      annotation: None,
    }
  }
}

/// Numeric variants (`font-variant-numeric`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericFigure {
  Normal,
  Lining,
  Oldstyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericSpacing {
  Normal,
  Proportional,
  Tabular,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericFraction {
  Normal,
  Diagonal,
  Stacked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontVariantNumeric {
  pub figure: NumericFigure,
  pub spacing: NumericSpacing,
  pub fraction: NumericFraction,
  pub ordinal: bool,
  pub slashed_zero: bool,
}

impl Default for FontVariantNumeric {
  fn default() -> Self {
    Self {
      figure: NumericFigure::Normal,
      spacing: NumericSpacing::Normal,
      fraction: NumericFraction::Normal,
      ordinal: false,
      slashed_zero: false,
    }
  }
}

/// Font ligature controls (font-variant-ligatures)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontVariantLigatures {
  pub common: bool,
  pub discretionary: bool,
  pub historical: bool,
  pub contextual: bool,
}

impl Default for FontVariantLigatures {
  fn default() -> Self {
    // Initial value "normal": common + contextual on; discretionary/historical off.
    Self {
      common: true,
      discretionary: false,
      historical: false,
      contextual: true,
    }
  }
}

/// Low-level OpenType feature override (`font-feature-settings`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontFeatureSetting {
  pub tag: [u8; 4],
  pub value: u32,
}

/// Low-level font variation override (`font-variation-settings`)
#[derive(Debug, Clone, PartialEq)]
pub struct FontVariationSetting {
  pub tag: [u8; 4],
  pub value: f32,
}

/// Overrides the OpenType language system when shaping text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FontLanguageOverride {
  /// Use the element/Document language (default)
  Normal,
  /// Override with an explicit OpenType language system tag (1–4 ASCII letters)
  Override(String),
}

/// Palette selection for color fonts (`font-palette`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FontPalette {
  Normal,
  Light,
  Dark,
  Named(String),
}

/// Optical sizing control (`font-optical-sizing`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontOpticalSizing {
  Auto,
  None,
}

impl Default for FontPalette {
  fn default() -> Self {
    FontPalette::Normal
  }
}

/// Emoji rendering preference (`font-variant-emoji`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontVariantEmoji {
  Normal,
  Emoji,
  Text,
  Unicode,
}

/// East Asian variants (`font-variant-east-asian`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EastAsianVariant {
  Jis78,
  Jis83,
  Jis90,
  Jis04,
  Simplified,
  Traditional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EastAsianWidth {
  FullWidth,
  ProportionalWidth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontVariantEastAsian {
  pub variant: Option<EastAsianVariant>,
  pub width: Option<EastAsianWidth>,
  pub ruby: bool,
}

impl Default for FontVariantEastAsian {
  fn default() -> Self {
    Self {
      variant: None,
      width: None,
      ruby: false,
    }
  }
}

/// Positional variants (`font-variant-position`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontVariantPosition {
  Normal,
  Sub,
  Super,
}

/// Controls which font properties may be synthetically generated (`font-synthesis`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontSynthesis {
  pub weight: bool,
  pub style: bool,
  pub small_caps: bool,
  pub position: bool,
}

impl Default for FontSynthesis {
  fn default() -> Self {
    Self {
      weight: true,
      style: true,
      small_caps: true,
      position: true,
    }
  }
}

/// Font size adjustment ratio (`font-size-adjust`)
///
/// CSS Fonts 4 extends `font-size-adjust` to allow selecting which font metric the ratio applies
/// to (ex-height, cap-height, ch-width, ic-width, ic-height).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FontSizeAdjustMetric {
  /// `ex-height` (x-height). This is the default when the metric is omitted.
  ExHeight,
  /// `cap-height` (cap height).
  CapHeight,
  /// `ch-width` (advance width of U+0030 '0').
  ChWidth,
  /// `ic-width` (advance width of a representative ideograph, typically U+6C34 '水').
  IcWidth,
  /// `ic-height` (advance height of a representative ideograph, typically U+6C34 '水').
  IcHeight,
}

impl Default for FontSizeAdjustMetric {
  fn default() -> Self {
    Self::ExHeight
  }
}

impl FontSizeAdjustMetric {
  pub fn parse(keyword: &str) -> Option<Self> {
    if keyword.eq_ignore_ascii_case("ex-height") {
      Some(Self::ExHeight)
    } else if keyword.eq_ignore_ascii_case("cap-height") {
      Some(Self::CapHeight)
    } else if keyword.eq_ignore_ascii_case("ch-width") {
      Some(Self::ChWidth)
    } else if keyword.eq_ignore_ascii_case("ic-width") {
      Some(Self::IcWidth)
    } else if keyword.eq_ignore_ascii_case("ic-height") {
      Some(Self::IcHeight)
    } else {
      None
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontSizeAdjust {
  None,
  Number {
    ratio: f32,
    metric: FontSizeAdjustMetric,
  },
  FromFont {
    metric: FontSizeAdjustMetric,
  },
}

impl Default for FontSizeAdjust {
  fn default() -> Self {
    FontSizeAdjust::None
  }
}

/// Controls text inflation (`text-size-adjust`)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextSizeAdjust {
  Auto,
  None,
  Percentage(f32),
}

impl Default for TextSizeAdjust {
  fn default() -> Self {
    TextSizeAdjust::Auto
  }
}

/// Kerning control (`font-kerning`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontKerning {
  Auto,
  Normal,
  None,
}

impl Default for FontKerning {
  fn default() -> Self {
    FontKerning::Auto
  }
}

/// Font stretch
///
/// CSS: `font-stretch`
/// Reference: CSS Fonts Module Level 4
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontStretch {
  UltraCondensed,
  ExtraCondensed,
  Condensed,
  SemiCondensed,
  Normal,
  SemiExpanded,
  Expanded,
  ExtraExpanded,
  UltraExpanded,
  /// Percentage stretch (50%-200%)
  Percentage(f32),
}

impl FontStretch {
  /// Creates a FontStretch from a percentage value, clamped to the spec range (50%-200%).
  pub fn from_percentage(percent: f32) -> Self {
    let clamped = percent.clamp(50.0, 200.0);
    FontStretch::Percentage(clamped)
  }

  /// Returns the percentage representation of this stretch value.
  pub fn to_percentage(self) -> f32 {
    match self {
      FontStretch::UltraCondensed => 50.0,
      FontStretch::ExtraCondensed => 62.5,
      FontStretch::Condensed => 75.0,
      FontStretch::SemiCondensed => 87.5,
      FontStretch::Normal => 100.0,
      FontStretch::SemiExpanded => 112.5,
      FontStretch::Expanded => 125.0,
      FontStretch::ExtraExpanded => 150.0,
      FontStretch::UltraExpanded => 200.0,
      FontStretch::Percentage(p) => p.clamp(50.0, 200.0),
    }
  }
}

/// Line height specification
///
/// CSS: `line-height`
/// Reference: CSS 2.1 Section 10.8
#[derive(Debug, Clone, PartialEq)]
pub enum LineHeight {
  Normal,
  Number(f32),
  Length(Length),
  Percentage(f32),
}

/// Vertical alignment
///
/// CSS: `vertical-align`
/// Reference: CSS 2.1 §10.8.1
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum VerticalAlign {
  /// Align baseline with parent's baseline (initial value)
  #[default]
  Baseline,
  /// Lower baseline to parent's subscript position
  Sub,
  /// Raise baseline to parent's superscript position
  Super,
  /// Align box top with the parent's text-top edge
  TextTop,
  /// Align box bottom with the parent's text-bottom edge
  TextBottom,
  /// Center box within available space
  Middle,
  /// Align box top with container top
  Top,
  /// Align box bottom with container bottom
  Bottom,
  /// Shift baseline by a specific length (positive = up)
  Length(Length),
  /// Shift baseline by a percentage of the line-height
  Percentage(f32),
}

impl VerticalAlign {
  /// Returns true if the value participates in baseline alignment
  pub fn is_baseline_relative(self) -> bool {
    matches!(
      self,
      VerticalAlign::Baseline
        | VerticalAlign::Sub
        | VerticalAlign::Super
        | VerticalAlign::TextTop
        | VerticalAlign::TextBottom
        | VerticalAlign::Length(_)
        | VerticalAlign::Percentage(_)
    )
  }
}

/// Text horizontal alignment
///
/// CSS: `text-align`
/// Reference: CSS Text Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
  Start,
  End,
  Left,
  Right,
  Center,
  Justify,
  /// Justify all lines, including the last (text-align: justify-all)
  JustifyAll,
  MatchParent,
}

/// CSS `text-align-last`
///
/// Reference: CSS Text Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlignLast {
  Auto,
  Start,
  End,
  Left,
  Right,
  Center,
  Justify,
  MatchParent,
}

/// CSS `text-orientation`
///
/// Reference: CSS Writing Modes Level 4
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextOrientation {
  Mixed,
  Upright,
  Sideways,
  SidewaysLeft,
  SidewaysRight,
}

impl Default for TextOrientation {
  fn default() -> Self {
    TextOrientation::Mixed
  }
}

/// CSS `text-combine-upright`
///
/// Reference: CSS Writing Modes Level 4
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextCombineUpright {
  None,
  All,
  Digits(u8),
}

impl Default for TextCombineUpright {
  fn default() -> Self {
    TextCombineUpright::None
  }
}

/// CSS `text-justify`
///
/// Reference: CSS Text Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextJustify {
  Auto,
  None,
  InterWord,
  InterCharacter,
  Distribute,
}

/// CSS `hanging-punctuation`
///
/// Reference: CSS Text Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HangingPunctuation(pub u8);

impl HangingPunctuation {
  pub const NONE: Self = Self(0);
  pub const FIRST: Self = Self(1 << 0);
  pub const LAST: Self = Self(1 << 1);
  pub const FORCE_END: Self = Self(1 << 2);
  pub const ALLOW_END: Self = Self(1 << 3);

  #[inline]
  pub const fn contains(self, other: Self) -> bool {
    self.0 & other.0 == other.0
  }

  #[inline]
  pub const fn is_none(self) -> bool {
    self.0 == 0
  }

  #[inline]
  pub const fn has_first(self) -> bool {
    self.contains(Self::FIRST)
  }

  #[inline]
  pub const fn has_last(self) -> bool {
    self.contains(Self::LAST)
  }

  #[inline]
  pub const fn has_force_end(self) -> bool {
    self.contains(Self::FORCE_END)
  }

  #[inline]
  pub const fn has_allow_end(self) -> bool {
    self.contains(Self::ALLOW_END)
  }
}

impl Default for HangingPunctuation {
  fn default() -> Self {
    Self::NONE
  }
}

/// CSS Text 4 `text-spacing-trim` property.
///
/// Spec: <https://drafts.csswg.org/css-text-4/#text-spacing-trim-property>
///
/// Grammar: `<<spacing-trim>> | auto`, where `<<spacing-trim>> = space-all | normal | space-first
/// | trim-start | trim-both | trim-all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextSpacingTrim {
  /// UA-defined behavior.
  Auto,
  /// Default trimming behavior (no special hanging in FastRender's current implementation).
  Normal,
  SpaceAll,
  SpaceFirst,
  TrimStart,
  TrimBoth,
  TrimAll,
}

impl Default for TextSpacingTrim {
  fn default() -> Self {
    Self::Normal
  }
}

impl TextSpacingTrim {
  pub fn parse(keyword: &str) -> Option<Self> {
    match keyword.to_ascii_lowercase().as_str() {
      "auto" => Some(Self::Auto),
      "normal" => Some(Self::Normal),
      "space-all" => Some(Self::SpaceAll),
      "space-first" => Some(Self::SpaceFirst),
      "trim-start" => Some(Self::TrimStart),
      "trim-both" => Some(Self::TrimBoth),
      "trim-all" => Some(Self::TrimAll),
      _ => None,
    }
  }
}

/// CSS Text 4 `text-autospace` property.
///
/// This is primarily used by the `text-spacing` shorthand, which combines `text-spacing-trim` and
/// `text-autospace`.
///
/// Spec: <https://drafts.csswg.org/css-text-4/#text-autospace-property>
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextAutospace {
  /// UA-defined behavior.
  Auto,
  /// Default autospace behavior.
  Normal,
  /// Disables automatic spacing between character classes.
  NoAutospace,
  IdeographAlpha,
  IdeographNumeric,
  Punctuation,
  IdeographAlphaNumeric,
  IdeographAlphaPunctuation,
  IdeographNumericPunctuation,
  IdeographAlphaNumericPunctuation,
}

impl Default for TextAutospace {
  fn default() -> Self {
    Self::Normal
  }
}

impl TextAutospace {
  pub fn parse(raw: &str) -> Option<Self> {
    let tokens: Vec<&str> = raw
      .split(|ch: char| matches!(ch, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
      .filter(|t| !t.is_empty())
      .collect();
    if tokens.is_empty() {
      return None;
    }

    // Single-keyword forms.
    if tokens.len() == 1 {
      return match tokens[0].to_ascii_lowercase().as_str() {
        "auto" => Some(Self::Auto),
        "normal" => Some(Self::Normal),
        "no-autospace" => Some(Self::NoAutospace),
        "ideograph-alpha" => Some(Self::IdeographAlpha),
        "ideograph-numeric" => Some(Self::IdeographNumeric),
        "punctuation" => Some(Self::Punctuation),
        _ => None,
      };
    }

    // Combined keyword forms: `ideograph-alpha || ideograph-numeric || punctuation`.
    let mut ideograph_alpha = false;
    let mut ideograph_numeric = false;
    let mut punctuation = false;

    for token in tokens {
      match token.to_ascii_lowercase().as_str() {
        // `auto`/`normal`/`no-autospace` are not combinable.
        "auto" | "normal" | "no-autospace" => return None,
        "ideograph-alpha" => {
          if ideograph_alpha {
            return None;
          }
          ideograph_alpha = true;
        }
        "ideograph-numeric" => {
          if ideograph_numeric {
            return None;
          }
          ideograph_numeric = true;
        }
        "punctuation" => {
          if punctuation {
            return None;
          }
          punctuation = true;
        }
        _ => return None,
      }
    }

    if !(ideograph_alpha || ideograph_numeric || punctuation) {
      return None;
    }

    Some(match (ideograph_alpha, ideograph_numeric, punctuation) {
      (true, false, false) => Self::IdeographAlpha,
      (false, true, false) => Self::IdeographNumeric,
      (false, false, true) => Self::Punctuation,
      (true, true, false) => Self::IdeographAlphaNumeric,
      (true, false, true) => Self::IdeographAlphaPunctuation,
      (false, true, true) => Self::IdeographNumericPunctuation,
      (true, true, true) => Self::IdeographAlphaNumericPunctuation,
      _ => return None,
    })
  }
}

/// CSS `text-rendering`
///
/// Reference: SVG/CSS (non-standard, inherited)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRendering {
  Auto,
  OptimizeSpeed,
  OptimizeLegibility,
  GeometricPrecision,
}

impl Default for TextRendering {
  fn default() -> Self {
    TextRendering::Auto
  }
}

/// CSS `-webkit-font-smoothing` / `-moz-osx-font-smoothing` / `font-smooth`.
///
/// These properties are non-standard, but appear frequently in real-world stylesheets (resets and
/// icon fonts) to control font anti-aliasing modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontSmoothing {
  /// Use the renderer default.
  Auto,
  /// Prefer grayscale anti-aliasing (disable LCD/subpixel AA).
  Grayscale,
  /// Prefer LCD/subpixel anti-aliasing when available.
  Subpixel,
  /// Disable anti-aliasing.
  None,
}

impl Default for FontSmoothing {
  fn default() -> Self {
    Self::Auto
  }
}

impl FontSmoothing {
  pub fn parse_webkit(raw: &str) -> Option<Self> {
    match raw.to_ascii_lowercase().as_str() {
      "auto" => Some(Self::Auto),
      "none" => Some(Self::None),
      "antialiased" => Some(Self::Grayscale),
      "subpixel-antialiased" => Some(Self::Subpixel),
      _ => None,
    }
  }

  pub fn parse_moz_osx(raw: &str) -> Option<Self> {
    match raw.to_ascii_lowercase().as_str() {
      "auto" => Some(Self::Auto),
      "grayscale" => Some(Self::Grayscale),
      _ => None,
    }
  }

  pub fn parse_font_smooth(raw: &str) -> Option<Self> {
    match raw.to_ascii_lowercase().as_str() {
      "auto" => Some(Self::Auto),
      "never" => Some(Self::None),
      "always" => Some(Self::Grayscale),
      // Seen in some real-world stylesheets (treat as aliases).
      "grayscale" | "antialiased" => Some(Self::Grayscale),
      "subpixel-antialiased" => Some(Self::Subpixel),
      _ => None,
    }
  }
}

/// CSS `text-indent`
///
/// Reference: CSS Text Module Level 3
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextIndent {
  pub length: Length,
  pub hanging: bool,
  pub each_line: bool,
}

impl Default for TextIndent {
  fn default() -> Self {
    Self {
      length: Length::px(0.0),
      hanging: false,
      each_line: false,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineClampSource {
  Standard,
  Webkit,
}

impl Default for LineClampSource {
  fn default() -> Self {
    Self::Standard
  }
}

/// CSS `text-overflow`
///
/// Reference: CSS Overflow Module Level 3 / Text Overflow
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextOverflow {
  pub inline_start: TextOverflowSide,
  pub inline_end: TextOverflowSide,
}

impl TextOverflow {
  pub fn clip() -> Self {
    Self {
      inline_start: TextOverflowSide::Clip,
      inline_end: TextOverflowSide::Clip,
    }
  }

  pub fn start_for_direction(&self, direction: Direction) -> &TextOverflowSide {
    match direction {
      Direction::Ltr => &self.inline_start,
      Direction::Rtl => &self.inline_end,
    }
  }

  pub fn end_for_direction(&self, direction: Direction) -> &TextOverflowSide {
    match direction {
      Direction::Ltr => &self.inline_end,
      Direction::Rtl => &self.inline_start,
    }
  }

  pub fn is_clip_only(&self) -> bool {
    matches!(self.inline_start, TextOverflowSide::Clip)
      && matches!(self.inline_end, TextOverflowSide::Clip)
  }
}

/// Per-side text overflow behavior
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextOverflowSide {
  Clip,
  Ellipsis,
  String(String),
}

/// Text decoration lines
///
/// CSS: `text-decoration`
/// Reference: CSS Text Decoration Module Level 3
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextDecoration {
  pub lines: TextDecorationLine,
  pub style: TextDecorationStyle,
  /// None means currentColor
  pub color: Option<Rgba>,
  pub thickness: TextDecorationThickness,
}

impl Default for TextDecoration {
  fn default() -> Self {
    Self {
      lines: TextDecorationLine::NONE,
      style: TextDecorationStyle::Solid,
      color: None,
      thickness: TextDecorationThickness::Auto,
    }
  }
}

/// Individual text-decoration-line flags
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextDecorationLine(pub u8);

impl TextDecorationLine {
  pub const ALL: Self = Self(0b1_1111);
  pub const LINE_THROUGH: Self = Self(1 << 2);
  pub const GRAMMAR_ERROR: Self = Self(1 << 4);
  pub const NONE: Self = Self(0);
  pub const OVERLINE: Self = Self(1 << 1);
  pub const SPELLING_ERROR: Self = Self(1 << 3);
  pub const UNDERLINE: Self = Self(1 << 0);

  pub const fn contains(self, other: Self) -> bool {
    self.0 & other.0 == other.0
  }

  pub fn insert(&mut self, other: Self) {
    self.0 |= other.0;
  }

  pub fn remove(&mut self, other: Self) {
    self.0 &= !other.0;
  }

  pub const fn is_empty(self) -> bool {
    self.0 == 0
  }
}

/// Stroke style for text decorations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDecorationStyle {
  Solid,
  Double,
  Dotted,
  Dashed,
  Wavy,
}

/// Whether underlines skip glyph ink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDecorationSkipInk {
  Auto,
  None,
  All,
}

/// Controls whether ancestor text decorations should skip this element.
///
/// CSS: `text-decoration-skip-self`
/// Reference: CSS Text Decoration Module Level 4
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDecorationSkipSelf {
  Auto,
  NoSkip,
  Skip(TextDecorationLine),
}

impl Default for TextDecorationSkipSelf {
  fn default() -> Self {
    Self::Auto
  }
}

/// Controls whether ancestor text decorations should skip the element's box edges.
///
/// CSS: `text-decoration-skip-box`
/// Reference: CSS Text Decoration Module Level 4
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDecorationSkipBox {
  None,
  All,
}

impl Default for TextDecorationSkipBox {
  fn default() -> Self {
    Self::None
  }
}

/// Controls whether text decorations should skip spaces at line edges.
///
/// CSS: `text-decoration-skip-spaces`
/// Reference: CSS Text Decoration Module Level 4
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDecorationSkipSpaces {
  None,
  All,
  Start,
  End,
  StartEnd,
}

impl Default for TextDecorationSkipSpaces {
  fn default() -> Self {
    // Initial value per spec.
    Self::StartEnd
  }
}

impl TextDecorationSkipSpaces {
  pub fn skips_start(self) -> bool {
    matches!(self, Self::All | Self::Start | Self::StartEnd)
  }

  pub fn skips_end(self) -> bool {
    matches!(self, Self::All | Self::End | Self::StartEnd)
  }
}

/// Adjusts the start/end endpoints of line text decorations drawn by a decorating box.
///
/// CSS: `text-decoration-inset` (CSS Text Decoration Module Level 4)
///
/// The computed value is `auto` or an absolute length (font/viewport-relative units are resolved
/// during cascade in `style::cascade::resolve_absolute_lengths`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextDecorationInset {
  /// UA-defined inset that should introduce a small visible separation between adjacent identical
  /// underlined elements.
  Auto,
  /// Explicit start/end inset lengths (positive trims inward, negative extends outward).
  Lengths { start: Length, end: Length },
}

impl Default for TextDecorationInset {
  fn default() -> Self {
    // Initial value per spec.
    Self::Lengths {
      start: Length::px(0.0),
      end: Length::px(0.0),
    }
  }
}

impl TextDecorationInset {
  pub fn is_zero(self) -> bool {
    match self {
      Self::Auto => false,
      Self::Lengths { start, end } => start.to_px() == 0.0 && end.to_px() == 0.0,
    }
  }
}

/// Resolved text-decoration to apply after propagation.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTextDecoration {
  /// Stable identity of the decorating box that introduced this decoration.
  ///
  /// This is used at paint time to distinguish adjacent decorations that are otherwise identical.
  pub origin_id: usize,
  pub decoration: TextDecoration,
  pub skip_ink: TextDecorationSkipInk,
  pub underline_offset: TextUnderlineOffset,
  pub underline_position: TextUnderlinePosition,
  pub inset: TextDecorationInset,
}

/// Thickness of text decorations
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextDecorationThickness {
  Auto,
  FromFont,
  Length(Length),
}

/// Controls underline offset relative to the default position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextUnderlineOffset {
  Auto,
  Length(Length),
}

impl Default for TextUnderlineOffset {
  fn default() -> Self {
    TextUnderlineOffset::Auto
  }
}

/// Placement of underlines relative to the text and inline axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextUnderlinePosition {
  Auto,
  FromFont,
  Under,
  Left,
  Right,
  UnderLeft,
  UnderRight,
}

impl Default for TextUnderlinePosition {
  fn default() -> Self {
    TextUnderlinePosition::Auto
  }
}

/// Fill mode for emphasis marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEmphasisFill {
  Filled,
  Open,
}

/// Shape of emphasis marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEmphasisShape {
  Dot,
  Circle,
  DoubleCircle,
  Triangle,
  Sesame,
}

/// Emphasis style (mark or custom string).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextEmphasisStyle {
  None,
  Mark {
    fill: TextEmphasisFill,
    shape: Option<TextEmphasisShape>,
  },
  String(String),
}

impl Default for TextEmphasisStyle {
  fn default() -> Self {
    TextEmphasisStyle::None
  }
}

impl TextEmphasisStyle {
  pub fn is_none(&self) -> bool {
    matches!(self, TextEmphasisStyle::None)
  }
}

/// Placement of emphasis marks relative to text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEmphasisPosition {
  Auto,
  Over,
  Under,
  OverLeft,
  OverRight,
  UnderLeft,
  UnderRight,
}

impl Default for TextEmphasisPosition {
  fn default() -> Self {
    TextEmphasisPosition::Auto
  }
}

/// Controls which characters receive emphasis marks.
///
/// CSS: `text-emphasis-skip` (CSS Text Decoration Level 4)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextEmphasisSkip(pub u8);

impl TextEmphasisSkip {
  pub const NONE: Self = Self(0);
  pub const SPACES: Self = Self(1 << 0);
  pub const PUNCTUATION: Self = Self(1 << 1);
  pub const SYMBOLS: Self = Self(1 << 2);
  pub const NARROW: Self = Self(1 << 3);

  pub const fn contains(self, other: Self) -> bool {
    self.0 & other.0 != 0
  }

  pub fn insert(&mut self, other: Self) {
    self.0 |= other.0;
  }

  pub const fn is_empty(self) -> bool {
    self.0 == 0
  }
}

impl Default for TextEmphasisSkip {
  fn default() -> Self {
    // CSS Text Decoration 4: initial value is `spaces punctuation`.
    TextEmphasisSkip(TextEmphasisSkip::SPACES.0 | TextEmphasisSkip::PUNCTUATION.0)
  }
}

/// ruby-position values (CSS Ruby Layout Level 1)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RubyPosition {
  Over,
  Under,
  InterCharacter,
  Alternate,
}

impl Default for RubyPosition {
  fn default() -> Self {
    RubyPosition::Over
  }
}

/// ruby-align values
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RubyAlign {
  Auto,
  Start,
  Center,
  SpaceBetween,
  SpaceAround,
}

impl Default for RubyAlign {
  fn default() -> Self {
    RubyAlign::Auto
  }
}

/// ruby-merge values
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RubyMerge {
  Separate,
  Collapse,
  Auto,
}

impl Default for RubyMerge {
  fn default() -> Self {
    RubyMerge::Separate
  }
}

/// list-style-type values
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListStyleType {
  Disc,
  Circle,
  Square,
  Decimal,
  DecimalLeadingZero,
  LowerRoman,
  UpperRoman,
  LowerAlpha,
  UpperAlpha,
  Armenian,
  LowerArmenian,
  Georgian,
  LowerGreek,
  DisclosureOpen,
  DisclosureClosed,
  /// Anonymous counter style defined by the CSS Counter Styles `symbols()` function.
  ///
  /// Reference: <https://www.w3.org/TR/css-counter-styles-3/#symbols-function>
  Symbols(SymbolsCounterStyle),
  /// Custom counter style name (via `@counter-style`).
  Custom(String),
  /// Custom marker string value from list-style-type: `"<string>"`
  String(String),
  None,
}

/// Symbol algorithm type accepted by the CSS Counter Styles `symbols()` function.
///
/// Reference: <https://www.w3.org/TR/css-counter-styles-3/#symbols-function>
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolsType {
  Cyclic,
  Numeric,
  Alphabetic,
  Symbolic,
  Fixed,
}

/// Anonymous counter style constructed from the CSS Counter Styles `symbols()` function.
///
/// This is used by `list-style-type: symbols(...)`.
///
/// Reference: <https://www.w3.org/TR/css-counter-styles-3/#symbols-function>
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolsCounterStyle {
  pub system: SymbolsType,
  pub symbols: Vec<String>,
}

/// list-style-position values
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListStylePosition {
  Outside,
  Inside,
}

/// list-style-image values
#[derive(Debug, Clone, PartialEq)]
pub enum ListStyleImage {
  None,
  Url(BackgroundImageUrl),
}

/// Text case transformation
///
/// CSS: `text-transform`
/// Reference: CSS Text Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseTransform {
  None,
  Uppercase,
  Lowercase,
  Capitalize,
}

/// Combination of text transformation effects
///
/// The grammar allows one case transform and optional width/kana transforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextTransform {
  pub case: CaseTransform,
  pub full_width: bool,
  pub full_size_kana: bool,
}

impl Default for TextTransform {
  fn default() -> Self {
    Self {
      case: CaseTransform::None,
      full_width: false,
      full_size_kana: false,
    }
  }
}

impl TextTransform {
  pub const fn none() -> Self {
    Self {
      case: CaseTransform::None,
      full_width: false,
      full_size_kana: false,
    }
  }

  pub const fn with_case(case: CaseTransform) -> Self {
    Self {
      case,
      full_width: false,
      full_size_kana: false,
    }
  }

  pub const fn full_width() -> Self {
    Self {
      case: CaseTransform::None,
      full_width: true,
      full_size_kana: false,
    }
  }
}

/// White space handling mode
///
/// CSS: `white-space`
/// Reference: CSS Text Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhiteSpace {
  Normal,
  Nowrap,
  Pre,
  PreWrap,
  PreLine,
  BreakSpaces,
}

/// Text wrap mode
///
/// CSS: `text-wrap`
/// Reference: CSS Text Module Level 4
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextWrap {
  Auto,
  NoWrap,
  Balance,
  Pretty,
  Stable,
  AvoidOrphans,
}

impl Default for TextWrap {
  fn default() -> Self {
    TextWrap::Auto
  }
}

impl TextWrap {
  /// Parse a `text-wrap` value.
  ///
  /// Spec: CSS Text Module Level 4
  /// Grammar (shorthand): `<<text-wrap-mode>> || <<text-wrap-style>>`
  /// - `text-wrap-mode`: `wrap | nowrap` (initial: wrap)
  /// - `text-wrap-style`: `auto | balance | stable | pretty | avoid-orphans` (initial: auto)
  ///
  /// FastRender stores `text-wrap` as a single enum rather than exposing the `text-wrap-mode` /
  /// `text-wrap-style` longhands. When `nowrap` is specified we preserve only the wrapping
  /// disabling behavior (style keywords become irrelevant).
  pub fn parse(raw: &str) -> Option<Self> {
    let mut input = ParserInput::new(raw);
    let mut parser = Parser::new(&mut input);
    Self::parse_from_parser(&mut parser)
  }

  pub(crate) fn parse_from_parser<'i, 't>(parser: &mut Parser<'i, 't>) -> Option<Self> {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum WrapMode {
      Wrap,
      NoWrap,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum WrapStyle {
      Auto,
      Balance,
      Pretty,
      Stable,
      AvoidOrphans,
    }

    let mut mode: Option<WrapMode> = None;
    let mut style: Option<WrapStyle> = None;
    let mut saw_any = false;

    while let Ok(token) = parser.next_including_whitespace_and_comments() {
      match token {
        Token::WhiteSpace(_) | Token::Comment(_) => continue,
        Token::Ident(ident) => {
          saw_any = true;
          let ident = ident.as_ref();

          if ident.eq_ignore_ascii_case("wrap") {
            if mode.replace(WrapMode::Wrap).is_some() {
              return None;
            }
            continue;
          }

          if ident.eq_ignore_ascii_case("nowrap") {
            if mode.replace(WrapMode::NoWrap).is_some() {
              return None;
            }
            continue;
          }

          let parsed_style =
            if ident.eq_ignore_ascii_case("auto") || ident.eq_ignore_ascii_case("normal") {
              Some(WrapStyle::Auto)
            } else if ident.eq_ignore_ascii_case("balance") {
              Some(WrapStyle::Balance)
            } else if ident.eq_ignore_ascii_case("pretty") {
              Some(WrapStyle::Pretty)
            } else if ident.eq_ignore_ascii_case("stable") {
              Some(WrapStyle::Stable)
            } else if ident.eq_ignore_ascii_case("avoid-orphans") {
              Some(WrapStyle::AvoidOrphans)
            } else {
              None
            };

          if let Some(parsed_style) = parsed_style {
            if style.replace(parsed_style).is_some() {
              return None;
            }
            continue;
          }

          return None;
        }
        _ => return None,
      }
    }

    if !saw_any {
      return None;
    }

    let mode = mode.unwrap_or(WrapMode::Wrap);
    let style = style.unwrap_or(WrapStyle::Auto);

    if matches!(mode, WrapMode::NoWrap) {
      return Some(Self::NoWrap);
    }

    Some(match style {
      WrapStyle::Auto => Self::Auto,
      WrapStyle::Balance => Self::Balance,
      WrapStyle::Pretty => Self::Pretty,
      WrapStyle::Stable => Self::Stable,
      WrapStyle::AvoidOrphans => Self::AvoidOrphans,
    })
  }
}

/// Trimming behavior for the `text-box-trim` / `text-box` properties (CSS Inline Layout Level 3).
///
/// This is stored on [`crate::style::ComputedStyle`] as `text_box_trim`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextBoxTrim {
  None,
  TrimStart,
  TrimEnd,
  TrimBoth,
}

impl Default for TextBoxTrim {
  fn default() -> Self {
    Self::None
  }
}

/// Keywords used by the `text-edge` / `text-box-edge` properties.
///
/// Spec: <https://www.w3.org/TR/css-inline-3/#text-edge>
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextEdgeKeyword {
  Text,
  Cap,
  Ex,
  Alphabetic,
  Ideographic,
  IdeographicInk,
}

impl TextEdgeKeyword {
  pub fn parse(keyword: &str) -> Option<Self> {
    if keyword.eq_ignore_ascii_case("text") {
      Some(Self::Text)
    } else if keyword.eq_ignore_ascii_case("cap") {
      Some(Self::Cap)
    } else if keyword.eq_ignore_ascii_case("ex") {
      Some(Self::Ex)
    } else if keyword.eq_ignore_ascii_case("alphabetic") {
      Some(Self::Alphabetic)
    } else if keyword.eq_ignore_ascii_case("ideographic") {
      Some(Self::Ideographic)
    } else if keyword.eq_ignore_ascii_case("ideographic-ink") {
      Some(Self::IdeographicInk)
    } else {
      None
    }
  }
}

/// Computed value for `text-box-edge`.
///
/// `auto` defers to the UA's inline layout metrics (ultimately derived from `line-fit-edge` in the
/// spec). When explicit, the value stores separate over/under edge keywords.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextBoxEdge {
  Auto,
  Explicit {
    over: TextEdgeKeyword,
    under: TextEdgeKeyword,
  },
}

impl Default for TextBoxEdge {
  fn default() -> Self {
    Self::Auto
  }
}

/// Line break strictness
///
/// CSS: `line-break`
/// Reference: CSS Text Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineBreak {
  Auto,
  Loose,
  Normal,
  Strict,
  Anywhere,
}

/// Tab stop sizing
///
/// CSS: `tab-size`
/// Reference: CSS Text Module Level 3
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TabSize {
  /// Width expressed as a number of space advances
  Number(f32),
  /// Explicit length for each tab stop interval
  Length(Length),
}

impl Default for TabSize {
  fn default() -> Self {
    TabSize::Number(8.0)
  }
}

/// CSS `word-break`
///
/// Reference: CSS Text Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordBreak {
  Normal,
  BreakAll,
  KeepAll,
  AutoPhrase,
  BreakWord,
  Anywhere,
}

/// CSS `overflow-anchor`
///
/// Reference: CSS Scroll Anchoring Module Level 1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowAnchor {
  Auto,
  None,
}

impl Default for OverflowAnchor {
  fn default() -> Self {
    OverflowAnchor::Auto
  }
}

// === CSS Anchor Positioning (css-anchor-position-1) ===

/// Anchor side keywords supported by the `anchor()` inset function.
///
/// Baseline support: only the physical sides used by common tooltip/popover patterns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnchorSide {
  /// `inside` resolves to the same side as the inset property the function appears in.
  Inside,
  /// `outside` resolves to the opposite side from the inset property the function appears in.
  Outside,
  Top,
  Right,
  Bottom,
  Left,
  /// `start` resolves to the start side of the relevant axis using the containing block writing-mode.
  Start,
  /// `end` resolves to the end side of the relevant axis using the containing block writing-mode.
  End,
  /// `self-start` resolves to the start side of the relevant axis using the positioned element writing-mode.
  SelfStart,
  /// `self-end` resolves to the end side of the relevant axis using the positioned element writing-mode.
  SelfEnd,
  /// Logical inline-start side of the anchor element.
  InlineStart,
  /// Logical inline-end side of the anchor element.
  InlineEnd,
  /// Logical block-start side of the anchor element.
  BlockStart,
  /// Logical block-end side of the anchor element.
  BlockEnd,
  /// `center` is equivalent to `50%`.
  Center,
  /// Percentage position between the two sides of the relevant axis (e.g. 0% = left/top).
  ///
  /// Stored as the numeric percentage value (e.g. `50.0` for `50%`).
  Percent(f32),
}

/// Parsed `anchor()` function as used in inset properties (top/right/bottom/left, inset-*).
///
/// Note: In the spec, `anchor()` resolves at computed value time using style/layout interleaving.
/// FastRender resolves it during positioned layout from the already-laid-out fragment tree.
#[derive(Debug, Clone, PartialEq)]
pub struct AnchorFunction {
  /// Optional explicit anchor name (`anchor(--foo top)`); when absent, use `position-anchor`.
  pub name: Option<String>,
  pub side: AnchorSide,
  /// Optional fallback value (`anchor(top, 12px)`).
  pub fallback: Option<Length>,
}

/// Axis keywords supported by the `anchor-size()` function.
///
/// Baseline support: only the size axes needed to implement tooltip/popover sizing against an
/// anchor element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorSizeAxis {
  /// Axis keyword omitted (`anchor-size()` / `anchor-size(--foo)`); resolves to the axis of the
  /// property the function is used in (e.g. `width` -> `width`, `height` -> `height`).
  Omitted,
  Width,
  Height,
  /// Logical inline axis of the *containing block*.
  Inline,
  /// Logical block axis of the *containing block*.
  Block,
  /// Logical inline axis of the positioned element ("self").
  SelfInline,
  /// Logical block axis of the positioned element ("self").
  SelfBlock,
}

/// Parsed `anchor-size()` function as used in sizing properties (width/height/min/max).
///
/// Like `anchor()`, FastRender resolves `anchor-size()` during positioned layout from the
/// already-laid-out fragment tree.
#[derive(Debug, Clone, PartialEq)]
pub struct AnchorSizeFunction {
  /// Optional explicit anchor name (`anchor-size(--foo width)`); when absent, use `position-anchor`.
  pub name: Option<String>,
  pub axis: AnchorSizeAxis,
  /// Optional fallback value (`anchor-size(width, 12px)`).
  pub fallback: Option<Length>,
}

/// Computed inset value (`top/right/bottom/left`) supporting `anchor()`.
#[derive(Debug, Clone, PartialEq)]
pub enum InsetValue {
  Auto,
  Length(Length),
  Anchor(AnchorFunction),
}

impl Default for InsetValue {
  fn default() -> Self {
    Self::Auto
  }
}

impl InsetValue {
  pub fn is_auto(&self) -> bool {
    matches!(self, Self::Auto)
  }
}

/// Computed value for the `position-anchor` property.
#[derive(Debug, Clone, PartialEq)]
pub enum PositionAnchor {
  None,
  Auto,
  Name(String),
}

impl Default for PositionAnchor {
  fn default() -> Self {
    Self::None
  }
}

/// A track selection within a single axis of the position-area grid.
///
/// `position-area` conceptually selects rows/columns in a 3×3 grid defined by the pre-modification
/// containing block edges and the default anchor box edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionAreaTrack {
  Start,
  Center,
  End,
  SpanStart,
  SpanEnd,
  SpanAll,
}

impl PositionAreaTrack {
  pub fn flip(self) -> Self {
    match self {
      PositionAreaTrack::Start => PositionAreaTrack::End,
      PositionAreaTrack::End => PositionAreaTrack::Start,
      PositionAreaTrack::SpanStart => PositionAreaTrack::SpanEnd,
      PositionAreaTrack::SpanEnd => PositionAreaTrack::SpanStart,
      PositionAreaTrack::Center | PositionAreaTrack::SpanAll => self,
    }
  }
}

/// Resolved `position-area` tracks for the block and inline axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionAreaTracks {
  pub block: PositionAreaTrack,
  pub inline: PositionAreaTrack,
}

/// Keywords accepted by the `position-area` property.
///
/// This enum stores the computed-value-time keyword pair (per the spec) rather than resolving to
/// physical sides immediately. Resolution to block/inline tracks happens at layout time using the
/// element's writing mode and direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionAreaKeyword {
  // Common ambiguous keywords (allowed in all syntaxes).
  Center,
  SpanAll,

  // Ambiguous logical keywords (block/inline axis determined by position in the pair).
  Start,
  End,
  SpanStart,
  SpanEnd,

  // Self-relative ambiguous keywords.
  SelfStart,
  SelfEnd,
  SpanSelfStart,
  SpanSelfEnd,

  // Flow-relative block/inline keywords.
  BlockStart,
  BlockEnd,
  SpanBlockStart,
  SpanBlockEnd,
  InlineStart,
  InlineEnd,
  SpanInlineStart,
  SpanInlineEnd,

  // Self-relative block/inline keywords.
  SelfBlockStart,
  SelfBlockEnd,
  SpanSelfBlockStart,
  SpanSelfBlockEnd,
  SelfInlineStart,
  SelfInlineEnd,
  SpanSelfInlineStart,
  SpanSelfInlineEnd,

  // Physical X/Y keywords.
  Left,
  Right,
  SpanLeft,
  SpanRight,
  Top,
  Bottom,
  SpanTop,
  SpanBottom,
  XStart,
  XEnd,
  SpanXStart,
  SpanXEnd,
  YStart,
  YEnd,
  SpanYStart,
  SpanYEnd,
  SelfXStart,
  SelfXEnd,
  SpanSelfXStart,
  SpanSelfXEnd,
  SelfYStart,
  SelfYEnd,
  SpanSelfYStart,
  SpanSelfYEnd,
}

impl PositionAreaKeyword {
  fn parse(raw: &str) -> Option<Self> {
    let kw = raw.to_ascii_lowercase();
    Some(match kw.as_str() {
      "center" => PositionAreaKeyword::Center,
      "span-all" => PositionAreaKeyword::SpanAll,
      "start" => PositionAreaKeyword::Start,
      "end" => PositionAreaKeyword::End,
      "span-start" => PositionAreaKeyword::SpanStart,
      "span-end" => PositionAreaKeyword::SpanEnd,
      "self-start" => PositionAreaKeyword::SelfStart,
      "self-end" => PositionAreaKeyword::SelfEnd,
      "span-self-start" => PositionAreaKeyword::SpanSelfStart,
      "span-self-end" => PositionAreaKeyword::SpanSelfEnd,
      "block-start" => PositionAreaKeyword::BlockStart,
      "block-end" => PositionAreaKeyword::BlockEnd,
      "span-block-start" => PositionAreaKeyword::SpanBlockStart,
      "span-block-end" => PositionAreaKeyword::SpanBlockEnd,
      "inline-start" => PositionAreaKeyword::InlineStart,
      "inline-end" => PositionAreaKeyword::InlineEnd,
      "span-inline-start" => PositionAreaKeyword::SpanInlineStart,
      "span-inline-end" => PositionAreaKeyword::SpanInlineEnd,
      "self-block-start" => PositionAreaKeyword::SelfBlockStart,
      "self-block-end" => PositionAreaKeyword::SelfBlockEnd,
      "span-self-block-start" => PositionAreaKeyword::SpanSelfBlockStart,
      "span-self-block-end" => PositionAreaKeyword::SpanSelfBlockEnd,
      "self-inline-start" => PositionAreaKeyword::SelfInlineStart,
      "self-inline-end" => PositionAreaKeyword::SelfInlineEnd,
      "span-self-inline-start" => PositionAreaKeyword::SpanSelfInlineStart,
      "span-self-inline-end" => PositionAreaKeyword::SpanSelfInlineEnd,
      "left" => PositionAreaKeyword::Left,
      "right" => PositionAreaKeyword::Right,
      "span-left" => PositionAreaKeyword::SpanLeft,
      "span-right" => PositionAreaKeyword::SpanRight,
      "top" => PositionAreaKeyword::Top,
      "bottom" => PositionAreaKeyword::Bottom,
      "span-top" => PositionAreaKeyword::SpanTop,
      "span-bottom" => PositionAreaKeyword::SpanBottom,
      "x-start" => PositionAreaKeyword::XStart,
      "x-end" => PositionAreaKeyword::XEnd,
      "span-x-start" => PositionAreaKeyword::SpanXStart,
      "span-x-end" => PositionAreaKeyword::SpanXEnd,
      "y-start" => PositionAreaKeyword::YStart,
      "y-end" => PositionAreaKeyword::YEnd,
      "span-y-start" => PositionAreaKeyword::SpanYStart,
      "span-y-end" => PositionAreaKeyword::SpanYEnd,
      "self-x-start" => PositionAreaKeyword::SelfXStart,
      "self-x-end" => PositionAreaKeyword::SelfXEnd,
      "span-self-x-start" => PositionAreaKeyword::SpanSelfXStart,
      "span-self-x-end" => PositionAreaKeyword::SpanSelfXEnd,
      "self-y-start" => PositionAreaKeyword::SelfYStart,
      "self-y-end" => PositionAreaKeyword::SelfYEnd,
      "span-self-y-start" => PositionAreaKeyword::SpanSelfYStart,
      "span-self-y-end" => PositionAreaKeyword::SpanSelfYEnd,
      _ => return None,
    })
  }

  fn is_axis_unambiguous(self) -> bool {
    !matches!(
      self,
      PositionAreaKeyword::Center
        | PositionAreaKeyword::SpanAll
        | PositionAreaKeyword::Start
        | PositionAreaKeyword::End
        | PositionAreaKeyword::SpanStart
        | PositionAreaKeyword::SpanEnd
        | PositionAreaKeyword::SelfStart
        | PositionAreaKeyword::SelfEnd
        | PositionAreaKeyword::SpanSelfStart
        | PositionAreaKeyword::SpanSelfEnd
    )
  }

  fn is_plain_ambiguous(self) -> bool {
    matches!(
      self,
      PositionAreaKeyword::Start
        | PositionAreaKeyword::End
        | PositionAreaKeyword::SpanStart
        | PositionAreaKeyword::SpanEnd
        | PositionAreaKeyword::SelfStart
        | PositionAreaKeyword::SelfEnd
        | PositionAreaKeyword::SpanSelfStart
        | PositionAreaKeyword::SpanSelfEnd
    )
  }
}

/// Computed value for the CSS `position-area` property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PositionArea {
  None,
  Keywords(PositionAreaKeyword, PositionAreaKeyword),
}

impl Default for PositionArea {
  fn default() -> Self {
    Self::None
  }
}

impl PositionArea {
  pub fn parse(raw: &str) -> Option<Self> {
    let raw = raw.trim();
    if raw.is_empty() {
      return None;
    }
    if raw.eq_ignore_ascii_case("none") {
      return Some(Self::None);
    }

    let mut input = ParserInput::new(raw);
    let mut parser = Parser::new(&mut input);
    parser.skip_whitespace();
    if parser.is_exhausted() {
      return None;
    }

    let mut parts: Vec<PositionAreaKeyword> = Vec::new();
    while !parser.is_exhausted() {
      let ident = parser.expect_ident().ok()?;
      let kw = PositionAreaKeyword::parse(ident.as_ref())?;
      parts.push(kw);
      parser.skip_whitespace();
    }

    match parts.as_slice() {
      [] => None,
      [single] => {
        let first = *single;
        let second = if first.is_axis_unambiguous() {
          PositionAreaKeyword::SpanAll
        } else {
          first
        };
        Some(Self::Keywords(first, second))
      }
      [a, b] => Some(Self::Keywords(*a, *b)),
      _ => None,
    }
  }

  pub fn from_tracks(tracks: PositionAreaTracks) -> Self {
    fn block_kw(track: PositionAreaTrack) -> PositionAreaKeyword {
      match track {
        PositionAreaTrack::Start => PositionAreaKeyword::BlockStart,
        PositionAreaTrack::End => PositionAreaKeyword::BlockEnd,
        PositionAreaTrack::SpanStart => PositionAreaKeyword::SpanBlockStart,
        PositionAreaTrack::SpanEnd => PositionAreaKeyword::SpanBlockEnd,
        PositionAreaTrack::Center => PositionAreaKeyword::Center,
        PositionAreaTrack::SpanAll => PositionAreaKeyword::SpanAll,
      }
    }

    fn inline_kw(track: PositionAreaTrack) -> PositionAreaKeyword {
      match track {
        PositionAreaTrack::Start => PositionAreaKeyword::InlineStart,
        PositionAreaTrack::End => PositionAreaKeyword::InlineEnd,
        PositionAreaTrack::SpanStart => PositionAreaKeyword::SpanInlineStart,
        PositionAreaTrack::SpanEnd => PositionAreaKeyword::SpanInlineEnd,
        PositionAreaTrack::Center => PositionAreaKeyword::Center,
        PositionAreaTrack::SpanAll => PositionAreaKeyword::SpanAll,
      }
    }

    Self::Keywords(block_kw(tracks.block), inline_kw(tracks.inline))
  }

  pub fn resolve_tracks(
    &self,
    writing_mode: WritingMode,
    direction: Direction,
  ) -> Option<PositionAreaTracks> {
    let (a, b) = match self {
      PositionArea::None => return None,
      PositionArea::Keywords(a, b) => (*a, *b),
    };

    #[derive(Clone, Copy)]
    enum AxisValue {
      Track(PositionAreaTrack),
      PhysicalStart,
      PhysicalEnd,
      SpanPhysicalStart,
      SpanPhysicalEnd,
    }

    #[derive(Clone, Copy)]
    enum TokenKind {
      Block(PositionAreaTrack),
      Inline(PositionAreaTrack),
      X(AxisValue),
      Y(AxisValue),
      Ambiguous(PositionAreaTrack),
    }

    fn token_from_keyword(kw: PositionAreaKeyword) -> (TokenKind, bool) {
      let plain_ambiguous = kw.is_plain_ambiguous();
      let kind = match kw {
        PositionAreaKeyword::Center => TokenKind::Ambiguous(PositionAreaTrack::Center),
        PositionAreaKeyword::SpanAll => TokenKind::Ambiguous(PositionAreaTrack::SpanAll),
        PositionAreaKeyword::Start | PositionAreaKeyword::SelfStart => {
          TokenKind::Ambiguous(PositionAreaTrack::Start)
        }
        PositionAreaKeyword::End | PositionAreaKeyword::SelfEnd => {
          TokenKind::Ambiguous(PositionAreaTrack::End)
        }
        PositionAreaKeyword::SpanStart | PositionAreaKeyword::SpanSelfStart => {
          TokenKind::Ambiguous(PositionAreaTrack::SpanStart)
        }
        PositionAreaKeyword::SpanEnd | PositionAreaKeyword::SpanSelfEnd => {
          TokenKind::Ambiguous(PositionAreaTrack::SpanEnd)
        }

        PositionAreaKeyword::BlockStart | PositionAreaKeyword::SelfBlockStart => {
          TokenKind::Block(PositionAreaTrack::Start)
        }
        PositionAreaKeyword::BlockEnd | PositionAreaKeyword::SelfBlockEnd => {
          TokenKind::Block(PositionAreaTrack::End)
        }
        PositionAreaKeyword::SpanBlockStart | PositionAreaKeyword::SpanSelfBlockStart => {
          TokenKind::Block(PositionAreaTrack::SpanStart)
        }
        PositionAreaKeyword::SpanBlockEnd | PositionAreaKeyword::SpanSelfBlockEnd => {
          TokenKind::Block(PositionAreaTrack::SpanEnd)
        }

        PositionAreaKeyword::InlineStart | PositionAreaKeyword::SelfInlineStart => {
          TokenKind::Inline(PositionAreaTrack::Start)
        }
        PositionAreaKeyword::InlineEnd | PositionAreaKeyword::SelfInlineEnd => {
          TokenKind::Inline(PositionAreaTrack::End)
        }
        PositionAreaKeyword::SpanInlineStart | PositionAreaKeyword::SpanSelfInlineStart => {
          TokenKind::Inline(PositionAreaTrack::SpanStart)
        }
        PositionAreaKeyword::SpanInlineEnd | PositionAreaKeyword::SpanSelfInlineEnd => {
          TokenKind::Inline(PositionAreaTrack::SpanEnd)
        }

        PositionAreaKeyword::Left => TokenKind::X(AxisValue::PhysicalStart),
        PositionAreaKeyword::Right => TokenKind::X(AxisValue::PhysicalEnd),
        PositionAreaKeyword::SpanLeft => TokenKind::X(AxisValue::SpanPhysicalStart),
        PositionAreaKeyword::SpanRight => TokenKind::X(AxisValue::SpanPhysicalEnd),
        PositionAreaKeyword::XStart | PositionAreaKeyword::SelfXStart => {
          TokenKind::X(AxisValue::Track(PositionAreaTrack::Start))
        }
        PositionAreaKeyword::XEnd | PositionAreaKeyword::SelfXEnd => {
          TokenKind::X(AxisValue::Track(PositionAreaTrack::End))
        }
        PositionAreaKeyword::SpanXStart | PositionAreaKeyword::SpanSelfXStart => {
          TokenKind::X(AxisValue::Track(PositionAreaTrack::SpanStart))
        }
        PositionAreaKeyword::SpanXEnd | PositionAreaKeyword::SpanSelfXEnd => {
          TokenKind::X(AxisValue::Track(PositionAreaTrack::SpanEnd))
        }

        PositionAreaKeyword::Top => TokenKind::Y(AxisValue::PhysicalStart),
        PositionAreaKeyword::Bottom => TokenKind::Y(AxisValue::PhysicalEnd),
        PositionAreaKeyword::SpanTop => TokenKind::Y(AxisValue::SpanPhysicalStart),
        PositionAreaKeyword::SpanBottom => TokenKind::Y(AxisValue::SpanPhysicalEnd),
        PositionAreaKeyword::YStart | PositionAreaKeyword::SelfYStart => {
          TokenKind::Y(AxisValue::Track(PositionAreaTrack::Start))
        }
        PositionAreaKeyword::YEnd | PositionAreaKeyword::SelfYEnd => {
          TokenKind::Y(AxisValue::Track(PositionAreaTrack::End))
        }
        PositionAreaKeyword::SpanYStart | PositionAreaKeyword::SpanSelfYStart => {
          TokenKind::Y(AxisValue::Track(PositionAreaTrack::SpanStart))
        }
        PositionAreaKeyword::SpanYEnd | PositionAreaKeyword::SpanSelfYEnd => {
          TokenKind::Y(AxisValue::Track(PositionAreaTrack::SpanEnd))
        }
      };

      (kind, plain_ambiguous)
    }

    let (t1, plain1) = token_from_keyword(a);
    let (t2, plain2) = token_from_keyword(b);

    // If the author uses the axis-ambiguous `start`/`end` keywords, the value must come from the
    // `{1,2}` ambiguous syntax and cannot mix with the axis-specific keywords.
    if plain1 || plain2 {
      match (t1, t2) {
        (TokenKind::Ambiguous(track1), TokenKind::Ambiguous(track2)) => {
          return Some(PositionAreaTracks {
            block: track1,
            inline: track2,
          });
        }
        _ => return None,
      }
    }

    let has_logical = matches!(t1, TokenKind::Block(_) | TokenKind::Inline(_))
      || matches!(t2, TokenKind::Block(_) | TokenKind::Inline(_));
    let has_physical = matches!(t1, TokenKind::X(_) | TokenKind::Y(_))
      || matches!(t2, TokenKind::X(_) | TokenKind::Y(_));

    if has_logical && has_physical {
      return None;
    }

    // Axis-ambiguous-only values like `center center` default to block + inline.
    if !has_logical && !has_physical {
      let TokenKind::Ambiguous(track1) = t1 else {
        return None;
      };
      let TokenKind::Ambiguous(track2) = t2 else {
        return None;
      };
      return Some(PositionAreaTracks {
        block: track1,
        inline: track2,
      });
    }

    if has_logical {
      let mut block: Option<PositionAreaTrack> = None;
      let mut inline: Option<PositionAreaTrack> = None;
      let mut ambiguous: Option<PositionAreaTrack> = None;

      for token in [t1, t2] {
        match token {
          TokenKind::Block(track) => {
            if block.replace(track).is_some() {
              return None;
            }
          }
          TokenKind::Inline(track) => {
            if inline.replace(track).is_some() {
              return None;
            }
          }
          TokenKind::Ambiguous(track) => {
            if ambiguous.replace(track).is_some() {
              return None;
            }
          }
          _ => return None,
        }
      }

      match (block, inline, ambiguous) {
        (Some(block), Some(inline), None) => Some(PositionAreaTracks { block, inline }),
        (Some(block), None, Some(amb)) => Some(PositionAreaTracks { block, inline: amb }),
        (None, Some(inline), Some(amb)) => Some(PositionAreaTracks { block: amb, inline }),
        // Disallow `block-start block-end` etc (missing inline axis).
        _ => None,
      }
    } else {
      // Physical syntax (x/y), mapped to block/inline based on the containing block writing mode.
      let mut x: Option<AxisValue> = None;
      let mut y: Option<AxisValue> = None;
      let mut ambiguous: Option<AxisValue> = None;

      for token in [t1, t2] {
        match token {
          TokenKind::X(value) => {
            if x.replace(value).is_some() {
              return None;
            }
          }
          TokenKind::Y(value) => {
            if y.replace(value).is_some() {
              return None;
            }
          }
          TokenKind::Ambiguous(track) => {
            let value = AxisValue::Track(track);
            if ambiguous.replace(value).is_some() {
              return None;
            }
          }
          _ => return None,
        }
      }

      let (x, y) = match (x, y, ambiguous) {
        (Some(x), Some(y), None) => (x, y),
        (Some(x), None, Some(amb)) => (x, amb),
        (None, Some(y), Some(amb)) => (amb, y),
        _ => return None,
      };

      fn axis_sides(
        horizontal: bool,
        positive: bool,
      ) -> (crate::style::PhysicalSide, crate::style::PhysicalSide) {
        match (horizontal, positive) {
          (true, true) => (
            crate::style::PhysicalSide::Left,
            crate::style::PhysicalSide::Right,
          ),
          (true, false) => (
            crate::style::PhysicalSide::Right,
            crate::style::PhysicalSide::Left,
          ),
          (false, true) => (
            crate::style::PhysicalSide::Top,
            crate::style::PhysicalSide::Bottom,
          ),
          (false, false) => (
            crate::style::PhysicalSide::Bottom,
            crate::style::PhysicalSide::Top,
          ),
        }
      }

      let inline_sides = axis_sides(
        crate::style::inline_axis_is_horizontal(writing_mode),
        crate::style::inline_axis_positive(writing_mode, direction),
      );
      let block_sides = axis_sides(
        crate::style::block_axis_is_horizontal(writing_mode),
        crate::style::block_axis_positive(writing_mode),
      );

      let x_is_inline = crate::style::inline_axis_is_horizontal(writing_mode);

      let (logical_for_x, logical_for_y) = if x_is_inline {
        (
          crate::style::LogicalAxis::Inline,
          crate::style::LogicalAxis::Block,
        )
      } else {
        (
          crate::style::LogicalAxis::Block,
          crate::style::LogicalAxis::Inline,
        )
      };

      let physical_start_for_axis =
        |axis: crate::style::LogicalAxis| -> crate::style::PhysicalSide {
          match axis {
            crate::style::LogicalAxis::Inline => inline_sides.0,
            crate::style::LogicalAxis::Block => block_sides.0,
          }
        };

      let map_physical_value =
        |value: AxisValue, axis: crate::style::LogicalAxis| -> Option<PositionAreaTrack> {
          let start_side = physical_start_for_axis(axis);
          let physical_start_side = match axis {
            crate::style::LogicalAxis::Inline => {
              if x_is_inline {
                crate::style::PhysicalSide::Left
              } else {
                crate::style::PhysicalSide::Top
              }
            }
            crate::style::LogicalAxis::Block => {
              if x_is_inline {
                crate::style::PhysicalSide::Top
              } else {
                crate::style::PhysicalSide::Left
              }
            }
          };

          Some(match value {
            AxisValue::Track(track) => track,
            AxisValue::PhysicalStart => {
              if start_side == physical_start_side {
                PositionAreaTrack::Start
              } else {
                PositionAreaTrack::End
              }
            }
            AxisValue::PhysicalEnd => {
              if start_side == physical_start_side {
                PositionAreaTrack::End
              } else {
                PositionAreaTrack::Start
              }
            }
            AxisValue::SpanPhysicalStart => {
              if start_side == physical_start_side {
                PositionAreaTrack::SpanStart
              } else {
                PositionAreaTrack::SpanEnd
              }
            }
            AxisValue::SpanPhysicalEnd => {
              if start_side == physical_start_side {
                PositionAreaTrack::SpanEnd
              } else {
                PositionAreaTrack::SpanStart
              }
            }
          })
        };

      let x_track = map_physical_value(x, logical_for_x)?;
      let y_track = map_physical_value(y, logical_for_y)?;

      let (block, inline) = if x_is_inline {
        (y_track, x_track)
      } else {
        (x_track, y_track)
      };

      Some(PositionAreaTracks { block, inline })
    }
  }
}

/// Computed value for the `position-try-order` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionTryOrder {
  Normal,
  MostWidth,
  MostHeight,
  MostBlockSize,
  MostInlineSize,
}

impl Default for PositionTryOrder {
  fn default() -> Self {
    Self::Normal
  }
}

/// Computed value for the `anchor-scope` property.
#[derive(Debug, Clone, PartialEq)]
pub enum AnchorScope {
  None,
  All,
  Names(Vec<String>),
}

impl Default for AnchorScope {
  fn default() -> Self {
    Self::None
  }
}

/// CSS `overflow-wrap` (formerly `word-wrap`)
///
/// Reference: CSS Text Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowWrap {
  Normal,
  BreakWord,
  Anywhere,
}

/// Background image specification
///
/// CSS: `background-image`
/// Reference: CSS Backgrounds and Borders Module Level 3
#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundImageUrl {
  pub url: String,
  /// Optional override for the image's resolution (image pixels per CSS px, dppx).
  ///
  /// This is primarily sourced from `image-set()` density selection (e.g. `2x`), and is used when
  /// computing the image's natural size in CSS px (`background-size: auto`, border-image/mask-border
  /// slicing, etc).
  pub override_resolution: Option<f32>,
}

impl BackgroundImageUrl {
  pub fn new(url: impl Into<String>) -> Self {
    Self {
      url: url.into(),
      override_resolution: None,
    }
  }
}

impl std::hash::Hash for BackgroundImageUrl {
  fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
    std::hash::Hash::hash(&self.url, state);
    // `f32` does not implement `Hash`; encode the raw IEEE bits instead.
    let bits = self.override_resolution.map(|v| v.to_bits());
    std::hash::Hash::hash(&bits, state);
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BackgroundImage {
  None,
  Url(BackgroundImageUrl),
  LinearGradient {
    angle: f32,
    stops: Vec<ColorStop>,
  },
  RadialGradient {
    shape: RadialGradientShape,
    size: RadialGradientSize,
    position: BackgroundPosition,
    stops: Vec<ColorStop>,
  },
  RepeatingLinearGradient {
    angle: f32,
    stops: Vec<ColorStop>,
  },
  RepeatingRadialGradient {
    shape: RadialGradientShape,
    size: RadialGradientSize,
    position: BackgroundPosition,
    stops: Vec<ColorStop>,
  },
  ConicGradient {
    from_angle: f32,
    position: BackgroundPosition,
    stops: Vec<ColorStop>,
  },
  RepeatingConicGradient {
    from_angle: f32,
    position: BackgroundPosition,
    stops: Vec<ColorStop>,
  },
}

/// Background sizing keywords
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundSizeKeyword {
  Cover,
  Contain,
}

/// Background sizing component (per axis)
///
/// CSS: `background-size`
/// Reference: CSS Backgrounds and Borders Module Level 3
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BackgroundSizeComponent {
  Auto,
  Length(Length),
}

/// Background image sizing
///
/// CSS: `background-size`
/// Reference: CSS Backgrounds and Borders Module Level 3
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BackgroundSize {
  Keyword(BackgroundSizeKeyword),
  Explicit(BackgroundSizeComponent, BackgroundSizeComponent),
}

/// Box reference for background painting/positioning
///
/// CSS: `background-origin`, `background-clip`
/// Reference: CSS Backgrounds and Borders Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundBox {
  BorderBox,
  PaddingBox,
  ContentBox,
  Text,
}

/// Reference box for clip-path shapes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceBox {
  BorderBox,
  PaddingBox,
  ContentBox,
  MarginBox,
  FillBox,
  StrokeBox,
  ViewBox,
}

/// Background position component with alignment (percentage of available space)
/// and an offset applied after alignment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackgroundPositionComponent {
  /// Alignment fraction in the range `0..=1` (e.g., 0 = start, 0.5 = center, 1 = end)
  pub alignment: f32,
  /// Offset applied after alignment; percentages resolve against the remaining space.
  pub offset: Length,
}

/// Background image positioning
///
/// CSS: `background-position`
/// Reference: CSS Backgrounds and Borders Module Level 3
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BackgroundPosition {
  Position {
    x: BackgroundPositionComponent,
    y: BackgroundPositionComponent,
  },
}

/// Fill rule used for polygon clip-path shapes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillRule {
  NonZero,
  EvenOdd,
}

/// Shape radius keyword or length for circle/ellipse clip-path shapes
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShapeRadius {
  Length(Length),
  ClosestSide,
  FarthestSide,
}

/// Rounded corner radii for inset() clip paths
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipRadii {
  pub top_left: BorderCornerRadius,
  pub top_right: BorderCornerRadius,
  pub bottom_right: BorderCornerRadius,
  pub bottom_left: BorderCornerRadius,
}

/// Basic shapes supported by CSS clip-path
#[derive(Debug, Clone, PartialEq)]
pub enum BasicShape {
  Inset {
    top: Length,
    right: Length,
    bottom: Length,
    left: Length,
    border_radius: Box<Option<ClipRadii>>,
  },
  Circle {
    radius: ShapeRadius,
    position: BackgroundPosition,
  },
  Ellipse {
    radius_x: ShapeRadius,
    radius_y: ShapeRadius,
    position: BackgroundPosition,
  },
  Polygon {
    fill: FillRule,
    points: Vec<(Length, Length)>,
  },
  Path {
    fill: FillRule,
    data: Arc<str>,
  },
}

/// CSS clip-path computed value
#[derive(Debug, Clone, PartialEq)]
pub enum ClipPath {
  None,
  /// Fragment-only `url(#id)` (and potentially external URLs in the future).
  ///
  /// The optional `ReferenceBox` corresponds to the geometry-box component that can appear
  /// alongside `url(...)` in the authored value (e.g. `clip-path: url(#clip) content-box`).
  Url(String, Option<ReferenceBox>),
  Box(ReferenceBox),
  BasicShape(Box<BasicShape>, Option<ReferenceBox>),
}

/// CSS shape-outside computed value
#[derive(Debug, Clone, PartialEq)]
pub enum ShapeOutside {
  None,
  Box(ReferenceBox),
  BasicShape(Box<BasicShape>, Option<ReferenceBox>),
  Image(BackgroundImage),
}

/// Reference box used by transforms
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformBox {
  BorderBox,
  ContentBox,
  FillBox,
  StrokeBox,
  ViewBox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformStyle {
  Flat,
  Preserve3d,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfaceVisibility {
  Visible,
  Hidden,
}

/// Background attachment behavior
///
/// CSS: `background-attachment`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundAttachment {
  Scroll,
  Fixed,
  Local,
}

/// Background image repeat mode
///
/// CSS: `background-repeat`
/// Reference: CSS Backgrounds and Borders Module Level 3
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundRepeatKeyword {
  Repeat,
  Space,
  Round,
  NoRepeat,
}

/// Per-axis repeat keywords (x then y)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundRepeat {
  pub x: BackgroundRepeatKeyword,
  pub y: BackgroundRepeatKeyword,
}

impl BackgroundRepeat {
  pub const fn repeat() -> Self {
    Self {
      x: BackgroundRepeatKeyword::Repeat,
      y: BackgroundRepeatKeyword::Repeat,
    }
  }

  pub const fn repeat_x() -> Self {
    Self {
      x: BackgroundRepeatKeyword::Repeat,
      y: BackgroundRepeatKeyword::NoRepeat,
    }
  }

  pub const fn repeat_y() -> Self {
    Self {
      x: BackgroundRepeatKeyword::NoRepeat,
      y: BackgroundRepeatKeyword::Repeat,
    }
  }

  pub const fn no_repeat() -> Self {
    Self {
      x: BackgroundRepeatKeyword::NoRepeat,
      y: BackgroundRepeatKeyword::NoRepeat,
    }
  }
}

/// Single background layer with per-layer properties.
#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundLayer {
  pub image: Option<BackgroundImage>,
  pub position: BackgroundPosition,
  pub size: BackgroundSize,
  pub repeat: BackgroundRepeat,
  pub attachment: BackgroundAttachment,
  pub origin: BackgroundBox,
  pub clip: BackgroundBox,
  pub blend_mode: MixBlendMode,
}

/// Single mask layer with per-layer properties.
#[derive(Debug, Clone, PartialEq)]
pub struct MaskLayer {
  pub image: Option<BackgroundImage>,
  pub position: BackgroundPosition,
  pub size: BackgroundSize,
  pub repeat: BackgroundRepeat,
  pub mode: MaskMode,
  pub origin: MaskOrigin,
  pub clip: MaskClip,
  pub composite: MaskComposite,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClipComponent {
  Auto,
  Length(Length),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClipRect {
  pub top: ClipComponent,
  pub right: ClipComponent,
  pub bottom: ClipComponent,
  pub left: ClipComponent,
}

impl Default for BackgroundLayer {
  fn default() -> Self {
    Self {
      image: None,
      position: BackgroundPosition::Position {
        x: BackgroundPositionComponent {
          alignment: 0.0,
          offset: Length::percent(0.0),
        },
        y: BackgroundPositionComponent {
          alignment: 0.0,
          offset: Length::percent(0.0),
        },
      },
      size: BackgroundSize::Explicit(BackgroundSizeComponent::Auto, BackgroundSizeComponent::Auto),
      repeat: BackgroundRepeat::repeat(),
      attachment: BackgroundAttachment::Scroll,
      origin: BackgroundBox::PaddingBox,
      clip: BackgroundBox::BorderBox,
      blend_mode: MixBlendMode::Normal,
    }
  }
}

impl Default for MaskLayer {
  fn default() -> Self {
    Self {
      image: None,
      position: BackgroundPosition::Position {
        x: BackgroundPositionComponent {
          alignment: 0.0,
          offset: Length::percent(0.0),
        },
        y: BackgroundPositionComponent {
          alignment: 0.0,
          offset: Length::percent(0.0),
        },
      },
      size: BackgroundSize::Explicit(BackgroundSizeComponent::Auto, BackgroundSizeComponent::Auto),
      repeat: BackgroundRepeat::repeat(),
      mode: MaskMode::MatchSource,
      origin: MaskOrigin::BorderBox,
      clip: MaskClip::BorderBox,
      composite: MaskComposite::Add,
    }
  }
}
