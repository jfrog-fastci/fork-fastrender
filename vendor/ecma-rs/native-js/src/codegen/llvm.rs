use crate::CompileOptions;
use parse_js::ast::expr::pat::Pat;
use parse_js::ast::expr::{CallArg, Expr};
use parse_js::ast::func::FuncBody;
use parse_js::ast::node::Node;
use parse_js::ast::stmt::decl::FuncDecl;
use parse_js::ast::stmt::Stmt;
use parse_js::ast::stx::TopLevel;
use parse_js::ast::type_expr::TypeExpr;
use parse_js::operator::OperatorName;
use std::collections::{BTreeMap, HashMap};

use super::builtins::{recognize_builtin, BuiltinCall};
use super::CodegenError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ty {
  Number,
  Bool,
  String,
  Null,
  Undefined,
  Void,
}

#[derive(Clone, Debug)]
struct Value {
  ty: Ty,
  ir: String,
}

impl Value {
  fn void() -> Self {
    Self {
      ty: Ty::Void,
      ir: String::new(),
    }
  }
}

fn f64_to_llvm_const(value: f64) -> String {
  format!("0x{:016X}", value.to_bits())
}

/// Signature information for a user-defined function that can be called from generated code.
#[derive(Clone, Debug)]
pub(crate) struct UserFunctionSig {
  /// LLVM symbol name (including the leading `@`).
  pub llvm_name: String,
  pub ret: Ty,
  pub params: Vec<Ty>,
}

#[derive(Default)]
struct StringPool {
  next_id: usize,
  // Map from raw bytes (without null terminator) to (global name, array length including terminator).
  interned: HashMap<Vec<u8>, (String, usize)>,
  defs: Vec<String>,
}

fn llvm_escape_bytes(bytes: &[u8]) -> String {
  let mut out = String::new();
  for &b in bytes {
    if (0x20..=0x7e).contains(&b) && b != b'"' && b != b'\\' {
      out.push(b as char);
    } else {
      out.push('\\');
      out.push_str(&format!("{b:02X}"));
    }
  }
  out
}

impl StringPool {
  fn intern(&mut self, bytes: &[u8]) -> (String, usize) {
    if let Some(existing) = self.interned.get(bytes) {
      return existing.clone();
    }

    let name = format!("@.str{}", self.next_id);
    self.next_id += 1;

    let mut with_null = bytes.to_vec();
    with_null.push(0);
    let len = with_null.len();
    let escaped = llvm_escape_bytes(&with_null);
    self.defs.push(format!(
      "{name} = private unnamed_addr constant [{len} x i8] c\"{escaped}\", align 1"
    ));

    self.interned.insert(bytes.to_vec(), (name.clone(), len));

    (name, len)
  }
}

struct Codegen {
  opts: CompileOptions,
  strings: StringPool,
  function_sigs: HashMap<String, FunctionSig>,
  function_llvm_names: HashMap<String, String>,
  function_defs: Vec<String>,
  tmp_counter: usize,
  block_counter: usize,
  vars: HashMap<String, (Ty, String)>,
  body: Vec<String>,
  /// If `Some`, we're compiling a user-defined function body and this is its return type.
  current_return_ty: Option<Ty>,
  /// Whether the current basic block is terminated (e.g. by `br`, `ret`, or `unreachable`).
  block_terminated: bool,
}

impl Codegen {
  fn new(opts: CompileOptions) -> Self {
    Self {
      opts,
      strings: StringPool::default(),
      function_sigs: HashMap::new(),
      function_llvm_names: HashMap::new(),
      function_defs: Vec::new(),
      tmp_counter: 0,
      block_counter: 0,
      vars: HashMap::new(),
      body: Vec::new(),
      current_return_ty: None,
      block_terminated: false,
    }
  }

  fn tmp(&mut self) -> String {
    let name = format!("%t{}", self.tmp_counter);
    self.tmp_counter += 1;
    name
  }

  fn fresh_block(&mut self, prefix: &str) -> String {
    let name = format!("{prefix}{}", self.block_counter);
    self.block_counter += 1;
    name
  }

  fn emit(&mut self, line: impl Into<String>) {
    let line = line.into();
    let trimmed = line.trim();
    // Basic block labels always end with `:`.
    if trimmed.ends_with(':') {
      self.block_terminated = false;
    } else {
      let inst = trimmed.trim_start();
      if inst.starts_with("br ") || inst.starts_with("ret ") || inst == "unreachable" {
        self.block_terminated = true;
      }
    }
    self.body.push(line);
  }

