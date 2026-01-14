use super::{FinallyContext, HirSourceToInst, JumpTarget, LabeledTarget, VarType, DUMMY_LABEL};
use crate::il::inst::{Arg, BinOp, Const, Inst};
use crate::symbol::semantics::SymbolId;
use crate::unsupported_syntax_range;
use crate::util::counter::Counter;
use crate::OptimizeResult;
use crate::ProgramCompiler;
use crate::TextRange;
use hir_js::hir::CatchClause;
use hir_js::{
  Body, BodyId, BodyKind, DefKind, ExprId, ExprKind, ForHead, ForInit, NameId, ObjectKey, PatId,
  PatKind, StmtId, StmtKind, VarDecl, VarDeclKind, VarDeclarator,
};
use parse_js::loc::Loc;
use parse_js::num::JsNumber;

const COMPLETION_NORMAL: u32 = 0;
const COMPLETION_RETURN: u32 = 1;
const COMPLETION_THROW: u32 = 2;

pub fn key_arg(compiler: &mut HirSourceToInst<'_>, key: &ObjectKey) -> OptimizeResult<Arg> {
  Ok(match key {
    ObjectKey::Ident(name) => Arg::Const(Const::Str(compiler.name_for(*name))),
    ObjectKey::String(s) => Arg::Const(Const::Str(s.clone())),
    ObjectKey::Number(n) => Arg::Const(Const::Str(n.clone())),
    ObjectKey::Computed(expr) => compiler.compile_expr(*expr)?,
  })
}

fn root_statements(body: &Body) -> Vec<StmtId> {
  let mut referenced = vec![false; body.stmts.len()];
  for stmt in body.stmts.iter() {
    match &stmt.kind {
      StmtKind::Block(stmts) => {
        for id in stmts {
          referenced[id.0 as usize] = true;
        }
      }
      StmtKind::If {
        consequent,
        alternate,
        ..
      } => {
        referenced[consequent.0 as usize] = true;
        if let Some(alt) = alternate {
          referenced[alt.0 as usize] = true;
        }
      }
      StmtKind::While { body, .. } | StmtKind::DoWhile { body, .. } => {
        referenced[body.0 as usize] = true;
      }
      StmtKind::For { body, .. } | StmtKind::ForIn { body, .. } => {
        referenced[body.0 as usize] = true;
      }
      StmtKind::Switch { cases, .. } => {
        for case in cases {
          for stmt in case.consequent.iter() {
            referenced[stmt.0 as usize] = true;
          }
        }
      }
      StmtKind::Try {
        block,
        catch,
        finally_block,
      } => {
        referenced[block.0 as usize] = true;
        if let Some(catch) = catch {
          referenced[catch.body.0 as usize] = true;
        }
        if let Some(finally) = finally_block {
          referenced[finally.0 as usize] = true;
        }
      }
      StmtKind::Labeled { body, .. } | StmtKind::With { body, .. } => {
        referenced[body.0 as usize] = true;
      }
      _ => {}
    }
  }
  let mut roots: Vec<_> = body
    .stmts
    .iter()
    .enumerate()
    .filter_map(|(idx, _)| (!referenced[idx]).then_some(StmtId(idx as u32)))
    .collect();
  roots.sort_by_key(|id| body.stmts[id.0 as usize].span.start);
  roots
}

impl<'p> HirSourceToInst<'p> {
  fn completion_code_arg(code: u32) -> Arg {
    Arg::Const(Const::Num(JsNumber(code as f64)))
  }

  fn current_finally_depth(&self) -> usize {
    self.finally_stack.len()
  }

  fn jump_target(&self, label: u32) -> JumpTarget {
    JumpTarget {
      label,
      finally_depth: self.current_finally_depth(),
    }
  }

  fn emit_completion_to_finally(&mut self, code: u32, value: Option<Arg>) {
    let ctx = self
      .finally_stack
      .last()
      .expect("emit_completion_to_finally with empty finally_stack");
    if let Some(value) = value {
      self
        .out
        .push(Inst::var_assign(ctx.completion_value, value));
    }
    self.out.push(Inst::var_assign(
      ctx.completion_kind,
      Self::completion_code_arg(code),
    ));
    self.out.push(Inst::goto(ctx.finally_label));
  }

  fn emit_goto(&mut self, target: JumpTarget) {
    if self.finally_stack.len() > target.finally_depth {
      let code = {
        let ctx = self
          .finally_stack
          .last_mut()
          .expect("checked by if condition");
        ctx.jump_code_for(target)
      };
      self.emit_completion_to_finally(code, None);
    } else {
      self.out.push(Inst::goto(target.label));
    }
  }

  fn emit_return(&mut self, value: Option<Arg>) {
    if self.finally_stack.is_empty() {
      self.out.push(Inst::ret(value));
      return;
    }
    let value = value.unwrap_or(Arg::Const(Const::Undefined));
    self.emit_completion_to_finally(COMPLETION_RETURN, Some(value));
  }

  fn emit_throw(&mut self, value: Arg) {
    self.out.push(self.throw_or_throw_to(value));
  }

