use crate::analysis::async_elision::{await_operand, cfg_var_uses, AsyncElisionOptions};
use crate::analysis::value_types::ValueTypeSummaries;
use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, AwaitBehavior, Const, InstTyp, ValueTypeSummary};
use crate::opt::PassResult;

const INTERNAL_ARRAY_CALLEE: &str = "__optimize_js_array";
const PROMISE_ALL_CALLEE: &str = "Promise.all";

fn is_internal_array_call(inst: &crate::il::inst::Inst) -> Option<&[Arg]> {
  if inst.t != InstTyp::Call {
    return None;
  }
  let (_, callee, this, args, spreads) = inst.as_call();
  if !spreads.is_empty() {
    return None;
  }
  if !matches!(this, Arg::Const(Const::Undefined)) {
    return None;
  }
  if !matches!(callee, Arg::Builtin(path) if path == INTERNAL_ARRAY_CALLEE) {
    return None;
  }
  Some(args)
}

fn is_semantic_promise_all_call(inst: &crate::il::inst::Inst) -> Option<u32> {
  if inst.t != InstTyp::Call {
    return None;
  }
  let (_, callee, this, args, spreads) = inst.as_call();
  if !spreads.is_empty() {
    return None;
  }
  if !matches!(this, Arg::Const(Const::Undefined)) {
    return None;
  }
  if !matches!(callee, Arg::Builtin(path) if path == PROMISE_ALL_CALLEE) {
    return None;
  }
  if args.len() != 1 {
    return None;
  }
  match &args[0] {
    Arg::Var(v) => Some(*v),
    _ => None,
  }
}

fn rewrite_await_promise_all_singleton_or_empty_block(
  block: &mut Vec<crate::il::inst::Inst>,
  var_uses: &std::collections::BTreeMap<u32, usize>,
  options: AsyncElisionOptions,
) -> bool {
  if !options.rewrite {
    return false;
  }

  let mut changed = false;
  let mut i = 0usize;
  while i < block.len() {
    #[cfg(feature = "native-async-ops")]
    {
      if block[i].t == InstTyp::Await {
        // Handle `await PromiseAll(...)` lowered as first-class async ops.
        let Some(await_arg) = block[i].args.get(0).cloned() else {
          i += 1;
          continue;
        };
        let Arg::Var(promise_var) = await_arg else {
          i += 1;
          continue;
        };
        if var_uses.get(&promise_var).copied().unwrap_or(0) != 1 {
          i += 1;
          continue;
        }

        let Some(promise_idx) =
          (0..i).rev().find(|&idx| block[idx].tgts.get(0) == Some(&promise_var))
        else {
          i += 1;
          continue;
        };
        if block[promise_idx].t != InstTyp::PromiseAll {
          i += 1;
          continue;
        }

        match block[promise_idx].args.len() {
          0 => {
            // `await Promise.all([])` -> `await []`
            //
            // Rewrite the PromiseAll op into a literal `[]` allocation and await that array.
            let inst = &mut block[promise_idx];
            inst.t = InstTyp::Call;
            inst.args = vec![
              Arg::Builtin(INTERNAL_ARRAY_CALLEE.to_string()),
              Arg::Const(Const::Undefined),
            ];
            inst.spreads.clear();
            inst.meta.parallel = None;
            inst.meta.clear_result_var_metadata();
            inst.value_type = ValueTypeSummary::UNKNOWN;
            changed = true;
          }
          1 => {
            // `await Promise.all([p])` -> `await p; [value]`
            let elem = block[promise_idx].args[0].clone();

            // Rewrite PromiseAll temp instruction into `await elem`.
            {
              let inst = &mut block[promise_idx];
              inst.t = InstTyp::Await;
              inst.args = vec![elem];
              inst.spreads.clear();
              inst.meta.await_behavior = None;
              #[cfg(feature = "native-async-ops")]
              {
                inst.meta.await_known_resolved = false;
              }
              inst.meta.parallel = None;
              inst.meta.clear_result_var_metadata();
              inst.value_type = ValueTypeSummary::UNKNOWN;
            }

            // Rewrite the original await into an array literal of the awaited element.
            {
              let inst = &mut block[i];
              inst.t = InstTyp::Call;
              inst.args = vec![
                Arg::Builtin(INTERNAL_ARRAY_CALLEE.to_string()),
                Arg::Const(Const::Undefined),
                Arg::Var(promise_var),
              ];
              inst.spreads.clear();
              inst.meta.await_behavior = None;
              #[cfg(feature = "native-async-ops")]
              {
                inst.meta.await_known_resolved = false;
              }
            }

            changed = true;
          }
          _ => {}
        }

        i += 1;
        continue;
      }
    }

    // We only handle `await Promise.all([...])` where the Promise.all result is awaited exactly once.
    let Some(await_arg) = await_operand(&block[i]).cloned() else {
      i += 1;
      continue;
    };
    let Arg::Var(promise_var) = await_arg else {
      i += 1;
      continue;
    };
    if var_uses.get(&promise_var).copied().unwrap_or(0) != 1 {
      i += 1;
      continue;
    }

    // Find the defining instruction for the Promise.all temp in this block.
    let Some(promise_idx) = (0..i).rev().find(|&idx| block[idx].tgts.get(0) == Some(&promise_var)) else {
      i += 1;
      continue;
    };
    let Some(array_var) = is_semantic_promise_all_call(&block[promise_idx]) else {
      i += 1;
      continue;
    };

    // Find the defining instruction for the array literal temp in this block.
    let Some(array_idx) = (0..promise_idx)
      .rev()
      .find(|&idx| block[idx].tgts.get(0) == Some(&array_var))
    else {
      i += 1;
      continue;
    };
    let Some(array_elems) = is_internal_array_call(&block[array_idx]) else {
      i += 1;
      continue;
    };

    // Only handle literal arrays with no spreads (semantic-ops lowering guarantees this).
    match array_elems.len() {
      0 => {
        // `await Promise.all([])` -> `await []`
        //
        // We can reuse the array literal allocation as the awaited result (the input literal is
        // otherwise unreachable).
        //
        // - Rewrite the await argument from the Promise.all temp to the array literal temp.
        // - Remove the now-unused Promise.all call.
        block[i].args = vec![
          Arg::Builtin(crate::analysis::async_elision::INTERNAL_AWAIT_CALLEE.to_string()),
          Arg::Const(Const::Undefined),
          Arg::Var(array_var),
        ];
        block[i].meta.await_behavior = None;
        block[i].meta.parallel = None;

        // Remove Promise.all instruction.
        block.remove(promise_idx);
        changed = true;
        // Adjust i if we removed before the current element.
        if promise_idx < i {
          i -= 1;
        }
      }
      1 => {
        // `await Promise.all([p])` -> `await p; [value]`
        //
        // Reuse the Promise.all temp to store the awaited element value, then build the result
        // array in place of the original await instruction.
        let elem = array_elems[0].clone();

        // Rewrite Promise.all temp instruction into `await elem`.
        {
          let inst = &mut block[promise_idx];
          inst.args = vec![
            Arg::Builtin(crate::analysis::async_elision::INTERNAL_AWAIT_CALLEE.to_string()),
            Arg::Const(Const::Undefined),
            elem,
          ];
          inst.spreads.clear();
          inst.meta.await_behavior = None;
          inst.meta.parallel = None;
          // This instruction no longer corresponds to the original `Promise.all(...)` expression.
          inst.meta.clear_result_var_metadata();
          inst.value_type = ValueTypeSummary::UNKNOWN;
        }

        // Rewrite the original await into an array literal of the awaited element.
        {
          let inst = &mut block[i];
          inst.args = vec![
            Arg::Builtin(INTERNAL_ARRAY_CALLEE.to_string()),
            Arg::Const(Const::Undefined),
            Arg::Var(promise_var),
          ];
          inst.spreads.clear();
          // This is no longer an await.
          inst.meta.await_behavior = None;
          inst.meta.parallel = None;
        }

        // Remove the now-unused input array literal.
        block.remove(array_idx);
        changed = true;
        if array_idx < i {
          i -= 1;
        }
      }
      _ => {}
    }

    i += 1;
  }

  changed
}