  fn llvm_type_of(ty: Ty) -> &'static str {
    match ty {
      Ty::Number => "double",
      Ty::Bool => "i1",
      Ty::String => "ptr",
      Ty::Null | Ty::Undefined => "i1",
      Ty::Void => "void",
    }
  }

  fn llvm_align_of(ty: Ty) -> u32 {
    match ty {
      Ty::Number => 8,
      Ty::Bool => 1,
      Ty::String => 8,
      Ty::Null | Ty::Undefined => 1,
      Ty::Void => 1,
    }
  }

  fn emit_alloca(&mut self, ty: Ty) -> Result<String, CodegenError> {
    if ty == Ty::Void {
      return Err(CodegenError::TypeError(
        "cannot allocate storage for void".to_string(),
      ));
    }
    let llvm_ty = Self::llvm_type_of(ty);
    let align = Self::llvm_align_of(ty);
    let out = self.tmp();
    self.emit(format!("  {out} = alloca {llvm_ty}, align {align}"));
    Ok(out)
  }

  fn emit_store(&mut self, ty: Ty, value_ir: &str, ptr_ir: &str) -> Result<(), CodegenError> {
    if ty == Ty::Void {
      return Err(CodegenError::TypeError(
        "cannot store a void value".to_string(),
      ));
    }
    let llvm_ty = Self::llvm_type_of(ty);
    let align = Self::llvm_align_of(ty);
    self.emit(format!(
      "  store {llvm_ty} {value_ir}, ptr {ptr_ir}, align {align}"
    ));
    Ok(())
  }

  fn emit_load(&mut self, ty: Ty, ptr_ir: &str) -> Result<String, CodegenError> {
    if ty == Ty::Void {
      return Err(CodegenError::TypeError(
        "cannot load a void value".to_string(),
      ));
    }
    let llvm_ty = Self::llvm_type_of(ty);
    let align = Self::llvm_align_of(ty);
    let out = self.tmp();
    self.emit(format!(
      "  {out} = load {llvm_ty}, ptr {ptr_ir}, align {align}"
    ));
    Ok(out)
  }

  fn emit_string_ptr(&mut self, bytes: &[u8]) -> String {
    let (global, len) = self.strings.intern(bytes);
    let tmp = self.tmp();
    self.emit(format!(
      "  {tmp} = getelementptr inbounds [{len} x i8], ptr {global}, i64 0, i64 0"
    ));
    tmp
  }

  fn emit_print_value(&mut self, value: Value) -> Result<(), CodegenError> {
    match value.ty {
      Ty::Number => {
        self.emit_print_number_inline(&value.ir)?;
        let empty = self.emit_string_ptr(b"");
        self.emit(format!("  notail call i32 @puts(ptr {empty})"));
        Ok(())
      }
      Ty::Bool => {
        let true_ptr = self.emit_string_ptr(b"true");
        let false_ptr = self.emit_string_ptr(b"false");
        let sel = self.tmp();
        self.emit(format!(
          "  {sel} = select i1 {}, ptr {true_ptr}, ptr {false_ptr}",
          value.ir
        ));
        self.emit(format!("  notail call i32 @puts(ptr {sel})"));
        Ok(())
      }
      Ty::String => {
        self.emit(format!("  notail call i32 @puts(ptr {})", value.ir));
        Ok(())
      }
      Ty::Null => {
        let null_ptr = self.emit_string_ptr(b"null");
        self.emit(format!("  notail call i32 @puts(ptr {null_ptr})"));
        Ok(())
      }
      Ty::Undefined => {
        let undef_ptr = self.emit_string_ptr(b"undefined");
        self.emit(format!("  notail call i32 @puts(ptr {undef_ptr})"));
        Ok(())
      }
      Ty::Void => Err(CodegenError::TypeError(
        "cannot print a void expression".to_string(),
      )),
    }
  }

  fn emit_print_value_inline(&mut self, value: Value) -> Result<(), CodegenError> {
    match value.ty {
      Ty::Number => self.emit_print_number_inline(&value.ir),
      Ty::Bool => {
        let true_ptr = self.emit_string_ptr(b"true");
        let false_ptr = self.emit_string_ptr(b"false");
        let sel = self.tmp();
        self.emit(format!(
          "  {sel} = select i1 {}, ptr {true_ptr}, ptr {false_ptr}",
          value.ir
        ));
        let fmt = self.emit_string_ptr(b"%s");
        self.emit(format!(
          "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {sel})"
        ));
        Ok(())
      }
      Ty::String => {
        let fmt = self.emit_string_ptr(b"%s");
        self.emit(format!(
          "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {})",
          value.ir
        ));
        Ok(())
      }
      Ty::Null => {
        let fmt = self.emit_string_ptr(b"%s");
        let null_ptr = self.emit_string_ptr(b"null");
        self.emit(format!(
          "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {null_ptr})"
        ));
        Ok(())
      }
      Ty::Undefined => {
        let fmt = self.emit_string_ptr(b"%s");
        let undef_ptr = self.emit_string_ptr(b"undefined");
        self.emit(format!(
          "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {undef_ptr})"
        ));
        Ok(())
      }
      Ty::Void => Err(CodegenError::TypeError(
        "cannot print a void expression".to_string(),
      )),
    }
  }

  fn emit_print_number_inline(&mut self, value_ir: &str) -> Result<(), CodegenError> {
    let is_nan = self.tmp();
    self.emit(format!(
      "  {is_nan} = fcmp uno double {value_ir}, {value_ir}"
    ));

    let nan = self.fresh_block("print.nan");
    let not_nan = self.fresh_block("print.not_nan");
    self.emit(format!("  br i1 {is_nan}, label %{nan}, label %{not_nan}"));

    let cont = self.fresh_block("print.num.cont");

    self.emit(format!("{nan}:"));
    {
      let fmt = self.emit_string_ptr(b"%s");
      let nan_ptr = self.emit_string_ptr(b"NaN");
      self.emit(format!(
        "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {nan_ptr})"
      ));
      self.emit(format!("  br label %{cont}"));
    }

    self.emit(format!("{not_nan}:"));
    let is_pos_inf = self.tmp();
    self.emit(format!(
      "  {is_pos_inf} = fcmp oeq double {value_ir}, {}",
      f64_to_llvm_const(f64::INFINITY)
    ));

    let pos_inf = self.fresh_block("print.pos_inf");
    let not_pos_inf = self.fresh_block("print.not_pos_inf");
    self.emit(format!(
      "  br i1 {is_pos_inf}, label %{pos_inf}, label %{not_pos_inf}"
    ));

    self.emit(format!("{pos_inf}:"));
    {
      let fmt = self.emit_string_ptr(b"%s");
      let inf_ptr = self.emit_string_ptr(b"Infinity");
      self.emit(format!(
        "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {inf_ptr})"
      ));
      self.emit(format!("  br label %{cont}"));
    }

    self.emit(format!("{not_pos_inf}:"));
    let is_neg_inf = self.tmp();
    self.emit(format!(
      "  {is_neg_inf} = fcmp oeq double {value_ir}, {}",
      f64_to_llvm_const(f64::NEG_INFINITY)
    ));

    let neg_inf = self.fresh_block("print.neg_inf");
    let finite = self.fresh_block("print.finite");
    self.emit(format!(
      "  br i1 {is_neg_inf}, label %{neg_inf}, label %{finite}"
    ));

    self.emit(format!("{neg_inf}:"));
    {
      let fmt = self.emit_string_ptr(b"%s");
      let inf_ptr = self.emit_string_ptr(b"-Infinity");
      self.emit(format!(
        "  notail call i32 (ptr, ...) @printf(ptr {fmt}, ptr {inf_ptr})"
      ));
      self.emit(format!("  br label %{cont}"));
    }

    self.emit(format!("{finite}:"));
    {
      // `%.15g` matches JS `Number#toString` reasonably well for debugging: it avoids the very
      // low default precision of `%g` while still keeping common values like `0.1` and `0.3`
      // readable.
      let fmt = self.emit_string_ptr(b"%.15g");
      self.emit(format!(
        "  notail call i32 (ptr, ...) @printf(ptr {fmt}, double {value_ir})"
      ));
      self.emit(format!("  br label %{cont}"));
    }

    self.emit(format!("{cont}:"));
    Ok(())
  }

  fn emit_strcmp_eq(&mut self, left: &str, right: &str) -> Result<String, CodegenError> {
    let cmp = self.tmp();
    self.emit(format!(
      "  {cmp} = notail call i32 @strcmp(ptr {left}, ptr {right})"
    ));
    let out = self.tmp();
    self.emit(format!("  {out} = icmp eq i32 {cmp}, 0"));
    Ok(out)
  }

  fn emit_print_log_call(&mut self, args: &[Node<CallArg>]) -> Result<(), CodegenError> {
    if args.is_empty() {
      let empty = self.emit_string_ptr(b"");
      self.emit(format!("  notail call i32 @puts(ptr {empty})"));
      return Ok(());
    }

    for (idx, arg) in args.iter().enumerate() {
      if arg.stx.spread {
        return Err(CodegenError::UnsupportedExpr);
      }
      let v = self.compile_expr(&arg.stx.value)?;
      self.emit_print_value_inline(v)?;
      if idx + 1 != args.len() {
        let space = self.emit_string_ptr(b" ");
        self.emit(format!("  notail call i32 (ptr, ...) @printf(ptr {space})"));
      }
    }

    let empty = self.emit_string_ptr(b"");
    self.emit(format!("  notail call i32 @puts(ptr {empty})"));
    Ok(())
  }

  fn emit_truthy_to_bool(&mut self, value: Value) -> Result<String, CodegenError> {
    match value.ty {
      Ty::Bool => Ok(value.ir),
      Ty::Number => {
        // JS truthiness: `0`, `-0`, and `NaN` are falsy; other numbers are truthy.
        let out = self.tmp();
        self.emit(format!(
          "  {out} = fcmp one double {}, {}",
          value.ir,
          f64_to_llvm_const(0.0)
        ));
        Ok(out)
      }
      Ty::String => {
        // JS truthiness: the empty string is falsy; all other strings are truthy.
        let first = self.tmp();
        self.emit(format!(
          "  {first} = load i8, ptr {}, align 1",
          value.ir
        ));
        let out = self.tmp();
        self.emit(format!("  {out} = icmp ne i8 {first}, 0"));
        Ok(out)
      }
      Ty::Null | Ty::Undefined => Ok("0".to_string()),
      Ty::Void => Err(CodegenError::TypeError(
        "cannot use a void expression as a condition".to_string(),
      )),
    }
  }

  fn compile_stmt(&mut self, stmt: &Node<Stmt>) -> Result<(), CodegenError> {
    // Never emit instructions after a terminator. If we do, LLVM will reject the IR.
    // Instead, start a fresh (unreachable) basic block.
    if self.block_terminated {
      let cont = self.fresh_block("after.term");
      self.emit(format!("{cont}:"));
    }

    match stmt.stx.as_ref() {
      Stmt::Block(block) => {
        for stmt in &block.stx.body {
          self.compile_stmt(stmt)?;
        }
        Ok(())
      }
      Stmt::Empty(_) => Ok(()),
      Stmt::Expr(expr_stmt) => {
        let _ = self.compile_expr(&expr_stmt.stx.expr)?;
        Ok(())
      }
      Stmt::If(if_stmt) => {
        let cond = self.compile_expr(&if_stmt.stx.test)?;
        let cond_bool = self.emit_truthy_to_bool(cond)?;

        let then_label = self.fresh_block("if.then");
        let else_label = self.fresh_block("if.else");
        let end_label = self.fresh_block("if.end");

        let false_label = if if_stmt.stx.alternate.is_some() {
          else_label.as_str()
        } else {
          end_label.as_str()
        };

        self.emit(format!(
          "  br i1 {}, label %{then_label}, label %{false_label}",
          cond_bool
        ));

        self.emit(format!("{then_label}:"));
        self.compile_stmt(&if_stmt.stx.consequent)?;
        if !self.block_terminated {
          self.emit(format!("  br label %{end_label}"));
        }

        if let Some(alt) = if_stmt.stx.alternate.as_ref() {
          self.emit(format!("{else_label}:"));
          self.compile_stmt(alt)?;
          if !self.block_terminated {
            self.emit(format!("  br label %{end_label}"));
          }
        }

        self.emit(format!("{end_label}:"));
        Ok(())
      }
      Stmt::While(while_stmt) => {
        let cond_label = self.fresh_block("while.cond");
        let body_label = self.fresh_block("while.body");
        let end_label = self.fresh_block("while.end");

        self.emit(format!("  br label %{cond_label}"));

        self.emit(format!("{cond_label}:"));
        let cond = self.compile_expr(&while_stmt.stx.condition)?;
        let cond_bool = self.emit_truthy_to_bool(cond)?;
        self.emit(format!(
          "  br i1 {}, label %{body_label}, label %{end_label}",
          cond_bool
        ));

        self.emit(format!("{body_label}:"));
        self.compile_stmt(&while_stmt.stx.body)?;
        if !self.block_terminated {
          self.emit(format!("  br label %{cond_label}"));
        }

        self.emit(format!("{end_label}:"));
        Ok(())
      }
      Stmt::Return(ret) => {
        let Some(expected) = self.current_return_ty else {
          return Err(CodegenError::TypeError(
            "`return` is not allowed at the top level".to_string(),
          ));
        };

        match (expected, ret.stx.value.as_ref()) {
          (Ty::Void, None) => {
            self.emit("  ret void".to_string());
            Ok(())
          }
          (Ty::Void, Some(_)) => Err(CodegenError::TypeError(
            "cannot return a value from a `void` function".to_string(),
          )),
          (expected, Some(expr)) => {
            let value = self.compile_expr(expr)?;
            if value.ty == Ty::Void {
              return Err(CodegenError::TypeError(
                "cannot return a void expression".to_string(),
              ));
            }
            if value.ty != expected {
              return Err(CodegenError::TypeError(format!(
                "return type mismatch: expected {expected:?}, got {got:?}",
                got = value.ty
              )));
            }

            let llvm_ty = Self::llvm_type_of(expected);
            let value_ir = match expected {
              Ty::Null | Ty::Undefined => "0".to_string(),
              _ => value.ir,
            };
            self.emit(format!("  ret {llvm_ty} {value_ir}"));
            Ok(())
          }
          (expected, None) => Err(CodegenError::TypeError(format!(
            "missing return value for function returning {expected:?}"
          ))),
        }
      }
      // Top-level function declarations are compiled separately (hoisted). We don't model nested
      // function declarations in the minimal emitter.
      Stmt::FunctionDecl(_) => Ok(()),
      Stmt::VarDecl(decl) => {
        for declarator in &decl.stx.declarators {
          let name = match declarator.pattern.stx.pat.stx.as_ref() {
            Pat::Id(id) => id.stx.name.clone(),
            _ => return Err(CodegenError::UnsupportedStmt),
          };

          let value = if let Some(init) = declarator.initializer.as_ref() {
            self.compile_expr(init)?
          } else {
            Value {
              ty: Ty::Undefined,
              ir: "0".to_string(),
            }
          };

          let slot = self.emit_alloca(value.ty)?;
          let store_val = match value.ty {
            Ty::Null | Ty::Undefined => "0",
            _ => value.ir.as_str(),
          };
          self.emit_store(value.ty, store_val, &slot)?;
          self.vars.insert(name, (value.ty, slot));
        }
        Ok(())
      }
      _ => Err(CodegenError::UnsupportedStmt),
    }
  }

  fn compile_expr(&mut self, expr: &Node<Expr>) -> Result<Value, CodegenError> {
    match expr.stx.as_ref() {
      Expr::LitNum(num) => Ok(Value {
        ty: Ty::Number,
        ir: f64_to_llvm_const(num.stx.value.0),
      }),
      Expr::LitBool(b) => Ok(Value {
        ty: Ty::Bool,
        ir: if b.stx.value { "1" } else { "0" }.to_string(),
      }),
      Expr::LitNull(_) => Ok(Value {
        ty: Ty::Null,
        ir: String::new(),
      }),
      Expr::LitStr(s) => {
        let ptr = self.emit_string_ptr(s.stx.value.as_bytes());
        Ok(Value {
          ty: Ty::String,
          ir: ptr,
        })
      }
      Expr::Id(id) => match id.stx.name.as_str() {
        name => {
          if let Some((ty, slot)) = self.vars.get(name).cloned() {
            match ty {
              Ty::Null | Ty::Undefined => {
                return Ok(Value {
                  ty,
                  ir: "0".to_string(),
                });
              }
              _ => {
                let loaded = self.emit_load(ty, &slot)?;
                return Ok(Value { ty, ir: loaded });
              }
            }
          }

          match name {
            "undefined" => Ok(Value {
              ty: Ty::Undefined,
              ir: String::new(),
            }),
            "NaN" => Ok(Value {
              ty: Ty::Number,
              ir: f64_to_llvm_const(f64::NAN),
            }),
            "Infinity" => Ok(Value {
              ty: Ty::Number,
              ir: f64_to_llvm_const(f64::INFINITY),
            }),
            _ => Err(CodegenError::UnsupportedExpr),
          }
        }
      },

      Expr::Binary(bin) => {
        match bin.stx.operator {
          OperatorName::Assignment => {
            let target = match bin.stx.left.stx.as_ref() {
              Expr::IdPat(id) => id.stx.name.as_str(),
              _ => {
                return Err(CodegenError::TypeError(
                  "invalid assignment target".to_string(),
                ))
              }
            };

            let rhs = self.compile_expr(&bin.stx.right)?;
            if rhs.ty == Ty::Void {
              return Err(CodegenError::TypeError(
                "cannot assign a void expression".to_string(),
              ));
            }

            if let Some((existing_ty, existing_slot)) = self.vars.get(target).cloned() {
              if existing_ty == rhs.ty {
                let store_val = match rhs.ty {
                  Ty::Null | Ty::Undefined => "0",
                  _ => rhs.ir.as_str(),
                };
                self.emit_store(rhs.ty, store_val, &existing_slot)?;
              } else {
                // The minimal `parse-js`-driven emitter doesn't typecheck; allow the binding's
                // type to change by allocating a fresh slot and updating the map.
                let new_slot = self.emit_alloca(rhs.ty)?;
                let store_val = match rhs.ty {
                  Ty::Null | Ty::Undefined => "0",
                  _ => rhs.ir.as_str(),
                };
                self.emit_store(rhs.ty, store_val, &new_slot)?;
                self.vars.insert(target.to_string(), (rhs.ty, new_slot));
              }
            } else {
              return Err(CodegenError::TypeError(format!(
                "assignment to undeclared variable `{target}`"
              )));
            }

            Ok(rhs)
          }
          OperatorName::AssignmentAddition => {
            let target = match bin.stx.left.stx.as_ref() {
              Expr::IdPat(id) => id.stx.name.as_str(),
              _ => {
                return Err(CodegenError::TypeError(
                  "invalid assignment target".to_string(),
                ))
              }
            };

            let (lhs_ty, lhs_slot) = self.vars.get(target).cloned().ok_or_else(|| {
              CodegenError::TypeError(format!("assignment to undeclared variable `{target}`"))
            })?;

            if lhs_ty != Ty::Number {
              return Err(CodegenError::TypeError(
                "operator `+=` currently only supports number variables".to_string(),
              ));
            }

            let rhs = self.compile_expr(&bin.stx.right)?;
            if rhs.ty != Ty::Number {
              return Err(CodegenError::TypeError(
                "operator `+=` currently only supports number RHS".to_string(),
              ));
            }

            let lhs_val = self.emit_load(Ty::Number, &lhs_slot)?;
            let out = self.tmp();
            self.emit(format!("  {out} = fadd double {lhs_val}, {}", rhs.ir));
            self.emit_store(Ty::Number, &out, &lhs_slot)?;

            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::Addition => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Number || right.ty != Ty::Number {
              return Err(CodegenError::TypeError(
                "binary `+` currently only supports numbers".to_string(),
              ));
            }
            let out = self.tmp();
            self.emit(format!("  {out} = fadd double {}, {}", left.ir, right.ir));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::Subtraction => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Number || right.ty != Ty::Number {
              return Err(CodegenError::TypeError(
                "binary `-` currently only supports numbers".to_string(),
              ));
            }
            let out = self.tmp();
            self.emit(format!("  {out} = fsub double {}, {}", left.ir, right.ir));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::Multiplication => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Number || right.ty != Ty::Number {
              return Err(CodegenError::TypeError(
                "binary `*` currently only supports numbers".to_string(),
              ));
            }
            let out = self.tmp();
            self.emit(format!("  {out} = fmul double {}, {}", left.ir, right.ir));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::Division => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Number || right.ty != Ty::Number {
              return Err(CodegenError::TypeError(
                "binary `/` currently only supports numbers".to_string(),
              ));
            }
            let out = self.tmp();
            self.emit(format!("  {out} = fdiv double {}, {}", left.ir, right.ir));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::StrictEquality => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty == Ty::Void || right.ty == Ty::Void {
              return Err(CodegenError::TypeError(
                "cannot compare a void expression".to_string(),
              ));
            }
            if left.ty != right.ty {
              // JS semantics: different types are always strictly not equal.
              return Ok(Value {
                ty: Ty::Bool,
                ir: "0".to_string(),
              });
            }
            let out = self.tmp();
            match left.ty {
              Ty::Number => {
                self.emit(format!(
                  "  {out} = fcmp oeq double {}, {}",
                  left.ir, right.ir
                ));
              }
              Ty::Bool => {
                self.emit(format!("  {out} = icmp eq i1 {}, {}", left.ir, right.ir));
              }
              Ty::String => {
                let eq = self.emit_strcmp_eq(&left.ir, &right.ir)?;
                return Ok(Value {
                  ty: Ty::Bool,
                  ir: eq,
                });
              }
              Ty::Null | Ty::Undefined => {
                // `null === null` and `undefined === undefined`.
                return Ok(Value {
                  ty: Ty::Bool,
                  ir: "1".to_string(),
                });
              }
              _ => {
                return Err(CodegenError::TypeError(
                  "`===` currently only supports numbers, booleans, strings, null, and undefined"
                    .to_string(),
                ));
              }
            }
            Ok(Value {
              ty: Ty::Bool,
              ir: out,
            })
          }
          OperatorName::StrictInequality => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty == Ty::Void || right.ty == Ty::Void {
              return Err(CodegenError::TypeError(
                "cannot compare a void expression".to_string(),
              ));
            }
            if left.ty != right.ty {
              // JS semantics: different types are always strictly not equal.
              return Ok(Value {
                ty: Ty::Bool,
                ir: "1".to_string(),
              });
            }

            match left.ty {
              Ty::Number => {
                let eq = self.tmp();
                self.emit(format!(
                  "  {eq} = fcmp oeq double {}, {}",
                  left.ir, right.ir
                ));
                let out = self.tmp();
                self.emit(format!("  {out} = xor i1 {eq}, true"));
                Ok(Value {
                  ty: Ty::Bool,
                  ir: out,
                })
              }
              Ty::Bool => {
                let eq = self.tmp();
                self.emit(format!("  {eq} = icmp eq i1 {}, {}", left.ir, right.ir));
                let out = self.tmp();
                self.emit(format!("  {out} = xor i1 {eq}, true"));
                Ok(Value {
                  ty: Ty::Bool,
                  ir: out,
                })
              }
              Ty::String => {
                let eq = self.emit_strcmp_eq(&left.ir, &right.ir)?;
                let out = self.tmp();
                self.emit(format!("  {out} = xor i1 {eq}, true"));
                Ok(Value {
                  ty: Ty::Bool,
                  ir: out,
                })
              }
              Ty::Null | Ty::Undefined => Ok(Value {
                ty: Ty::Bool,
                ir: "0".to_string(),
              }),
              _ => Err(CodegenError::TypeError(
                "`!==` currently only supports numbers, booleans, strings, null, and undefined"
                  .to_string(),
              )),
            }
          }
          OperatorName::LessThan
          | OperatorName::LessThanOrEqual
          | OperatorName::GreaterThan
          | OperatorName::GreaterThanOrEqual => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Number || right.ty != Ty::Number {
              return Err(CodegenError::TypeError(
                "numeric comparison currently only supports numbers".to_string(),
              ));
            }
            let out = self.tmp();
            let pred = match bin.stx.operator {
              OperatorName::LessThan => "olt",
              OperatorName::LessThanOrEqual => "ole",
              OperatorName::GreaterThan => "ogt",
              OperatorName::GreaterThanOrEqual => "oge",
              _ => unreachable!(),
            };
            self.emit(format!(
              "  {out} = fcmp {pred} double {}, {}",
              left.ir, right.ir
            ));
            Ok(Value {
              ty: Ty::Bool,
              ir: out,
            })
          }
          OperatorName::LogicalAnd | OperatorName::LogicalOr => {
            // Support short-circuit semantics for boolean-only `&&`/`||`.
            //
            // We implement this using a local alloca + stores instead of an SSA phi node so we
            // don't need to track the current basic block label name.
            let left = self.compile_expr(&bin.stx.left)?;
            if left.ty != Ty::Bool {
              return Err(CodegenError::TypeError(
                "logical operators currently only support booleans".to_string(),
              ));
            }

            let result_slot = self.emit_alloca(Ty::Bool)?;
            let rhs = self.fresh_block("logic.rhs");
            let short = self.fresh_block("logic.short");
            let cont = self.fresh_block("logic.cont");

            match bin.stx.operator {
              OperatorName::LogicalAnd => {
                // false && rhs  => false (skip rhs)
                self.emit(format!(
                  "  br i1 {}, label %{rhs}, label %{short}",
                  left.ir
                ));
                self.emit(format!("{short}:"));
                self.emit_store(Ty::Bool, "0", &result_slot)?;
                self.emit(format!("  br label %{cont}"));
              }
              OperatorName::LogicalOr => {
                // true || rhs => true (skip rhs)
                self.emit(format!(
                  "  br i1 {}, label %{short}, label %{rhs}",
                  left.ir
                ));
                self.emit(format!("{short}:"));
                self.emit_store(Ty::Bool, "1", &result_slot)?;
                self.emit(format!("  br label %{cont}"));
              }
              _ => unreachable!(),
            }

            self.emit(format!("{rhs}:"));
            let right = self.compile_expr(&bin.stx.right)?;
            if right.ty != Ty::Bool {
              return Err(CodegenError::TypeError(
                "logical operators currently only support booleans".to_string(),
              ));
            }
            self.emit_store(Ty::Bool, right.ir.as_str(), &result_slot)?;
            self.emit(format!("  br label %{cont}"));

            self.emit(format!("{cont}:"));
            let loaded = self.emit_load(Ty::Bool, &result_slot)?;
            Ok(Value {
              ty: Ty::Bool,
              ir: loaded,
            })
          }
          other => Err(CodegenError::UnsupportedOperator(other)),
        }
      }

      Expr::Unary(unary) => {
        let arg = self.compile_expr(&unary.stx.argument)?;
        match unary.stx.operator {
          OperatorName::UnaryNegation => {
            if arg.ty != Ty::Number {
              return Err(CodegenError::TypeError(
                "unary `-` currently only supports numbers".to_string(),
              ));
            }
            let out = self.tmp();
            self.emit(format!("  {out} = fneg double {}", arg.ir));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::UnaryPlus => {
            if arg.ty != Ty::Number {
              return Err(CodegenError::TypeError(
                "unary `+` currently only supports numbers".to_string(),
              ));
            }
            Ok(arg)
          }
          OperatorName::LogicalNot => {
            let arg_bool = self.emit_truthy_to_bool(arg)?;
            let out = self.tmp();
            self.emit(format!("  {out} = xor i1 {arg_bool}, true"));
            Ok(Value {
              ty: Ty::Bool,
              ir: out,
            })
          }
          other => Err(CodegenError::UnsupportedOperator(other)),
        }
      }

      Expr::Call(call) => {
        let builtin = recognize_builtin(call);
        if let Some(builtin) = builtin {
          if !self.opts.builtins {
            return Err(CodegenError::BuiltinsDisabled);
          }

          match builtin {
            BuiltinCall::Print { args } => {
              self.emit_print_log_call(args)?;
              // Make stdout useful for debugging even when the program later traps (e.g. SIGSEGV).
              self.emit("  notail call i32 @fflush(ptr null)".to_string());
              Ok(Value::void())
            }
            BuiltinCall::Assert { cond, msg } => {
              let cond_v = self.compile_expr(cond)?;
              let cond_bool = self.emit_truthy_to_bool(cond_v)?;

              let ok = self.fresh_block("assert.ok");
              let fail = self.fresh_block("assert.fail");
              self.emit(format!(
                "  br i1 {cond_bool}, label %{ok}, label %{fail}"
              ));

              self.emit(format!("{fail}:"));
              if let Some(msg) = msg {
                let msg_v = self.compile_expr(msg)?;
                self.emit_print_value(msg_v)?;
              } else {
                let default_msg = self.emit_string_ptr(b"assertion failed");
                self.emit(format!("  notail call i32 @puts(ptr {default_msg})"));
              }
              self.emit("  notail call i32 @fflush(ptr null)".to_string());
              self.emit("  notail call void @abort()".to_string());
              self.emit("  unreachable".to_string());

              self.emit(format!("{ok}:"));
              Ok(Value::void())
            }
            BuiltinCall::Panic { msg } => {
              if let Some(msg) = msg {
                let msg_v = self.compile_expr(msg)?;
                self.emit_print_value(msg_v)?;
              }
              self.emit("  notail call i32 @fflush(ptr null)".to_string());
              self.emit("  notail call void @abort()".to_string());
              self.emit("  unreachable".to_string());

              // Keep the IR structurally valid by starting a fresh (unreachable) block for any
              // subsequent statements / the implicit final `ret`.
              let cont = self.fresh_block("panic.after");
              self.emit(format!("{cont}:"));
              Ok(Value::void())
            }
            BuiltinCall::Trap => {
              self.emit("  notail call i32 @fflush(ptr null)".to_string());
              self.emit("  notail call void @llvm.trap()".to_string());
              self.emit("  unreachable".to_string());

              let cont = self.fresh_block("trap.after");
              self.emit(format!("{cont}:"));
              Ok(Value::void())
            }
          }
        } else {
          // Minimal support for direct calls to user-defined functions.
          if call.stx.optional_chaining {
            return Err(CodegenError::UnsupportedExpr);
          }

          let callee = match call.stx.callee.stx.as_ref() {
            Expr::Id(id) => id.stx.name.as_str(),
            _ => return Err(CodegenError::UnsupportedExpr),
          };

          let sig = self.function_sigs.get(callee).cloned().ok_or_else(|| {
            CodegenError::TypeError(format!("call to unknown function `{callee}`"))
          })?;
          let llvm_name = self
            .function_llvm_names
            .get(callee)
            .cloned()
            .expect("collected function LLVM names earlier");

          if sig.params.len() != call.stx.arguments.len() {
            return Err(CodegenError::TypeError(format!(
              "function `{callee}` expects {} args, got {}",
              sig.params.len(),
              call.stx.arguments.len()
            )));
          }

          let mut arg_irs = Vec::with_capacity(sig.params.len());
          for (idx, (param_ty, arg)) in sig.params.iter().zip(&call.stx.arguments).enumerate() {
            if arg.stx.spread {
              return Err(CodegenError::UnsupportedExpr);
            }
            let v = self.compile_expr(&arg.stx.value)?;
            if v.ty != *param_ty {
              return Err(CodegenError::TypeError(format!(
                "argument {idx} to `{callee}` has type {got:?}, expected {expected:?}",
                got = v.ty,
                expected = param_ty
              )));
            }
            let llvm_ty = Self::llvm_type_of(*param_ty);
            let value_ir = match v.ty {
              Ty::Null | Ty::Undefined => "0".to_string(),
              _ => v.ir,
            };
            arg_irs.push(format!("{llvm_ty} {value_ir}"));
          }

          let ret_ty = sig.ret;
          if ret_ty == Ty::Void {
            self.emit(format!(
              "  notail call void {llvm_name}({})",
              arg_irs.join(", ")
            ));
            Ok(Value::void())
          } else {
            let out = self.tmp();
            let llvm_ret = Self::llvm_type_of(ret_ty);
            self.emit(format!(
              "  {out} = notail call {llvm_ret} {llvm_name}({})",
              arg_irs.join(", ")
            ));
            Ok(Value {
              ty: ret_ty,
              ir: out,
            })
          }
        }
      }

      _ => Err(CodegenError::UnsupportedExpr),
    }
  }
}

