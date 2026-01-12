use super::stmt::key_arg;
use super::{Chain, HirSourceToInst, VarType, DUMMY_LABEL};
use crate::il::inst::{
  Arg, BinOp, Const, FieldAccessMeta, FieldRef, Inst, InstTyp, UnOp, ValueTypeSummary,
};
#[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
use crate::il::inst::ArrayChainOpData;
use crate::symbol::semantics::SymbolId;
use crate::unsupported_syntax;
use crate::unsupported_syntax_range;
use crate::OptimizeResult;
use hir_js::{
  AssignOp, BinaryOp, CallExpr, ExprId, ExprKind, MemberExpr, NameId, PatId, UnaryOp, UpdateOp,
};
#[cfg(feature = "semantic-ops")]
use hir_js::ArrayChainOp as HirArrayChainOp;
use num_bigint::BigInt;
use parse_js::loc::Loc;
use parse_js::num::JsNumber;
use std::sync::atomic::Ordering;

#[cfg(feature = "typed")]
use types_ts_interned::{FieldKey, Layout, PtrKind, PropKey};

pub struct CompiledMemberExpr {
  pub left: Arg,
  pub res: Arg,
}

impl<'p> HirSourceToInst<'p> {
  const INTERNAL_IN_CALLEE: &'static str = "__optimize_js_in";
  const INTERNAL_INSTANCEOF_CALLEE: &'static str = "__optimize_js_instanceof";
  const INTERNAL_DELETE_CALLEE: &'static str = "__optimize_js_delete";
  const INTERNAL_NEW_CALLEE: &'static str = "__optimize_js_new";
  const INTERNAL_REGEX_CALLEE: &'static str = "__optimize_js_regex";
  const INTERNAL_ARRAY_CALLEE: &'static str = "__optimize_js_array";
  const INTERNAL_ARRAY_HOLE: &'static str = "__optimize_js_array_hole";
  const INTERNAL_OBJECT_CALLEE: &'static str = "__optimize_js_object";
  const INTERNAL_OBJECT_PROP_MARKER: &'static str = "__optimize_js_object_prop";
  const INTERNAL_OBJECT_COMPUTED_MARKER: &'static str = "__optimize_js_object_prop_computed";
  const INTERNAL_OBJECT_SPREAD_MARKER: &'static str = "__optimize_js_object_spread";
  #[cfg(not(feature = "typed"))]
  const INTERNAL_TEMPLATE_CALLEE: &'static str = "__optimize_js_template";
  const INTERNAL_TAGGED_TEMPLATE_CALLEE: &'static str = "__optimize_js_tagged_template";
  #[cfg(not(feature = "native-async-ops"))]
  const INTERNAL_AWAIT_CALLEE: &'static str = "__optimize_js_await";

  pub fn temp_var_arg(&mut self, f: impl FnOnce(u32) -> Inst) -> Arg {
    let tgt = self.c_temp.bump();
    self.out.push(f(tgt));
    Arg::Var(tgt)
  }

  /// Gets the existing chain or sets one up. This must be called at the beginning of any possible chain node e.g. Call, ComputedMember, Member.
  /// See `Chain` for more details.
  fn maybe_setup_chain(&mut self, chain: impl Into<Option<Chain>>) -> (bool, Chain) {
    match chain.into() {
      Some(chain) => (false, chain),
      None => (
        true,
        Chain {
          is_nullish_label: self.c_label.bump(),
        },
      ),
    }
  }

  /// Jumps to the on-nullish chain label if the `left_arg` value to the left of the operator with `optional_chaining` is null or undefined.
  /// Does nothing if the operator is not `optional_chaining`.
  /// See `Chain` for more details.
  fn conditional_chain_jump(&mut self, optional_chaining: bool, left_arg: &Arg, chain: Chain) {
    if optional_chaining {
      let is_undefined_tmp_var = self.c_temp.bump();
      self.out.push(Inst::bin(
        is_undefined_tmp_var,
        left_arg.clone(),
        BinOp::LooseEq,
        Arg::Const(Const::Null),
      ));
      self.out.push(Inst::cond_goto(
        Arg::Var(is_undefined_tmp_var),
        chain.is_nullish_label,
        DUMMY_LABEL,
      ));
    }
  }

  /// If a chain was set up by the current node, add the jump target and action for on-nullish for the entire chain.
  /// This must be called at the end of any node that called `maybe_setup_chain`.
  /// See `Chain` for more details.
  fn complete_chain_setup(
    &mut self,
    expr_id: ExprId,
    did_chain_setup: bool,
    res_tmp_var: u32,
    chain: Chain,
  ) {
    if did_chain_setup {
      let after_chain_label = self.c_label.bump();
      // This is for when our chain was fully evaluated i.e. there was no short-circuiting due to optional chaining.
      self.out.push(Inst::goto(after_chain_label));
      self.out.push(Inst::label(chain.is_nullish_label));
      self.push_value_inst(
        expr_id,
        Inst::var_assign(res_tmp_var, Arg::Const(Const::Undefined)),
      );
      self.out.push(Inst::goto(after_chain_label));
      self.out.push(Inst::label(after_chain_label));
    }
  }

  fn classify_ident(&self, expr: ExprId, name: NameId) -> VarType {
    let symbol = self.symbol_for_expr(expr);
    let name = self.name_for(name);
    self.classify_symbol(symbol, name)
  }

  fn literal_arg(&mut self, expr_id: ExprId, span: Loc, lit: &hir_js::Literal) -> OptimizeResult<Arg> {
    Ok(match lit {
      hir_js::Literal::Boolean(v) => Arg::Const(Const::Bool(*v)),
      hir_js::Literal::Number(v) => {
        Arg::Const(Const::Num(JsNumber(v.parse::<f64>().unwrap_or_default())))
      }
      hir_js::Literal::String(v) => Arg::Const(Const::Str(v.lossy.clone())),
      hir_js::Literal::Null => Arg::Const(Const::Null),
      hir_js::Literal::Undefined => Arg::Const(Const::Undefined),
      hir_js::Literal::BigInt(v) => {
        let value = BigInt::parse_bytes(v.as_bytes(), 10).ok_or_else(|| {
          unsupported_syntax(
            self.program.lower.hir.file,
            span,
            format!("invalid bigint literal {v:?}"),
          )
        })?;
        Arg::Const(Const::BigInt(value))
      }
      hir_js::Literal::Regex(v) => {
        let tmp = self.c_temp.bump();
        self.push_value_inst(
          expr_id,
          self.call_or_invoke(
            Some(tmp),
            Arg::Builtin(Self::INTERNAL_REGEX_CALLEE.to_string()),
            Arg::Const(Const::Undefined),
            vec![Arg::Const(Const::Str(v.clone()))],
            Vec::new(),
          ),
        );
        Arg::Var(tmp)
      }
    })
  }

  #[cfg(feature = "typed")]
  fn const_member_key(&self, key: &hir_js::ObjectKey) -> Option<String> {
    use hir_js::ObjectKey;
    match key {
      ObjectKey::Ident(name) => Some(self.name_for(*name)),
      ObjectKey::String(value) => Some(value.clone()),
      ObjectKey::Number(value) => Some(value.clone()),
      ObjectKey::Computed(expr) => match &self.body.exprs[expr.0 as usize].kind {
        ExprKind::Literal(lit) => match lit {
          hir_js::Literal::String(value) => Some(value.lossy.clone()),
          hir_js::Literal::Number(value) => Some(value.clone()),
          _ => None,
        },
        _ => None,
      },
    }
  }

  #[cfg(feature = "typed")]
  fn layout_contains_gc_ptr(store: &types_ts_interned::TypeStore, layout: types_ts_interned::LayoutId) -> bool {
    match store.layout(layout) {
      Layout::Scalar { .. } => false,
      Layout::Ptr { to } => matches!(
        to,
        PtrKind::GcObject { .. } | PtrKind::GcArray { .. } | PtrKind::GcString
      ),
      Layout::Struct { fields, .. } => fields
        .iter()
        .any(|field| Self::layout_contains_gc_ptr(store, field.layout)),
      Layout::TaggedUnion { variants, .. } => variants
        .iter()
        .any(|variant| Self::layout_contains_gc_ptr(store, variant.layout)),
    }
  }

  #[cfg(feature = "typed")]
  fn try_resolve_field_access(
    &self,
    receiver_expr: ExprId,
    key: &hir_js::ObjectKey,
  ) -> Option<(FieldRef, FieldAccessMeta)> {
    let program = self.program.types.program.as_ref()?;
    let store = program.interned_type_store();
    let receiver_ty = self.expr_type_id(receiver_expr)?;

    let receiver_layout = store.layout_of(receiver_ty);
    let receiver_payload_layout = match store.layout(receiver_layout) {
      Layout::Ptr {
        to: PtrKind::GcObject { layout },
      } => layout,
      // Arrays are handled separately in the strict-native pipeline; keep the existing
      // GetProp/PropAssign lowering as a fallback here.
      Layout::Ptr {
        to: PtrKind::GcArray { .. },
      } => return None,
      // Tuples/value structs lower directly as struct layouts.
      Layout::Struct { .. } => receiver_layout,
      // Tagged unions (and other layouts) require additional lowering/guarding
      // logic; keep the dynamic form for now.
      _ => return None,
    };

    let Layout::Struct { fields, .. } = store.layout(receiver_payload_layout) else {
      return None;
    };

    let key = self.const_member_key(key)?;
    let key_as_u32 = key.parse::<u32>().ok();
    let key_as_i64 = key.parse::<i64>().ok();

    let field = fields.iter().find(|field| match &field.key {
      FieldKey::TupleIndex(i) => key_as_u32 == Some(*i),
      FieldKey::Prop(prop) => match prop {
        PropKey::String(name) => store.name(*name) == key.as_str(),
        PropKey::Number(n) => key_as_i64 == Some(*n),
        PropKey::Symbol(_) => false,
      },
      FieldKey::Internal(name) => name == &key,
    })?;

    let field_ref = match &field.key {
      FieldKey::TupleIndex(i) => FieldRef::TupleIndex(*i),
      FieldKey::Prop(prop) => match prop {
        PropKey::String(name) => FieldRef::Prop(store.name(*name)),
        PropKey::Number(n) => FieldRef::Prop(n.to_string()),
        // Symbol keys are not representable as constant string keys.
        PropKey::Symbol(_) => return None,
      },
      FieldKey::Internal(name) => FieldRef::Internal(name.clone()),
    };

    let meta = FieldAccessMeta {
      receiver_payload_layout,
      field_layout: field.layout,
      offset: field.offset,
      requires_write_barrier: Self::layout_contains_gc_ptr(store.as_ref(), field.layout),
    };

    Some((field_ref, meta))
  }

  pub fn compile_func(
    &mut self,
    def: hir_js::DefId,
    body: hir_js::BodyId,
    name: Option<NameId>,
  ) -> OptimizeResult<Arg> {
    let _ = def;
    let pg = self.program.clone();
    let id = pg.next_fn_id.fetch_add(1, Ordering::Relaxed);
    let func = crate::compile_hir_body(&pg, body)?;
    if let Some(name) = name {
      let _ = def;
      let _ = name;
    }
    pg.functions.insert(id, func);
    Ok(Arg::Fn(id))
  }

  fn compile_id_expr(&mut self, expr: ExprId, name: NameId) -> OptimizeResult<Arg> {
    Ok(match self.classify_ident(expr, name) {
      VarType::Local(local) => {
        #[cfg(feature = "typed")]
        {
          let sym_tmp = self.symbol_to_temp(local);
          self.temp_var_arg_for_expr(expr, |tgt| {
            let mut inst = Inst::var_assign(tgt, Arg::Var(sym_tmp));
            inst.meta.preserve_var_assign = true;
            inst
          })
        }
        #[cfg(not(feature = "typed"))]
        {
          Arg::Var(self.symbol_to_temp(local))
        }
      }
      VarType::Builtin(builtin) => Arg::Builtin(builtin),
      VarType::Foreign(foreign) => {
        self.temp_var_arg_for_expr(expr, |tgt| Inst::foreign_load(tgt, foreign))
      }
      VarType::Unknown(name) => self.temp_var_arg_for_expr(expr, |tgt| Inst::unknown_load(tgt, name)),
    })
  }

  #[cfg(feature = "typed")]
  fn native_array_elem_layout_for_expr(&mut self, expr: ExprId) -> Option<types_ts_interned::LayoutId> {
    let layout = self.program.types.expr_layout_id(self.body_id, expr)?;
    let program = self.program.types.program.as_ref()?;
    let store = program.interned_type_store();
    match store.layout(layout) {
      types_ts_interned::Layout::Ptr {
        to: types_ts_interned::PtrKind::GcArray { elem },
      } => Some(elem),
      _ => None,
    }
  }

