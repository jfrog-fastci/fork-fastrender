use crate::cfg::cfg::Cfg;
use crate::opt::PassResult;

/// Elide async operations that are proven not to suspend.
///
/// Currently this only handles `Await` when `InstMeta.await_known_resolved` is true.
#[cfg(feature = "native-async-ops")]
pub fn optpass_async_elide(cfg: &mut Cfg) -> PassResult {
  use crate::il::inst::{Inst, InstTyp};

  let mut result = PassResult::default();

  for (_, block) in cfg.bblocks.all_mut() {
    let mut idx = 0;
    while idx < block.len() {
      let inst = &block[idx];
      if inst.t != InstTyp::Await || !inst.meta.await_known_resolved {
        idx += 1;
        continue;
      }

      // A known-resolved await cannot suspend, so it is equivalent to forwarding the awaited value.
      //
      // If the awaited value is unused (`tgts` was cleared by DCE), the await itself becomes a no-op
      // and can be removed entirely.
      let Some(&tgt) = inst.tgts.get(0) else {
        block.remove(idx);
        result.mark_changed();
        continue;
      };

      let [value] = inst.args.as_slice() else {
        panic!("await expects exactly 1 arg");
      };

      let mut new_inst = Inst::var_assign(tgt, value.clone());
      // Preserve lowering metadata (HIR expr id, type ids, etc) so downstream analyses/codegen
      // still see a value-defining instruction at this program point.
      new_inst.meta = inst.meta.clone();
      new_inst.meta.await_known_resolved = false;
      block[idx] = new_inst;
      result.mark_changed();
      idx += 1;
    }
  }

  result
}

#[cfg(not(feature = "native-async-ops"))]
pub fn optpass_async_elide(_cfg: &mut Cfg) -> PassResult {
  PassResult::default()
}