pub(super) fn emit_llvm_module(
  ast: &Node<TopLevel>,
  opts: CompileOptions,
) -> Result<String, CodegenError> {
  // The minimal parse-js-driven emitter is intended for single-module programs. Module-level
  // `import`/`export` syntax requires project compilation so we can build a module graph, resolve
  // bindings, and order initializers deterministically.
  //
  // Scan upfront so we return `UnsupportedStmt` consistently (instead of e.g. failing with
  // "call to unknown function" while compiling a function body that references an imported
  // binding). `native-js-cli` relies on this error to decide when to fall back to the project
  // pipeline.
  for stmt in &ast.stx.body {
    match stmt.stx.as_ref() {
      Stmt::Import(_)
      | Stmt::ExportList(_)
      | Stmt::ExportDefaultExpr(_)
      | Stmt::ExportAssignmentDecl(_)
      | Stmt::ExportAsNamespaceDecl(_)
      | Stmt::ExportTypeDecl(_)
      | Stmt::ImportTypeDecl(_)
      | Stmt::ImportEqualsDecl(_) => return Err(CodegenError::UnsupportedStmt),
      _ => {}
    }
  }

  let mut cg = Codegen::new(opts);

  cg.collect_function_signatures(ast)?;
  cg.compile_function_decls(ast)?;

  cg.reset_fn_ctx(None);
  cg.emit("entry:");
  for stmt in &ast.stx.body {
    cg.compile_stmt(stmt)?;
  }
  cg.emit("  ret i32 0");

  let mut out = String::new();
  out.push_str("; ModuleID = 'native-js'\n");
  out.push_str("source_filename = \"native-js\"\n\n");

  for def in &cg.strings.defs {
    out.push_str(def);
    out.push('\n');
  }
  if !cg.strings.defs.is_empty() {
    out.push('\n');
  }

  out.push_str("declare i32 @puts(ptr)\n");
  out.push_str("declare i32 @printf(ptr, ...)\n");
  out.push_str("declare i32 @fflush(ptr)\n");
  out.push_str("declare i32 @strcmp(ptr, ptr)\n");
  out.push_str("declare void @abort()\n");
  out.push_str("declare void @llvm.trap()\n\n");

  for func in &cg.function_defs {
    out.push_str(func);
    out.push('\n');
  }

  // Stack-walkability invariants for precise GC:
  // - Keep frame pointers so the runtime can walk the frame chain.
  // - Disable tail calls so frames are not elided.
  //
  // See `native-js/docs/gc_stack_walking.md`.
  out.push_str("define i32 @main() #0 {\n");
  for line in &cg.body {
    out.push_str(line);
    out.push('\n');
  }
  out.push_str("}\n");
  out.push_str("\nattributes #0 = { \"frame-pointer\"=\"all\" \"disable-tail-calls\"=\"true\" }\n");

  Ok(out)
}