  #[cfg(feature = "typed")]
  fn array_index_arg_from_key(&self, key: &hir_js::ObjectKey, key_arg: &Arg) -> Option<Arg> {
    use parse_js::num::JsNumber;

    fn parse_array_index_str(s: &str) -> Option<u32> {
      let Ok(idx) = s.parse::<u32>() else {
        return None;
      };
      // Array indices are canonical uint32 strings excluding 2^32-1.
      if idx == u32::MAX || idx.to_string() != s {
        return None;
      }
      Some(idx)
    }

    match key_arg {
      Arg::Const(Const::Num(n)) => {
        let value = n.0;
        if value.is_finite()
          && value.fract() == 0.0
          && value >= 0.0
          && value < (u32::MAX as f64)
        {
          Some(Arg::Const(Const::Num(*n)))
        } else {
          None
        }
      }
      Arg::Const(Const::Str(s)) => {
        let idx = parse_array_index_str(s)?;
        Some(Arg::Const(Const::Num(JsNumber(idx as f64))))
      }
      Arg::Var(_) => match key {
        hir_js::ObjectKey::Computed(expr) => {
          let summary = self
            .program
            .types
            .expr_value_type_summary(self.body_id, *expr);
          summary.is_definitely_number().then(|| key_arg.clone())
        }
        _ => None,
      },
      _ => None,
    }
  }

  pub fn compile_assignment(
    &mut self,
    assign_expr_id: ExprId,
    span: Loc,
    operator: AssignOp,
    target: PatId,
    value: ExprId,
  ) -> OptimizeResult<Arg> {
    use hir_js::PatKind;

    let pat = &self.body.pats[target.0 as usize];
    match pat.kind {
      PatKind::Array(_) | PatKind::Object(_) => {
        if operator != AssignOp::Assign {
          return Err(unsupported_syntax_range(
            self.program.lower.hir.file,
            self.body.pats[target.0 as usize].span,
            format!("unsupported destructuring assignment operator {operator:?}"),
          ));
        }
        let value_tmp_var = self.c_temp.bump();
        let value_arg = self.compile_expr(value)?;
        self.push_value_inst(assign_expr_id, Inst::var_assign(value_tmp_var, value_arg));
        self.compile_destructuring(target, Arg::Var(value_tmp_var))?;
        Ok(Arg::Var(value_tmp_var))
      }
      PatKind::Ident(name_id) => {
        let dummy_val = Arg::Const(Const::Num(JsNumber(0xdeadbeefu32 as f64)));
        let var_type = self.classify_symbol(self.symbol_for_pat(target), self.name_for(name_id));
        let mut assign_inst = match var_type {
          VarType::Local(local) => Inst::var_assign(self.symbol_to_temp(local), dummy_val),
          VarType::Foreign(foreign) => Inst::foreign_store(foreign, dummy_val),
          VarType::Unknown(name) => Inst::unknown_store(name, dummy_val),
          VarType::Builtin(builtin) => {
            return Err(unsupported_syntax(
              self.program.lower.hir.file,
              span,
              format!("assignment to builtin {builtin}"),
            ))
          }
        };
        let value_tmp_var = self.c_temp.bump();
        match operator {
          AssignOp::Assign => {
            let value_arg = self.compile_expr(value)?;
            self.push_value_inst(
              assign_expr_id,
              Inst::var_assign(value_tmp_var, value_arg.clone()),
            );
            *assign_inst.args.last_mut().unwrap() = value_arg;
            if assign_inst.t == InstTyp::VarAssign {
              self.push_value_inst(assign_expr_id, assign_inst);
            } else {
              assign_inst.meta.span = Some(self.body.exprs[assign_expr_id.0 as usize].span);
              self.out.push(assign_inst);
            }
            Ok(Arg::Var(value_tmp_var))
          }
          AssignOp::LogicalAndAssign | AssignOp::LogicalOrAssign | AssignOp::NullishAssign => {
            let left_arg = match assign_inst.t {
              InstTyp::VarAssign => Arg::Var(assign_inst.tgts[0]),
              InstTyp::ForeignStore => {
                let left_tmp_var = self.c_temp.bump();
                self
                  .out
                  .push(Inst::foreign_load(left_tmp_var, assign_inst.foreign));
                Arg::Var(left_tmp_var)
              }
              InstTyp::UnknownStore => {
                let left_tmp_var = self.c_temp.bump();
                self.out.push(Inst::unknown_load(
                  left_tmp_var,
                  assign_inst.unknown.clone(),
                ));
                Arg::Var(left_tmp_var)
              }
              _ => {
                return Err(unsupported_syntax(
                  self.program.lower.hir.file,
                  span,
                  "unsupported assignment target",
                ))
              }
            };

            self.push_value_inst(
              assign_expr_id,
              Inst::var_assign(value_tmp_var, left_arg.clone()),
            );
            let converge_label_id = self.c_label.bump();

            match operator {
              AssignOp::LogicalAndAssign => self.out.push(Inst::cond_goto(
                Arg::Var(value_tmp_var),
                DUMMY_LABEL,
                converge_label_id,
              )),
              AssignOp::LogicalOrAssign => self.out.push(Inst::cond_goto(
                Arg::Var(value_tmp_var),
                converge_label_id,
                DUMMY_LABEL,
              )),
              AssignOp::NullishAssign => {
                let is_nullish_tmp_var = self.c_temp.bump();
                self.out.push(Inst::bin(
                  is_nullish_tmp_var,
                  Arg::Var(value_tmp_var),
                  BinOp::LooseEq,
                  Arg::Const(Const::Null),
                ));
                self.out.push(Inst::cond_goto(
                  Arg::Var(is_nullish_tmp_var),
                  DUMMY_LABEL,
                  converge_label_id,
                ));
              }
              _ => unreachable!(),
            }

            let rhs = self.compile_expr(value)?;
            self.push_value_inst(
              assign_expr_id,
              Inst::var_assign(value_tmp_var, rhs.clone()),
            );
            *assign_inst.args.last_mut().unwrap() = rhs;
            if assign_inst.t == InstTyp::VarAssign {
              self.push_value_inst(assign_expr_id, assign_inst);
            } else {
              assign_inst.meta.span = Some(self.body.exprs[assign_expr_id.0 as usize].span);
              self.out.push(assign_inst);
            }
            self.out.push(Inst::label(converge_label_id));

            Ok(Arg::Var(value_tmp_var))
          }
          _ => {
            let value_arg = self.compile_expr(value)?;
            let op = match operator {
              AssignOp::AddAssign => BinOp::Add,
              AssignOp::SubAssign => BinOp::Sub,
              AssignOp::MulAssign => BinOp::Mul,
              AssignOp::DivAssign => BinOp::Div,
              AssignOp::RemAssign => BinOp::Mod,
              AssignOp::ShiftLeftAssign => BinOp::Shl,
              AssignOp::ShiftRightAssign => BinOp::Shr,
              AssignOp::ShiftRightUnsignedAssign => BinOp::UShr,
              AssignOp::BitAndAssign => BinOp::BitAnd,
              AssignOp::BitOrAssign => BinOp::BitOr,
              AssignOp::BitXorAssign => BinOp::BitXor,
              AssignOp::ExponentAssign => BinOp::Exp,
              _ => {
                return Err(unsupported_syntax(
                  self.program.lower.hir.file,
                  span,
                  format!("unsupported assignment operator {operator:?}"),
                ))
              }
            };
            let left_arg = match assign_inst.t {
              InstTyp::VarAssign => Arg::Var(assign_inst.tgts[0]),
              InstTyp::ForeignStore => {
                let left_tmp_var = self.c_temp.bump();
                self
                  .out
                  .push(Inst::foreign_load(left_tmp_var, assign_inst.foreign));
                Arg::Var(left_tmp_var)
              }
              InstTyp::UnknownStore => {
                let left_tmp_var = self.c_temp.bump();
                self.out.push(Inst::unknown_load(
                  left_tmp_var,
                  assign_inst.unknown.clone(),
                ));
                Arg::Var(left_tmp_var)
              }
              _ => {
                return Err(unsupported_syntax(
                  self.program.lower.hir.file,
                  span,
                  "unsupported assignment target",
                ))
              }
            };
            let rhs_inst = Inst::bin(value_tmp_var, left_arg, op, value_arg);
            self.push_value_inst(assign_expr_id, rhs_inst);
            *assign_inst.args.last_mut().unwrap() = Arg::Var(value_tmp_var);
            if assign_inst.t == InstTyp::VarAssign {
              self.push_value_inst(assign_expr_id, assign_inst);
            } else {
              assign_inst.meta.span = Some(self.body.exprs[assign_expr_id.0 as usize].span);
              self.out.push(assign_inst);
            }
            Ok(Arg::Var(value_tmp_var))
          }
        }
      }
      PatKind::AssignTarget(target_expr_id) => {
        let target_expr = &self.body.exprs[target_expr_id.0 as usize];
        let dummy_val = Arg::Const(Const::Num(JsNumber(0xdeadbeefu32 as f64)));
        #[derive(Clone)]
        enum AssignTargetStore {
          Prop { obj: Arg, prop: Arg },
          Array {
            array: Arg,
            index: Arg,
            elem_layout: crate::il::inst::ArrayElemLayoutId,
          },
          Field {
            obj: Arg,
            field: FieldRef,
            meta: FieldAccessMeta,
          },
        }

        let store = match target_expr.kind {
          ExprKind::Member(ref member) => {
            if member.optional {
              return Err(unsupported_syntax(
                self.program.lower.hir.file,
                span,
                "optional chaining in assignment target",
              ));
            }
            let obj = self.compile_expr(member.object)?;
            #[cfg(feature = "typed")]
            let field_access = self.try_resolve_field_access(member.object, &member.property);
            #[cfg(not(feature = "typed"))]
            let field_access: Option<(FieldRef, FieldAccessMeta)> = None;
            if let Some((field, meta)) = field_access {
              AssignTargetStore::Field { obj, field, meta }
            } else {
              let prop = key_arg(self, &member.property)?;
              #[cfg(feature = "typed")]
              {
                if let Some(elem_layout) = self.native_array_elem_layout_for_expr(member.object) {
                  if let Some(index_arg) = self.array_index_arg_from_key(&member.property, &prop) {
                    AssignTargetStore::Array {
                      array: obj,
                      index: index_arg,
                      elem_layout,
                    }
                  } else {
                    AssignTargetStore::Prop { obj, prop }
                  }
                } else {
                  AssignTargetStore::Prop { obj, prop }
                }
              }
              #[cfg(not(feature = "typed"))]
              {
                AssignTargetStore::Prop { obj, prop }
              }
            }
          }
          _ => {
            return Err(unsupported_syntax(
              self.program.lower.hir.file,
              span,
              "unsupported assignment target",
            ))
          }
        };

        let mut assign_inst = match &store {
          AssignTargetStore::Prop { obj, prop } => {
            Inst::prop_assign(obj.clone(), prop.clone(), dummy_val.clone())
          }
          AssignTargetStore::Array {
            array,
            index,
            elem_layout,
          } => Inst::array_store(
            array.clone(),
            index.clone(),
            dummy_val.clone(),
            *elem_layout,
            true,
          ),
          AssignTargetStore::Field { obj, field, meta } => {
            let mut inst = Inst::field_store(obj.clone(), field.clone(), dummy_val.clone());
            inst.meta.field_access = Some(meta.clone());
            inst
          }
        };
        let value_tmp_var = self.c_temp.bump();
        match operator {
          AssignOp::Assign => {
            let value_arg = self.compile_expr(value)?;
            self.push_value_inst(
              assign_expr_id,
              Inst::var_assign(value_tmp_var, value_arg.clone()),
            );
            *assign_inst.args.last_mut().unwrap() = value_arg;
            assign_inst.meta.span = Some(self.body.exprs[assign_expr_id.0 as usize].span);
            self.out.push(assign_inst);
            Ok(Arg::Var(value_tmp_var))
          }
          AssignOp::LogicalAndAssign | AssignOp::LogicalOrAssign | AssignOp::NullishAssign => {
            let left_tmp_var = self.c_temp.bump();
            match &store {
              AssignTargetStore::Prop { obj, prop } => {
                self.out.push(Inst::bin(
                  left_tmp_var,
                  obj.clone(),
                  BinOp::GetProp,
                  prop.clone(),
                ));
              }
              AssignTargetStore::Array {
                array,
                index,
                elem_layout,
              } => {
                self.out.push(Inst::array_load(
                  left_tmp_var,
                  array.clone(),
                  index.clone(),
                  *elem_layout,
                  true,
                ));
              }
              AssignTargetStore::Field { obj, field, meta } => {
                let mut inst = Inst::field_load(left_tmp_var, obj.clone(), field.clone());
                inst.meta.field_access = Some(meta.clone());
                self.out.push(inst);
              }
            }
            self.push_value_inst(
              assign_expr_id,
              Inst::var_assign(value_tmp_var, Arg::Var(left_tmp_var)),
            );

            let converge_label_id = self.c_label.bump();

            match operator {
              AssignOp::LogicalAndAssign => self.out.push(Inst::cond_goto(
                Arg::Var(value_tmp_var),
                DUMMY_LABEL,
                converge_label_id,
              )),
              AssignOp::LogicalOrAssign => self.out.push(Inst::cond_goto(
                Arg::Var(value_tmp_var),
                converge_label_id,
                DUMMY_LABEL,
              )),
              AssignOp::NullishAssign => {
                let is_nullish_tmp_var = self.c_temp.bump();
                self.out.push(Inst::bin(
                  is_nullish_tmp_var,
                  Arg::Var(value_tmp_var),
                  BinOp::LooseEq,
                  Arg::Const(Const::Null),
                ));
                self.out.push(Inst::cond_goto(
                  Arg::Var(is_nullish_tmp_var),
                  DUMMY_LABEL,
                  converge_label_id,
                ));
              }
              _ => unreachable!(),
            }

            let rhs = self.compile_expr(value)?;
            self.push_value_inst(
              assign_expr_id,
              Inst::var_assign(value_tmp_var, rhs.clone()),
            );
            *assign_inst.args.last_mut().unwrap() = rhs;
            assign_inst.meta.span = Some(self.body.exprs[assign_expr_id.0 as usize].span);
            self.out.push(assign_inst);
            self.out.push(Inst::label(converge_label_id));

            Ok(Arg::Var(value_tmp_var))
          }
          _ => {
            let value_arg = self.compile_expr(value)?;
            let op = match operator {
              AssignOp::AddAssign => BinOp::Add,
              AssignOp::SubAssign => BinOp::Sub,
              AssignOp::MulAssign => BinOp::Mul,
              AssignOp::DivAssign => BinOp::Div,
              AssignOp::RemAssign => BinOp::Mod,
              AssignOp::ShiftLeftAssign => BinOp::Shl,
              AssignOp::ShiftRightAssign => BinOp::Shr,
              AssignOp::ShiftRightUnsignedAssign => BinOp::UShr,
              AssignOp::BitAndAssign => BinOp::BitAnd,
              AssignOp::BitOrAssign => BinOp::BitOr,
              AssignOp::BitXorAssign => BinOp::BitXor,
              AssignOp::ExponentAssign => BinOp::Exp,
              _ => {
                return Err(unsupported_syntax(
                  self.program.lower.hir.file,
                  span,
                  format!("unsupported assignment operator {operator:?}"),
                ))
              }
            };
            let left_tmp_var = self.c_temp.bump();
            match &store {
              AssignTargetStore::Prop { obj, prop } => {
                self.out.push(Inst::bin(
                  left_tmp_var,
                  obj.clone(),
                  BinOp::GetProp,
                  prop.clone(),
                ));
              }
              AssignTargetStore::Array {
                array,
                index,
                elem_layout,
              } => {
                self.out.push(Inst::array_load(
                  left_tmp_var,
                  array.clone(),
                  index.clone(),
                  *elem_layout,
                  true,
                ));
              }
              AssignTargetStore::Field { obj, field, meta } => {
                let mut inst = Inst::field_load(left_tmp_var, obj.clone(), field.clone());
                inst.meta.field_access = Some(meta.clone());
                self.out.push(inst);
              }
            }
            let rhs_inst = Inst::bin(value_tmp_var, Arg::Var(left_tmp_var), op, value_arg);
            self.push_value_inst(assign_expr_id, rhs_inst);
            *assign_inst.args.last_mut().unwrap() = Arg::Var(value_tmp_var);
            assign_inst.meta.span = Some(self.body.exprs[assign_expr_id.0 as usize].span);
            self.out.push(assign_inst);
            Ok(Arg::Var(value_tmp_var))
          }
        }
      }
      _ => Err(unsupported_syntax(
        self.program.lower.hir.file,
        span,
        "unsupported assignment target",
      )),
    }
  }