  fn compile_try_stmt(
    &mut self,
    span: TextRange,
    block: StmtId,
    catch: Option<&CatchClause>,
    finally_block: Option<StmtId>,
  ) -> OptimizeResult<()> {
    let file = self.program.lower.hir.file;

    if catch.is_none() && finally_block.is_none() {
      return Err(unsupported_syntax_range(
        file,
        span,
        "try statement must have catch or finally",
      ));
    }

    let after_try_label = self.c_label.bump();

    // When a finally block is present, we lower all non-exception abrupt
    // completions (return/break/continue) through a completion record and funnel
    // them through `finally_label`.
    let (finally_label, landingpad_label, completion_kind, completion_value) =
      if finally_block.is_some() {
        (
          Some(self.c_label.bump()),
          Some(self.c_label.bump()),
          Some(self.c_temp.bump()),
          Some(self.c_temp.bump()),
        )
      } else {
        (None, None, None, None)
      };

    let catch_label = catch.map(|_| self.c_label.bump());

    if let Some(finally_label) = finally_label {
      let completion_kind = completion_kind.expect("set with finally_label");
      let completion_value = completion_value.expect("set with finally_label");
      self.finally_stack.push(FinallyContext::new(
        finally_label,
        after_try_label,
        completion_kind,
        completion_value,
      ));
    }

    // Compile the try block with an active exception handler when needed.
    let try_handler = catch_label.or(landingpad_label);
    if let Some(handler) = try_handler {
      self.exception_stack.push(handler);
    }
    self.compile_stmt(block)?;
    if try_handler.is_some() {
      self.exception_stack.pop();
    }

    // Normal completion of the try block.
    if let Some(finally_label) = finally_label {
      let ctx = self
        .finally_stack
        .last()
        .expect("pushed above when finally_label is Some");
      self.out.push(Inst::var_assign(
        ctx.completion_kind,
        Self::completion_code_arg(COMPLETION_NORMAL),
      ));
      self.out.push(Inst::goto(finally_label));
    } else {
      self.out.push(Inst::goto(after_try_label));
    }

    // Catch handler (if present).
    if let Some(catch) = catch {
      let catch_label = catch_label.expect("catch_label set when catch is Some");
      self.out.push(Inst::label(catch_label));
      let catch_val = self.c_temp.bump();
      self.out.push(Inst::catch(catch_val));

      // Catch clauses do not catch exceptions thrown inside themselves; restore
      // the outer handler. When a finally block exists, exceptions in catch
      // must still execute finally, so use the landingpad as the handler.
      let catch_handler = landingpad_label;
      if let Some(handler) = catch_handler {
        self.exception_stack.push(handler);
      }

      if let Some(param) = catch.param {
        self.compile_destructuring(param, Arg::Var(catch_val))?;
      }
      self.compile_stmt(catch.body)?;
      if catch_handler.is_some() {
        self.exception_stack.pop();
      }

      // Normal completion of catch.
      if let Some(finally_label) = finally_label {
        let ctx = self
          .finally_stack
          .last()
          .expect("pushed above when finally_label is Some");
        self.out.push(Inst::var_assign(
          ctx.completion_kind,
          Self::completion_code_arg(COMPLETION_NORMAL),
        ));
        self.out.push(Inst::goto(finally_label));
      } else {
        self.out.push(Inst::goto(after_try_label));
      }
    }

    // Landingpad to capture exceptions and funnel through finally.
    if let Some(landingpad_label) = landingpad_label {
      let finally_label = finally_label.expect("landingpad_label implies finally_label");
      let completion_kind = completion_kind.expect("landingpad_label implies completion_kind");
      let completion_value = completion_value.expect("landingpad_label implies completion_value");

      self.out.push(Inst::label(landingpad_label));
      let exc = self.c_temp.bump();
      self.out.push(Inst::catch(exc));
      self
        .out
        .push(Inst::var_assign(completion_value, Arg::Var(exc)));
      self.out.push(Inst::var_assign(
        completion_kind,
        Self::completion_code_arg(COMPLETION_THROW),
      ));
      self.out.push(Inst::goto(finally_label));
    }

    if let Some(finally_stmt) = finally_block {
      let finally_label = finally_label.expect("finally_block implies finally_label");
      let ctx = self
        .finally_stack
        .pop()
        .expect("pushed above when finally_block is Some");
      self.out.push(Inst::label(finally_label));

      // Compile the finally body outside the current finally context so control
      // flow statements inside it do not re-enter the same finally block.
      self.compile_stmt(finally_stmt)?;

      // If the finally block completes normally, dispatch the pending completion.
      let kind_var = ctx.completion_kind;
      let value_var = ctx.completion_value;

      // return?
      let is_return_tmp = self.c_temp.bump();
      self.out.push(Inst::bin(
        is_return_tmp,
        Arg::Var(kind_var),
        BinOp::StrictEq,
        Self::completion_code_arg(COMPLETION_RETURN),
      ));
      let return_label = self.c_label.bump();
      self
        .out
        .push(Inst::cond_goto(Arg::Var(is_return_tmp), return_label, DUMMY_LABEL));

      // throw?
      let is_throw_tmp = self.c_temp.bump();
      self.out.push(Inst::bin(
        is_throw_tmp,
        Arg::Var(kind_var),
        BinOp::StrictEq,
        Self::completion_code_arg(COMPLETION_THROW),
      ));
      let throw_label = self.c_label.bump();
      self
        .out
        .push(Inst::cond_goto(Arg::Var(is_throw_tmp), throw_label, DUMMY_LABEL));

      // jump cases (break/continue).
      let mut jump_labels = Vec::with_capacity(ctx.jump_targets.len());
      for (code, _target) in ctx.jump_targets.iter().copied() {
        let is_jump_tmp = self.c_temp.bump();
        self.out.push(Inst::bin(
          is_jump_tmp,
          Arg::Var(kind_var),
          BinOp::StrictEq,
          Self::completion_code_arg(code),
        ));
        let jump_label = self.c_label.bump();
        jump_labels.push(jump_label);
        self.out.push(Inst::cond_goto(
          Arg::Var(is_jump_tmp),
          jump_label,
          DUMMY_LABEL,
        ));
      }

      // Default: normal completion.
      self.out.push(Inst::goto(ctx.after_label));

      // Action blocks.
      self.out.push(Inst::label(return_label));
      self.emit_return(Some(Arg::Var(value_var)));

      self.out.push(Inst::label(throw_label));
      self.emit_throw(Arg::Var(value_var));

      for ((_, target), action_label) in ctx.jump_targets.iter().copied().zip(jump_labels) {
        self.out.push(Inst::label(action_label));
        self.emit_goto(target);
      }
    } else if finally_label.is_some() {
      // Defensive: finally_label implies finally_block.
      self
        .finally_stack
        .pop()
        .expect("pushed above when finally_label is Some");
    }

    self.out.push(Inst::label(after_try_label));
    Ok(())
  }

