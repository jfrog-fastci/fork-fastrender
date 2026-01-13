#!/usr/bin/env python3
"""
Generate a lightweight progress snapshot from a test262-semantic JSON report.

This is intended to turn the question "what is our current curated test262 pass %?"
into a repeatable artifact under `progress/test262/`.

Usage (from repo root):
  python3 scripts/test262_progress_report.py \
    --report target/js/test262.json \
    --out progress/test262/latest_summary.md
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path
from typing import Any, Iterable, Tuple


def _git_head() -> Tuple[str, bool]:
    """
    Return (hash, dirty) where dirty only considers tracked changes.
    """
    try:
        head = (
            subprocess.check_output(["git", "rev-parse", "HEAD"], stderr=subprocess.DEVNULL)
            .decode("utf-8")
            .strip()
        )
    except Exception:
        return ("<unknown>", False)

    dirty = subprocess.run(
        ["git", "diff-index", "--quiet", "HEAD", "--"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ).returncode != 0
    return (head, dirty)


def _bucket_for_id(test_id: str) -> str:
    parts = test_id.split("/")
    if len(parts) >= 2:
        return f"{parts[0]}/{parts[1]}"
    if parts:
        return parts[0]
    return "<unknown>"


_WHITESPACE_RE = re.compile(r"\s+")


def _failure_reason(outcome: str, error: str | None) -> str:
    if outcome == "timed_out":
        return "<timed_out>"
    if not error:
        return "<no_error>"
    first = error.splitlines()[0].strip()
    # Normalize whitespace so stack-trace indentation doesn't produce separate buckets.
    first = _WHITESPACE_RE.sub(" ", first)
    return first


def _sorted_top(counter: Counter[str], limit: int) -> Iterable[Tuple[str, int]]:
    # Deterministic ordering: count desc, key asc.
    items = sorted(counter.items(), key=lambda kv: (-kv[1], kv[0]))
    return items[:limit]


def _render_table(rows: Iterable[Tuple[str, int]], label: str) -> str:
    out = []
    out.append(f"| {label} | Count |")
    out.append("| --- | ---: |")
    for key, count in rows:
        out.append(f"| `{key}` | {count} |")
    return "\n".join(out)


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--report", type=Path, default=Path("target/js/test262.json"))
    ap.add_argument("--out", type=Path, default=Path("progress/test262/latest_summary.md"))
    ap.add_argument("--top", type=int, default=10)
    args = ap.parse_args(argv)

    try:
        raw = args.report.read_text(encoding="utf-8")
    except FileNotFoundError:
        print(f"error: report not found: {args.report}", file=sys.stderr)
        return 2

    report: dict[str, Any] = json.loads(raw)
    if report.get("schema_version") != 1:
        print(
            f"error: unsupported schema_version={report.get('schema_version')!r} (expected 1)",
            file=sys.stderr,
        )
        return 2

    summary: dict[str, Any] = report["summary"]
    total = int(summary.get("total", 0))
    passed = int(summary.get("passed", 0))
    failed = int(summary.get("failed", 0))
    timed_out = int(summary.get("timed_out", 0))
    skipped = int(summary.get("skipped", 0))
    pass_pct = (passed / total * 100.0) if total else 0.0

    mismatches = summary.get("mismatches") or {}
    mism_expected = int(mismatches.get("expected", 0))
    mism_unexpected = int(mismatches.get("unexpected", 0))
    mism_flaky = int(mismatches.get("flaky", 0))

    failing_buckets: Counter[str] = Counter()
    failing_reasons: Counter[str] = Counter()
    timed_out_tests: list[str] = []

    for r in report["results"]:
        outcome = r.get("outcome")
        if outcome == "passed" or outcome == "skipped":
            continue

        test_id = r.get("id", "<missing id>")
        failing_buckets[_bucket_for_id(test_id)] += 1
        failing_reasons[_failure_reason(outcome, r.get("error"))] += 1

        if outcome == "timed_out":
            variant = r.get("variant", "<unknown>")
            timed_out_tests.append(f"{test_id}#{variant}")

    now = _dt.datetime.now(tz=_dt.timezone.utc).replace(microsecond=0).isoformat()
    head, dirty = _git_head()

    out_lines: list[str] = []
    out_lines.append("# test262 curated snapshot\n")
    out_lines.append(f"- Date (UTC): `{now}`")
    out_lines.append(f"- Git HEAD: `{head}`{' (dirty)' if dirty else ''}")
    out_lines.append(
        "- Command: `timeout -k 10 600 bash scripts/cargo_agent.sh xtask js test262 "
        "--suite curated --fail-on none --report target/js/test262.json "
        "--summary target/js/test262_summary.md`"
    )
    out_lines.append("")
    out_lines.append("## Totals\n")
    out_lines.append("| Metric | Count |")
    out_lines.append("| --- | ---: |")
    out_lines.append(f"| Total | {total} |")
    out_lines.append(f"| Passed | {passed} ({pass_pct:.2f}%) |")
    out_lines.append(f"| Failed | {failed} |")
    out_lines.append(f"| Timed out | {timed_out} |")
    out_lines.append(f"| Skipped | {skipped} |")
    out_lines.append("")
    out_lines.append("## Mismatches (manifest-aware)\n")
    out_lines.append("| Kind | Count |")
    out_lines.append("| --- | ---: |")
    out_lines.append(f"| Unexpected | {mism_unexpected} |")
    out_lines.append(f"| Expected | {mism_expected} |")
    out_lines.append(f"| Flaky | {mism_flaky} |")
    out_lines.append("")
    out_lines.append(f"## Top failing areas (first two path components, top {args.top})\n")
    out_lines.append(_render_table(_sorted_top(failing_buckets, args.top), "Area prefix"))
    out_lines.append("")
    out_lines.append(f"## Top failure reasons (first line of `error`, top {args.top})\n")
    out_lines.append(_render_table(_sorted_top(failing_reasons, args.top), "Reason"))
    out_lines.append("")

    if timed_out_tests:
        out_lines.append("## Timed-out tests\n")
        for t in sorted(timed_out_tests):
            out_lines.append(f"- `{t}`")
        out_lines.append("")

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text("\n".join(out_lines), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
