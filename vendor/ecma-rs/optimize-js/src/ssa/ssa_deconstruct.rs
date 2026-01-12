use crate::cfg::cfg::Cfg;
use crate::il::inst::{Arg, Inst, InstTyp, ValueTypeSummary};
use crate::util::counter::Counter;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::iter::zip;

pub fn deconstruct_ssa(cfg: &mut Cfg, c_label: &mut Counter) {
  let mut var_types = HashMap::<u32, ValueTypeSummary>::new();
  for (_, bblock) in cfg.bblocks.all() {
    for inst in bblock.iter() {
      if inst.value_type.is_unknown() {
        continue;
      }
      for &tgt in inst.tgts.iter() {
        var_types
          .entry(tgt)
          .and_modify(|existing| *existing |= inst.value_type)
          .or_insert(inst.value_type);
      }
    }
  }

  let arg_value_type = |arg: &Arg, map: &HashMap<u32, ValueTypeSummary>| match arg {
    Arg::Const(c) => ValueTypeSummary::from_const(c),
    Arg::Var(v) => map.get(v).copied().unwrap_or(ValueTypeSummary::UNKNOWN),
    Arg::Fn(_) => ValueTypeSummary::FUNCTION,
    Arg::Builtin(_) => ValueTypeSummary::UNKNOWN,
  };

  struct NewBblock {
    label: u32,
    parent: u32,
    child: u32,
    insts: Vec<Inst>,
  }
  let mut new_bblocks = Vec::<NewBblock>::new();
  let mut labels: Vec<u32> = cfg.bblocks.all().map(|(label, _)| label).collect();
  labels.sort_unstable();
  for label in labels {
    let bblock = cfg.bblocks.get_mut(label);
    let mut new_bblocks_by_parent = BTreeMap::<u32, NewBblock>::new();
    while bblock.first().is_some_and(|i| i.t == InstTyp::Phi) {
      let Inst {
        tgts,
        labels,
        args,
        meta,
        ..
      } = bblock.remove(0);
      let tgt = tgts[0];
      for (parent, value) in zip(labels, args) {
        let value_type = arg_value_type(&value, &var_types);
        new_bblocks_by_parent
          .entry(parent)
          .or_insert_with(|| NewBblock {
            label: c_label.bump(),
            parent,
            child: label,
            insts: Vec::new(),
          })
          .insts
          .push({
            let mut inst = Inst::var_assign(tgt, value);
            inst.meta.copy_result_var_metadata_from(&meta);
            inst.value_type = value_type;
            inst
          });
      }
    }
    new_bblocks.extend(new_bblocks_by_parent.into_values());
  }
  new_bblocks.sort_by_key(|b| b.label);
  for b in new_bblocks {
    // Detach parent from child.
    cfg.graph.disconnect(b.parent, b.child);
    // Update any terminator inst in parent that encodes the edge we're rewriting.
    if let Some(parent_goto) = cfg.bblocks.get_mut(b.parent).last_mut() {
      match parent_goto.t {
        InstTyp::CondGoto | InstTyp::Invoke | InstTyp::Throw => {
          for l in parent_goto.labels.iter_mut() {
            if *l == b.child {
              *l = b.label;
            };
          }
        }
        _ => {}
      }
    };
    // Attach new bblock.
    cfg.graph.connect(b.parent, b.label);
    cfg.graph.connect(b.label, b.child);
    // Insert new bblock.
    cfg.bblocks.add(b.label, b.insts);
  }
}
