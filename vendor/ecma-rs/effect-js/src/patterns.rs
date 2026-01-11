use crate::{properties, Api, CallSiteInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayOpKind {
  Map,
  Filter,
  Reduce,
  ForEach,
}

impl ArrayOpKind {
  pub fn from_api_name(name: &str) -> Option<Self> {
    match name {
      "Array.prototype.map" => Some(Self::Map),
      "Array.prototype.filter" => Some(Self::Filter),
      "Array.prototype.reduce" => Some(Self::Reduce),
      "Array.prototype.forEach" => Some(Self::ForEach),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayOpMetadata {
  pub fusable: bool,
  pub parallelizable: bool,
  pub output_length_relation: properties::OutputLengthRelation,
}

#[derive(Debug, Clone)]
pub struct ArrayOp {
  pub api: Api,
  pub callsite: CallSiteInfo,
  pub kind: ArrayOpKind,
  pub meta: ArrayOpMetadata,
}

#[derive(Debug, Clone)]
pub struct MapFilterReduce {
  pub ops: Vec<ArrayOp>,
}

impl MapFilterReduce {
  /// Recognize a (map|filter)+reduce pipeline based purely on a sequence of APIs.
  ///
  /// This is intentionally structural only; callers are expected to supply the
  /// `Api` + `CallSiteInfo` sequence extracted from AST/IR pattern matching.
  pub fn recognize(ops: Vec<(Api, CallSiteInfo)>) -> Option<Self> {
    if ops.len() < 2 {
      return None;
    }

    let apis: Vec<_> = ops.iter().map(|(api, _)| api.clone()).collect();
    let kinds: Vec<_> = ops
      .iter()
      .map(|(api, _)| ArrayOpKind::from_api_name(&api.name))
      .collect::<Option<Vec<_>>>()?;

    if !matches!(kinds.last(), Some(ArrayOpKind::Reduce)) {
      return None;
    }
    if kinds[..kinds.len() - 1]
      .iter()
      .any(|k| !matches!(k, ArrayOpKind::Map | ArrayOpKind::Filter))
    {
      return None;
    }

    let annotated_ops = ops
      .into_iter()
      .enumerate()
      .map(|(idx, (api, callsite))| {
        let kind = ArrayOpKind::from_api_name(&api.name)
          .expect("validated by initial kind extraction above");

        let fusable = match (idx.checked_sub(1).map(|p| &apis[p]), apis.get(idx + 1)) {
          (_, Some(next)) => {
            properties::fusable_with(&api, next) || properties::fusable_with(next, &api)
          }
          (Some(prev), None) => {
            properties::fusable_with(prev, &api) || properties::fusable_with(&api, prev)
          }
          (None, None) => false,
        };

        let meta = ArrayOpMetadata {
          fusable,
          parallelizable: properties::is_parallelizable(&api, &callsite),
          output_length_relation: properties::output_length_relation(&api),
        };

        ArrayOp {
          api,
          callsite,
          kind,
          meta,
        }
      })
      .collect();

    Some(Self { ops: annotated_ops })
  }
}