  pub fn compile_logical_expr(
    &mut self,
    expr_id: ExprId,
    span: Loc,
    operator: BinaryOp,
    left: ExprId,
    right: ExprId,
  ) -> OptimizeResult<Arg> {
    let left_truthiness = self.expr_truthiness(left);
    match (operator, left_truthiness) {
      (BinaryOp::LogicalAnd, Some(crate::types::Truthiness::AlwaysTruthy)) => {
        let _ = self.compile_expr(left)?;
        return self.compile_expr(right);
      }
      (BinaryOp::LogicalAnd, Some(crate::types::Truthiness::AlwaysFalsy)) => {
        return self.compile_expr(left);
      }
      (BinaryOp::LogicalOr, Some(crate::types::Truthiness::AlwaysTruthy)) => {
        return self.compile_expr(left);
      }
      (BinaryOp::LogicalOr, Some(crate::types::Truthiness::AlwaysFalsy)) => {
        let _ = self.compile_expr(left)?;
        return self.compile_expr(right);
      }
      _ => {}
    }

    let converge_label_id = self.c_label.bump();
    let res_tmp_var = self.c_temp.bump();
    let left = self.compile_expr(left)?;
    self.push_value_inst(expr_id, Inst::var_assign(res_tmp_var, left.clone()));
    self.out.push(match operator {
      BinaryOp::LogicalAnd => Inst::cond_goto(left, DUMMY_LABEL, converge_label_id),
      BinaryOp::LogicalOr => Inst::cond_goto(left, converge_label_id, DUMMY_LABEL),
      other => {
        return Err(unsupported_syntax(
          self.program.lower.hir.file,
          span,
          format!("unsupported logical operator {other:?}"),
        ))
      }
    });
    let right = self.compile_expr(right)?;
    self.push_value_inst(expr_id, Inst::var_assign(res_tmp_var, right));
    self.out.push(Inst::label(converge_label_id));
    Ok(Arg::Var(res_tmp_var))
  }

  pub fn compile_nullish_coalescing_expr(
    &mut self,
    expr_id: ExprId,
    left: ExprId,
    right: ExprId,
  ) -> OptimizeResult<Arg> {
    if self.expr_excludes_nullish(left) {
      return self.compile_expr(left);
    }

    let converge_label_id = self.c_label.bump();
    let res_tmp_var = self.c_temp.bump();

    let left_arg = self.compile_expr(left)?;
    self.push_value_inst(expr_id, Inst::var_assign(res_tmp_var, left_arg));

    let is_nullish_tmp_var = self.c_temp.bump();
    self.out.push(Inst::bin(
      is_nullish_tmp_var,
      Arg::Var(res_tmp_var),
      BinOp::LooseEq,
      Arg::Const(Const::Null),
    ));
    self.out.push(Inst::cond_goto(
      Arg::Var(is_nullish_tmp_var),
      DUMMY_LABEL,
      converge_label_id,
    ));

    let right_arg = self.compile_expr(right)?;
    self.push_value_inst(expr_id, Inst::var_assign(res_tmp_var, right_arg));
    self.out.push(Inst::label(converge_label_id));

    Ok(Arg::Var(res_tmp_var))
  }

  pub fn compile_comma_expr(&mut self, left: ExprId, right: ExprId) -> OptimizeResult<Arg> {
    let _ = self.compile_expr(left)?;
    self.compile_expr(right)
  }

  fn collect_string_add_chain_parts(&self, expr_id: ExprId, out: &mut Vec<ExprId>) {
    let expr = &self.body.exprs[expr_id.0 as usize];
    if let ExprKind::Binary {
      op: BinaryOp::Add,
      left,
      right,
    } = expr.kind
    {
      let summary = self
        .program
        .types
        .expr_value_type_summary(self.body_id, expr_id);
      // Only flatten chains that are *definitely* string concatenation. This
      // keeps us conservative around mixed numeric/string `+` (e.g. `1 + 2 +
      // "x"`) where earlier `+` operations may be numeric addition.
      if summary.is_definitely_string() {
        self.collect_string_add_chain_parts(left, out);
        self.collect_string_add_chain_parts(right, out);
        return;
      }
    }
    out.push(expr_id);
  }

  pub fn compile_binary_expr(
    &mut self,
    expr_id: ExprId,
    span: Loc,
    operator: BinaryOp,
    left: ExprId,
    right: ExprId,
  ) -> OptimizeResult<Arg> {
    if matches!(operator, BinaryOp::LogicalAnd | BinaryOp::LogicalOr) {
      return self.compile_logical_expr(expr_id, span, operator, left, right);
    }
    if operator == BinaryOp::NullishCoalescing {
      return self.compile_nullish_coalescing_expr(expr_id, left, right);
    }
    if operator == BinaryOp::Comma {
      return self.compile_comma_expr(left, right);
    }
    if matches!(operator, BinaryOp::In | BinaryOp::Instanceof) {
      let left = self.compile_expr(left)?;
      let right = self.compile_expr(right)?;
      let res_tmp_var = self.c_temp.bump();
      let callee = match operator {
        BinaryOp::In => Self::INTERNAL_IN_CALLEE,
        BinaryOp::Instanceof => Self::INTERNAL_INSTANCEOF_CALLEE,
        _ => unreachable!(),
      };
      self.push_value_inst(
        expr_id,
        self.call_or_invoke(
          Some(res_tmp_var),
          Arg::Builtin(callee.to_string()),
          Arg::Const(Const::Undefined),
          vec![left, right],
          Vec::new(),
        ),
      );
      return Ok(Arg::Var(res_tmp_var));
    }

    if operator == BinaryOp::Add {
      let summary = self
        .program
        .types
        .expr_value_type_summary(self.body_id, expr_id);
      // Typed string `+` chains are lowered to a single `StringConcat` so that
      // downstream encoding/native backends can see the full concatenation
      // structure.
      if summary.is_definitely_string() {
        let mut parts_exprs = Vec::new();
        self.collect_string_add_chain_parts(expr_id, &mut parts_exprs);
        // `collect_string_add_chain_parts` always pushes at least one element,
        // but we only care about actual `+` expressions.
        if parts_exprs.len() >= 2 {
          let mut parts = Vec::with_capacity(parts_exprs.len());
          for part_expr in parts_exprs {
            parts.push(self.compile_expr(part_expr)?);
          }
          let res_tmp_var = self.c_temp.bump();
          self.push_value_inst(expr_id, Inst::string_concat(res_tmp_var, parts));
          return Ok(Arg::Var(res_tmp_var));
        }
      }
    }

    let left_expr = &self.body.exprs[left.0 as usize];
    let right_expr = &self.body.exprs[right.0 as usize];
    let is_nullish = |expr_id: ExprId, expr: &hir_js::Expr| match expr.kind {
      ExprKind::Literal(hir_js::Literal::Null | hir_js::Literal::Undefined) => true,
      ExprKind::Unary {
        op: UnaryOp::Void, ..
      } => true,
      ExprKind::Ident(name) => {
        self.symbol_for_expr(expr_id).is_none()
          && self.program.names.resolve(name) == Some("undefined")
      }
      _ => false,
    };
    let left_nullish = is_nullish(left, left_expr);
    let right_nullish = is_nullish(right, right_expr);
    let typed_non_nullish_loose_eq_op =
      if matches!(operator, BinaryOp::Equality | BinaryOp::Inequality)
        && !left_nullish
        && !right_nullish
      {
        let left_tag = self.typeof_string_expr(left);
        let right_tag = self.typeof_string_expr(right);
        match (left_tag, right_tag) {
          (Some(tag), Some(other_tag)) if tag == other_tag => {
            if tag == "object"
              && !(self.expr_excludes_nullish(left) && self.expr_excludes_nullish(right))
            {
              None
            } else {
              Some(if operator == BinaryOp::Equality {
                BinOp::StrictEq
              } else {
                BinOp::NotStrictEq
              })
            }
          }
          _ => None,
        }
      } else {
        None
      };

    if matches!(
      operator,
      BinaryOp::StrictEquality
        | BinaryOp::StrictInequality
        | BinaryOp::Equality
        | BinaryOp::Inequality
    ) {
      if (left_nullish && self.expr_excludes_nullish(right))
        || (right_nullish && self.expr_excludes_nullish(left))
      {
        let _ = self.compile_expr(left)?;
        let _ = self.compile_expr(right)?;
        let is_inequality = matches!(operator, BinaryOp::StrictInequality | BinaryOp::Inequality);
        return Ok(Arg::Const(Const::Bool(is_inequality)));
      }

      let typeof_left = match left_expr.kind {
        ExprKind::Unary {
          op: UnaryOp::Typeof,
          expr,
        } => Some((left, expr)),
        _ => None,
      };
      let typeof_right = match right_expr.kind {
        ExprKind::Unary {
          op: UnaryOp::Typeof,
          expr,
        } => Some((right, expr)),
        _ => None,
      };

      // Type-driven folding for `typeof` equality/inequality is valid when the
      // comparison is effectively strict. This includes `==`/`!=` when both
      // operands are known to have the same `typeof` tag (e.g. string results).
      if matches!(
        operator,
        BinaryOp::StrictEquality
          | BinaryOp::StrictInequality
          | BinaryOp::Equality
          | BinaryOp::Inequality
      ) {
        if let Some(((typeof_expr, typeof_operand), typeof_on_left)) =
          match (typeof_left, typeof_right) {
            (Some((expr, operand)), None) => Some(((expr, operand), true)),
            (None, Some((expr, operand))) => Some(((expr, operand), false)),
            _ => None,
          }
        {
          let literal = if typeof_on_left {
            match &right_expr.kind {
              ExprKind::Literal(hir_js::Literal::String(value)) => Some(value.lossy.as_str()),
              _ => None,
            }
          } else {
            match &left_expr.kind {
              ExprKind::Literal(hir_js::Literal::String(value)) => Some(value.lossy.as_str()),
              _ => None,
            }
          };
          if let Some(literal) = literal {
            if let Some(known) = self.typeof_string_expr(typeof_operand) {
              let _ = self.compile_expr(typeof_expr)?;
              let eq = known == literal;
              let value = if operator == BinaryOp::StrictEquality {
                eq
              } else {
                !eq
              };
              return Ok(Arg::Const(Const::Bool(value)));
            }
          }
        }
      }
    }

    let op = match operator {
      BinaryOp::Add => BinOp::Add,
      BinaryOp::BitAnd => BinOp::BitAnd,
      BinaryOp::BitOr => BinOp::BitOr,
      BinaryOp::BitXor => BinOp::BitXor,
      BinaryOp::Divide => BinOp::Div,
      BinaryOp::LessThan => BinOp::Lt,
      BinaryOp::LessEqual => BinOp::Leq,
      BinaryOp::Multiply => BinOp::Mul,
      BinaryOp::Remainder => BinOp::Mod,
      BinaryOp::Exponent => BinOp::Exp,
      BinaryOp::ShiftLeft => BinOp::Shl,
      BinaryOp::ShiftRight => BinOp::Shr,
      BinaryOp::ShiftRightUnsigned => BinOp::UShr,
      BinaryOp::StrictEquality => BinOp::StrictEq,
      BinaryOp::StrictInequality => BinOp::NotStrictEq,
      BinaryOp::Subtract => BinOp::Sub,
      BinaryOp::GreaterThan => BinOp::Gt,
      BinaryOp::GreaterEqual => BinOp::Geq,
      BinaryOp::Equality if left_nullish || right_nullish => BinOp::LooseEq,
      BinaryOp::Inequality if left_nullish || right_nullish => BinOp::NotLooseEq,
      BinaryOp::Equality | BinaryOp::Inequality => {
        if let Some(op) = typed_non_nullish_loose_eq_op {
          op
        } else {
          return Err(unsupported_syntax(
            self.program.lower.hir.file,
            span,
            format!("unsupported binary operator {operator:?}"),
          ));
        }
      }
      _ => {
        return Err(unsupported_syntax(
          self.program.lower.hir.file,
          span,
          format!("unsupported binary operator {operator:?}"),
        ))
      }
    };
    let left = self.compile_expr(left)?;
    let right = self.compile_expr(right)?;
    let res_tmp_var = self.c_temp.bump();
    self.push_value_inst(expr_id, Inst::bin(res_tmp_var, left, op, right));
    Ok(Arg::Var(res_tmp_var))
  }

