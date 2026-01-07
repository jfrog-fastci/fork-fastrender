//! Code for resolving name grid lines and areas

use crate::{
  CheapCloneStr, GenericGridTemplateComponent, GenericRepetition as _, GridAreaAxis, GridAreaEnd,
  GridContainerStyle, GridPlacement, GridTemplateArea, Line,
  NonNamedGridPlacementWithNamedSpan, RepetitionCount,
};
use core::{
  borrow::Borrow,
  cmp::{max, min, Ordering},
  fmt::Debug,
};

use super::{GridLine, OriginZeroLine};
// use alloc::fmt::format;
use crate::sys::{format, single_value_vec, Map, Vec};

/// Wrap an `AsRef<str>` type with a type which implements Hash by first
/// deferring to the underlying `&str`'s implementation of Hash.
#[derive(Debug, Clone)]
pub(crate) struct StrHasher<T: CheapCloneStr>(pub T);
impl<T: CheapCloneStr> PartialOrd for StrHasher<T> {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}
impl<T: CheapCloneStr> Ord for StrHasher<T> {
  fn cmp(&self, other: &Self) -> Ordering {
    self.0.as_ref().cmp(other.0.as_ref())
  }
}
impl<T: CheapCloneStr> PartialEq for StrHasher<T> {
  fn eq(&self, other: &Self) -> bool {
    other.0.as_ref() == self.0.as_ref()
  }
}
impl<T: CheapCloneStr> Eq for StrHasher<T> {}
#[cfg(feature = "std")]
impl<T: CheapCloneStr> std::hash::Hash for StrHasher<T> {
  fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
    self.0.as_ref().hash(state)
  }
}
impl<T: CheapCloneStr> Borrow<str> for StrHasher<T> {
  fn borrow(&self) -> &str {
    self.0.as_ref()
  }
}

/// Resolver that takes grid lines names and area names as input and can then be used to
/// resolve line names of grid placement properties into line numbers.
pub(crate) struct NamedLineResolver<S: CheapCloneStr> {
  /// Map of row line names to line numbers. Each line name may correspond to multiple lines
  /// so we store a `Vec`
  row_lines: Map<StrHasher<S>, Vec<u16>>,
  /// Map of column line names to line numbers. Each line name may correspond to multiple lines
  /// so we store a `Vec`
  column_lines: Map<StrHasher<S>, Vec<u16>>,
  /// Map of area names to area definitions (start and end lines numbers in each axis)
  areas: Map<StrHasher<S>, GridTemplateArea<S>>,
  /// Number of columns implied by grid area definitions
  area_column_count: u16,
  /// Number of rows implied by grid area definitions
  area_row_count: u16,
  /// The number of explicit columns in the grid. This is an *input* to the `NamedLineResolver` and is
  /// used when computing the fallback line when a non-existent named line is specified.
  explicit_column_count: u16,
  /// The number of explicit rows in the grid. This is an *input* to the `NamedLineResolver` and is
  /// used when computing the fallback line when a non-existent named line is specified.
  explicit_row_count: u16,
}

/// Utility function to create or update an entry in a line name map
fn upsert_line_name_map<S: CheapCloneStr>(
  map: &mut Map<StrHasher<S>, Vec<u16>>,
  key: S,
  value: u16,
) {
  map
    .entry(StrHasher(key))
    .and_modify(|lines| lines.push(value))
    .or_insert_with(|| single_value_vec(value));
}

impl<S: CheapCloneStr> NamedLineResolver<S> {
  fn axis_line_maps(&self, axis: GridAreaAxis) -> (&Map<StrHasher<S>, Vec<u16>>, u16) {
    match axis {
      GridAreaAxis::Row => (&self.row_lines, self.explicit_row_count),
      GridAreaAxis::Column => (&self.column_lines, self.explicit_column_count),
    }
  }

  fn lookup_named_lines(
    &self,
    name: &S,
    axis: GridAreaAxis,
    end: GridAreaEnd,
  ) -> Option<&[u16]> {
    let (line_lookup, _) = self.axis_line_maps(axis);
    if let Some(lines) = line_lookup.get(name.as_ref()) {
      return Some(lines.as_slice());
    }

    let implicit_name = match end {
      GridAreaEnd::Start => format!("{}-start", name.as_ref()),
      GridAreaEnd::End => format!("{}-end", name.as_ref()),
    };
    line_lookup
      .get(implicit_name.as_str())
      .map(|lines| lines.as_slice())
  }