#[derive(Clone, Debug)]
struct FunctionSig {
  ret: Ty,
  params: Vec<Ty>,
}

#[derive(Clone, Debug)]
struct FnCtx {
  tmp_counter: usize,
  block_counter: usize,
  vars: HashMap<String, (Ty, String)>,
  body: Vec<String>,
  current_return_ty: Option<Ty>,
  block_terminated: bool,
}

impl Codegen {
  fn reset_fn_ctx(&mut self, ret: Option<Ty>) {
    self.tmp_counter = 0;
    self.block_counter = 0;
    self.vars.clear();
    self.body.clear();
    self.current_return_ty = ret;
    self.block_terminated = false;
  }

  fn take_fn_ctx(&mut self) -> FnCtx {
    FnCtx {
      tmp_counter: self.tmp_counter,
      block_counter: self.block_counter,
      vars: std::mem::take(&mut self.vars),
      body: std::mem::take(&mut self.body),
      current_return_ty: self.current_return_ty,
      block_terminated: self.block_terminated,
    }
  }

  fn restore_fn_ctx(&mut self, ctx: FnCtx) {
    self.tmp_counter = ctx.tmp_counter;
    self.block_counter = ctx.block_counter;
    self.vars = ctx.vars;
    self.body = ctx.body;
    self.current_return_ty = ctx.current_return_ty;
    self.block_terminated = ctx.block_terminated;
  }