  pub fn compile_cond_expr(
    &mut self,
    expr_id: ExprId,
    test: ExprId,
    consequent: ExprId,
    alternate: ExprId,
  ) -> OptimizeResult<Arg> {
    let known = self.expr_truthiness(test);
    let test_arg = self.compile_expr(test)?;
    if let Some(truthiness) = known {
      return match truthiness {
        crate::types::Truthiness::AlwaysTruthy => self.compile_expr(consequent),
        crate::types::Truthiness::AlwaysFalsy => self.compile_expr(alternate),
      };
    }
    let res_tmp_var = self.c_temp.bump();
    let cons_label_id = self.c_label.bump();
    let after_label_id = self.c_label.bump();
    self
      .out
      .push(Inst::cond_goto(test_arg, cons_label_id, DUMMY_LABEL));
    let alt_res = self.compile_expr(alternate)?;
    self.push_value_inst(expr_id, Inst::var_assign(res_tmp_var, alt_res));
    self.out.push(Inst::goto(after_label_id));
    self.out.push(Inst::label(cons_label_id));
    let cons_res = self.compile_expr(consequent)?;
    self.push_value_inst(expr_id, Inst::var_assign(res_tmp_var, cons_res));
    self.out.push(Inst::label(after_label_id));
    Ok(Arg::Var(res_tmp_var))
  }

  pub fn compile_update_expr(
    &mut self,
    expr_id: ExprId,
    span: Loc,
    operator: UpdateOp,
    argument: ExprId,
    prefix: bool,
  ) -> OptimizeResult<Arg> {
    let rhs = match operator {
      UpdateOp::Decrement => BinOp::Sub,
      UpdateOp::Increment => BinOp::Add,
    };

    let operand_summary = self
      .program
      .types
      .expr_value_type_summary(self.body_id, argument);
    let numeric_mode = if operand_summary == ValueTypeSummary::BIGINT {
      // Statically-known BigInt updates can use BigInt arithmetic directly.
      ValueTypeSummary::BIGINT
    } else if operand_summary == ValueTypeSummary::NUMBER {
      // Statically-known number updates can use the lightweight lowering.
      ValueTypeSummary::NUMBER
    } else {
      ValueTypeSummary::UNKNOWN
    };

    let one_num = Arg::Const(Const::Num(JsNumber(1.0)));
    let one_bigint = Arg::Const(Const::BigInt(BigInt::from(1)));

    #[derive(Clone, Debug)]
    enum UpdateStore {
      Local { tgt: u32 },
      Foreign { foreign: SymbolId },
      Unknown { name: String },
      Member { obj: Arg, prop: Arg },
      Field {
        obj: Arg,
        field: FieldRef,
        meta: FieldAccessMeta,
      },
    }

    fn emit_store(
      compiler: &mut HirSourceToInst<'_>,
      expr_id: ExprId,
      store: &UpdateStore,
      new_var: u32,
    ) {
      match store {
        UpdateStore::Local { tgt } => compiler.push_value_inst(
          expr_id,
          Inst::var_assign(*tgt, Arg::Var(new_var)),
        ),
        UpdateStore::Foreign { foreign } => {
          compiler
            .out
            .push(Inst::foreign_store(*foreign, Arg::Var(new_var)));
        }
        UpdateStore::Unknown { name } => {
          compiler
            .out
            .push(Inst::unknown_store(name.clone(), Arg::Var(new_var)));
        }
        UpdateStore::Member { obj, prop } => {
          compiler.out.push(Inst::prop_assign(
            obj.clone(),
            prop.clone(),
            Arg::Var(new_var),
          ));
        }
        UpdateStore::Field { obj, field, meta } => {
          let mut inst = Inst::field_store(obj.clone(), field.clone(), Arg::Var(new_var));
          inst.meta.field_access = Some(meta.clone());
          compiler.out.push(inst);
        }
      }
    }

    fn compile_dynamic_update(
      compiler: &mut HirSourceToInst<'_>,
      expr_id: ExprId,
      rhs: BinOp,
      prefix: bool,
      raw_var: u32,
      store: &UpdateStore,
      one_num: &Arg,
      one_bigint: &Arg,
    ) -> OptimizeResult<Arg> {
      fn ensure_primitive_or_throw(
        compiler: &mut HirSourceToInst<'_>,
        value_var: u32,
        ok_label: u32,
        throw_label: u32,
      ) {
        // Null is primitive but `typeof null === "object"`, so it needs a dedicated fast path.
        let is_null_tmp = compiler.c_temp.bump();
        compiler.out.push(Inst::bin(
          is_null_tmp,
          Arg::Var(value_var),
          BinOp::StrictEq,
          Arg::Const(Const::Null),
        ));
        compiler
          .out
          .push(Inst::cond_goto(Arg::Var(is_null_tmp), ok_label, DUMMY_LABEL));

        let typeof_tmp = compiler.c_temp.bump();
        compiler
          .out
          .push(Inst::un(typeof_tmp, UnOp::Typeof, Arg::Var(value_var)));
        let is_object_tmp = compiler.c_temp.bump();
        compiler.out.push(Inst::bin(
          is_object_tmp,
          Arg::Var(typeof_tmp),
          BinOp::StrictEq,
          Arg::Const(Const::Str("object".to_string())),
        ));
        compiler.out.push(Inst::cond_goto(
          Arg::Var(is_object_tmp),
          throw_label,
          DUMMY_LABEL,
        ));

        let is_function_tmp = compiler.c_temp.bump();
        compiler.out.push(Inst::bin(
          is_function_tmp,
          Arg::Var(typeof_tmp),
          BinOp::StrictEq,
          Arg::Const(Const::Str("function".to_string())),
        ));
        compiler.out.push(Inst::cond_goto(
          Arg::Var(is_function_tmp),
          throw_label,
          ok_label,
        ));
      }

      fn emit_throw_type_error(compiler: &mut HirSourceToInst<'_>) {
        let err_var = compiler.c_temp.bump();
        compiler.out.push(compiler.call_or_invoke(
          Some(err_var),
          Arg::Builtin("TypeError".to_string()),
          Arg::Const(Const::Undefined),
          vec![Arg::Const(Const::Str(
            "Cannot convert object to primitive value".to_string(),
          ))],
          Vec::new(),
        ));
        compiler
          .out
          .push(compiler.throw_or_throw_to(Arg::Var(err_var)));
      }

      // `++`/`--` perform `ToNumeric` on the operand:
      // - `ToPrimitive` with hint Number
      // - if result is BigInt, use BigInt arithmetic
      // - otherwise, `ToNumber` and use number arithmetic
      //
      // We lower this with a small runtime check. The fast-path lowering for known-number/known-bigint
      // stays above this.
      let bigint_label = compiler.c_label.bump();
      let after_label = compiler.c_label.bump();
      let object_label = compiler.c_label.bump();
      let after_to_primitive_label = compiler.c_label.bump();
      let throw_label = compiler.c_label.bump();
      let after_numeric_label = compiler.c_label.bump();

      let result_var = compiler.c_temp.bump();
      let prim_var = compiler.c_temp.bump();
      compiler
        .out
        .push(Inst::var_assign(prim_var, Arg::Var(raw_var)));

      // If `raw` is a non-null object or function, we must run `ToPrimitive(raw, Number)`.
      // Otherwise the primitive is the value itself.
      let raw_is_null_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        raw_is_null_tmp,
        Arg::Var(raw_var),
        BinOp::StrictEq,
        Arg::Const(Const::Null),
      ));
      compiler.out.push(Inst::cond_goto(
        Arg::Var(raw_is_null_tmp),
        after_to_primitive_label,
        DUMMY_LABEL,
      ));

      let typeof_raw_tmp = compiler.c_temp.bump();
      compiler
        .out
        .push(Inst::un(typeof_raw_tmp, UnOp::Typeof, Arg::Var(raw_var)));

