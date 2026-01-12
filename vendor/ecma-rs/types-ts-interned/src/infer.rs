use crate::eval::{ConditionalAssignability, EvaluatorLimits};
use crate::ids::{DefId, TypeId, TypeParamId};
use crate::kind::{TupleElem, TypeKind};
use crate::store::TypeStore;
use ahash::AHashSet;
use std::collections::BTreeMap;
use std::hash::Hash;
use std::hash::Hasher;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MatchKey {
  check: TypeId,
  pattern: TypeId,
}

impl Hash for MatchKey {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.check.hash(state);
    self.pattern.hash(state);
  }
}

struct InferCtx<'a> {
  store: &'a TypeStore,
  limits: EvaluatorLimits,
  steps: &'a mut usize,
  assignability: &'a dyn ConditionalAssignability,
  visited: AHashSet<MatchKey>,
  bindings: BTreeMap<TypeParamId, TypeId>,
}

impl<'a> InferCtx<'a> {
  fn bump_steps(&mut self) -> bool {
    if self.limits.step_limit == EvaluatorLimits::DEFAULT_STEP_LIMIT {
      return true;
    }

    if *self.steps >= self.limits.step_limit {
      return false;
    }

    *self.steps += 1;
    true
  }

  fn bind(&mut self, param: TypeParamId, ty: TypeId) -> bool {
    match self.bindings.get(&param) {
      Some(existing) => *existing == ty,
      None => {
        self.bindings.insert(param, ty);
        true
      }
    }
  }

  fn match_types(&mut self, check: TypeId, pattern: TypeId, depth: usize) -> bool {
    if depth >= self.limits.depth_limit {
      return false;
    }
    if !self.bump_steps() {
      return false;
    }

    if check == pattern {
      return true;
    }

    let key = MatchKey { check, pattern };
    if !self.visited.insert(key) {
      // Avoid infinite recursion on cyclic type graphs. This is conservative:
      // once we re-encounter the same `(check, pattern)` pair on the recursion
      // stack, treat the submatch as successful and rely on prior captures.
      return true;
    }

    let result = match self.store.type_kind(pattern) {
      // Inference patterns commonly use `any`/`unknown` as wildcards
      // (e.g. `T extends (...args: infer P) => any ? P : never`).
      TypeKind::Any | TypeKind::Unknown => true,
      TypeKind::Infer { param, constraint } => {
        if let Some(constraint) = constraint {
          if !self
            .assignability
            .is_assignable_for_conditional(check, constraint)
          {
            false
          } else {
            self.bind(param, check)
          }
        } else {
          self.bind(param, check)
        }
      }
      TypeKind::Callable { overloads: pat_ovs } => self.match_callable(check, &pat_ovs, depth + 1),
      TypeKind::Array {
        ty: pat_elem,
        readonly: pat_readonly,
      } => self.match_array(check, pat_elem, pat_readonly, depth + 1),
      TypeKind::Tuple(pat_elems) => self.match_tuple(check, &pat_elems, depth + 1),
      TypeKind::Ref {
        def: pat_def,
        args: pat_args,
      } => self.match_ref(check, pat_def, &pat_args, depth + 1),
      TypeKind::Union(pat_members) => self.match_union(check, &pat_members, depth + 1),
      TypeKind::Intersection(pat_members) => self.match_intersection(check, &pat_members, depth + 1),
      _ => false,
    };

    self.visited.remove(&key);
    result
  }

  fn match_callable(&mut self, check: TypeId, pat_overloads: &[crate::SignatureId], depth: usize) -> bool {
    let TypeKind::Callable {
      overloads: check_overloads,
    } = self.store.type_kind(check)
    else {
      return false;
    };

    if check_overloads.len() != 1 || pat_overloads.len() != 1 {
      return false;
    }

    let check_sig = self.store.signature(check_overloads[0]);
    let pat_sig = self.store.signature(pat_overloads[0]);

    // Return type matching (supports `(...args:any)=>infer R`).
    if !self.match_types(check_sig.ret, pat_sig.ret, depth + 1) {
      return false;
    }

    // Parameter matching.
    //
    // Minimum viable support:
    // - A rest `any` parameter matches any argument list (`ReturnType` pattern).
    // - A rest `infer P` captures a tuple of the check signature's parameters
    //   (`Parameters` pattern).
    if pat_sig.params.len() == 1 && pat_sig.params[0].rest {
      let pat_param = &pat_sig.params[0];

      match self.store.type_kind(pat_param.ty) {
        TypeKind::Infer { .. } => {
          let tuple = self.tuple_from_signature_params(&check_sig.params);
          self.match_types(tuple, pat_param.ty, depth + 1)
        }
        TypeKind::Any | TypeKind::Unknown => true,
        _ => false,
      }
    } else {
      if check_sig.params.len() != pat_sig.params.len() {
        return false;
      }

      for (check_param, pat_param) in check_sig.params.iter().zip(pat_sig.params.iter()) {
        // Ignore parameter names; match just the types.
        if check_param.rest != pat_param.rest {
          return false;
        }
        if !self.match_types(check_param.ty, pat_param.ty, depth + 1) {
          return false;
        }
      }
      true
    }
  }

