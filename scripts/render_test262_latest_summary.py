#!/usr/bin/env python3
"""
Render `progress/test262/latest_summary.md` from a `test262-semantic` JSON report.

This is intentionally focused on the repo's committed curated snapshot format.

Usage (from repo root):
  python3 scripts/render_test262_latest_summary.py \
    --report target/js/test262.json \
    --out progress/test262/latest_summary.md
"""

from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable


def _major_area(test_id: str) -> str:
    parts = test_id.split("/")
    return parts[0] if parts else "<unknown>"


def _bucket2(test_id: str) -> str:
    parts = test_id.split("/")
    if len(parts) >= 2:
        return f"{parts[0]}/{parts[1]}"
    return parts[0] if parts else "<unknown>"


def _first_nonempty_line(error: Any) -> str:
    if error is None:
        return "null"
    s = str(error)
    if not s:
        return "null"
    for line in s.splitlines():
        line = line.strip()
        if line:
            return line
    return "null"


def _expectation_kind(result: dict[str, Any]) -> str:
    # `expectation.expectation` is from the manifest: pass/xfail/skip/flaky.
    return str(result.get("expectation", {}).get("expectation", "<unknown>"))


def _outcome(result: dict[str, Any]) -> str:
    return str(result.get("outcome", "<unknown>"))


def _status(result: dict[str, Any]) -> str:
    exp = _expectation_kind(result)
    out = _outcome(result)
    if exp == "pass":
        return "PASS" if out == "passed" else "FAIL"
    if exp == "xfail":
        return "XPASS" if out == "passed" else "XFAIL"
    if exp == "skip":
        return "SKIP"
    if exp == "flaky":
        return "FLAKY_PASS" if out == "passed" else "FLAKY"
    return "UNKNOWN"


def _fmt_pct(n: int, d: int) -> str:
    if d == 0:
        return "0.00%"
    return f"{(n / d * 100.0):.2f}%"


@dataclass
class GroupStats:
    total: int = 0
    matched: int = 0
    mismatched: int = 0
    pass_: int = 0
    fail: int = 0
    xfail: int = 0
    xpass: int = 0
    skip: int = 0
    flaky: int = 0
    flaky_pass: int = 0

    def add(self, result: dict[str, Any]) -> None:
        self.total += 1
        out = _outcome(result)
        if out in ("passed", "skipped"):
            self.matched += 1
        else:
            self.mismatched += 1

        st = _status(result)
        if st == "PASS":
            self.pass_ += 1
        elif st == "FAIL":
            self.fail += 1
        elif st == "XFAIL":
            self.xfail += 1
        elif st == "XPASS":
            self.xpass += 1
        elif st == "SKIP":
            self.skip += 1
        elif st == "FLAKY":
            self.flaky += 1
        elif st == "FLAKY_PASS":
            self.flaky_pass += 1


def _sorted_items(counter: Counter[str], limit: int) -> list[tuple[str, int]]:
    items = sorted(counter.items(), key=lambda kv: (-kv[1], kv[0]))
    return items[:limit]