  fn origin_zero_to_grid_line(line: OriginZeroLine, explicit_track_count: u16) -> GridLine {
    let explicit_line_count = explicit_track_count + 1;
    let line = if line.0 >= 0 {
      line.0 as i32 + 1
    } else {
      line.0 as i32 - explicit_line_count as i32
    };
    GridLine::from(line.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
  }

  /// Create a resolver from explicit line name vectors for rows and columns
  pub(crate) fn from_line_names(
    row_line_names: Vec<Vec<S>>,
    column_line_names: Vec<Vec<S>>,
    explicit_row_count: u16,
    explicit_column_count: u16,
  ) -> Self {
    let mut row_lines: Map<StrHasher<S>, Vec<u16>> = Map::default();
    for (idx, names) in row_line_names.iter().enumerate() {
      let line_index = (idx as u16) + 1;
      for name in names {
        upsert_line_name_map(&mut row_lines, name.clone(), line_index);
      }
    }

    let mut column_lines: Map<StrHasher<S>, Vec<u16>> = Map::default();
    for (idx, names) in column_line_names.iter().enumerate() {
      let line_index = (idx as u16) + 1;
      for name in names {
        upsert_line_name_map(&mut column_lines, name.clone(), line_index);
      }
    }

    for lines in row_lines.values_mut() {
      lines.sort_unstable();
      lines.dedup();
    }
    for lines in column_lines.values_mut() {
      lines.sort_unstable();
      lines.dedup();
    }

    Self {
      area_column_count: 0,
      area_row_count: 0,
      explicit_column_count,
      explicit_row_count,
      areas: Map::default(),
      row_lines,
      column_lines,
    }
  }

  /// Create and initialise a new `NamedLineResolver`
  pub(crate) fn new(
    style: &impl GridContainerStyle<CustomIdent = S>,
    column_auto_repetitions: u16,
    row_auto_repetitions: u16,
  ) -> Self {
    let mut areas: Map<StrHasher<S>, GridTemplateArea<_>> = Map::default();
    let mut column_lines: Map<StrHasher<S>, Vec<u16>> = Map::default();
    let mut row_lines: Map<StrHasher<S>, Vec<u16>> = Map::default();

    let mut area_column_count = 0;
    let mut area_row_count = 0;
    if let Some(area_iter) = style.grid_template_areas() {
      for area in area_iter.into_iter() {
        // TODO: Investigate eliminating clones
        areas.insert(StrHasher(area.name.clone()), area.clone());

        area_column_count = area_column_count.max(area.column_end.max(1) - 1);
        area_row_count = area_row_count.max(area.row_end.max(1) - 1);

        let col_start_name = S::from(format!("{}-start", area.name.as_ref()));
        upsert_line_name_map(&mut column_lines, col_start_name, area.column_start);
        let col_end_name = S::from(format!("{}-end", area.name.as_ref()));
        upsert_line_name_map(&mut column_lines, col_end_name, area.column_end);
        let row_start_name = S::from(format!("{}-start", area.name.as_ref()));
        upsert_line_name_map(&mut row_lines, row_start_name, area.row_start);
        let row_end_name = S::from(format!("{}-end", area.name.as_ref()));
        upsert_line_name_map(&mut row_lines, row_end_name, area.row_end);
      }
    }

    // ---

    let mut current_line = 0;
    if let Some(mut column_tracks) = style.grid_template_columns() {
      if let Some(column_line_names_iter) = style.grid_template_column_names() {
        for line_names in column_line_names_iter {
          current_line += 1;
          for line_name in line_names.into_iter() {
            column_lines
              .entry(StrHasher(line_name.clone()))
              .and_modify(|lines: &mut Vec<u16>| lines.push(current_line))
              .or_insert_with(|| single_value_vec(current_line));
          }

          if let Some(GenericGridTemplateComponent::Repeat(repeat)) = column_tracks.next() {
            let repeat_count = match repeat.count() {
              RepetitionCount::Count(count) => count,
              RepetitionCount::AutoFill | RepetitionCount::AutoFit => column_auto_repetitions,
            };

            for _ in 0..repeat_count {
              for line_name_set in repeat.lines_names() {
                for line_name in line_name_set {
                  upsert_line_name_map(&mut column_lines, line_name.clone(), current_line);
                }
                current_line += 1;
              }
              // Last line name set collapses with following line name set
              current_line -= 1;
            }
            // Last line name set collapses with following line name set
            current_line -= 1;
          }
        }
      }
    }
    // Sort and dedup lines for each column name
    for lines in column_lines.values_mut() {
      lines.sort_unstable();
      lines.dedup();
    }

    let mut current_line = 0;
    if let Some(mut row_tracks) = style.grid_template_rows() {
      if let Some(row_line_names_iter) = style.grid_template_row_names() {
        for line_names in row_line_names_iter {
          current_line += 1;
          for line_name in line_names.into_iter() {
            row_lines
              .entry(StrHasher(line_name.clone()))
              .and_modify(|lines: &mut Vec<u16>| lines.push(current_line))
              .or_insert_with(|| single_value_vec(current_line));
          }

          if let Some(GenericGridTemplateComponent::Repeat(repeat)) = row_tracks.next() {
            let repeat_count = match repeat.count() {
              RepetitionCount::Count(count) => count,
              RepetitionCount::AutoFill | RepetitionCount::AutoFit => row_auto_repetitions,
            };

            for _ in 0..repeat_count {
              for line_name_set in repeat.lines_names() {
                for line_name in line_name_set {
                  upsert_line_name_map(&mut row_lines, line_name.clone(), current_line);
                }
                current_line += 1;
              }
              // Last line name set collapses with following line name set
              current_line -= 1;
            }
            // Last line name set collapses with following line name set
            current_line -= 1;
          }
        }
      }
    }
    // Sort and dedup lines for each row name
    for lines in row_lines.values_mut() {
      lines.sort_unstable();
      lines.dedup();
    }

    Self {
      area_column_count,
      area_row_count,
      explicit_column_count: 0, // Overwritten later
      explicit_row_count: 0,    // Overwritten later
      areas,
      row_lines,
      column_lines,
    }
  }

  /// Resolve named lines for both the `start` and `end` of a row-axis grid placement
  #[inline(always)]
  pub(crate) fn resolve_row_names(
    &self,
    line: &Line<GridPlacement<S>>,
  ) -> Line<NonNamedGridPlacementWithNamedSpan<S>> {
    self.resolve_line_names(line, GridAreaAxis::Row)
  }

  /// Resolve named lines for both the `start` and `end` of a column-axis grid placement
  #[inline(always)]
  pub(crate) fn resolve_column_names(
    &self,
    line: &Line<GridPlacement<S>>,
  ) -> Line<NonNamedGridPlacementWithNamedSpan<S>> {
    self.resolve_line_names(line, GridAreaAxis::Column)
  }

  /// Resolve named lines for both the `start` and `end` of a grid placement
  #[inline(always)]
  pub(crate) fn resolve_line_names(
    &self,
    line: &Line<GridPlacement<S>>,
    axis: GridAreaAxis,
  ) -> Line<NonNamedGridPlacementWithNamedSpan<S>> {
    let start_holder;
    let start_line_resolved = if let GridPlacement::NamedLine(name, idx) = &line.start {
      start_holder =
        GridPlacement::Line(
          self.find_line_index(name, *idx, axis, GridAreaEnd::Start, &|lines| lines),
        );
      &start_holder
    } else {
      &line.start
    };

    let end_holder;
    let end_line_resolved = if let GridPlacement::NamedLine(name, idx) = &line.end {
      end_holder =
        GridPlacement::Line(
          self.find_line_index(name, *idx, axis, GridAreaEnd::End, &|lines| lines),
        );
      &end_holder
    } else {
      &line.end
    };

    // If both the *-start and *-end values of its grid-placement properties specify a line, its grid span is implicit.
    // If it has an explicit span value, its grid span is explicit.
    // Otherwise, its grid span is automatic:
    //   - if it is subgridded in that axis, its grid span is determined from its <line-name-list>;
    //   - otherwise its grid span is 1.
    //
    // <https://drafts.csswg.org/css-grid-2/#grid-span>
    match (&start_line_resolved, &end_line_resolved) {
      // If the placement contains two spans, remove the one contributed by the end property.
      //
      // Without this, `span N / span <name> M` or `span <name> N / span M` would carry a
      // `NamedSpan` through to the auto-placement algorithm where it does not have enough context
      // to resolve the span relative to a concrete opposite edge, leading to panics.
      //
      // <https://drafts.csswg.org/css-grid-2/#grid-placement-errors>
      (GridPlacement::Span(span), GridPlacement::NamedSpan(_, _)) => Line {
        start: NonNamedGridPlacementWithNamedSpan::Span(*span),
        end: NonNamedGridPlacementWithNamedSpan::Auto,
      },
      (GridPlacement::NamedSpan(_, _), GridPlacement::Span(_) | GridPlacement::NamedSpan(_, _)) => {
        // If the placement contains only a span for a named line, replace it with a span of 1.
        Line {
          start: NonNamedGridPlacementWithNamedSpan::Span(1),
          end: NonNamedGridPlacementWithNamedSpan::Auto,
        }
      }
      (GridPlacement::Line(start_line), GridPlacement::NamedSpan(name, idx))
        if start_line.as_i16() != 0 =>
      {
        let explicit_track_count = match axis {
          GridAreaAxis::Row => self.explicit_row_count,
          GridAreaAxis::Column => self.explicit_column_count,
        };
        let start_line_oz = start_line.into_origin_zero_line(explicit_track_count);
        let end_line_oz = self.resolve_named_span_end_line(name, *idx, axis, start_line_oz);
        let end_line = Self::origin_zero_to_grid_line(end_line_oz, explicit_track_count);
        Line {
          start: NonNamedGridPlacementWithNamedSpan::Line(*start_line),
          end: NonNamedGridPlacementWithNamedSpan::Line(end_line),
        }
      }
      (GridPlacement::NamedSpan(name, idx), GridPlacement::Line(end_line))
        if end_line.as_i16() != 0 =>
      {
        let explicit_track_count = match axis {
          GridAreaAxis::Row => self.explicit_row_count,
          GridAreaAxis::Column => self.explicit_column_count,
        };
        let end_line_oz = end_line.into_origin_zero_line(explicit_track_count);
        let start_line_oz = self.resolve_named_span_start_line(name, *idx, axis, end_line_oz);
        let start_line = Self::origin_zero_to_grid_line(start_line_oz, explicit_track_count);
        Line {
          start: NonNamedGridPlacementWithNamedSpan::Line(start_line),
          end: NonNamedGridPlacementWithNamedSpan::Line(*end_line),
        }
      }
      (start, end) => Line {
        start: match start {
          GridPlacement::Auto => NonNamedGridPlacementWithNamedSpan::Auto,
          GridPlacement::Line(grid_line) => NonNamedGridPlacementWithNamedSpan::Line(*grid_line),
          GridPlacement::Span(span) => NonNamedGridPlacementWithNamedSpan::Span(*span),
          GridPlacement::NamedSpan(name, span) => {
            NonNamedGridPlacementWithNamedSpan::NamedSpan(name.clone(), *span)
          }
          _ => unreachable!(),
        },
        end: match end {
          GridPlacement::Auto => NonNamedGridPlacementWithNamedSpan::Auto,
          GridPlacement::Line(grid_line) => NonNamedGridPlacementWithNamedSpan::Line(*grid_line),
          GridPlacement::Span(span) => NonNamedGridPlacementWithNamedSpan::Span(*span),
          GridPlacement::NamedSpan(name, span) => {
            NonNamedGridPlacementWithNamedSpan::NamedSpan(name.clone(), *span)
          }
          _ => unreachable!(),
        },
      },
    }
  }

  /// Resolve the end edge of an item whose start edge is known and end edge is a `NamedSpan(...)`.
  pub(crate) fn resolve_named_span_end_line(
    &self,
    name: &S,
    idx: u16,
    axis: GridAreaAxis,
    start_line: OriginZeroLine,
  ) -> OriginZeroLine {
    let idx = idx.min(i16::MAX as u16).max(1);
    let (_, explicit_track_count) = self.axis_line_maps(axis);
    let explicit_end_line = OriginZeroLine(explicit_track_count as i16);

    let normalized_start_line = if start_line.0 >= 0 {
      (start_line.0 as i32 + 1).clamp(0, u16::MAX as i32) as u16
    } else {
      0
    };

    let named_lines = self.lookup_named_lines(name, axis, GridAreaEnd::End);
    let named_lines_after_start = named_lines.map(|lines| {
      let point = lines.partition_point(|line| *line <= normalized_start_line);
      &lines[point..]
    });

    if let Some(after) = named_lines_after_start {
      let after_len = after.len();
      let idx_usize = idx as usize;
      if after_len >= idx_usize {
        let line_index = after[idx_usize - 1];
        let oz = (line_index as i32 - 1).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        return OriginZeroLine(oz);
      }

      let remaining = idx - after_len as u16;
      let baseline = max(start_line, explicit_end_line);
      return OriginZeroLine((baseline.0 as i32 + remaining as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16);
    }

    let baseline = max(start_line, explicit_end_line);
    OriginZeroLine((baseline.0 as i32 + idx as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16)
  }

  /// Resolve the start edge of an item whose end edge is known and start edge is a `NamedSpan(...)`.
  pub(crate) fn resolve_named_span_start_line(
    &self,
    name: &S,
    idx: u16,
    axis: GridAreaAxis,
    end_line: OriginZeroLine,
  ) -> OriginZeroLine {
    let idx = idx.min(i16::MAX as u16).max(1);
    let normalized_end_line = if end_line.0 >= 0 {
      (end_line.0 as i32 + 1).clamp(0, u16::MAX as i32) as u16
    } else {
      0
    };

    let named_lines = self.lookup_named_lines(name, axis, GridAreaEnd::Start);
    let named_lines_before_end = named_lines.map(|lines| {
      let point = lines.partition_point(|line| *line < normalized_end_line);
      &lines[..point]
    });

    if let Some(before) = named_lines_before_end {
      let before_len = before.len();
      let idx_usize = idx as usize;
      if before_len >= idx_usize {
        let line_index = before[before_len - idx_usize];
        let oz = (line_index as i32 - 1).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        return OriginZeroLine(oz);
      }

      let remaining = idx - before_len as u16;
      let baseline = min(end_line, OriginZeroLine(0));
      return OriginZeroLine((baseline.0 as i32 - remaining as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16);
    }

    let baseline = min(end_line, OriginZeroLine(0));
    OriginZeroLine((baseline.0 as i32 - idx as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16)
  }

  /// Resolve the grid line for a named grid line or span
  fn find_line_index(
    &self,
    name: &S,
    idx: i16,
    axis: GridAreaAxis,
    end: GridAreaEnd,
    filter_lines: &dyn Fn(&[u16]) -> &[u16],
  ) -> GridLine {
    let name = name.as_ref();
    let mut idx = idx;
    let explicit_track_count = match axis {
      GridAreaAxis::Row => self.explicit_row_count as i16,
      GridAreaAxis::Column => self.explicit_column_count as i16,
    };

    // An index of 0 is used to represent "no index specified".
    if idx == 0 {
      idx = 1;
    }

    fn get_line(lines: &[u16], explicit_track_count: i16, idx: i16) -> i16 {
      let abs_idx = idx.abs();
      let enough_lines = abs_idx <= lines.len() as i16;
      if enough_lines {
        if idx > 0 {
          lines[(abs_idx - 1) as usize] as i16
        } else {
          lines[lines.len() - (abs_idx) as usize] as i16
        }
      } else {
        let remaining_lines = (abs_idx - lines.len() as i16) * idx.signum();
        if idx > 0 {
          (explicit_track_count + 1) + remaining_lines
        } else {
          -((explicit_track_count + 1) + remaining_lines)
        }
      }
    }

    // Lookup lines
    let line_lookup = match axis {
      GridAreaAxis::Row => &self.row_lines,
      GridAreaAxis::Column => &self.column_lines,
    };
    if let Some(lines) = line_lookup.get(name) {
      return GridLine::from(get_line(filter_lines(lines), explicit_track_count, idx));
    } else {
      // TODO: eliminate string allocations
      match end {
        GridAreaEnd::Start => {
          let implicit_name = format!("{name}-start");
          if let Some(lines) = line_lookup.get(&*implicit_name) {
            // println!("IMPLICIT COL {implicit_name}");
            return GridLine::from(get_line(filter_lines(lines), explicit_track_count, idx));
          }
        }
        GridAreaEnd::End => {
          let implicit_name = format!("{name}-end");
          if let Some(lines) = line_lookup.get(&*implicit_name) {
            // println!("IMPLICIT ROW {implicit_name}");
            return GridLine::from(get_line(filter_lines(lines), explicit_track_count, idx));
          }
        }
      }
    }

    // The CSS Grid specification has a weird quirk where it matches non-existent line names
    // to the first (positive) implicit line in the grid
    //
    // We add/subtract 2 to the explicit track count because (in each axis) a grid has one more explicit
    // grid line than it has tracks. And the fallback line is the line *after* that.
    //
    // See: <https://github.com/w3c/csswg-drafts/issues/966#issuecomment-277042153>
    let line = if idx > 0 {
      (explicit_track_count + 1) + idx
    } else {
      -((explicit_track_count + 1) + idx)
    };

    GridLine::from(line)
  }

  /// Get the number of columns defined by the grid areas
  pub(crate) fn area_column_count(&self) -> u16 {
    self.area_column_count
  }

  /// Get the number of rows defined by the grid areas
  pub(crate) fn area_row_count(&self) -> u16 {
    self.area_row_count
  }

  /// Set the number of columns in the explicit grid
  pub(crate) fn set_explicit_column_count(&mut self, count: u16) {
    self.explicit_column_count = count;
  }

  /// Set the number of rows in the explicit grid
  pub(crate) fn set_explicit_row_count(&mut self, count: u16) {
    self.explicit_row_count = count;
  }

  pub(crate) fn expanded_row_line_names(&self) -> Vec<Vec<S>> {
    self.expanded_line_names(GridAreaAxis::Row)
  }

  pub(crate) fn expanded_column_line_names(&self) -> Vec<Vec<S>> {
    self.expanded_line_names(GridAreaAxis::Column)
  }

  fn expanded_line_names(&self, axis: GridAreaAxis) -> Vec<Vec<S>> {
    let (map, explicit_track_count) = match axis {
      GridAreaAxis::Row => (&self.row_lines, self.explicit_row_count),
      GridAreaAxis::Column => (&self.column_lines, self.explicit_column_count),
    };

    let mut line_count_including_end = explicit_track_count.saturating_add(1);
    for lines in map.values() {
      if let Some(max_line) = lines.iter().copied().max() {
        line_count_including_end = line_count_including_end.max(max_line);
      }
    }

    let line_count = line_count_including_end as usize;
    let mut result: Vec<Vec<S>> = Vec::with_capacity(line_count);
    for _ in 0..line_count {
      result.push(Vec::new());
    }

    for (name, lines) in map.iter() {
      for &line in lines.iter() {
        if line == 0 {
          continue;
        }
        let idx = (line - 1) as usize;
        if idx < result.len() {
          result[idx].push(name.0.clone());
        }
      }
    }

    for names in result.iter_mut() {
      names.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
      names.dedup_by(|a, b| a.as_ref() == b.as_ref());
    }

    result
  }
}

impl<S: CheapCloneStr> Debug for NamedLineResolver<S> {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    writeln!(f, "Grid Areas:")?;
    for area in self.areas.values() {
      writeln!(
        f,
        "{}: row:{}/{} col: {}/{}",
        area.name.as_ref(),
        area.row_start,
        area.row_end,
        area.column_start,
        area.column_end
      )?;
    }

    writeln!(f, "Grid Rows:")?;
    for (name, lines) in self.row_lines.iter() {
      write!(f, "{}: ", name.0.as_ref())?;
      for line in lines {
        write!(f, "{line}  ")?;
      }
      writeln!(f)?;
    }

    writeln!(f, "Grid Columns:")?;
    for (name, lines) in self.column_lines.iter() {
      write!(f, "{}: ", name.0.as_ref())?;
      for line in lines {
        write!(f, "{line}  ")?;
      }
      writeln!(f)?;
    }

    Ok(())
  }
}
