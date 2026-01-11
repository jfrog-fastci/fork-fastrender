#![cfg(feature = "semantic-ops")]

use std::sync::Arc;

use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn semantic_ops_feature_smoke() {
  let mut host = MemoryHost::new();

  let file = FileKey::new("a.ts");
  host.insert(
    file.clone(),
    Arc::<str>::from(
      r#"
const nums: Array<number> = [1, 2, 3];
const mapped = nums.map((n: number) => n + 1).filter((n: number) => n > 0);
const summed = mapped.reduce((a: number, b: number) => a + b, 0);

export const out = Promise.all(nums.map((n: number) => Promise.resolve(n)));
export const total = summed;
"#,
    ),
  );

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics under semantic-ops: {diagnostics:?}"
  );
}