def _render_bucket_table(rows: Iterable[tuple[str, GroupStats]]) -> str:
    out: list[str] = []
    out.append("| Bucket | Total | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |")
    out.append("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    for bucket, s in rows:
        out.append(
            "| `{}` | {} | {} | {} | {} | {} | {} | {} | {} |".format(
                bucket,
                s.total,
                s.mismatched,
                _fmt_pct(s.mismatched, s.total),
                s.pass_,
                s.fail,
                s.xfail,
                s.xpass,
                s.skip,
            )
        )
    return "\n".join(out)


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--report", type=Path, default=Path("target/js/test262.json"))
    ap.add_argument("--out", type=Path, default=Path("progress/test262/latest_summary.md"))
    ap.add_argument("--top-buckets", type=int, default=10)
    ap.add_argument("--top-reasons", type=int, default=20)
    ap.add_argument("--appendix-min", type=int, default=50)
    ap.add_argument("--appendix-per-bucket", type=int, default=10)
    args = ap.parse_args(argv)

    report: dict[str, Any] = json.loads(args.report.read_text(encoding="utf-8"))
    if report.get("schema_version") != 1:
        raise SystemExit(f"unsupported schema_version={report.get('schema_version')!r} (expected 1)")

    results: list[dict[str, Any]] = report["results"]
    summary: dict[str, Any] = report["summary"]

    total = int(summary.get("total", len(results)))
    passed = int(summary.get("passed", 0))
    failed = int(summary.get("failed", 0))
    timed_out = int(summary.get("timed_out", 0))
    skipped = int(summary.get("skipped", 0))

    mismatches: dict[str, Any] = summary.get("mismatches") or {}
    mism_expected = int(mismatches.get("expected", 0))
    mism_unexpected = int(mismatches.get("unexpected", 0))
    mism_flaky = int(mismatches.get("flaky", 0))

    matched_upstream = passed + skipped
    mismatched_upstream = failed + timed_out

    # Expectations and PASS/FAIL/XFAIL/XPASS/SKIP buckets.
    exp_counts = Counter(_expectation_kind(r) for r in results)
    status_counts = Counter(_status(r) for r in results)

    # Major areas and bucket prefixes.
    areas: dict[str, GroupStats] = defaultdict(GroupStats)
    buckets: dict[str, GroupStats] = defaultdict(GroupStats)
    for r in results:
        areas[_major_area(r["id"])].add(r)
        buckets[_bucket2(r["id"])].add(r)

    sorted_major_areas = sorted(areas.items(), key=lambda kv: kv[0])
    sorted_buckets = sorted(buckets.items(), key=lambda kv: (-kv[1].mismatched, kv[0]))

    # Mismatch reasons.
    mismatch_kind_counts: Counter[str] = Counter()
    mismatch_reason_counts: Counter[tuple[str, str]] = Counter()
    timed_out_tests: list[str] = []
    for r in results:
        outc = _outcome(r)
        if outc == "timed_out":
            timed_out_tests.append(f"{r.get('id','<missing>')}#{r.get('variant','<missing>')}")
        if outc not in ("failed", "timed_out"):
            continue

        reason = _first_nonempty_line(r.get("error"))
        if reason.startswith("unimplemented:"):
            kind = "VmError::Unimplemented"
        elif reason.startswith("execution terminated:"):
            kind = "termination"
        else:
            kind = "exception/other"
        mismatch_kind_counts[kind] += 1
        mismatch_reason_counts[(kind, reason)] += 1

    mismatch_reason_rows = sorted(
        mismatch_reason_counts.items(),
        key=lambda kv: (-kv[1], kv[0][0], kv[0][1]),
    )[: args.top_reasons]

    out_lines: list[str] = []
    out_lines.append("# test262 (curated) — latest summary\n")
    out_lines.append("Committed snapshot of `vm-js` conformance on the curated `test262-semantic` suite.\n")
    out_lines.append("## Command\n")
    out_lines.append("```bash")
    out_lines.append("# from repo root\n")
    out_lines.append(
        "# Build the vendored runner first (outside the hard timeout so compilation doesn't eat the budget)."
    )
    out_lines.append(
        "CARGO_TARGET_DIR=target bash scripts/cargo_agent.sh build --manifest-path vendor/ecma-rs/Cargo.toml -p test262-semantic --release"
    )
    out_lines.append("")
    out_lines.append("# Run the curated suite under a hard timeout, writing the JSON report.")
    out_lines.append("LIMIT_STACK=64M timeout -k 10 600 bash scripts/run_limited.sh --as 64G -- \\")
    out_lines.append("  target/release/test262-semantic \\")
    out_lines.append("  --test262-dir vendor/ecma-rs/test262-semantic/data \\")
    out_lines.append("  --harness test262 \\")
    out_lines.append("  --suite-path tests/js/test262_suites/curated.toml \\")
    out_lines.append("  --manifest tests/js/test262_manifest.toml \\")
    out_lines.append("  --timeout-secs 10 \\")
    out_lines.append("  --jobs 4 \\")
    out_lines.append("  --report-path target/js/test262.json \\")
    out_lines.append("  --fail-on none")
    out_lines.append("```")
    out_lines.append("")
    out_lines.append("- RegExp-focused suite (separate from the curated suite):")
    out_lines.append("  ```bash")
    out_lines.append("  # from repo root")
    out_lines.append("  LIMIT_STACK=64M timeout -k 10 900 bash scripts/run_limited.sh --as 64G -- \\")
    out_lines.append("    target/release/test262-semantic \\")
    out_lines.append("    --test262-dir vendor/ecma-rs/test262-semantic/data \\")
    out_lines.append("    --harness test262 \\")
    out_lines.append("    --suite-path tests/js/test262_suites/regexp.toml \\")
    out_lines.append("    --manifest tests/js/test262_manifest.toml \\")
    out_lines.append("    --timeout-secs 10 \\")
    out_lines.append("    --jobs 4 \\")
    out_lines.append("    --report-path target/js/test262_regexp.json \\")
    out_lines.append("    --fail-on none")
    out_lines.append("  ```")
    out_lines.append("")
    out_lines.append("- RegExp `/v` Unicode sets suite (large generated corpus; kept separate from `regexp.toml`):")
    out_lines.append("  ```bash")
    out_lines.append("  # from repo root")
    out_lines.append("  LIMIT_STACK=64M timeout -k 10 900 bash scripts/run_limited.sh --as 64G -- \\")
    out_lines.append("    target/release/test262-semantic \\")
    out_lines.append("    --test262-dir vendor/ecma-rs/test262-semantic/data \\")
    out_lines.append("    --harness test262 \\")
    out_lines.append("    --suite-path tests/js/test262_suites/regexp_unicode_sets.toml \\")
    out_lines.append("    --manifest tests/js/test262_manifest.toml \\")
    out_lines.append("    --timeout-secs 10 \\")
    out_lines.append("    --jobs 4 \\")
    out_lines.append("    --report-path target/js/test262_regexp_unicode_sets.json \\")
    out_lines.append("    --fail-on none")
    out_lines.append("  ```")
    out_lines.append("")
    out_lines.append(
        "- RegExp Unicode property escapes (generated) suite (large; some known slow cases are excluded in the suite file):"
    )
    out_lines.append("  ```bash")
    out_lines.append("  # from repo root")
    out_lines.append("  LIMIT_STACK=64M timeout -k 10 900 bash scripts/run_limited.sh --as 64G -- \\")
    out_lines.append("    target/release/test262-semantic \\")
    out_lines.append("    --test262-dir vendor/ecma-rs/test262-semantic/data \\")
    out_lines.append("    --harness test262 \\")
    out_lines.append("    --suite-path tests/js/test262_suites/regexp_property_escapes_generated.toml \\")
    out_lines.append("    --manifest tests/js/test262_manifest.toml \\")
    out_lines.append("    --timeout-secs 10 \\")
    out_lines.append("    --jobs 4 \\")
    out_lines.append("    --report-path target/js/test262_regexp_property_escapes_generated.json \\")
    out_lines.append("    --fail-on none")
    out_lines.append("  ```")
    out_lines.append("")
    out_lines.append("- JSON report (not committed): `target/js/test262.json`")
    out_lines.append(
        "- Note: running `target/debug/test262-semantic` (or `target/release/test262-semantic`) directly requires"
    )
    out_lines.append(
        "  building it first (e.g. `CARGO_TARGET_DIR=target bash scripts/cargo_agent.sh build --manifest-path vendor/ecma-rs/Cargo.toml -p test262-semantic`)."
    )
    out_lines.append(
        "- Note: `test262-semantic` runs each case on a fresh large-stack thread (see"
    )
    out_lines.append(
        "  `vendor/ecma-rs/test262-semantic/src/vm_js_executor.rs`) so deep-recursion tests should fail"
    )
    out_lines.append(
        "  cleanly with a JS `RangeError` (call-stack exhaustion) rather than aborting the host process."
    )
    out_lines.append(
        "  `LIMIT_STACK=64M` (consumed by `scripts/run_limited.sh`) is still available as a safety net for"
    )
    out_lines.append("  other deeply recursive workloads.")
    out_lines.append("")
    out_lines.append("## Overall\n")
    out_lines.append("| Metric | Count |")
    out_lines.append("| --- | ---: |")
    out_lines.append(f"| Total cases | {total} |")
    out_lines.append(
        f"| Matched upstream expected | {matched_upstream} ({_fmt_pct(matched_upstream, total)}) |"
    )
    out_lines.append(
        f"| Mismatched upstream expected | {mismatched_upstream} ({_fmt_pct(mismatched_upstream, total)}) |"
    )
    out_lines.append(f"| Timeouts | {timed_out} |")
    out_lines.append(f"| Skipped | {skipped} |")
    out_lines.append(f"| Unexpected mismatches | {mism_unexpected} |")
    out_lines.append("")
    out_lines.append("### Outcomes (runner)\n")
    out_lines.append("| Outcome | Count |")
    out_lines.append("| --- | ---: |")
    out_lines.append(f"| passed | {passed} |")
    out_lines.append(f"| failed | {failed} |")
    out_lines.append(f"| timed_out | {timed_out} |")
    out_lines.append(f"| skipped | {skipped} |")
    out_lines.append("")
    out_lines.append("### Expectations (manifest)\n")
    out_lines.append("| Kind | Count |")
    out_lines.append("| --- | ---: |")
    out_lines.append(f"| pass | {exp_counts.get('pass', 0)} |")
    out_lines.append(f"| xfail | {exp_counts.get('xfail', 0)} |")
    out_lines.append(f"| skip | {exp_counts.get('skip', 0)} |")
    out_lines.append(f"| flaky | {exp_counts.get('flaky', 0)} |")
    out_lines.append("")
    out_lines.append("### Results vs expectations\n")
    out_lines.append("| Status | Count |")
    out_lines.append("| --- | ---: |")
    out_lines.append(f"| PASS | {status_counts.get('PASS', 0)} |")
    out_lines.append(f"| FAIL (unexpected) | {status_counts.get('FAIL', 0)} |")
    out_lines.append(f"| XFAIL | {status_counts.get('XFAIL', 0)} |")
    out_lines.append(f"| XPASS | {status_counts.get('XPASS', 0)} |")
    out_lines.append(f"| SKIP | {status_counts.get('SKIP', 0)} |")
    out_lines.append("")
    out_lines.append("## Breakdown by major area\n")
    out_lines.append(
        "| Area | Total | Matched | Mismatched | Mismatch rate | PASS | FAIL | XFAIL | XPASS | SKIP |"
    )
    out_lines.append(
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    )
    for area, s in sorted_major_areas:
        out_lines.append(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |".format(
                area,
                s.total,
                s.matched,
                s.mismatched,
                _fmt_pct(s.mismatched, s.total),
                s.pass_,
                s.fail,
                s.xfail,
                s.xpass,
                s.skip,
            )
        )
    out_lines.append("")
    out_lines.append("## Top failing buckets (by mismatched cases)\n")
    top_buckets = sorted_buckets[: args.top_buckets]
    out_lines.append(_render_bucket_table(top_buckets))
    out_lines.append("")
    total_buckets = len(buckets)
    zero_mismatch = sum(1 for _, s in buckets.items() if s.mismatched == 0)
    out_lines.append(f"(Total buckets: {total_buckets}; buckets with 0 mismatches: {zero_mismatch})")
    out_lines.append("")
    out_lines.append("## Top mismatch reasons (first line of `error`)\n")
    out_lines.append("Mismatched cases by high-level bucket:")
    mismatch_total = mismatched_upstream
    for kind in ("exception/other", "VmError::Unimplemented", "termination"):
        count = mismatch_kind_counts.get(kind, 0)
        out_lines.append(f"- {kind}: {count} ({_fmt_pct(count, mismatch_total)})")
    out_lines.append("")
    out_lines.append(f"### Top {args.top_reasons}\n")
    out_lines.append("| # | Kind | Count | Reason |")
    out_lines.append("| ---: | --- | ---: | --- |")
    for idx, ((kind, reason), count) in enumerate(mismatch_reason_rows, 1):
        out_lines.append(f"| {idx} | {kind} | {count} | `{reason}` |")
    out_lines.append("")
    out_lines.append("## Timed-out tests\n")
    if timed_out_tests:
        for t in sorted(timed_out_tests):
            out_lines.append(f"- `{t}`")
    else:
        out_lines.append("_None._")
    out_lines.append("")
    out_lines.append("## Appendix: top failing tests (IDs + first-line error)\n")
    out_lines.append(
        "At least 50 mismatched cases, grouped by the largest mismatch buckets.\n"
        "\n"
        "(If the suite only has a few buckets with mismatches, the largest buckets will show more\n"
        "than `--appendix-per-bucket` entries so the appendix still reaches the minimum count.)\n"
    )

    mismatched_by_bucket: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for r in results:
        if _outcome(r) not in ("failed", "timed_out"):
            continue
        mismatched_by_bucket[_bucket2(r["id"])].append(r)

    # Determine how many entries to show per bucket.
    buckets_with_mismatches = [bucket for bucket, _ in sorted_buckets if mismatched_by_bucket.get(bucket)]
    show_count: dict[str, int] = {}
    shown_total = 0
    for bucket in buckets_with_mismatches:
        base = min(args.appendix_per_bucket, len(mismatched_by_bucket[bucket]))
        show_count[bucket] = base
        shown_total += base

    # Top up the largest buckets so we always show at least `appendix_min` entries overall.
    need = max(0, args.appendix_min - shown_total)
    if need:
        for bucket in buckets_with_mismatches:
            mism = mismatched_by_bucket[bucket]
            avail = len(mism) - show_count[bucket]
            if avail <= 0:
                continue
            extra = min(avail, need)
            show_count[bucket] += extra
            need -= extra
            if need <= 0:
                break

    for bucket in buckets_with_mismatches:
        mism = mismatched_by_bucket[bucket]
        shown = show_count[bucket]
        out_lines.append(f"### `{bucket}` ({shown} shown / {len(mism)} mismatches)\n")
        mism_sorted = sorted(mism, key=lambda r: (r.get("id", ""), r.get("variant", "")))
        for r in mism_sorted[:shown]:
            test_id = r.get("id", "<missing id>")
            variant = r.get("variant", "<missing variant>")
            reason = _first_nonempty_line(r.get("error"))
            out_lines.append(f"- `{test_id}#{variant}`: `{reason}`")
        out_lines.append("")

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text("\n".join(out_lines).rstrip() + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(__import__("sys").argv[1:]))