  fn collect_pat_binding_symbols(&self, pat: PatId, out: &mut Vec<SymbolId>) {
    match &self.body.pats[pat.0 as usize].kind {
      PatKind::Ident(_) => {
        if let Some(sym) = self.symbol_for_pat(pat) {
          out.push(sym);
        }
      }
      PatKind::Array(arr) => {
        for element in arr.elements.iter().flatten() {
          self.collect_pat_binding_symbols(element.pat, out);
        }
        if let Some(rest) = arr.rest {
          self.collect_pat_binding_symbols(rest, out);
        }
      }
      PatKind::Object(obj) => {
        for prop in obj.props.iter() {
          self.collect_pat_binding_symbols(prop.value, out);
        }
        if let Some(rest) = obj.rest {
          self.collect_pat_binding_symbols(rest, out);
        }
      }
      PatKind::Rest(inner) => self.collect_pat_binding_symbols(**inner, out),
      PatKind::Assign { target, .. } => self.collect_pat_binding_symbols(*target, out),
      PatKind::AssignTarget(_) => {}
    }
  }

  fn hoist_var_decls(&mut self) {
    let mut declared = Vec::<SymbolId>::new();
    for stmt in self.body.stmts.iter() {
      match &stmt.kind {
        StmtKind::Var(decl) if decl.kind == VarDeclKind::Var => {
          for declarator in decl.declarators.iter() {
            self.collect_pat_binding_symbols(declarator.pat, &mut declared);
          }
        }
        StmtKind::For {
          init: Some(ForInit::Var(decl)),
          ..
        } if decl.kind == VarDeclKind::Var => {
          for declarator in decl.declarators.iter() {
            self.collect_pat_binding_symbols(declarator.pat, &mut declared);
          }
        }
        StmtKind::ForIn {
          left: ForHead::Var(decl),
          ..
        } if decl.kind == VarDeclKind::Var => {
          for declarator in decl.declarators.iter() {
            self.collect_pat_binding_symbols(declarator.pat, &mut declared);
          }
        }
        _ => {}
      }
    }

    if declared.is_empty() {
      return;
    }

    let mut params = Vec::<SymbolId>::new();
    if let Some(function) = &self.body.function {
      for param in function.params.iter() {
        self.collect_pat_binding_symbols(param.pat, &mut params);
      }
    }
    params.sort_by_key(|sym| sym.raw_id());
    params.dedup();

    declared.retain(|sym| params.binary_search(sym).is_err());
    declared.sort_by_key(|sym| sym.raw_id());
    declared.dedup();

    for sym in declared {
      if self.program.foreign_vars.contains(&sym) {
        self
          .out
          .push(Inst::foreign_store(sym, Arg::Const(Const::Undefined)));
      } else {
        let tmp = self.symbol_to_temp(sym);
        self
          .out
          .push(Inst::var_assign(tmp, Arg::Const(Const::Undefined)));
      }
    }
  }

  fn hoist_function_decls(&mut self) -> OptimizeResult<()> {
    let mut decls = Vec::new();
    for stmt in self.body.stmts.iter() {
      if let StmtKind::Decl(def_id) = stmt.kind {
        decls.push((stmt.span.start, stmt.span.end, def_id));
      }
    }
    decls.sort_by_key(|(start, end, def_id)| (*start, *end, *def_id));

    for (_, _, def_id) in decls {
      let Some(def) = self.program.lower.def(def_id) else {
        continue;
      };
      if def.path.kind != DefKind::Function {
        continue;
      }
      let Some(body) = def.body else {
        // TypeScript overload signatures and ambient function declarations have no runtime body.
        continue;
      };
      let fn_arg = self.compile_func(def_id, body, Some(def.name))?;

      let inst = match self.symbol_for_def(def_id) {
        Some(sym) => {
          if self.program.foreign_vars.contains(&sym) {
            Inst::foreign_store(sym, fn_arg)
          } else {
            let tmp = self.symbol_to_temp(sym);
            Inst::var_assign(tmp, fn_arg)
          }
        }
        None => Inst::unknown_store(self.name_for(def.name), fn_arg),
      };
      self.out.push(inst);
    }
    Ok(())
  }