  fn type_from_type_expr(ty: &Node<TypeExpr>) -> Result<Ty, CodegenError> {
    match ty.stx.as_ref() {
      TypeExpr::Number(_) => Ok(Ty::Number),
      TypeExpr::Boolean(_) => Ok(Ty::Bool),
      TypeExpr::String(_) => Ok(Ty::String),
      TypeExpr::Void(_) => Ok(Ty::Void),
      TypeExpr::Null(_) => Ok(Ty::Null),
      TypeExpr::Undefined(_) => Ok(Ty::Undefined),
      other => Err(CodegenError::TypeError(format!(
        "unsupported type annotation: {other:?}"
      ))),
    }
  }

  fn default_value_ir(ty: Ty) -> String {
    match ty {
      Ty::Number => f64_to_llvm_const(0.0),
      Ty::Bool => "0".to_string(),
      Ty::String => "null".to_string(),
      Ty::Null | Ty::Undefined => "0".to_string(),
      Ty::Void => String::new(),
    }
  }

  fn collect_function_signatures(&mut self, ast: &Node<TopLevel>) -> Result<(), CodegenError> {
    for stmt in &ast.stx.body {
      let Stmt::FunctionDecl(decl) = stmt.stx.as_ref() else {
        continue;
      };
      let Some(name) = decl.stx.name.as_ref().map(|n| n.stx.name.clone()) else {
        return Err(CodegenError::TypeError(
          "function declarations must have a name".to_string(),
        ));
      };
      if name == "main" {
        return Err(CodegenError::TypeError(
          "`main` is reserved for the native entrypoint; use a different function name".to_string(),
        ));
      }
      if self.function_sigs.contains_key(&name) {
        return Err(CodegenError::TypeError(format!(
          "duplicate function declaration `{name}`"
        )));
      }

      let func = &decl.stx.function;
      if func.stx.async_ || func.stx.generator {
        return Err(CodegenError::TypeError(format!(
          "function `{name}` must not be async or a generator"
        )));
      }

      let ret = match func.stx.return_type.as_ref() {
        Some(ret) => Self::type_from_type_expr(ret)?,
        None => Ty::Number,
      };

      let mut params = Vec::new();
      for param in &func.stx.parameters {
        if param.stx.rest || param.stx.optional {
          return Err(CodegenError::TypeError(format!(
            "function `{name}` has unsupported parameter syntax"
          )));
        }
        let param_ty = match param.stx.type_annotation.as_ref() {
          Some(ann) => Self::type_from_type_expr(ann)?,
          None => Ty::Number,
        };
        params.push(param_ty);
      }

      self
        .function_llvm_names
        .insert(name.clone(), format!("@{name}"));
      self.function_sigs.insert(name, FunctionSig { ret, params });
    }
    Ok(())
  }

