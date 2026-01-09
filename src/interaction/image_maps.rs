use crate::dom::DomNode;
use crate::geometry::{Point, Rect};
use crate::tree::fragment_tree::{FragmentNode, FragmentTree};

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}'))
}

#[derive(Debug, Clone, PartialEq)]
enum AreaShape {
  Empty,
  Default,
  Rect { x1: f32, y1: f32, x2: f32, y2: f32 },
  Circle { x: f32, y: f32, r: f32 },
  Poly { points: Vec<Point> },
}

fn parse_list_of_floats(input: &str) -> Vec<f32> {
  // HTML "rules for parsing a list of floating-point numbers" are fairly permissive and treat
  // comma/ASCII whitespace as separators. For FastRender's current interaction surface, we only
  // need to support those separator rules and parse into f32s.
  input
    .split(|c: char| c == ',' || c.is_ascii_whitespace())
    .filter_map(|part| {
      let part = trim_ascii_whitespace(part);
      if part.is_empty() {
        return None;
      }
      let value = part.parse::<f32>().ok()?;
      value.is_finite().then_some(value)
    })
    .collect()
}

fn parse_area_shape(area: &DomNode) -> AreaShape {
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  enum ShapeState {
    Circle,
    Default,
    Poly,
    Rect,
  }

  let shape_state = match area
    .get_attribute_ref("shape")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
  {
    Some(value) if value.eq_ignore_ascii_case("circle") => ShapeState::Circle,
    Some(value) if value.eq_ignore_ascii_case("default") => ShapeState::Default,
    Some(value) if value.eq_ignore_ascii_case("poly") => ShapeState::Poly,
    Some(value) if value.eq_ignore_ascii_case("rect") => ShapeState::Rect,
    _ => ShapeState::Rect,
  };

  let mut coords = area
    .get_attribute_ref("coords")
    .map(parse_list_of_floats)
    .unwrap_or_default();

  match shape_state {
    ShapeState::Default => AreaShape::Default,
    ShapeState::Circle => {
      if coords.len() < 3 {
        return AreaShape::Empty;
      }
      coords.truncate(3);
      let x = coords[0];
      let y = coords[1];
      let r = coords[2];
      if r <= 0.0 {
        return AreaShape::Empty;
      }
      AreaShape::Circle { x, y, r }
    }
    ShapeState::Rect => {
      if coords.len() < 4 {
        return AreaShape::Empty;
      }
      coords.truncate(4);
      let mut x1 = coords[0];
      let mut y1 = coords[1];
      let mut x2 = coords[2];
      let mut y2 = coords[3];
      if x1 > x2 {
        std::mem::swap(&mut x1, &mut x2);
      }
      if y1 > y2 {
        std::mem::swap(&mut y1, &mut y2);
      }
      AreaShape::Rect { x1, y1, x2, y2 }
    }
    ShapeState::Poly => {
      if coords.len() < 6 {
        return AreaShape::Empty;
      }
      if coords.len() % 2 == 1 {
        coords.pop();
      }
      if coords.len() < 6 {
        return AreaShape::Empty;
      }
      let mut points = Vec::with_capacity(coords.len() / 2);
      for pair in coords.chunks_exact(2) {
        points.push(Point::new(pair[0], pair[1]));
      }
      AreaShape::Poly { points }
    }
  }
}

fn point_on_segment(p: Point, a: Point, b: Point) -> bool {
  // Basic segment hit test with an epsilon for floating point inputs.
  const EPS: f32 = 1e-6;
  let abx = b.x - a.x;
  let aby = b.y - a.y;
  let apx = p.x - a.x;
  let apy = p.y - a.y;

  let cross = apx * aby - apy * abx;
  if cross.abs() > EPS {
    return false;
  }

  let dot = apx * abx + apy * aby;
  if dot < -EPS {
    return false;
  }
  let len_sq = abx * abx + aby * aby;
  if dot > len_sq + EPS {
    return false;
  }

  true
}