  pub fn compile_destructuring_via_prop(
    &mut self,
    obj: Arg,
    prop: Arg,
    target: PatId,
    default_value: Option<ExprId>,
  ) -> OptimizeResult<()> {
    let tmp_var = self.c_temp.bump();
    self.out.push(Inst::bin(tmp_var, obj, BinOp::GetProp, prop));
    if let Some(dv) = default_value {
      let after_label_id = self.c_label.bump();
      let is_undefined_tmp_var = self.c_temp.bump();
      self.out.push(Inst::bin(
        is_undefined_tmp_var,
        Arg::Var(tmp_var),
        BinOp::StrictEq,
        Arg::Const(Const::Undefined),
      ));
      self.out.push(Inst::cond_goto(
        Arg::Var(is_undefined_tmp_var),
        DUMMY_LABEL,
        after_label_id,
      ));
      let dv_arg = self.compile_expr(dv)?;
      self.push_value_inst(dv, Inst::var_assign(tmp_var, dv_arg));
      self.out.push(Inst::label(after_label_id));
    };
    self.compile_destructuring(target, Arg::Var(tmp_var))
  }

  pub fn compile_destructuring(&mut self, pat: PatId, rval: Arg) -> OptimizeResult<()> {
    match &self.body.pats[pat.0 as usize].kind {
      PatKind::Array(arr) => {
        for (i, e) in arr.elements.iter().enumerate() {
          let Some(e) = e else {
            continue;
          };
          self.compile_destructuring_via_prop(
            rval.clone(),
            Arg::Const(Const::Num(JsNumber(i as f64))),
            e.pat,
            e.default_value,
          )?;
        }
      }
      PatKind::Object(obj) => {
        for p in obj.props.iter() {
          let prop = key_arg(self, &p.key)?;
          self.compile_destructuring_via_prop(rval.clone(), prop, p.value, p.default_value)?;
        }
      }
      PatKind::Ident(name) => {
        let var_type = self.classify_symbol(self.symbol_for_pat(pat), self.name_for(*name));
        let inst = match var_type {
          VarType::Local(local) => {
            let tgt = self.symbol_to_temp(local);
            #[cfg(feature = "typed")]
            let mut inst = Inst::var_assign(tgt, rval.clone());
            #[cfg(not(feature = "typed"))]
            let inst = Inst::var_assign(tgt, rval.clone());
            #[cfg(feature = "typed")]
            {
              let layout_for_const = |program: &typecheck_ts::Program, c: &Const| {
                let store = program.interned_type_store();
                let prim = store.primitive_ids();
                let ty = match c {
                  Const::Bool(_) => prim.boolean,
                  Const::Num(_) => prim.number,
                  Const::Str(_) => prim.string,
                  Const::Null => prim.null,
                  Const::Undefined => prim.undefined,
                  Const::BigInt(_) => prim.bigint,
                };
                store.layout_of(ty)
              };

              if let Some(program) = self.program.types.program.as_ref() {
                inst.meta.native_layout = match &rval {
                  Arg::Var(src) => self.var_layouts.get(src).copied(),
                  Arg::Const(c) => Some(layout_for_const(program, c)),
                  Arg::Builtin(_) | Arg::Fn(_) => {
                    let store = program.interned_type_store();
                    Some(store.layout_of(store.primitive_ids().unknown))
                  }
                };
              }

              if let Some(layout) = inst.meta.native_layout {
                self.var_layouts.insert(tgt, layout);
              }
            }
            inst
          }
          VarType::Foreign(foreign) => Inst::foreign_store(foreign, rval.clone()),
          VarType::Unknown(unknown) => Inst::unknown_store(unknown, rval.clone()),
          VarType::Builtin(builtin) => {
            return Err(unsupported_syntax_range(
              self.program.lower.hir.file,
              self.body.pats[pat.0 as usize].span,
              format!("assignment to builtin {builtin}"),
            ))
          }
        };
        self.out.push(inst);
      }
      PatKind::AssignTarget(expr_id) => {
        let expr = &self.body.exprs[expr_id.0 as usize];
        let inst = match &expr.kind {
          ExprKind::Member(member) => {
            if member.optional {
              return Err(unsupported_syntax_range(
                self.program.lower.hir.file,
                expr.span,
                "optional chaining in assignment target",
              ));
            }
            let obj = self.compile_expr(member.object)?;
            let prop = key_arg(self, &member.property)?;
            Inst::prop_assign(obj, prop, rval.clone())
          }
          other => {
            return Err(unsupported_syntax_range(
              self.program.lower.hir.file,
              expr.span,
              format!("unsupported assignment target {other:?}"),
            ))
          }
        };
        self.out.push(inst);
      }
      _ => {
        return Err(unsupported_syntax_range(
          self.program.lower.hir.file,
          self.body.pats[pat.0 as usize].span,
          "unsupported destructuring pattern",
        ))
      }
    };
    Ok(())
  }