  fn tuple_from_signature_params(&self, params: &[crate::Param]) -> TypeId {
    let elems: Vec<TupleElem> = params
      .iter()
      .map(|param| TupleElem {
        ty: param.ty,
        optional: param.optional,
        rest: param.rest,
        readonly: false,
      })
      .collect();
    self.store.intern_type(TypeKind::Tuple(elems))
  }

  fn match_array(&mut self, check: TypeId, pat_elem: TypeId, pat_readonly: bool, depth: usize) -> bool {
    let TypeKind::Array {
      ty: check_elem,
      readonly: check_readonly,
    } = self.store.type_kind(check)
    else {
      return false;
    };

    // A readonly pattern accepts both readonly and mutable arrays; a mutable
    // pattern only accepts mutable arrays.
    if !pat_readonly && check_readonly {
      return false;
    }

    self.match_types(check_elem, pat_elem, depth + 1)
  }

  fn match_tuple(&mut self, check: TypeId, pat_elems: &[TupleElem], depth: usize) -> bool {
    let TypeKind::Tuple(check_elems) = self.store.type_kind(check) else {
      return false;
    };

    if check_elems.len() != pat_elems.len() {
      return false;
    }

    for (check_elem, pat_elem) in check_elems.iter().zip(pat_elems.iter()) {
      if check_elem.rest != pat_elem.rest || check_elem.optional != pat_elem.optional {
        return false;
      }
      if !self.match_types(check_elem.ty, pat_elem.ty, depth + 1) {
        return false;
      }
    }

    true
  }

  fn match_ref(&mut self, check: TypeId, pat_def: DefId, pat_args: &[TypeId], depth: usize) -> bool {
    let TypeKind::Ref {
      def: check_def,
      args: check_args,
    } = self.store.type_kind(check)
    else {
      return false;
    };

    if check_def != pat_def || check_args.len() != pat_args.len() {
      return false;
    }

    for (check_arg, pat_arg) in check_args.iter().zip(pat_args.iter()) {
      if !self.match_types(*check_arg, *pat_arg, depth + 1) {
        return false;
      }
    }

    true
  }

  fn match_union(&mut self, check: TypeId, pat_members: &[TypeId], depth: usize) -> bool {
    let TypeKind::Union(check_members) = self.store.type_kind(check) else {
      return false;
    };

    if check_members.len() != pat_members.len() {
      return false;
    }

    for (check_m, pat_m) in check_members.iter().zip(pat_members.iter()) {
      if !self.match_types(*check_m, *pat_m, depth + 1) {
        return false;
      }
    }

    true
  }

  fn match_intersection(&mut self, check: TypeId, pat_members: &[TypeId], depth: usize) -> bool {
    let TypeKind::Intersection(check_members) = self.store.type_kind(check) else {
      return false;
    };

    if check_members.len() != pat_members.len() {
      return false;
    }

    for (check_m, pat_m) in check_members.iter().zip(pat_members.iter()) {
      if !self.match_types(*check_m, *pat_m, depth + 1) {
        return false;
      }
    }

    true
  }
}

/// Attempt to infer `infer` placeholders in a conditional type `extends` operand
/// by structurally matching `check` against the `extends_pattern`.
///
/// Returns `None` if inference fails *or* if the pattern does not contain any
/// `infer` placeholders (i.e. no bindings would be produced).
pub(crate) fn infer_from_extends_pattern(
  store: &TypeStore,
  limits: EvaluatorLimits,
  steps: &mut usize,
  assignability: &dyn ConditionalAssignability,
  check: TypeId,
  extends_pattern: TypeId,
  depth: usize,
) -> Option<Vec<(TypeParamId, TypeId)>> {
  let mut ctx = InferCtx {
    store,
    limits,
    steps,
    assignability,
    visited: AHashSet::new(),
    bindings: BTreeMap::new(),
  };

  if !ctx.match_types(check, extends_pattern, depth) {
    return None;
  }

  if ctx.bindings.is_empty() {
    return None;
  }

  Some(ctx.bindings.into_iter().collect())
}