      let raw_is_object_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        raw_is_object_tmp,
        Arg::Var(typeof_raw_tmp),
        BinOp::StrictEq,
        Arg::Const(Const::Str("object".to_string())),
      ));
      compiler.out.push(Inst::cond_goto(
        Arg::Var(raw_is_object_tmp),
        object_label,
        DUMMY_LABEL,
      ));

      let raw_is_function_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        raw_is_function_tmp,
        Arg::Var(typeof_raw_tmp),
        BinOp::StrictEq,
        Arg::Const(Const::Str("function".to_string())),
      ));
      compiler.out.push(Inst::cond_goto(
        Arg::Var(raw_is_function_tmp),
        object_label,
        after_to_primitive_label,
      ));

      // `ToPrimitive(raw, Number)` path.
      compiler.out.push(Inst::label(object_label));
      let fallback_label = compiler.c_label.bump();
      let symbol_to_primitive = Arg::Builtin("Symbol.toPrimitive".to_string());
      let exotic_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        exotic_tmp,
        Arg::Var(raw_var),
        BinOp::GetProp,
        symbol_to_primitive,
      ));
      let exotic_is_undefined_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        exotic_is_undefined_tmp,
        Arg::Var(exotic_tmp),
        BinOp::StrictEq,
        Arg::Const(Const::Undefined),
      ));
      compiler.out.push(Inst::cond_goto(
        Arg::Var(exotic_is_undefined_tmp),
        fallback_label,
        DUMMY_LABEL,
      ));

      // If `@@toPrimitive` exists, call it as `exotic.call(raw, "number")`.
      compiler.out.push(compiler.call_or_invoke(
        Some(prim_var),
        Arg::Var(exotic_tmp),
        Arg::Var(raw_var),
        vec![Arg::Const(Const::Str("number".to_string()))],
        Vec::new(),
      ));
      ensure_primitive_or_throw(compiler, prim_var, after_numeric_label, throw_label);

      // OrdinaryToPrimitive(raw, Number): try `valueOf` then `toString`.
      compiler.out.push(Inst::label(fallback_label));

      let value_of_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        value_of_tmp,
        Arg::Var(raw_var),
        BinOp::GetProp,
        Arg::Const(Const::Str("valueOf".to_string())),
      ));
      let typeof_value_of_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::un(
        typeof_value_of_tmp,
        UnOp::Typeof,
        Arg::Var(value_of_tmp),
      ));
      let value_of_is_function_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        value_of_is_function_tmp,
        Arg::Var(typeof_value_of_tmp),
        BinOp::StrictEq,
        Arg::Const(Const::Str("function".to_string())),
      ));

      let value_of_call_label = compiler.c_label.bump();
      let after_value_of_call_label = compiler.c_label.bump();
      let value_of_res_tmp = compiler.c_temp.bump();
      compiler
        .out
        .push(Inst::var_assign(value_of_res_tmp, Arg::Var(raw_var)));
      compiler.out.push(Inst::cond_goto(
        Arg::Var(value_of_is_function_tmp),
        value_of_call_label,
        after_value_of_call_label,
      ));
      compiler.out.push(Inst::label(value_of_call_label));
      compiler.out.push(compiler.call_or_invoke(
        Some(value_of_res_tmp),
        Arg::Var(value_of_tmp),
        Arg::Var(raw_var),
        Vec::new(),
        Vec::new(),
      ));
      compiler.out.push(Inst::goto(after_value_of_call_label));
      compiler.out.push(Inst::label(after_value_of_call_label));

      let value_of_prim_label = compiler.c_label.bump();
      let to_string_label = compiler.c_label.bump();
      let value_of_is_null_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        value_of_is_null_tmp,
        Arg::Var(value_of_res_tmp),
        BinOp::StrictEq,
        Arg::Const(Const::Null),
      ));
      compiler.out.push(Inst::cond_goto(
        Arg::Var(value_of_is_null_tmp),
        value_of_prim_label,
        DUMMY_LABEL,
      ));
      let typeof_value_of_res_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::un(
        typeof_value_of_res_tmp,
        UnOp::Typeof,
        Arg::Var(value_of_res_tmp),
      ));
      let value_of_res_is_object_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        value_of_res_is_object_tmp,
        Arg::Var(typeof_value_of_res_tmp),
        BinOp::StrictEq,
        Arg::Const(Const::Str("object".to_string())),
      ));
      compiler.out.push(Inst::cond_goto(
        Arg::Var(value_of_res_is_object_tmp),
        to_string_label,
        DUMMY_LABEL,
      ));
      let value_of_res_is_function_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        value_of_res_is_function_tmp,
        Arg::Var(typeof_value_of_res_tmp),
        BinOp::StrictEq,
        Arg::Const(Const::Str("function".to_string())),
      ));
      compiler.out.push(Inst::cond_goto(
        Arg::Var(value_of_res_is_function_tmp),
        to_string_label,
        value_of_prim_label,
      ));

      // Primitive result from valueOf.
      compiler.out.push(Inst::label(value_of_prim_label));
      compiler
        .out
        .push(Inst::var_assign(prim_var, Arg::Var(value_of_res_tmp)));
      compiler.out.push(Inst::goto(after_numeric_label));

      // Try toString.
      compiler.out.push(Inst::label(to_string_label));
      let to_string_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        to_string_tmp,
        Arg::Var(raw_var),
        BinOp::GetProp,
        Arg::Const(Const::Str("toString".to_string())),
      ));
      let typeof_to_string_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::un(
        typeof_to_string_tmp,
        UnOp::Typeof,
        Arg::Var(to_string_tmp),
      ));
      let to_string_is_function_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        to_string_is_function_tmp,
        Arg::Var(typeof_to_string_tmp),
        BinOp::StrictEq,
        Arg::Const(Const::Str("function".to_string())),
      ));
      compiler.out.push(Inst::cond_goto(
        Arg::Var(to_string_is_function_tmp),
        DUMMY_LABEL,
        throw_label,
      ));

      let to_string_res_tmp = compiler.c_temp.bump();
      compiler.out.push(compiler.call_or_invoke(
        Some(to_string_res_tmp),
        Arg::Var(to_string_tmp),
        Arg::Var(raw_var),
        Vec::new(),
        Vec::new(),
      ));
      compiler
        .out
        .push(Inst::var_assign(prim_var, Arg::Var(to_string_res_tmp)));
      ensure_primitive_or_throw(compiler, prim_var, after_numeric_label, throw_label);

      // TypeError for `ToPrimitive` results that are not primitive values.
      compiler.out.push(Inst::label(throw_label));
      emit_throw_type_error(compiler);

      compiler.out.push(Inst::label(after_numeric_label));
      compiler.out.push(Inst::goto(after_to_primitive_label));

      // Shared continuation after `ToPrimitive`.
      compiler
        .out
        .push(Inst::label(after_to_primitive_label));

      // `ToNumeric`: BigInt stays BigInt; everything else coerces to number.
      let typeof_prim_tmp = compiler.c_temp.bump();
      compiler
        .out
        .push(Inst::un(typeof_prim_tmp, UnOp::Typeof, Arg::Var(prim_var)));
      let is_bigint_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        is_bigint_tmp,
        Arg::Var(typeof_prim_tmp),
        BinOp::StrictEq,
        Arg::Const(Const::Str("bigint".to_string())),
      ));
      compiler.out.push(Inst::cond_goto(
        Arg::Var(is_bigint_tmp),
        bigint_label,
        DUMMY_LABEL,
      ));

      // Number path (fallthrough): old = +prim; new = old (+|-) 1
      let old_num_tmp = compiler.c_temp.bump();
      compiler
        .out
        .push(Inst::un(old_num_tmp, UnOp::Plus, Arg::Var(prim_var)));
      let new_num_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        new_num_tmp,
        Arg::Var(old_num_tmp),
        rhs,
        one_num.clone(),
      ));
      if prefix {
        compiler.push_value_inst(expr_id, Inst::var_assign(result_var, Arg::Var(new_num_tmp)));
      } else {
        compiler.push_value_inst(expr_id, Inst::var_assign(result_var, Arg::Var(old_num_tmp)));
      }
      emit_store(compiler, expr_id, store, new_num_tmp);
      compiler.out.push(Inst::goto(after_label));

      // BigInt path: old = prim; new = old (+|-) 1n
      compiler.out.push(Inst::label(bigint_label));
      let new_big_tmp = compiler.c_temp.bump();
      compiler.out.push(Inst::bin(
        new_big_tmp,
        Arg::Var(prim_var),
        rhs,
        one_bigint.clone(),
      ));
      if prefix {
        compiler.push_value_inst(expr_id, Inst::var_assign(result_var, Arg::Var(new_big_tmp)));
      } else {
        compiler.push_value_inst(expr_id, Inst::var_assign(result_var, Arg::Var(prim_var)));
      }
      emit_store(compiler, expr_id, store, new_big_tmp);

      compiler.out.push(Inst::label(after_label));
      Ok(Arg::Var(result_var))
    }

    match &self.body.exprs[argument.0 as usize].kind {
      ExprKind::Ident(name) => {
        let var_type = self.classify_ident(argument, *name);
        match var_type {
          VarType::Builtin(builtin) => Err(unsupported_syntax(
            self.program.lower.hir.file,
            span,
            format!("assignment to builtin {builtin}"),
          )),
          VarType::Local(local) => {
            let arg = self.compile_expr(argument)?;
            // In typed builds, local identifier reads materialize as a `VarAssign` copy so we can
            // attach per-expression type metadata. For update expressions (e.g. `x++`) the operand
            // is also the assignment target, so we must ensure we mutate the original variable
            // rather than the typed identifier-read temporary.
            let update_tgt = self.symbol_to_temp(local);
            match numeric_mode {
              ValueTypeSummary::NUMBER => {
                let rhs_one = one_num.clone();
                if prefix {
                  self.push_value_inst(expr_id, Inst::bin(update_tgt, arg, rhs, rhs_one));
                  Ok(Arg::Var(update_tgt))
                } else {
                  let tmp_var = self.c_temp.bump();
                  self.push_value_inst(expr_id, Inst::var_assign(tmp_var, arg.clone()));
                  self.push_value_inst(expr_id, Inst::bin(update_tgt, arg, rhs, rhs_one));
                  Ok(Arg::Var(tmp_var))
                }
              }
              ValueTypeSummary::BIGINT => {
                let rhs_one = one_bigint.clone();
                if prefix {
                  self.push_value_inst(expr_id, Inst::bin(update_tgt, arg, rhs, rhs_one));
                  Ok(Arg::Var(update_tgt))
                } else {
                  let tmp_var = self.c_temp.bump();
                  self.push_value_inst(expr_id, Inst::var_assign(tmp_var, arg.clone()));
                  self.push_value_inst(expr_id, Inst::bin(update_tgt, arg, rhs, rhs_one));
                  Ok(Arg::Var(tmp_var))
                }
              }
              _ => {
                let raw_var = arg.to_var();
                let store = UpdateStore::Local { tgt: update_tgt };
                compile_dynamic_update(
                  self,
                  expr_id,
                  rhs,
                  prefix,
                  raw_var,
                  &store,
                  &one_num,
                  &one_bigint,
                )
              }
            }
          }
          VarType::Foreign(foreign) => {
            let arg = self.compile_expr(argument)?;
            match numeric_mode {
              ValueTypeSummary::NUMBER | ValueTypeSummary::BIGINT => {
                let rhs_one = if numeric_mode == ValueTypeSummary::BIGINT {
                  one_bigint.clone()
                } else {
                  one_num.clone()
                };
                if prefix {
                  let new_var = self.c_temp.bump();
                  self.push_value_inst(expr_id, Inst::bin(new_var, arg, rhs, rhs_one));
                  self.out.push(Inst::foreign_store(foreign, Arg::Var(new_var)));
                  Ok(Arg::Var(new_var))
                } else {
                  let tmp_var = self.c_temp.bump();
                  self.push_value_inst(expr_id, Inst::var_assign(tmp_var, arg.clone()));
                  let new_var = self.c_temp.bump();
                  self.push_value_inst(expr_id, Inst::bin(new_var, arg, rhs, rhs_one));
                  self.out.push(Inst::foreign_store(foreign, Arg::Var(new_var)));
                  Ok(Arg::Var(tmp_var))
                }
              }
              _ => {
                let raw_var = arg.to_var();
                let store = UpdateStore::Foreign { foreign };
                compile_dynamic_update(
                  self,
                  expr_id,
                  rhs,
                  prefix,
                  raw_var,
                  &store,
                  &one_num,
                  &one_bigint,
                )
              }
            }
          }
          VarType::Unknown(name) => {
            let arg = self.compile_expr(argument)?;
            match numeric_mode {
              ValueTypeSummary::NUMBER | ValueTypeSummary::BIGINT => {
                let rhs_one = if numeric_mode == ValueTypeSummary::BIGINT {
                  one_bigint.clone()
                } else {
                  one_num.clone()
                };
                if prefix {
                  let new_var = self.c_temp.bump();
                  self.push_value_inst(expr_id, Inst::bin(new_var, arg, rhs, rhs_one));
                  self.out.push(Inst::unknown_store(name, Arg::Var(new_var)));
                  Ok(Arg::Var(new_var))
                } else {
                  let tmp_var = self.c_temp.bump();
                  self.push_value_inst(expr_id, Inst::var_assign(tmp_var, arg.clone()));
                  let new_var = self.c_temp.bump();
                  self.push_value_inst(expr_id, Inst::bin(new_var, arg, rhs, rhs_one));
                  self.out.push(Inst::unknown_store(name, Arg::Var(new_var)));
                  Ok(Arg::Var(tmp_var))
                }
              }
              _ => {
                let raw_var = arg.to_var();
                let store = UpdateStore::Unknown { name };
                compile_dynamic_update(
                  self,
                  expr_id,
                  rhs,
                  prefix,
                  raw_var,
                  &store,
                  &one_num,
                  &one_bigint,
                )
              }
            }
          }
        }
      }
      ExprKind::Member(member) => {
        if member.optional {
          return Err(unsupported_syntax(
            self.program.lower.hir.file,
            span,
            "optional chaining in update operand",
          ));
        }
        let obj_arg = self.compile_expr(member.object)?;
        #[cfg(feature = "typed")]
        let field_access = self.try_resolve_field_access(member.object, &member.property);
        #[cfg(not(feature = "typed"))]
        let field_access: Option<(FieldRef, FieldAccessMeta)> = None;
        let prop_arg = if field_access.is_none() {
          Some(key_arg(self, &member.property)?)
        } else {
          None
        };

        // Load the existing property value once.
        let old_var = self.c_temp.bump();
        if let Some((field, meta)) = field_access.as_ref() {
          let mut inst = Inst::field_load(old_var, obj_arg.clone(), field.clone());
          inst.meta.field_access = Some(meta.clone());
          self.push_value_inst(argument, inst);
        } else {
          let prop_arg = prop_arg.as_ref().expect("expected property arg for member update");
          self.push_value_inst(
            argument,
            Inst::bin(old_var, obj_arg.clone(), BinOp::GetProp, prop_arg.clone()),
          );
        }

        match numeric_mode {
          ValueTypeSummary::NUMBER | ValueTypeSummary::BIGINT => {
            let rhs_one = if numeric_mode == ValueTypeSummary::BIGINT {
              one_bigint.clone()
            } else {
              one_num.clone()
            };
            if prefix {
              let new_var = self.c_temp.bump();
              self.push_value_inst(
                expr_id,
                Inst::bin(new_var, Arg::Var(old_var), rhs, rhs_one),
              );
              if let Some((field, meta)) = field_access.as_ref() {
                let mut inst = Inst::field_store(obj_arg, field.clone(), Arg::Var(new_var));
                inst.meta.field_access = Some(meta.clone());
                self.out.push(inst);
              } else {
                let prop_arg = prop_arg.expect("expected property arg for member update");
                self
                  .out
                  .push(Inst::prop_assign(obj_arg, prop_arg, Arg::Var(new_var)));
              }
              Ok(Arg::Var(new_var))
            } else {
              let tmp_var = self.c_temp.bump();
              self.push_value_inst(expr_id, Inst::var_assign(tmp_var, Arg::Var(old_var)));
              let new_var = self.c_temp.bump();
              self.push_value_inst(
                expr_id,
                Inst::bin(new_var, Arg::Var(old_var), rhs, rhs_one),
              );
              if let Some((field, meta)) = field_access.as_ref() {
                let mut inst = Inst::field_store(obj_arg, field.clone(), Arg::Var(new_var));
                inst.meta.field_access = Some(meta.clone());
                self.out.push(inst);
              } else {
                let prop_arg = prop_arg.expect("expected property arg for member update");
                self
                  .out
                  .push(Inst::prop_assign(obj_arg, prop_arg, Arg::Var(new_var)));
              }
              Ok(Arg::Var(tmp_var))
            }
          }
          _ => {
            let store = if let Some((field, meta)) = field_access {
              UpdateStore::Field {
                obj: obj_arg,
                field,
                meta,
              }
            } else {
              UpdateStore::Member {
                obj: obj_arg,
                prop: prop_arg.expect("expected property arg for member update"),
              }
            };
            compile_dynamic_update(
              self,
              expr_id,
              rhs,
              prefix,
              old_var,
              &store,
              &one_num,
              &one_bigint,
            )
          }
        }
      }
      _ => Err(unsupported_syntax(
        self.program.lower.hir.file,
        span,
        "unsupported update operand",
      )),
    }
  }

  pub fn compile_unary_expr(
    &mut self,
    expr_id: ExprId,
    span: Loc,
    operator: UnaryOp,
    argument: ExprId,
  ) -> OptimizeResult<Arg> {
    match operator {
      UnaryOp::Not => {
        if let ExprKind::Unary {
          op: UnaryOp::Not,
          expr: inner,
        } = &self.body.exprs[argument.0 as usize].kind
        {
          if self.expr_is_boolean(*inner) {
            return self.compile_expr(*inner);
          }
        }
        let arg = self.compile_expr(argument)?;
        let tmp = self.c_temp.bump();
        self.push_value_inst(expr_id, Inst::un(tmp, UnOp::Not, arg));
        Ok(Arg::Var(tmp))
      }
      UnaryOp::BitNot => {
        let arg = self.compile_expr(argument)?;
        let tmp = self.c_temp.bump();
        self.push_value_inst(expr_id, Inst::un(tmp, UnOp::BitNot, arg));
        Ok(Arg::Var(tmp))
      }
      UnaryOp::Minus => {
        let arg = self.compile_expr(argument)?;
        let tmp = self.c_temp.bump();
        self.push_value_inst(expr_id, Inst::un(tmp, UnOp::Neg, arg));
        Ok(Arg::Var(tmp))
      }
      UnaryOp::Plus => {
        let arg = self.compile_expr(argument)?;
        let tmp = self.c_temp.bump();
        self.push_value_inst(expr_id, Inst::un(tmp, UnOp::Plus, arg));
        Ok(Arg::Var(tmp))
      }
      UnaryOp::Typeof => {
        let arg = match self.body.exprs[argument.0 as usize].kind {
          ExprKind::Ident(name) => match self.classify_ident(argument, name) {
            VarType::Unknown(name) => Arg::Builtin(name),
            _ => self.compile_expr(argument)?,
          },
          _ => self.compile_expr(argument)?,
        };
        let tmp = self.c_temp.bump();
        self.push_value_inst(expr_id, Inst::un(tmp, UnOp::Typeof, arg));
        Ok(Arg::Var(tmp))
      }
      UnaryOp::Void => {
        let arg = self.compile_expr(argument)?;
        let tmp = self.c_temp.bump();
        self.push_value_inst(expr_id, Inst::un(tmp, UnOp::Void, arg));
        Ok(Arg::Var(tmp))
      }
      UnaryOp::Delete => {
        let arg_expr = &self.body.exprs[argument.0 as usize];
        match &arg_expr.kind {
      ExprKind::Member(member) => {
            if member.optional {
              return Err(unsupported_syntax(
                self.program.lower.hir.file,
                span,
                "optional chaining in delete operand",
              ));
            }
            let object_arg = self.compile_expr(member.object)?;
            let prop_arg = key_arg(self, &member.property)?;
            let tmp = self.c_temp.bump();
            self.push_value_inst(
              expr_id,
              self.call_or_invoke(
                Some(tmp),
                Arg::Builtin(Self::INTERNAL_DELETE_CALLEE.to_string()),
                Arg::Const(Const::Undefined),
                vec![object_arg, prop_arg],
                Vec::new(),
              ),
            );
            Ok(Arg::Var(tmp))
          }
          _ => Err(unsupported_syntax(
            self.program.lower.hir.file,
            span,
            "unsupported delete operand",
          )),
        }
      }
      _ => Err(unsupported_syntax(
        self.program.lower.hir.file,
        span,
        format!("unsupported unary operator {operator:?}"),
      )),
    }
  }

  pub fn compile_member_expr(
    &mut self,
    expr_id: ExprId,
    member: &MemberExpr,
    chain: impl Into<Option<Chain>>,
  ) -> OptimizeResult<CompiledMemberExpr> {
    let (did_chain_setup, chain) = self.maybe_setup_chain(chain);
    let left_arg = self.compile_expr_with_chain(member.object, chain)?;
    let optional = member.optional && !self.expr_excludes_nullish(member.object);
    self.conditional_chain_jump(optional, &left_arg, chain);
    #[cfg(feature = "typed")]
    {
      // When type information is available we materialize explicit null checks for
      // non-optional property loads. This makes the implicit `null`/`undefined`
      // traps in downstream lowering backends explicit and enables null-check
      // elimination/hoisting in later optimization passes.
      if !optional && self.program.types.program.is_some() {
        self.out.push(Inst::null_check(None::<u32>, left_arg.clone()));
      }
    }
    let res_tmp_var = self.c_temp.bump();
    let right_arg = key_arg(self, &member.property)?;

    #[cfg(feature = "typed")]
    let field_access = self.try_resolve_field_access(member.object, &member.property);
    #[cfg(not(feature = "typed"))]
    let field_access: Option<(FieldRef, FieldAccessMeta)> = None;
    if let Some((field, meta)) = field_access {
      let mut inst = Inst::field_load(res_tmp_var, left_arg.clone(), field);
      inst.meta.field_access = Some(meta);
      self.push_value_inst(expr_id, inst);
    } else {
      #[cfg(feature = "typed")]
      let lowered_array = (|| {
        let elem_layout = self.native_array_elem_layout_for_expr(member.object)?;

        // `arr.length` / `arr["length"]`
        if matches!(&right_arg, Arg::Const(Const::Str(s)) if s == "length") {
          self.push_value_inst(
            expr_id,
            Inst::array_len(res_tmp_var, left_arg.clone(), elem_layout),
          );
          return Some(());
        }

        // `arr[i]`
        let index_arg = self.array_index_arg_from_key(&member.property, &right_arg)?;
        self.push_value_inst(
          expr_id,
          Inst::array_load(res_tmp_var, left_arg.clone(), index_arg, elem_layout, true),
        );
        Some(())
      })();
      #[cfg(not(feature = "typed"))]
      let lowered_array: Option<()> = None;

      if lowered_array.is_none() {
        self.push_value_inst(
          expr_id,
          Inst::bin(res_tmp_var, left_arg.clone(), BinOp::GetProp, right_arg),
        );
      }
    }
    self.complete_chain_setup(expr_id, did_chain_setup, res_tmp_var, chain);
    Ok(CompiledMemberExpr {
      res: Arg::Var(res_tmp_var),
      left: left_arg.clone(),
    })
  }

  pub fn compile_call_expr(
    &mut self,
    expr_id: ExprId,
    span: Loc,
    call: &CallExpr,
    chain: impl Into<Option<Chain>>,
  ) -> OptimizeResult<Arg> {
    if !call.is_new {
      if let ExprKind::Ident(name) = self.body.exprs[call.callee.0 as usize].kind {
        if self.name_for(name) == "eval" && self.symbol_for_expr(call.callee).is_none() {
          return Err(unsupported_syntax(
            self.program.lower.hir.file,
            span,
            "direct eval is not supported",
          ));
        }
      }
    }

    if call.is_new {
      if call.optional {
        return Err(unsupported_syntax(
          self.program.lower.hir.file,
          span,
          "optional chaining in new expressions is not supported",
        ));
      }

      let (did_chain_setup, chain) = self.maybe_setup_chain(chain);

      let ctor_arg = self.compile_expr(call.callee)?;
      let res_tmp_var = self.c_temp.bump();

      let mut args = Vec::new();
      let mut spreads = Vec::new();
      for a in call.args.iter() {
        let arg = self.compile_expr(a.expr)?;
        let arg_idx = args.len();
        args.push(arg);
        if a.spread {
          spreads.push(arg_idx + 2);
        }
      }

      self.push_value_inst(
        expr_id,
        self.call_or_invoke(
          Some(res_tmp_var),
          Arg::Builtin(Self::INTERNAL_NEW_CALLEE.to_string()),
          ctor_arg,
          args,
          spreads,
        ),
      );

      self.complete_chain_setup(expr_id, did_chain_setup, res_tmp_var, chain);
      return Ok(Arg::Var(res_tmp_var));
    }

    // Assertion-as-contract support: treat certain runtime assertions as
    // analysis-visible assumptions.
    //
    // Semantics:
    // - The call itself remains (runtime check).
    // - An `Assume(cond)` instruction is appended so analyses can treat `cond` as
    //   true on the fallthrough path.
    //
    // We only match simple, statically-recognizable forms:
    // - `assert(cond, ...)`
    // - `console.assert(cond, ...)`
    //
    // NOTE: This is intentionally conservative and ignores dynamic call targets.
    let is_assert_call = (|| {
      if call.optional {
        return false;
      }
      let callee_expr = &self.body.exprs[call.callee.0 as usize];
      match &callee_expr.kind {
        ExprKind::Ident(name) => self.name_for(*name) == "assert",
        ExprKind::Member(member) => {
          if member.optional {
            return false;
          }
          let prop_is_assert = match &member.property {
            hir_js::ObjectKey::Ident(name) => self.name_for(*name) == "assert",
            hir_js::ObjectKey::String(s) => s == "assert",
            _ => false,
          };
          if !prop_is_assert {
            return false;
          }
          match &self.body.exprs[member.object.0 as usize].kind {
            ExprKind::Ident(obj_name) => {
              self.name_for(*obj_name) == "console" && self.symbol_for_expr(member.object).is_none()
            }
            _ => false,
          }
        }
        _ => false,
      }
    })();

    if is_assert_call {
      // If we can prove the condition is always truthy/falsy at compile time,
      // handle it eagerly:
      // - always truthy => drop the assert entirely.
      // - always falsy  => hard error.
      if let Some(first_arg) = call.args.first() {
        let cond_expr = first_arg.expr;
        let cond_is_bool_lit = match &self.body.exprs[cond_expr.0 as usize].kind {
          ExprKind::Literal(hir_js::Literal::Boolean(v)) => Some(*v),
          _ => None,
        };

        #[allow(unused_mut)]
        let mut proven = cond_is_bool_lit;
        #[cfg(feature = "typed")]
        {
          // Prefer type-driven literal/truthiness when available.
          if proven.is_none() {
            proven = self.bool_literal_expr(cond_expr);
          }
          if proven.is_none() {
            proven = match self.expr_truthiness(cond_expr) {
              Some(crate::types::Truthiness::AlwaysTruthy) => Some(true),
              Some(crate::types::Truthiness::AlwaysFalsy) => Some(false),
              None => None,
            };
          }
        }

        if proven == Some(true) {
          // `assert(true)` is a no-op.
          return Ok(Arg::Const(Const::Undefined));
        }
        if proven == Some(false) {
          return Err(vec![crate::diagnostic_with_span(
            self.program.lower.hir.file,
            "OPT0010",
            "assertion is always false",
            span,
          )]);
        }
      }
    }

    let (did_chain_setup, chain) = self.maybe_setup_chain(chain);
    let (this_arg, callee_arg) = match self.body.exprs[call.callee.0 as usize].kind.clone() {
      ExprKind::Member(m) => {
        let c = self.compile_member_expr(call.callee, &m, chain)?;
        (c.left, c.res)
      }
      _ => {
        let c = self.compile_expr_with_chain(call.callee, chain)?;
        let this = Arg::Const(Const::Undefined);
        (this, c)
      }
    };
    let res_tmp_var = self.c_temp.bump();
    let optional = call.optional && !self.expr_excludes_nullish(call.callee);
    self.conditional_chain_jump(optional, &callee_arg, chain);

    let mut args = Vec::new();
    let mut spreads = Vec::new();
    for a in call.args.iter() {
      let arg = self.compile_expr(a.expr)?;
      let arg_idx = args.len();
      args.push(arg);
      if a.spread {
        spreads.push(arg_idx + 2);
      }
    }
    let assumed_cond = is_assert_call.then(|| args.first().cloned()).flatten();
    #[cfg(feature = "typed")]
    {
      if !optional && self.program.types.program.is_some() {
        self.out.push(Inst::null_check(None::<u32>, callee_arg.clone()));
      }
    }
    self.push_value_inst(
      expr_id,
      self.call_or_invoke(Some(res_tmp_var), callee_arg, this_arg, args, spreads),
    );

    if let Some(cond) = assumed_cond {
      self.out.push(Inst::assume(cond));
    }

    self.complete_chain_setup(expr_id, did_chain_setup, res_tmp_var, chain);
    Ok(Arg::Var(res_tmp_var))
  }

  pub fn compile_expr_with_chain(
    &mut self,
    expr_id: ExprId,
    chain: impl Into<Option<Chain>>,
  ) -> OptimizeResult<Arg> {
    let expr = &self.body.exprs[expr_id.0 as usize];
    let value_type = self
      .program
      .types
      .expr_value_type_summary(self.body_id, expr_id);
    let span = Loc(expr.span.start as usize, expr.span.end as usize);
    let res = match &expr.kind {
      ExprKind::Binary { op, left, right } => self.compile_binary_expr(expr_id, span, *op, *left, *right),
      ExprKind::Call(call) => self.compile_call_expr(expr_id, span, call, chain),
      ExprKind::Member(member) => Ok(self.compile_member_expr(expr_id, member, chain)?.res),
      ExprKind::Conditional {
        test,
        consequent,
        alternate,
      } => self.compile_cond_expr(expr_id, *test, *consequent, *alternate),
      ExprKind::Array(array) => {
        let mut args = Vec::new();
        let mut spreads = Vec::new();
        for element in array.elements.iter() {
          match element {
            hir_js::ArrayElement::Expr(expr) => {
              args.push(self.compile_expr(*expr)?);
            }
            hir_js::ArrayElement::Spread(expr) => {
              let arg = self.compile_expr(*expr)?;
              let idx = args.len();
              args.push(arg);
              spreads.push(idx + 2);
            }
            hir_js::ArrayElement::Empty => {
              args.push(Arg::Builtin(Self::INTERNAL_ARRAY_HOLE.to_string()));
            }
          }
        }
        let tmp = self.c_temp.bump();
        self.push_value_inst(
          expr_id,
          self.call_or_invoke(
            Some(tmp),
            Arg::Builtin(Self::INTERNAL_ARRAY_CALLEE.to_string()),
            Arg::Const(Const::Undefined),
            args,
            spreads,
          ),
        );
        Ok(Arg::Var(tmp))
      }
      ExprKind::Object(obj) => {
        let mut args = Vec::new();
        for property in obj.properties.iter() {
          match property {
            hir_js::ObjectProperty::KeyValue {
              key,
              value,
              method,
              shorthand: _,
            } => {
              if *method {
                return Err(unsupported_syntax(
                  self.program.lower.hir.file,
                  span,
                  "object method literals are not supported",
                ));
              }
              match key {
                hir_js::ObjectKey::Computed(expr) => {
                  args.push(Arg::Builtin(
                    Self::INTERNAL_OBJECT_COMPUTED_MARKER.to_string(),
                  ));
                  args.push(self.compile_expr(*expr)?);
                }
                hir_js::ObjectKey::Ident(name) => {
                  args.push(Arg::Builtin(Self::INTERNAL_OBJECT_PROP_MARKER.to_string()));
                  args.push(Arg::Const(Const::Str(self.name_for(*name))));
                }
                hir_js::ObjectKey::String(value) => {
                  args.push(Arg::Builtin(Self::INTERNAL_OBJECT_PROP_MARKER.to_string()));
                  args.push(Arg::Const(Const::Str(value.clone())));
                }
                hir_js::ObjectKey::Number(value) => {
                  args.push(Arg::Builtin(Self::INTERNAL_OBJECT_PROP_MARKER.to_string()));
                  args.push(Arg::Const(Const::Str(value.clone())));
                }
              }
              args.push(self.compile_expr(*value)?);
            }
            hir_js::ObjectProperty::Spread(expr) => {
              args.push(Arg::Builtin(
                Self::INTERNAL_OBJECT_SPREAD_MARKER.to_string(),
              ));
              args.push(self.compile_expr(*expr)?);
              args.push(Arg::Const(Const::Undefined));
            }
            hir_js::ObjectProperty::Getter { .. } | hir_js::ObjectProperty::Setter { .. } => {
              return Err(unsupported_syntax(
                self.program.lower.hir.file,
                span,
                "object accessor literals are not supported",
              ));
            }
          }
        }
        let tmp = self.c_temp.bump();
        self.push_value_inst(
          expr_id,
          self.call_or_invoke(
            Some(tmp),
            Arg::Builtin(Self::INTERNAL_OBJECT_CALLEE.to_string()),
            Arg::Const(Const::Undefined),
            args,
            Vec::new(),
          ),
        );
        Ok(Arg::Var(tmp))
      }
      ExprKind::Template(template) => {
        let mut args = Vec::new();
        args.push(Arg::Const(Const::Str(template.head.clone())));
        for span in template.spans.iter() {
          args.push(self.compile_expr(span.expr)?);
          args.push(Arg::Const(Const::Str(span.literal.clone())));
        }
        let tmp = self.c_temp.bump();
        #[cfg(feature = "typed")]
        {
          let mut inst = Inst::string_concat(tmp, args);
          inst.meta.string_concat_is_template = true;
          self.push_value_inst(expr_id, inst);
        }
        #[cfg(not(feature = "typed"))]
        {
          self.push_value_inst(
            expr_id,
            self.call_or_invoke(
              Some(tmp),
              Arg::Builtin(Self::INTERNAL_TEMPLATE_CALLEE.to_string()),
              Arg::Const(Const::Undefined),
              args,
              Vec::new(),
            ),
          );
        }
        Ok(Arg::Var(tmp))
      }
      ExprKind::TaggedTemplate { tag, template } => {
        let mut args = Vec::new();
        args.push(self.compile_expr(*tag)?);
        args.push(Arg::Const(Const::Str(template.head.clone())));
        for span in template.spans.iter() {
          args.push(self.compile_expr(span.expr)?);
          args.push(Arg::Const(Const::Str(span.literal.clone())));
        }
        let tmp = self.c_temp.bump();
        self.push_value_inst(
          expr_id,
          self.call_or_invoke(
            Some(tmp),
            Arg::Builtin(Self::INTERNAL_TAGGED_TEMPLATE_CALLEE.to_string()),
            Arg::Const(Const::Undefined),
            args,
            Vec::new(),
          ),
        );
        Ok(Arg::Var(tmp))
      }
      ExprKind::ImportCall {
        argument,
        attributes,
      } => {
        let mut args = Vec::new();
        args.push(self.compile_expr(*argument)?);
        if let Some(attrs) = attributes {
          args.push(self.compile_expr(*attrs)?);
        }
        let tmp = self.c_temp.bump();
        self.push_value_inst(
          expr_id,
          self.call_or_invoke(
            Some(tmp),
            Arg::Builtin("import".to_string()),
            Arg::Const(Const::Undefined),
            args,
            Vec::new(),
          ),
        );
        Ok(Arg::Var(tmp))
      }
      ExprKind::Await { expr } => {
        let arg = self.compile_expr(*expr)?;
        #[cfg(feature = "native-async-ops")]
        {
          // `InstTyp::Await` currently has no exception edge, so when an exception handler is
          // active we lower to the call form so it can be represented as an `Invoke`.
          if self.current_exception_handler().is_none() {
            let tmp = self.c_temp.bump();
            self.push_value_inst(expr_id, Inst::await_(tmp, arg, false));
            Ok(Arg::Var(tmp))
          } else {
            let tmp = self.c_temp.bump();
            self.push_value_inst(
              expr_id,
              self.call_or_invoke(
                Some(tmp),
                Arg::Builtin("__optimize_js_await".to_string()),
                Arg::Const(Const::Undefined),
                vec![arg],
                Vec::new(),
              ),
            );
            Ok(Arg::Var(tmp))
          }
        }
        #[cfg(not(feature = "native-async-ops"))]
        {
          let tmp = self.c_temp.bump();
          self.push_value_inst(
            expr_id,
            self.call_or_invoke(
              Some(tmp),
              Arg::Builtin(Self::INTERNAL_AWAIT_CALLEE.to_string()),
              Arg::Const(Const::Undefined),
              vec![arg],
              Vec::new(),
            ),
          );
          Ok(Arg::Var(tmp))
        }
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::AwaitExpr {
        value: expr,
        known_resolved,
      } => {
        let arg = self.compile_expr(*expr)?;
        let known_resolved =
          *known_resolved || (self.in_function && matches!(&arg, Arg::Const(_)));
        #[cfg(feature = "native-async-ops")]
        {
          if self.current_exception_handler().is_none() {
            let tmp = self.c_temp.bump();
            self.push_value_inst(expr_id, Inst::await_(tmp, arg, known_resolved));
            Ok(Arg::Var(tmp))
          } else {
            let tmp = self.c_temp.bump();
            self.push_value_inst(
              expr_id,
              self.call_or_invoke(
                Some(tmp),
                Arg::Builtin("__optimize_js_await".to_string()),
                Arg::Const(Const::Undefined),
                vec![arg],
                Vec::new(),
              ),
            );
            Ok(Arg::Var(tmp))
          }
        }
        #[cfg(not(feature = "native-async-ops"))]
        {
          let _ = known_resolved;
          let tmp = self.c_temp.bump();
          self.push_value_inst(
            expr_id,
            self.call_or_invoke(
              Some(tmp),
              Arg::Builtin(Self::INTERNAL_AWAIT_CALLEE.to_string()),
              Arg::Const(Const::Undefined),
              vec![arg],
              Vec::new(),
            ),
          );
          Ok(Arg::Var(tmp))
        }
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayMap { array, callback } => {
        let this_arg = self.compile_expr(*array)?;
        let callback_arg = self.compile_expr(*callback)?;
        let tmp = self.c_temp.bump();
        #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
        if self.current_exception_handler().is_none() {
          self.push_value_inst(
            expr_id,
            Inst::array_chain(tmp, this_arg, vec![ArrayChainOpData::Map {
              callback: callback_arg,
            }]),
          );
          return Ok(Arg::Var(tmp));
        }
        self.push_value_inst(
          expr_id,
          self.call_or_invoke(
            Some(tmp),
            Arg::Builtin("Array.prototype.map".to_string()),
            this_arg,
            vec![callback_arg],
            Vec::new(),
          ),
        );
        Ok(Arg::Var(tmp))
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayFilter { array, callback } => {
        let this_arg = self.compile_expr(*array)?;
        let callback_arg = self.compile_expr(*callback)?;
        let tmp = self.c_temp.bump();
        #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
        if self.current_exception_handler().is_none() {
          self.push_value_inst(
            expr_id,
            Inst::array_chain(tmp, this_arg, vec![ArrayChainOpData::Filter {
              callback: callback_arg,
            }]),
          );
          return Ok(Arg::Var(tmp));
        }
        self.push_value_inst(
          expr_id,
          self.call_or_invoke(
            Some(tmp),
            Arg::Builtin("Array.prototype.filter".to_string()),
            this_arg,
            vec![callback_arg],
            Vec::new(),
          ),
        );
        Ok(Arg::Var(tmp))
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayReduce {
        array,
        callback,
        init,
      } => {
        let this_arg = self.compile_expr(*array)?;
        let callback_arg = self.compile_expr(*callback)?;
        let init_arg = match init {
          Some(init) => Some(self.compile_expr(*init)?),
          None => None,
        };
        let tmp = self.c_temp.bump();

        #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
        if self.current_exception_handler().is_none() {
          self.push_value_inst(
            expr_id,
            Inst::array_chain(tmp, this_arg, vec![ArrayChainOpData::Reduce {
              callback: callback_arg,
              init: init_arg,
            }]),
          );
          return Ok(Arg::Var(tmp));
        }

        let mut args = vec![callback_arg];
        if let Some(init_arg) = init_arg {
          args.push(init_arg);
        }
        self.push_value_inst(
          expr_id,
          self.call_or_invoke(
            Some(tmp),
            Arg::Builtin("Array.prototype.reduce".to_string()),
            this_arg,
            args,
            Vec::new(),
          ),
        );
        Ok(Arg::Var(tmp))
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayFind { array, callback } => {
        let this_arg = self.compile_expr(*array)?;
        let callback_arg = self.compile_expr(*callback)?;
        let tmp = self.c_temp.bump();
        #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
        if self.current_exception_handler().is_none() {
          self.push_value_inst(
            expr_id,
            Inst::array_chain(tmp, this_arg, vec![ArrayChainOpData::Find {
              callback: callback_arg,
            }]),
          );
          return Ok(Arg::Var(tmp));
        }
        self.push_value_inst(
          expr_id,
          self.call_or_invoke(
            Some(tmp),
            Arg::Builtin("Array.prototype.find".to_string()),
            this_arg,
            vec![callback_arg],
            Vec::new(),
          ),
        );
        Ok(Arg::Var(tmp))
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayEvery { array, callback } => {
        let this_arg = self.compile_expr(*array)?;
        let callback_arg = self.compile_expr(*callback)?;
        let tmp = self.c_temp.bump();
        #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
        if self.current_exception_handler().is_none() {
          self.push_value_inst(
            expr_id,
            Inst::array_chain(tmp, this_arg, vec![ArrayChainOpData::Every {
              callback: callback_arg,
            }]),
          );
          return Ok(Arg::Var(tmp));
        }
        self.push_value_inst(
          expr_id,
          self.call_or_invoke(
            Some(tmp),
            Arg::Builtin("Array.prototype.every".to_string()),
            this_arg,
            vec![callback_arg],
            Vec::new(),
          ),
        );
        Ok(Arg::Var(tmp))
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArraySome { array, callback } => {
        let this_arg = self.compile_expr(*array)?;
        let callback_arg = self.compile_expr(*callback)?;
        let tmp = self.c_temp.bump();
        #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
        if self.current_exception_handler().is_none() {
          self.push_value_inst(
            expr_id,
            Inst::array_chain(tmp, this_arg, vec![ArrayChainOpData::Some {
              callback: callback_arg,
            }]),
          );
          return Ok(Arg::Var(tmp));
        }
        self.push_value_inst(
          expr_id,
          self.call_or_invoke(
            Some(tmp),
            Arg::Builtin("Array.prototype.some".to_string()),
            this_arg,
            vec![callback_arg],
            Vec::new(),
          ),
        );
        Ok(Arg::Var(tmp))
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::ArrayChain { array, ops } => {
        let base_array = self.compile_expr(*array)?;
        #[cfg(any(feature = "native-fusion", feature = "native-array-ops"))]
        if self.current_exception_handler().is_none() {
          let mut compiled_ops = Vec::with_capacity(ops.len());
          for op in ops {
            match op {
              HirArrayChainOp::Map(callback) => compiled_ops.push(ArrayChainOpData::Map {
                callback: self.compile_expr(*callback)?,
              }),
              HirArrayChainOp::Filter(callback) => compiled_ops.push(ArrayChainOpData::Filter {
                callback: self.compile_expr(*callback)?,
              }),
              HirArrayChainOp::Find(callback) => compiled_ops.push(ArrayChainOpData::Find {
                callback: self.compile_expr(*callback)?,
              }),
              HirArrayChainOp::Every(callback) => compiled_ops.push(ArrayChainOpData::Every {
                callback: self.compile_expr(*callback)?,
              }),
              HirArrayChainOp::Some(callback) => compiled_ops.push(ArrayChainOpData::Some {
                callback: self.compile_expr(*callback)?,
              }),
              HirArrayChainOp::Reduce(callback, init) => compiled_ops.push(ArrayChainOpData::Reduce {
                callback: self.compile_expr(*callback)?,
                init: match init {
                  Some(init) => Some(self.compile_expr(*init)?),
                  None => None,
                },
              }),
            }
          }
          let tmp = self.c_temp.bump();
          self.push_value_inst(expr_id, Inst::array_chain(tmp, base_array, compiled_ops));
          return Ok(Arg::Var(tmp));
        }

        let mut current = base_array;
        for (pos, op) in ops.iter().enumerate() {
          let (builtin, args) = match op {
            HirArrayChainOp::Map(callback) => (
              "Array.prototype.map",
              vec![self.compile_expr(*callback)?],
            ),
            HirArrayChainOp::Filter(callback) => (
              "Array.prototype.filter",
              vec![self.compile_expr(*callback)?],
            ),
            HirArrayChainOp::Find(callback) => (
              "Array.prototype.find",
              vec![self.compile_expr(*callback)?],
            ),
            HirArrayChainOp::Every(callback) => (
              "Array.prototype.every",
              vec![self.compile_expr(*callback)?],
            ),
            HirArrayChainOp::Some(callback) => (
              "Array.prototype.some",
              vec![self.compile_expr(*callback)?],
            ),
            HirArrayChainOp::Reduce(callback, init) => {
              let mut args = vec![self.compile_expr(*callback)?];
              if let Some(init) = init {
                args.push(self.compile_expr(*init)?);
              }
              ("Array.prototype.reduce", args)
            }
          };
          let tmp = self.c_temp.bump();
          let inst = self.call_or_invoke(
            Some(tmp),
            Arg::Builtin(builtin.to_string()),
            current,
            args,
            Vec::new(),
          );
          if pos == ops.len().saturating_sub(1) {
            self.push_value_inst(expr_id, inst);
          } else {
            self.out.push(inst);
          }
          current = Arg::Var(tmp);
        }
        Ok(current)
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::PromiseAll { promises } | ExprKind::PromiseRace { promises } => {
        let mut args = Vec::new();
        for promise in promises {
          args.push(self.compile_expr(*promise)?);
        }
        #[cfg(feature = "native-async-ops")]
        {
          // `InstTyp::{PromiseAll,PromiseRace}` currently have no exception edge, so when an
          // exception handler is active we lower to the call form so it can be represented as an
          // `Invoke`.
          if self.current_exception_handler().is_none() {
            let tmp = self.c_temp.bump();
            let inst = match &expr.kind {
              ExprKind::PromiseAll { .. } => Inst::promise_all(tmp, args),
              ExprKind::PromiseRace { .. } => Inst::promise_race(tmp, args),
              _ => unreachable!(),
            };
            self.push_value_inst(expr_id, inst);
            Ok(Arg::Var(tmp))
          } else {
            let array_tmp = self.c_temp.bump();
            self.out.push(self.call_or_invoke(
              Some(array_tmp),
              Arg::Builtin(Self::INTERNAL_ARRAY_CALLEE.to_string()),
              Arg::Const(Const::Undefined),
              args,
              Vec::new(),
            ));

            let tmp = self.c_temp.bump();
            self.push_value_inst(
              expr_id,
              self.call_or_invoke(
                Some(tmp),
                Arg::Builtin(
                  match &expr.kind {
                    ExprKind::PromiseAll { .. } => "Promise.all",
                    ExprKind::PromiseRace { .. } => "Promise.race",
                    _ => unreachable!(),
                  }
                  .to_string(),
                ),
                Arg::Const(Const::Undefined),
                vec![Arg::Var(array_tmp)],
                Vec::new(),
              ),
            );
            Ok(Arg::Var(tmp))
          }
        }
        #[cfg(not(feature = "native-async-ops"))]
        {
          let array_tmp = self.c_temp.bump();
          self.out.push(self.call_or_invoke(
            Some(array_tmp),
            Arg::Builtin(Self::INTERNAL_ARRAY_CALLEE.to_string()),
            Arg::Const(Const::Undefined),
            args,
            Vec::new(),
          ));

          let tmp = self.c_temp.bump();
          self.push_value_inst(
            expr_id,
            self.call_or_invoke(
              Some(tmp),
              Arg::Builtin(
                match &expr.kind {
                  ExprKind::PromiseAll { .. } => "Promise.all",
                  ExprKind::PromiseRace { .. } => "Promise.race",
                  _ => unreachable!(),
                }
                .to_string(),
              ),
              Arg::Const(Const::Undefined),
              vec![Arg::Var(array_tmp)],
              Vec::new(),
            ),
          );
          Ok(Arg::Var(tmp))
        }
      }
      #[cfg(feature = "semantic-ops")]
      ExprKind::KnownApiCall { api, args } => {
        let mut compiled_args = Vec::new();
        for arg in args {
          compiled_args.push(self.compile_expr(*arg)?);
        }
        let tmp = self.c_temp.bump();
        self.push_value_inst(
          expr_id,
          Inst::known_api_call(tmp, *api, compiled_args),
        );
        Ok(Arg::Var(tmp))
      }
      ExprKind::Ident(name) => self.compile_id_expr(expr_id, *name),
      ExprKind::Literal(lit) => self.literal_arg(expr_id, span, lit),
      ExprKind::This => Ok(Arg::Builtin("this".to_string())),
      ExprKind::ImportMeta => Ok(Arg::Builtin("import.meta".to_string())),
      ExprKind::NewTarget => Ok(Arg::Builtin("new.target".to_string())),
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::Instantiation { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Satisfies { expr, .. } => self.compile_expr(*expr),
      ExprKind::Unary { op, expr } => self.compile_unary_expr(expr_id, span, *op, *expr),
      ExprKind::Update { op, expr, prefix } => {
        self.compile_update_expr(expr_id, span, *op, *expr, *prefix)
      }
      ExprKind::Assignment { op, target, value } => {
        self.compile_assignment(expr_id, span, *op, *target, *value)
      }
      ExprKind::FunctionExpr {
        def,
        body,
        name,
        is_arrow: _,
      } => self.compile_func(*def, *body, *name),
      other => Err(unsupported_syntax(
        self.program.lower.hir.file,
        span,
        format!("unsupported expression {other:?}"),
      )),
    }?;

    if !value_type.is_unknown() {
      if let Arg::Var(var) = res {
        // A temporary can be defined multiple times within the lowered IL, most
        // notably for optional chaining roots where we assign `undefined` on the
        // nullish edge after emitting the "real" producer instruction.
        //
        // Annotate every definition we've seen so far so that optimizations that
        // eliminate copy/phi nodes still have access to the type summary on the
        // original value-producing instruction (e.g. the `Call` itself).
        for def in self.out.iter_mut().rev() {
          if def.tgts.first() != Some(&var) {
            continue;
          }
          if def.value_type.is_unknown() {
            def.value_type = value_type;
          }
        }
        return Ok(Arg::Var(var));
      }
    }

    Ok(res)
  }

  pub fn compile_expr(&mut self, expr_id: ExprId) -> OptimizeResult<Arg> {
    self.compile_expr_with_chain(expr_id, None)
  }
}