  pub fn compile_var_decl(&mut self, decl: &VarDecl) -> OptimizeResult<()> {
    for VarDeclarator { pat, init, .. } in decl.declarators.iter() {
      let pat_span = self.body.pats[pat.0 as usize].span;
      match init {
        Some(init) => {
          let tmp = self.c_temp.bump();
          let rval = self.compile_expr(*init)?;
          self.push_value_inst(*init, Inst::var_assign(tmp, rval));
          self.compile_destructuring(*pat, Arg::Var(tmp))?;
        }
        None => match decl.kind {
          VarDeclKind::Const | VarDeclKind::Using | VarDeclKind::AwaitUsing => {
            return Err(unsupported_syntax_range(
              self.program.lower.hir.file,
              pat_span,
              format!("{:?} declarations must have initializers", decl.kind),
            ));
          }
          VarDeclKind::Let => match self.body.pats[pat.0 as usize].kind {
            PatKind::Ident(_) => {
              self.compile_destructuring(*pat, Arg::Const(Const::Undefined))?;
            }
            _ => {
              return Err(unsupported_syntax_range(
                self.program.lower.hir.file,
                pat_span,
                "destructuring declarations must have initializers",
              ));
            }
          },
          VarDeclKind::Var => {
            // `var x;` is hoisted and has no runtime effect at the declaration site, so
            // we do not emit an explicit assignment here.
          }
        },
      }
    }
    Ok(())
  }

  fn compile_for_head(
    &mut self,
    span: TextRange,
    head: &ForHead,
    value: Arg,
  ) -> OptimizeResult<()> {
    match head {
      ForHead::Pat(pat) => self.compile_destructuring(*pat, value),
      ForHead::Var(decl) => {
        if decl.declarators.len() != 1 {
          return Err(unsupported_syntax_range(
            self.program.lower.hir.file,
            span,
            "for-in/of variable declarations must have a single declarator",
          ));
        }
        self.compile_destructuring(decl.declarators[0].pat, value)
      }
    }
  }

  fn compile_for_in_of_stmt(
    &mut self,
    span: TextRange,
    left: &ForHead,
    right: ExprId,
    body: StmtId,
    is_for_of: bool,
    await_: bool,
    label: Option<NameId>,
  ) -> OptimizeResult<()> {
    if await_ && !is_for_of {
      return Err(unsupported_syntax_range(
        self.program.lower.hir.file,
        span,
        "for-in statements do not support await",
      ));
    }

    let iterable_tmp_var = if is_for_of {
      let iterable_tmp_var = self.c_temp.bump();
      let iterable_arg = self.compile_expr(right)?;
      self.push_value_inst(right, Inst::var_assign(iterable_tmp_var, iterable_arg));
      iterable_tmp_var
    } else {
      let obj_tmp_var = self.c_temp.bump();
      let obj_arg = self.compile_expr(right)?;
      self.push_value_inst(right, Inst::var_assign(obj_tmp_var, obj_arg));

      let keys_tmp_var = self.c_temp.bump();
      self.out.push(self.call_or_invoke(
        Some(keys_tmp_var),
        Arg::Builtin("Object.keys".to_string()),
        Arg::Const(Const::Undefined),
        vec![Arg::Var(obj_tmp_var)],
        Vec::new(),
      ));
      keys_tmp_var
    };

    let iterator_method_tmp_var = self.c_temp.bump();
    self.out.push(Inst::bin(
      iterator_method_tmp_var,
      Arg::Var(iterable_tmp_var),
      BinOp::GetProp,
      Arg::Builtin(
        if await_ {
          "Symbol.asyncIterator"
        } else {
          "Symbol.iterator"
        }
        .to_string(),
      ),
    ));

    let iterator_tmp_var = self.c_temp.bump();
    self.out.push(self.call_or_invoke(
      Some(iterator_tmp_var),
      Arg::Var(iterator_method_tmp_var),
      Arg::Var(iterable_tmp_var),
      Vec::new(),
      Vec::new(),
    ));

    let loop_entry_label = self.c_label.bump();
    let after_loop_label = self.c_label.bump();
    self.out.push(Inst::label(loop_entry_label));

    let next_method_tmp_var = self.c_temp.bump();
    self.out.push(Inst::bin(
      next_method_tmp_var,
      Arg::Var(iterator_tmp_var),
      BinOp::GetProp,
      Arg::Const(Const::Str("next".to_string())),
    ));
    let next_result_tmp_var = self.c_temp.bump();
    self.out.push(self.call_or_invoke(
      Some(next_result_tmp_var),
      Arg::Var(next_method_tmp_var),
      Arg::Var(iterator_tmp_var),
      Vec::new(),
      Vec::new(),
    ));

    let iter_result_tmp_var = if await_ {
      let awaited_tmp_var = self.c_temp.bump();
      #[cfg(feature = "native-async-ops")]
      {
        // `InstTyp::Await` currently has no exception edge, so when an exception handler is
        // active we lower to the call form so it can be represented as an `Invoke`.
        if self.current_exception_handler().is_none() {
          self
            .out
            .push(Inst::await_(awaited_tmp_var, Arg::Var(next_result_tmp_var), false));
        } else {
          self.out.push(self.call_or_invoke(
            Some(awaited_tmp_var),
            Arg::Builtin("__optimize_js_await".to_string()),
            Arg::Const(Const::Undefined),
            vec![Arg::Var(next_result_tmp_var)],
            Vec::new(),
          ));
        }
      }
      #[cfg(not(feature = "native-async-ops"))]
      {
        self.out.push(self.call_or_invoke(
          Some(awaited_tmp_var),
          Arg::Builtin("__optimize_js_await".to_string()),
          Arg::Const(Const::Undefined),
          vec![Arg::Var(next_result_tmp_var)],
          Vec::new(),
        ));
      }
      awaited_tmp_var
    } else {
      next_result_tmp_var
    };

    let done_tmp_var = self.c_temp.bump();
    self.out.push(Inst::bin(
      done_tmp_var,
      Arg::Var(iter_result_tmp_var),
      BinOp::GetProp,
      Arg::Const(Const::Str("done".to_string())),
    ));
    self.out.push(Inst::cond_goto(
      Arg::Var(done_tmp_var),
      after_loop_label,
      DUMMY_LABEL,
    ));

    let value_tmp_var = self.c_temp.bump();
    self.out.push(Inst::bin(
      value_tmp_var,
      Arg::Var(iter_result_tmp_var),
      BinOp::GetProp,
      Arg::Const(Const::Str("value".to_string())),
    ));
    self.compile_for_head(span, left, Arg::Var(value_tmp_var))?;

    self.break_stack.push(self.jump_target(after_loop_label));
    self.continue_stack.push(self.jump_target(loop_entry_label));
    if let Some(label) = label {
      self.label_stack.push(LabeledTarget {
        label,
        break_target: self.jump_target(after_loop_label),
        continue_target: Some(self.jump_target(loop_entry_label)),
      });
    }
    let res = self.compile_stmt(body);
    if label.is_some() {
      self.label_stack.pop();
    }
    self.continue_stack.pop();
    self.break_stack.pop();
    res?;

    self.out.push(Inst::goto(loop_entry_label));
    self.out.push(Inst::label(after_loop_label));
    Ok(())
  }

