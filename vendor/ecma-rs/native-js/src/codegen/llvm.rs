use crate::CompileOptions;
use parse_js::ast::expr::{CallArg, Expr};
use parse_js::ast::expr::pat::Pat;
use parse_js::ast::node::Node;
use parse_js::ast::stmt::Stmt;
use parse_js::ast::stx::TopLevel;
use parse_js::operator::OperatorName;
use std::collections::HashMap;

use super::builtins::{recognize_builtin, BuiltinCall};
use super::CodegenError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ty {
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

    self
      .interned
      .insert(bytes.to_vec(), (name.clone(), len));

    (name, len)
  }
}

struct Codegen {
  opts: CompileOptions,
  strings: StringPool,
  tmp_counter: usize,
  block_counter: usize,
  vars: HashMap<String, (Ty, String)>,
  main_body: Vec<String>,
}

impl Codegen {
  fn new(opts: CompileOptions) -> Self {
    Self {
      opts,
      strings: StringPool::default(),
      tmp_counter: 0,
      block_counter: 0,
      vars: HashMap::new(),
      main_body: Vec::new(),
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
    self.main_body.push(line.into());
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
        self.emit(format!("  call i32 @puts(ptr {empty})"));
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
        self.emit(format!("  call i32 @puts(ptr {sel})"));
        Ok(())
      }
      Ty::String => {
        self.emit(format!("  call i32 @puts(ptr {})", value.ir));
        Ok(())
      }
      Ty::Null => {
        let null_ptr = self.emit_string_ptr(b"null");
        self.emit(format!("  call i32 @puts(ptr {null_ptr})"));
        Ok(())
      }
      Ty::Undefined => {
        let undef_ptr = self.emit_string_ptr(b"undefined");
        self.emit(format!("  call i32 @puts(ptr {undef_ptr})"));
        Ok(())
      }
      Ty::Void => Err(CodegenError::TypeError(
        "cannot print a void expression".to_string(),
      )),
    }
  }

  fn emit_print_value_inline(&mut self, value: Value) -> Result<(), CodegenError> {
    match value.ty {
      Ty::Number => {
        self.emit_print_number_inline(&value.ir)
      }
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
          "  call i32 (ptr, ...) @printf(ptr {fmt}, ptr {sel})"
        ));
        Ok(())
      }
      Ty::String => {
        let fmt = self.emit_string_ptr(b"%s");
        self.emit(format!(
          "  call i32 (ptr, ...) @printf(ptr {fmt}, ptr {})",
          value.ir
        ));
        Ok(())
      }
      Ty::Null => {
        let fmt = self.emit_string_ptr(b"%s");
        let null_ptr = self.emit_string_ptr(b"null");
        self.emit(format!(
          "  call i32 (ptr, ...) @printf(ptr {fmt}, ptr {null_ptr})"
        ));
        Ok(())
      }
      Ty::Undefined => {
        let fmt = self.emit_string_ptr(b"%s");
        let undef_ptr = self.emit_string_ptr(b"undefined");
        self.emit(format!(
          "  call i32 (ptr, ...) @printf(ptr {fmt}, ptr {undef_ptr})"
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
    self.emit(format!(
      "  br i1 {is_nan}, label %{nan}, label %{not_nan}"
    ));

    let cont = self.fresh_block("print.num.cont");

    self.emit(format!("{nan}:"));
    {
      let fmt = self.emit_string_ptr(b"%s");
      let nan_ptr = self.emit_string_ptr(b"NaN");
      self.emit(format!(
        "  call i32 (ptr, ...) @printf(ptr {fmt}, ptr {nan_ptr})"
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
        "  call i32 (ptr, ...) @printf(ptr {fmt}, ptr {inf_ptr})"
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
        "  call i32 (ptr, ...) @printf(ptr {fmt}, ptr {inf_ptr})"
      ));
      self.emit(format!("  br label %{cont}"));
    }

    self.emit(format!("{finite}:"));
    {
      let fmt = self.emit_string_ptr(b"%g");
      self.emit(format!(
        "  call i32 (ptr, ...) @printf(ptr {fmt}, double {value_ir})"
      ));
      self.emit(format!("  br label %{cont}"));
    }

    self.emit(format!("{cont}:"));
    Ok(())
  }

  fn emit_strcmp_eq(&mut self, left: &str, right: &str) -> Result<String, CodegenError> {
    let cmp = self.tmp();
    self.emit(format!("  {cmp} = call i32 @strcmp(ptr {left}, ptr {right})"));
    let out = self.tmp();
    self.emit(format!("  {out} = icmp eq i32 {cmp}, 0"));
    Ok(out)
  }

  fn emit_print_log_call(&mut self, args: &[Node<CallArg>]) -> Result<(), CodegenError> {
    if args.is_empty() {
      let empty = self.emit_string_ptr(b"");
      self.emit(format!("  call i32 @puts(ptr {empty})"));
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
        self.emit(format!("  call i32 (ptr, ...) @printf(ptr {space})"));
      }
    }

    let empty = self.emit_string_ptr(b"");
    self.emit(format!("  call i32 @puts(ptr {empty})"));
    Ok(())
  }

  fn compile_stmt(&mut self, stmt: &Node<Stmt>) -> Result<(), CodegenError> {
    match stmt.stx.as_ref() {
      Stmt::Empty(_) => Ok(()),
      Stmt::Expr(expr_stmt) => {
        let _ = self.compile_expr(&expr_stmt.stx.expr)?;
        Ok(())
      }
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
        Ok(Value { ty: Ty::String, ir: ptr })
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
              _ => return Err(CodegenError::TypeError("invalid assignment target".to_string())),
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
              _ => return Err(CodegenError::TypeError("invalid assignment target".to_string())),
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
            self.emit(format!(
              "  {out} = fadd double {}, {}",
              left.ir, right.ir
            ));
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
            self.emit(format!(
              "  {out} = fsub double {}, {}",
              left.ir, right.ir
            ));
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
            self.emit(format!(
              "  {out} = fmul double {}, {}",
              left.ir, right.ir
            ));
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
            self.emit(format!(
              "  {out} = fdiv double {}, {}",
              left.ir, right.ir
            ));
            Ok(Value {
              ty: Ty::Number,
              ir: out,
            })
          }
          OperatorName::StrictEquality => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != right.ty {
              return Err(CodegenError::TypeError(
                "`===` currently requires both sides to have the same type".to_string(),
              ));
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
                self.emit(format!(
                  "  {out} = icmp eq i1 {}, {}",
                  left.ir, right.ir
                ));
              }
              Ty::String => {
                let eq = self.emit_strcmp_eq(&left.ir, &right.ir)?;
                return Ok(Value { ty: Ty::Bool, ir: eq });
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
            Ok(Value { ty: Ty::Bool, ir: out })
          }
          OperatorName::StrictInequality => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
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
                Ok(Value { ty: Ty::Bool, ir: out })
              }
              Ty::Bool => {
                let eq = self.tmp();
                self.emit(format!(
                  "  {eq} = icmp eq i1 {}, {}",
                  left.ir, right.ir
                ));
                let out = self.tmp();
                self.emit(format!("  {out} = xor i1 {eq}, true"));
                Ok(Value { ty: Ty::Bool, ir: out })
              }
              Ty::String => {
                let eq = self.emit_strcmp_eq(&left.ir, &right.ir)?;
                let out = self.tmp();
                self.emit(format!("  {out} = xor i1 {eq}, true"));
                Ok(Value { ty: Ty::Bool, ir: out })
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
            Ok(Value { ty: Ty::Bool, ir: out })
          }
          OperatorName::LogicalAnd | OperatorName::LogicalOr => {
            let left = self.compile_expr(&bin.stx.left)?;
            let right = self.compile_expr(&bin.stx.right)?;
            if left.ty != Ty::Bool || right.ty != Ty::Bool {
              return Err(CodegenError::TypeError(
                "logical operators currently only support booleans".to_string(),
              ));
            }
            let out = self.tmp();
            let op = match bin.stx.operator {
              OperatorName::LogicalAnd => "and",
              OperatorName::LogicalOr => "or",
              _ => unreachable!(),
            };
            self.emit(format!("  {out} = {op} i1 {}, {}", left.ir, right.ir));
            Ok(Value { ty: Ty::Bool, ir: out })
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
            if arg.ty != Ty::Bool {
              return Err(CodegenError::TypeError(
                "unary `!` currently only supports booleans".to_string(),
              ));
            }
            let out = self.tmp();
            self.emit(format!("  {out} = xor i1 {}, true", arg.ir));
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
              self.emit("  call i32 @fflush(ptr null)".to_string());
              Ok(Value::void())
            }
             BuiltinCall::Assert { cond, msg } => {
              let cond_v = self.compile_expr(cond)?;
              if cond_v.ty != Ty::Bool {
                return Err(CodegenError::TypeError(
                  "`assert` condition must be a boolean".to_string(),
                ));
              }

              let ok = self.fresh_block("assert.ok");
              let fail = self.fresh_block("assert.fail");
              self.emit(format!(
                "  br i1 {}, label %{ok}, label %{fail}",
                cond_v.ir
              ));

              self.emit(format!("{fail}:"));
              if let Some(msg) = msg {
                let msg_v = self.compile_expr(msg)?;
                self.emit_print_value(msg_v)?;
              }
              self.emit("  call i32 @fflush(ptr null)".to_string());
              self.emit("  call void @abort()".to_string());
              self.emit("  unreachable".to_string());

               self.emit(format!("{ok}:"));
               Ok(Value::void())
             }
             BuiltinCall::Panic { msg } => {
               if let Some(msg) = msg {
                 let msg_v = self.compile_expr(msg)?;
                 self.emit_print_value(msg_v)?;
               }
               self.emit("  call i32 @fflush(ptr null)".to_string());
               self.emit("  call void @abort()".to_string());
               self.emit("  unreachable".to_string());

               // Keep the IR structurally valid by starting a fresh (unreachable) block for any
               // subsequent statements / the implicit final `ret`.
               let cont = self.fresh_block("panic.after");
               self.emit(format!("{cont}:"));
               Ok(Value::void())
             }
             BuiltinCall::Trap => {
               self.emit("  call i32 @fflush(ptr null)".to_string());
               self.emit("  call void @llvm.trap()".to_string());
               self.emit("  unreachable".to_string());

               let cont = self.fresh_block("trap.after");
               self.emit(format!("{cont}:"));
               Ok(Value::void())
             }
           }
         } else {
           Err(CodegenError::UnsupportedExpr)
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
  let mut cg = Codegen::new(opts);

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

  out.push_str("define i32 @main() {\n");
  for line in &cg.main_body {
    out.push_str(line);
    out.push('\n');
  }
  out.push_str("}\n");

  Ok(out)
}