  fn compile_function_decls(&mut self, ast: &Node<TopLevel>) -> Result<(), CodegenError> {
    for stmt in &ast.stx.body {
      let Stmt::FunctionDecl(decl) = stmt.stx.as_ref() else {
        continue;
      };
      self.compile_function_decl(decl)?;
    }
    Ok(())
  }

  fn compile_function_decl(&mut self, decl: &Node<FuncDecl>) -> Result<(), CodegenError> {
    let Some(name) = decl.stx.name.as_ref().map(|n| n.stx.name.clone()) else {
      return Err(CodegenError::TypeError(
        "function declarations must have a name".to_string(),
      ));
    };
    let sig = self
      .function_sigs
      .get(&name)
      .expect("collected function signatures earlier")
      .clone();

    let saved = self.take_fn_ctx();
    self.reset_fn_ctx(Some(sig.ret));

    // Emit function prologue.
    self.emit("entry:");

    let mut param_decls = Vec::new();
    // Map parameters into local slots, so we can use the same variable lookup logic as locals.
    for (idx, param) in decl.stx.function.stx.parameters.iter().enumerate() {
      let param_name = match param.stx.pattern.stx.pat.stx.as_ref() {
        Pat::Id(id) => id.stx.name.clone(),
        _ => {
          return Err(CodegenError::TypeError(format!(
            "function `{name}` parameter {idx} must be an identifier"
          )))
        }
      };

      let expected_ty = sig
        .params
        .get(idx)
        .copied()
        .ok_or_else(|| CodegenError::TypeError("parameter list mismatch".to_string()))?;
      let llvm_ty = Self::llvm_type_of(expected_ty);
      param_decls.push(format!("{llvm_ty} %{param_name}"));

      let slot = self.emit_alloca(expected_ty)?;
      self.emit_store(expected_ty, &format!("%{param_name}"), &slot)?;
      self.vars.insert(param_name, (expected_ty, slot));
    }

    match decl.stx.function.stx.body.as_ref() {
      Some(FuncBody::Block(stmts)) => {
        for stmt in stmts {
          self.compile_stmt(stmt)?;
        }
      }
      Some(FuncBody::Expression(expr)) => {
        let value = self.compile_expr(expr)?;
        if value.ty != sig.ret {
          return Err(CodegenError::TypeError(format!(
            "function `{name}` returns {got:?}, expected {expected:?}",
            got = value.ty,
            expected = sig.ret
          )));
        }
        let llvm_ty = Self::llvm_type_of(sig.ret);
        let value_ir = match sig.ret {
          Ty::Null | Ty::Undefined => "0".to_string(),
          _ => value.ir,
        };
        self.emit(format!("  ret {llvm_ty} {value_ir}"));
      }
      None => {}
    }

    // Ensure the function is well-formed even if the source forgot a `return`.
    if !self.block_terminated {
      match sig.ret {
        Ty::Void => self.emit("  ret void".to_string()),
        other => {
          let llvm_ty = Self::llvm_type_of(other);
          let value_ir = Self::default_value_ir(other);
          self.emit(format!("  ret {llvm_ty} {value_ir}"));
        }
      }
    }

    let llvm_name = self
      .function_llvm_names
      .get(&name)
      .expect("collected function LLVM names earlier");
    let mut def = String::new();
    def.push_str(&format!(
      "define {} {llvm_name}({}) #0 {{\n",
      Self::llvm_type_of(sig.ret),
      param_decls.join(", ")
    ));
    for line in &self.body {
      def.push_str(line);
      def.push('\n');
    }
    def.push_str("}\n");
    self.function_defs.push(def);

    self.restore_fn_ctx(saved);
    Ok(())
  }
}