  fn compile_for_stmt(
    &mut self,
    _span: Loc,
    init: &Option<ForInit>,
    cond: &Option<ExprId>,
    post: &Option<ExprId>,
    body: StmtId,
    label: Option<NameId>,
  ) -> OptimizeResult<()> {
    match init {
      Some(ForInit::Expr(e)) => {
        self.compile_expr(*e)?;
      }
      Some(ForInit::Var(d)) => {
        self.compile_var_decl(d)?;
      }
      None => {}
    };
    let loop_entry_label = self.c_label.bump();
    let loop_continue_label = self.c_label.bump();
    let after_loop_label = self.c_label.bump();
    self.out.push(Inst::label(loop_entry_label));
    if let Some(cond) = cond {
      let cond_arg = self.compile_expr(*cond)?;
      self
        .out
        .push(Inst::cond_goto(cond_arg, DUMMY_LABEL, after_loop_label));
    };
    self.break_stack.push(self.jump_target(after_loop_label));
    self.continue_stack
      .push(self.jump_target(loop_continue_label));
    if let Some(label) = label {
      self.label_stack.push(LabeledTarget {
        label,
        break_target: self.jump_target(after_loop_label),
        continue_target: Some(self.jump_target(loop_continue_label)),
      });
    }
    let res = self.compile_stmt(body);
    if label.is_some() {
      self.label_stack.pop();
    }
    self.continue_stack.pop();
    self.break_stack.pop().unwrap();
    res?;
    self.out.push(Inst::label(loop_continue_label));
    if let Some(post) = post {
      self.compile_expr(*post)?;
    };
    self.out.push(Inst::goto(loop_entry_label));
    self.out.push(Inst::label(after_loop_label));
    Ok(())
  }

  fn compile_if_stmt(
    &mut self,
    _span: Loc,
    test: ExprId,
    consequent: StmtId,
    alternate: Option<StmtId>,
  ) -> OptimizeResult<()> {
    let known = self.expr_truthiness(test);
    let test_arg = self.compile_expr(test)?;
    if let Some(truthiness) = known {
      match truthiness {
        crate::types::Truthiness::AlwaysTruthy => {
          self.compile_stmt(consequent)?;
        }
        crate::types::Truthiness::AlwaysFalsy => {
          if let Some(alternate) = alternate {
            self.compile_stmt(alternate)?;
          }
        }
      }
      return Ok(());
    }
    match alternate {
      Some(alternate) => {
        let cons_label_id = self.c_label.bump();
        let after_label_id = self.c_label.bump();
        self
          .out
          .push(Inst::cond_goto(test_arg, cons_label_id, DUMMY_LABEL));
        self.compile_stmt(alternate)?;
        self.out.push(Inst::goto(after_label_id));
        self.out.push(Inst::label(cons_label_id));
        self.compile_stmt(consequent)?;
        self.out.push(Inst::label(after_label_id));
      }
      None => {
        let after_label_id = self.c_label.bump();
        self
          .out
          .push(Inst::cond_goto(test_arg, DUMMY_LABEL, after_label_id));
        self.compile_stmt(consequent)?;
        self.out.push(Inst::label(after_label_id));
      }
    };
    Ok(())
  }