fn point_in_polygon_even_odd(p: Point, points: &[Point]) -> bool {
  let n = points.len();
  if n < 3 {
    return false;
  }

  let mut inside = false;
  for i in 0..n {
    let a = points[i];
    let b = points[(i + 1) % n];

    if point_on_segment(p, a, b) {
      return true;
    }

    let intersects = (a.y > p.y) != (b.y > p.y)
      && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x;
    if intersects {
      inside = !inside;
    }
  }
  inside
}

fn area_contains_point(shape: &AreaShape, point: Point) -> bool {
  match shape {
    AreaShape::Empty => false,
    AreaShape::Default => true,
    AreaShape::Rect { x1, y1, x2, y2 } => point.x >= *x1 && point.x <= *x2 && point.y >= *y1 && point.y <= *y2,
    AreaShape::Circle { x, y, r } => {
      let dx = point.x - *x;
      let dy = point.y - *y;
      dx * dx + dy * dy <= r * r
    }
    AreaShape::Poly { points } => point_in_polygon_even_odd(point, points),
  }
}

pub fn resolve_usemap<'a>(dom_root: &'a DomNode, usemap: &str) -> Option<&'a DomNode> {
  // WHATWG HTML: "rules for parsing a hash-name reference"
  let hash = usemap.find('#')?;
  if hash + 1 >= usemap.len() {
    return None;
  }
  let s = &usemap[(hash + 1)..];

  let mut stack: Vec<&DomNode> = vec![dom_root];
  while let Some(node) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("map"))
      && (node.get_attribute_ref("id") == Some(s) || node.get_attribute_ref("name") == Some(s))
    {
      return Some(node);
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  None
}

pub fn hit_test_image_map<'a>(
  dom_root: &'a DomNode,
  usemap: &str,
  point_in_image: Point,
) -> Option<&'a DomNode> {
  let map = resolve_usemap(dom_root, usemap)?;

  // Collect all descendant `<area>` elements in tree order and return the first that contains the
  // point. Per HTML image map layering, this means the first `<area>` in tree order is top-most.
  let mut stack: Vec<&DomNode> = Vec::new();
  for child in map.children.iter().rev() {
    stack.push(child);
  }
  while let Some(node) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("area"))
    {
      let shape = parse_area_shape(node);
      if area_contains_point(&shape, point_in_image) {
        return Some(node);
      }
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  None
}

pub fn absolute_bounds_for_fragment<'a>(
  fragment_tree: &'a FragmentTree,
  fragment: &'a FragmentNode,
) -> Option<Rect> {
  let target = fragment as *const FragmentNode;
  let mut stack: Vec<(&FragmentNode, Point)> = Vec::new();
  for root in fragment_tree.additional_fragments.iter().rev() {
    stack.push((root, Point::ZERO));
  }
  stack.push((&fragment_tree.root, Point::ZERO));

  while let Some((node, parent_origin)) = stack.pop() {
    let abs_bounds = node.bounds.translate(parent_origin);
    if (node as *const FragmentNode) == target {
      return Some(abs_bounds);
    }

    let self_origin = abs_bounds.origin;
    for child in node.children.iter().rev() {
      stack.push((child, self_origin));
    }
  }

  None
}

pub fn local_point_in_fragment(
  fragment_tree: &FragmentTree,
  fragment: &FragmentNode,
  page_point: Point,
) -> Option<Point> {
  let bounds = absolute_bounds_for_fragment(fragment_tree, fragment)?;
  Some(Point::new(page_point.x - bounds.x(), page_point.y - bounds.y()))
}

pub fn first_img_referencing_map<'a>(dom_root: &'a DomNode, map_ptr: *const DomNode) -> Option<&'a DomNode> {
  let mut stack: Vec<&DomNode> = vec![dom_root];
  while let Some(node) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("img"))
    {
      if let Some(usemap) = node.get_attribute_ref("usemap") {
        if resolve_usemap(dom_root, usemap).is_some_and(|map| (map as *const DomNode) == map_ptr) {
          return Some(node);
        }
      }
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  None
}