pub(crate) struct LlvmModuleBuilder {
  cg: Codegen,
}

impl LlvmModuleBuilder {
  pub(crate) fn new(opts: CompileOptions) -> Self {
    Self { cg: Codegen::new(opts) }
  }

  fn with_call_targets<T>(
    &mut self,
    call_targets: &BTreeMap<String, UserFunctionSig>,
    f: impl FnOnce(&mut Codegen) -> Result<T, CodegenError>,
  ) -> Result<T, CodegenError> {
    let saved_sigs = std::mem::take(&mut self.cg.function_sigs);
    let saved_names = std::mem::take(&mut self.cg.function_llvm_names);

    for (local, sig) in call_targets {
      self.cg.function_sigs.insert(
        local.clone(),
        FunctionSig {
          ret: sig.ret,
          params: sig.params.clone(),
        },
      );
      self
        .cg
        .function_llvm_names
        .insert(local.clone(), sig.llvm_name.clone());
    }

    let out = f(&mut self.cg);

    self.cg.function_sigs = saved_sigs;
    self.cg.function_llvm_names = saved_names;

    out
  }

  pub(crate) fn add_init_function(
    &mut self,
    llvm_name: &str,
    stmts: &[&Node<Stmt>],
    call_targets: &BTreeMap<String, UserFunctionSig>,
  ) -> Result<(), CodegenError> {
    self.with_call_targets(call_targets, |cg| {
      let saved = cg.take_fn_ctx();
      cg.reset_fn_ctx(Some(Ty::Void));
      cg.emit("entry:");
      for stmt in stmts {
        cg.compile_stmt(stmt)?;
      }
      if !cg.block_terminated {
        cg.emit("  ret void".to_string());
      }

      let mut def = String::new();
      def.push_str(&format!("define void {llvm_name}() #0 {{\n"));
      for line in &cg.body {
        def.push_str(line);
        def.push('\n');
      }
      def.push_str("}\n");
      cg.function_defs.push(def);

      cg.restore_fn_ctx(saved);
      Ok(())
    })
  }