  fn compile_switch_stmt(
    &mut self,
    span: Loc,
    discriminant: ExprId,
    cases: &[hir_js::hir::SwitchCase],
  ) -> OptimizeResult<()> {
    let discriminant_tmp_var = self.c_temp.bump();
    let discriminant_arg = self.compile_expr(discriminant)?;
    self.push_value_inst(
      discriminant,
      Inst::var_assign(discriminant_tmp_var, discriminant_arg),
    );

    if cases.is_empty() {
      return Ok(());
    }

    let after_switch_label = self.c_label.bump();
    self.break_stack.push(self.jump_target(after_switch_label));

    let mut case_labels = Vec::with_capacity(cases.len());
    let mut default_label = None;
    for case in cases.iter() {
      let label = self.c_label.bump();
      if case.test.is_none() && default_label.is_none() {
        default_label = Some(label);
      }
      case_labels.push(label);
    }

    let default_or_after = default_label.unwrap_or(after_switch_label);
    let test_indices: Vec<usize> = cases
      .iter()
      .enumerate()
      .filter_map(|(idx, case)| case.test.map(|_| idx))
      .collect();

    for (pos, &idx) in test_indices.iter().enumerate() {
      let test_expr = cases[idx].test.expect("case has test");
      let test_arg = self.compile_expr(test_expr)?;
      let cmp_tmp_var = self.c_temp.bump();
      self.out.push(Inst::bin(
        cmp_tmp_var,
        Arg::Var(discriminant_tmp_var),
        BinOp::StrictEq,
        test_arg,
      ));
      let fallthrough = if pos == test_indices.len() - 1 {
        default_or_after
      } else {
        DUMMY_LABEL
      };
      self.out.push(Inst::cond_goto(
        Arg::Var(cmp_tmp_var),
        case_labels[idx],
        fallthrough,
      ));
    }

    if test_indices.is_empty() {
      // Only a `default` clause can exist in this scenario, so always jump to it.
      self.out.push(Inst::goto(default_or_after));
    }

    for (idx, case) in cases.iter().enumerate() {
      self.out.push(Inst::label(case_labels[idx]));
      for stmt in case.consequent.iter() {
        self.compile_stmt(*stmt)?;
      }
    }

    self.break_stack.pop();
    self.out.push(Inst::label(after_switch_label));

    let _ = span;
    Ok(())
  }

  fn compile_do_while_stmt(
    &mut self,
    test: ExprId,
    body: StmtId,
    _span: Loc,
    label: Option<NameId>,
  ) -> OptimizeResult<()> {
    let loop_entry_label = self.c_label.bump();
    let loop_continue_label = self.c_label.bump();
    let after_loop_label = self.c_label.bump();
    self.out.push(Inst::label(loop_entry_label));
    self.break_stack.push(self.jump_target(after_loop_label));
    self.continue_stack
      .push(self.jump_target(loop_continue_label));
    if let Some(label) = label {
      self.label_stack.push(LabeledTarget {
        label,
        break_target: self.jump_target(after_loop_label),
        continue_target: Some(self.jump_target(loop_continue_label)),
      });
    }
    let res = self.compile_stmt(body);
    if label.is_some() {
      self.label_stack.pop();
    }
    self.continue_stack.pop();
    self.break_stack.pop();
    res?;
    self.out.push(Inst::label(loop_continue_label));
    let test_arg = self.compile_expr(test)?;
    self.out.push(Inst::cond_goto(
      test_arg,
      loop_entry_label,
      after_loop_label,
    ));
    self.out.push(Inst::label(after_loop_label));
    Ok(())
  }

  fn compile_while_stmt(
    &mut self,
    test: ExprId,
    body: StmtId,
    _span: Loc,
    label: Option<NameId>,
  ) -> OptimizeResult<()> {
    let before_test_label = self.c_label.bump();
    let after_loop_label = self.c_label.bump();
    self.out.push(Inst::label(before_test_label));
    let test_arg = self.compile_expr(test)?;
    self
      .out
      .push(Inst::cond_goto(test_arg, DUMMY_LABEL, after_loop_label));
    self.break_stack.push(self.jump_target(after_loop_label));
    self.continue_stack.push(self.jump_target(before_test_label));
    if let Some(label) = label {
      self.label_stack.push(LabeledTarget {
        label,
        break_target: self.jump_target(after_loop_label),
        continue_target: Some(self.jump_target(before_test_label)),
      });
    }
    let res = self.compile_stmt(body);
    if label.is_some() {
      self.label_stack.pop();
    }
    self.continue_stack.pop();
    self.break_stack.pop();
    res?;
    self.out.push(Inst::goto(before_test_label));
    self.out.push(Inst::label(after_loop_label));
    Ok(())
  }