pub fn optpass_async_elision(cfg: &mut Cfg, options: AsyncElisionOptions) -> PassResult {
  let mut result = PassResult::default();

  // Promise.all micro-optimizations (rewrite-only).
  let uses = cfg_var_uses(cfg);
  for label in cfg.graph.labels_sorted() {
    let changed = {
      let block = cfg.bblocks.get_mut(label);
      rewrite_await_promise_all_singleton_or_empty_block(block, &uses, options)
    };
    if changed {
      result.mark_changed();
    }
  }

  // Recompute value type summaries after any rewrites above.
  let types = ValueTypeSummaries::new(cfg);

  // Await classification + optional await removal.
  for label in cfg.graph.labels_sorted() {
    let block = cfg.bblocks.get_mut(label);
    for inst in block.iter_mut() {
      let Some(operand) = await_operand(inst).cloned() else {
        continue;
      };

      let behavior = if !options.aggressive {
        AwaitBehavior::MustYield
      } else {
        #[cfg(feature = "native-async-ops")]
        if inst.t == InstTyp::Await && inst.meta.await_known_resolved {
          // `await` on a value proven to be ready (by lowering/analysis) is eligible for yield
          // elision when aggressive mode is enabled.
          AwaitBehavior::MayNotYield
        } else {
          match types.arg(&operand) {
            Some(summary)
              if !summary.is_unknown()
                && !summary.contains(ValueTypeSummary::OBJECT)
                && !summary.contains(ValueTypeSummary::FUNCTION) =>
            {
              AwaitBehavior::MayNotYield
            }
            _ => AwaitBehavior::MustYield,
          }
        }
        #[cfg(not(feature = "native-async-ops"))]
        match types.arg(&operand) {
          Some(summary)
            if !summary.is_unknown()
              && !summary.contains(ValueTypeSummary::OBJECT)
              && !summary.contains(ValueTypeSummary::FUNCTION) =>
          {
            AwaitBehavior::MayNotYield
          }
          _ => AwaitBehavior::MustYield,
        }
      };

      let desired_meta = match behavior {
        AwaitBehavior::MustYield => None,
        AwaitBehavior::MayNotYield => Some(AwaitBehavior::MayNotYield),
      };

      if inst.meta.await_behavior != desired_meta {
        inst.meta.await_behavior = desired_meta;
        result.mark_changed();
      }

      if options.rewrite && behavior == AwaitBehavior::MayNotYield {
        // `await x` where x is proven non-thenable can be rewritten to a simple copy of x.
        //
        // Note: this is an opt-in semantic relaxation; strict ECMAScript semantics always yield.
        inst.t = InstTyp::VarAssign;
        inst.args = vec![operand];
        inst.spreads.clear();
        inst.meta.await_behavior = None;
        inst.meta.parallel = None;
        #[cfg(feature = "native-async-ops")]
        {
          inst.meta.await_known_resolved = false;
        }
        result.mark_changed();
      }
    }
  }

  result
}