  pub(crate) fn add_ts_function(
    &mut self,
    _llvm_name: &str,
    decl: &Node<FuncDecl>,
    call_targets: &BTreeMap<String, UserFunctionSig>,
  ) -> Result<(), CodegenError> {
    self.with_call_targets(call_targets, |cg| cg.compile_function_decl(decl))
  }

  pub(crate) fn add_main(
    &mut self,
    init_symbols: &[String],
    entry_call: Option<&UserFunctionSig>,
  ) -> Result<(), CodegenError> {
    self.cg.reset_fn_ctx(None);
    self.cg.emit("entry:");
    for init in init_symbols {
      self.cg.emit(format!("  call void {init}()"));
    }
    if let Some(entry) = entry_call {
      let ret = Codegen::llvm_type_of(entry.ret);
      self.cg
        .emit(format!("  call {ret} {}()", entry.llvm_name));
    }
    self.cg.emit("  ret i32 0");
    Ok(())
  }

  pub(crate) fn finish(self) -> String {
    let mut out = String::new();
    out.push_str("; ModuleID = 'native-js'\n");
    out.push_str("source_filename = \"native-js\"\n\n");

    for def in &self.cg.strings.defs {
      out.push_str(def);
      out.push('\n');
    }
    if !self.cg.strings.defs.is_empty() {
      out.push('\n');
    }

    out.push_str("declare i32 @puts(ptr)\n");
    out.push_str("declare i32 @printf(ptr, ...)\n");
    out.push_str("declare i32 @fflush(ptr)\n");
    out.push_str("declare i32 @strcmp(ptr, ptr)\n");
    out.push_str("declare void @abort()\n");
    out.push_str("declare void @llvm.trap()\n\n");

    for func in &self.cg.function_defs {
      out.push_str(func);
      out.push('\n');
    }

    // Stack-walkability invariants for precise GC:
    // - Keep frame pointers so the runtime can walk the frame chain.
    // - Disable tail calls so frames are not elided.
    //
    // See `native-js/docs/gc_stack_walking.md`.
    out.push_str("define i32 @main() #0 {\n");
    for line in &self.cg.body {
      out.push_str(line);
      out.push('\n');
    }
    out.push_str("}\n");
    out.push_str("\nattributes #0 = { \"frame-pointer\"=\"all\" \"disable-tail-calls\"=\"true\" }\n");

    out
  }
}