  pub fn compile_stmt(&mut self, stmt_id: StmtId) -> OptimizeResult<()> {
    let file = self.program.lower.hir.file;
    let stmt = &self.body.stmts[stmt_id.0 as usize];
    let span = Loc(stmt.span.start as usize, stmt.span.end as usize);
    match &stmt.kind {
      StmtKind::Block(stmts) => {
        for stmt in stmts {
          self.compile_stmt(*stmt)?;
        }
        Ok(())
      }
      StmtKind::Break(label) => {
        let target = if let Some(label) = label {
          self
            .label_stack
            .iter()
            .rev()
            .find(|entry| entry.label == *label)
            .map(|entry| entry.break_target)
            .ok_or_else(|| {
              unsupported_syntax_range(
                file,
                stmt.span,
                format!("break to unknown label {}", self.name_for(*label)),
              )
            })?
        } else {
          self.break_stack.last().copied().ok_or_else(|| {
            unsupported_syntax_range(file, stmt.span, "break statement outside loop")
          })?
        };
        self.emit_goto(target);
        Ok(())
      }
      StmtKind::Continue(label) => {
        let target = if let Some(label) = label {
          let entry = self
            .label_stack
            .iter()
            .rev()
            .find(|entry| entry.label == *label)
            .ok_or_else(|| {
              unsupported_syntax_range(
                file,
                stmt.span,
                format!("continue to unknown label {}", self.name_for(*label)),
              )
            })?;
          entry.continue_target.ok_or_else(|| {
            unsupported_syntax_range(
              file,
              stmt.span,
              format!("continue to non-loop label {}", self.name_for(*label)),
            )
          })?
        } else {
          self.continue_stack.last().copied().ok_or_else(|| {
            unsupported_syntax_range(file, stmt.span, "continue statement outside loop")
          })?
        };
        self.emit_goto(target);
        Ok(())
      }
      StmtKind::Labeled { label, body } => {
        let body_kind = self.body.stmts[body.0 as usize].kind.clone();
        match body_kind {
          StmtKind::While { test, body } => self.compile_while_stmt(test, body, span, Some(*label)),
          StmtKind::DoWhile { test, body } => {
            self.compile_do_while_stmt(test, body, span, Some(*label))
          }
          StmtKind::For {
            init,
            test,
            update,
            body,
          } => self.compile_for_stmt(span, &init, &test, &update, body, Some(*label)),
          StmtKind::ForIn {
            left,
            right,
            body,
            is_for_of,
            await_,
          } => self.compile_for_in_of_stmt(
            stmt.span,
            &left,
            right,
            body,
            is_for_of,
            await_,
            Some(*label),
          ),
          _ => {
            let after_label = self.c_label.bump();
            self.label_stack.push(LabeledTarget {
              label: *label,
              break_target: self.jump_target(after_label),
              continue_target: None,
            });
            let res = self.compile_stmt(*body);
            self.label_stack.pop();
            res?;
            self.out.push(Inst::label(after_label));
            Ok(())
          }
        }
      }
      StmtKind::Return(value) => {
        if self.body.kind != BodyKind::Function {
          return Err(unsupported_syntax_range(
            file,
            stmt.span,
            "return statement outside function",
          ));
        }
        let value = match value {
          Some(expr) => Some(self.compile_expr(*expr)?),
          None => None,
        };
        self.emit_return(value);
        Ok(())
      }
      StmtKind::Throw(value) => {
        let value = self.compile_expr(*value)?;
        self.emit_throw(value);
        Ok(())
      }
      StmtKind::Expr(expr) => {
        self.compile_expr(*expr)?;
        Ok(())
      }
      StmtKind::ExportDefaultExpr(expr) => {
        self.compile_expr(*expr)?;
        Ok(())
      }
      StmtKind::For {
        init,
        test,
        update,
        body,
      } => self.compile_for_stmt(span, init, test, update, *body, None),
      StmtKind::ForIn {
        left,
        right,
        body,
        is_for_of,
        await_,
      } => self.compile_for_in_of_stmt(stmt.span, left, *right, *body, *is_for_of, *await_, None),
      StmtKind::If {
        test,
        consequent,
        alternate,
      } => self.compile_if_stmt(span, *test, *consequent, *alternate),
      StmtKind::Switch {
        discriminant,
        cases,
      } => self.compile_switch_stmt(span, *discriminant, cases),
      StmtKind::Try {
        block,
        catch,
        finally_block,
      } => self.compile_try_stmt(stmt.span, *block, catch.as_ref(), *finally_block),
      StmtKind::Var(decl) => self.compile_var_decl(decl),
      StmtKind::While { test, body } => self.compile_while_stmt(*test, *body, span, None),
      StmtKind::DoWhile { test, body } => self.compile_do_while_stmt(*test, *body, span, None),
      StmtKind::Debugger => Ok(()),
      StmtKind::Empty => Ok(()),
      StmtKind::Decl(_) => Ok(()),
      StmtKind::With { .. } => Err(unsupported_syntax_range(
        file,
        stmt.span,
        "with statements introduce dynamic scope and are not supported",
      )),
    }
  }
}

pub fn translate_body(
  program: &ProgramCompiler,
  body_id: BodyId,
) -> OptimizeResult<(Vec<Inst>, Counter, Counter, Vec<u32>)> {
  let mut compiler = HirSourceToInst::new(program, body_id);
  let mut params = Vec::new();
  if compiler.body.kind == BodyKind::Function {
    if let Some(function) = &compiler.body.function {
      let mut symbols = Vec::new();
      for param in function.params.iter() {
        compiler.collect_pat_binding_symbols(param.pat, &mut symbols);
      }
      for sym in symbols {
        params.push(compiler.symbol_to_temp(sym));
      }
    }
  }
  compiler.hoist_var_decls();
  compiler.hoist_function_decls()?;
  for stmt in root_statements(compiler.body) {
    compiler.compile_stmt(stmt)?;
  }
  if compiler.body.kind == BodyKind::Function {
    // Reaching the end of a function body in JS implicitly returns `undefined`.
    // We append this unconditionally; if the function already returns/throws on
    // all paths, CFG pruning will drop the unreachable trailing block.
    compiler.out.push(Inst::ret(None));
  }
  Ok((compiler.out, compiler.c_label, compiler.c_temp, params))
}
