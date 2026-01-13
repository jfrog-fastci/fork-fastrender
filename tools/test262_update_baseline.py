#!/usr/bin/env python3
"""
Update the committed test262 semantic baseline artifacts from a JSON report.

This script exists as a lightweight fallback when `xtask js test262 --update-baseline`
cannot be built (e.g. due to unrelated compilation failures in the top-level
`fastrender` crate).

Input:  `test262-semantic` JSON report (schema_version=1).
Output: `progress/test262/{baseline.json,summary.md,trend.json}` (schema_version=1).

Usage (from repo root):
  python3 tools/test262_update_baseline.py \
    --report target/js/test262.json \
    --baseline progress/test262/baseline.json

The output formats intentionally match the Rust `xtask/src/js/test262_report.rs`
rendering logic (pretty JSON with stable ordering + markdown tables).
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, Iterable, Mapping, Optional, Tuple


# Keep these in sync with:
# - xtask/src/js/test262_report.rs::{TEST262_REPORT_SCHEMA_VERSION,TEST262_BASELINE_SCHEMA_VERSION,TEST262_TREND_SCHEMA_VERSION}
TEST262_REPORT_SCHEMA_VERSION = 1
TEST262_BASELINE_SCHEMA_VERSION = 1
TEST262_TREND_SCHEMA_VERSION = 1


def _die(msg: str) -> "NoReturn":
    print(f"error: {msg}", file=sys.stderr)
    raise SystemExit(2)


def _bucket_for_id(test_id: str) -> str:
    # Mirrors xtask/src/js/test262_report.rs::bucket_for_id.
    parts = test_id.split("/")
    first = parts[0] if parts and parts[0] else "<unknown>"
    if len(parts) >= 2 and parts[1]:
        return f"{first}/{parts[1]}"
    return first


def _display_path_for_markdown(path: Path) -> str:
    # Mirrors xtask/src/js/test262_report.rs::display_path_for_markdown.
    p = path
    if p.is_absolute():
        try:
            p = p.relative_to(Path.cwd())
        except Exception:
            pass
    return str(p).replace("\\", "/")


def _normalize_markdown(markdown: str) -> str:
    # Mirrors xtask/src/js/test262_report.rs::normalize_markdown.
    lines = [line.rstrip() for line in markdown.splitlines()]
    out = "\n".join(lines)
    if not out.endswith("\n"):
        out += "\n"
    return out


def _json_pretty(obj: Any) -> str:
    # serde_json::to_string_pretty uses 2-space indentation.
    return json.dumps(obj, indent=2, ensure_ascii=False)


def _parse_required_str(value: Any, *, ctx: str) -> str:
    if not isinstance(value, str) or not value:
        _die(f"{ctx}: expected non-empty string, got {value!r}")
    return value


def _parse_required_bool(value: Any, *, ctx: str) -> bool:
    if not isinstance(value, bool):
        _die(f"{ctx}: expected bool, got {value!r}")
    return value


def _parse_required_int(value: Any, *, ctx: str) -> int:
    if not isinstance(value, int):
        _die(f"{ctx}: expected int, got {value!r}")
    return value


def _parse_expectation_kind(value: Any, *, ctx: str) -> str:
    # Matches conformance_harness::ExpectationKind's serde representation.
    v = _parse_required_str(value, ctx=ctx)
    if v not in ("pass", "skip", "xfail", "flaky"):
        _die(f"{ctx}: unknown expectation kind {v!r}")
    return v


def _parse_outcome(value: Any, *, ctx: str) -> str:
    v = _parse_required_str(value, ctx=ctx)
    if v not in ("passed", "failed", "timed_out", "skipped"):
        _die(f"{ctx}: unknown outcome {v!r}")
    return v


def _parse_variant(value: Any, *, ctx: str) -> str:
    v = _parse_required_str(value, ctx=ctx)
    if v not in ("non_strict", "strict", "module"):
        _die(f"{ctx}: unknown variant {v!r}")
    return v


def _result_key(test_id: str, variant: str) -> str:
    return f"{test_id}#{variant}"


@dataclass
class KindTotals:
    # Matches xtask/src/js/test262_report.rs::KindTotals field order.
    pass_: int = 0
    xfail: int = 0
    skip: int = 0
    flaky: int = 0

    def to_json_obj(self) -> Dict[str, int]:
        return {
            "pass": self.pass_,
            "xfail": self.xfail,
            "skip": self.skip,
            "flaky": self.flaky,
        }


@dataclass
class BucketCounts:
    # Matches xtask/src/js/test262_report.rs::BucketCounts field order.
    total: int = 0
    matched: int = 0
    mismatched: int = 0
    kinds: KindTotals = field(default_factory=KindTotals)
    pass_: int = 0
    unexpected_fail: int = 0
    xfail: int = 0
    xpass: int = 0
    skip: int = 0
    flaky: int = 0
    flaky_pass: int = 0
    timed_out: int = 0

    def observe(self, *, outcome: str, expectation: str, mismatched: bool) -> None:
        self.total += 1
        if mismatched:
            self.mismatched += 1
        else:
            self.matched += 1

        if outcome == "timed_out":
            self.timed_out += 1

        if expectation == "pass":
            self.kinds.pass_ += 1
            if mismatched:
                self.unexpected_fail += 1
            else:
                self.pass_ += 1
        elif expectation == "xfail":
            self.kinds.xfail += 1
            if mismatched:
                self.xfail += 1
            else:
                self.xpass += 1
        elif expectation == "skip":
            self.kinds.skip += 1
            self.skip += 1
        elif expectation == "flaky":
            self.kinds.flaky += 1
            if mismatched:
                self.flaky += 1
            else:
                self.flaky_pass += 1
        else:
            _die(f"internal error: unknown expectation kind {expectation!r}")

    def to_json_obj(self) -> Dict[str, Any]:
        return {
            "total": self.total,
            "matched": self.matched,
            "mismatched": self.mismatched,
            "kinds": self.kinds.to_json_obj(),
            "pass": self.pass_,
            "unexpected_fail": self.unexpected_fail,
            "xfail": self.xfail,
            "xpass": self.xpass,
            "skip": self.skip,
            "flaky": self.flaky,
            "flaky_pass": self.flaky_pass,
            "timed_out": self.timed_out,
        }


def _compute_report_stats(report: Mapping[str, Any]) -> Tuple[BucketCounts, Dict[str, BucketCounts]]:
    overall = BucketCounts()
    by_bucket: Dict[str, BucketCounts] = {}
    for idx, r in enumerate(report.get("results") or []):
        if not isinstance(r, dict):
            _die(f"report.results[{idx}]: expected object, got {type(r).__name__}")
        test_id = _parse_required_str(r.get("id"), ctx=f"report.results[{idx}].id")
        variant = _parse_variant(r.get("variant"), ctx=f"report.results[{idx}].variant")
        outcome = _parse_outcome(r.get("outcome"), ctx=f"report.results[{idx}].outcome")
        mismatched = _parse_required_bool(r.get("mismatched", False), ctx=f"report.results[{idx}].mismatched")

        expectation_obj = r.get("expectation")
        if not isinstance(expectation_obj, dict):
            _die(f"report.results[{idx}].expectation: expected object, got {expectation_obj!r}")
        expectation = _parse_expectation_kind(
            expectation_obj.get("expectation"), ctx=f"report.results[{idx}].expectation.expectation"
        )

        overall.observe(outcome=outcome, expectation=expectation, mismatched=mismatched)

        bucket = _bucket_for_id(test_id)
        entry = by_bucket.get(bucket)
        if entry is None:
            entry = BucketCounts()
            by_bucket[bucket] = entry
        entry.observe(outcome=outcome, expectation=expectation, mismatched=mismatched)

    # Deterministic key ordering (Rust uses BTreeMap).
    by_bucket_sorted = {k: by_bucket[k] for k in sorted(by_bucket.keys())}
    return overall, by_bucket_sorted


def _baseline_from_report(report: Mapping[str, Any]) -> Dict[str, Any]:
    results: Dict[str, Any] = {}
    for idx, r in enumerate(report.get("results") or []):
        if not isinstance(r, dict):
            _die(f"report.results[{idx}]: expected object, got {type(r).__name__}")
        test_id = _parse_required_str(r.get("id"), ctx=f"report.results[{idx}].id")
        variant = _parse_variant(r.get("variant"), ctx=f"report.results[{idx}].variant")
        key = _result_key(test_id, variant)

        outcome = _parse_outcome(r.get("outcome"), ctx=f"report.results[{idx}].outcome")
        mismatched = _parse_required_bool(r.get("mismatched", False), ctx=f"report.results[{idx}].mismatched")

        expectation_obj = r.get("expectation")
        if not isinstance(expectation_obj, dict):
            _die(f"report.results[{idx}].expectation: expected object, got {expectation_obj!r}")
        expectation = _parse_expectation_kind(
            expectation_obj.get("expectation"), ctx=f"report.results[{idx}].expectation.expectation"
        )

        if key in results:
            _die(f"test262 report contains duplicate result key {key!r}")

        # Field order matches xtask::BaselineEntry.
        results[key] = {
            "outcome": outcome,
            "mismatched": mismatched,
            "expectation": expectation,
        }

    results_sorted = {k: results[k] for k in sorted(results.keys())}
    return {
        "schema_version": TEST262_BASELINE_SCHEMA_VERSION,
        "results": results_sorted,
    }


def _trend_from_stats(overall: BucketCounts, by_bucket: Mapping[str, BucketCounts]) -> Dict[str, Any]:
    return {
        "schema_version": TEST262_TREND_SCHEMA_VERSION,
        "overall": overall.to_json_obj(),
        "by_bucket": {k: by_bucket[k].to_json_obj() for k in by_bucket.keys()},
    }


def _render_baseline_markdown(
    *,
    report: Mapping[str, Any],
    overall: BucketCounts,
    by_bucket: Mapping[str, BucketCounts],
    report_path: Path,
    title: str,
) -> str:
    summary = report.get("summary")
    if not isinstance(summary, dict):
        _die("report.summary: expected object")

    total = _parse_required_int(summary.get("total", 0), ctx="report.summary.total")
    timed_out = _parse_required_int(summary.get("timed_out", 0), ctx="report.summary.timed_out")

    mismatches_obj = summary.get("mismatches")
    mismatches: Optional[Tuple[int, int, int]] = None
    if mismatches_obj is not None:
        if not isinstance(mismatches_obj, dict):
            _die("report.summary.mismatches: expected object")
        mism_expected = _parse_required_int(mismatches_obj.get("expected", 0), ctx="report.summary.mismatches.expected")
        mism_unexpected = _parse_required_int(
            mismatches_obj.get("unexpected", 0), ctx="report.summary.mismatches.unexpected"
        )
        mism_flaky = _parse_required_int(mismatches_obj.get("flaky", 0), ctx="report.summary.mismatches.flaky")
        mismatches = (mism_expected, mism_unexpected, mism_flaky)

    mismatched_total = sum(mismatches) if mismatches else 0
    matched_total = total - mismatched_total

    out: list[str] = []
    out.append(f"# {title}")
    out.append("")
    out.append(f"- Report: `{_display_path_for_markdown(report_path)}`")
    out.append("")

    out.append("## Summary")
    out.append("")
    out.append("| Metric | Count |")
    out.append("| --- | ---: |")
    out.append(f"| Total cases | {total} |")
    out.append(f"| Matched upstream expected | {matched_total} |")
    out.append(f"| Mismatched upstream expected | {mismatched_total} |")
    out.append(f"| Timeouts | {timed_out} |")
    out.append("")

    out.append("### Manifest expectations (kind)")
    out.append("")
    out.append("| Kind | Count |")
    out.append("| --- | ---: |")
    out.append(f"| pass | {overall.kinds.pass_} |")
    out.append(f"| xfail | {overall.kinds.xfail} |")
    out.append(f"| skip | {overall.kinds.skip} |")
    out.append(f"| flaky | {overall.kinds.flaky} |")
    out.append("")

    out.append("### Results vs expectations")
    out.append("")
    out.append("| Status | Count |")
    out.append("| --- | ---: |")
    out.append(f"| PASS (pass+matched) | {overall.pass_} |")
    out.append(f"| XFAIL (xfail+mismatched) | {overall.xfail} |")
    out.append(f"| SKIP | {overall.skip} |")
    if overall.flaky > 0 or overall.flaky_pass > 0:
        out.append(f"| FLAKY (flaky+mismatched) | {overall.flaky} |")
    if overall.unexpected_fail > 0:
        out.append(f"| Unexpected failures (pass+mismatched) | {overall.unexpected_fail} |")
    if overall.xpass > 0:
        out.append(f"| XPASS (xfail+matched) | {overall.xpass} |")
    if overall.flaky_pass > 0:
        out.append(f"| Flaky XPASS (flaky+matched) | {overall.flaky_pass} |")
    out.append("")

    if mismatches:
        mism_expected, mism_unexpected, mism_flaky = mismatches
        out.append("### Mismatch classification (for `--fail-on`)")
        out.append("")
        out.append("| Kind | Count |")
        out.append("| --- | ---: |")
        out.append(f"| expected | {mism_expected} |")
        out.append(f"| unexpected | {mism_unexpected} |")
        out.append(f"| flaky | {mism_flaky} |")
        out.append("")

    out.append("## Breakdown by area")
    out.append("")
    out.append(
        "| Area | Total | PASS | XFAIL | SKIP | Unexpected | Timeouts | ΔPASS | ΔXFAIL | ΔTimeout |"
    )
    out.append("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")

    for bucket, counts in by_bucket.items():
        # Baseline markdown intentionally omits baseline deltas (leave columns empty).
        out.append(
            f"| `{bucket}` | {counts.total} | {counts.pass_} | {counts.xfail} | {counts.skip} | {counts.unexpected_fail} | {counts.timed_out} |  |  |  |"
        )

    out.append("")
    return _normalize_markdown("\n".join(out))


def _load_json(path: Path) -> Dict[str, Any]:
    try:
        raw = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        _die(f"report not found: {path}")
    except Exception as e:
        _die(f"failed to read {path}: {e}")

    try:
        data = json.loads(raw)
    except json.JSONDecodeError as e:
        _die(f"failed to parse JSON report {path}: {e}")

    if not isinstance(data, dict):
        _die(f"report root: expected object, got {type(data).__name__}")

    schema_version = data.get("schema_version")
    if schema_version != TEST262_REPORT_SCHEMA_VERSION:
        _die(
            f"unsupported test262 report schema_version={schema_version!r} (expected {TEST262_REPORT_SCHEMA_VERSION})"
        )

    return data


def _write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description="Update progress/test262 baseline files from a test262-semantic JSON report.")
    ap.add_argument("--report", type=Path, default=Path("target/js/test262.json"))
    ap.add_argument("--baseline", type=Path, default=Path("progress/test262/baseline.json"))
    ap.add_argument(
        "--summary",
        type=Path,
        default=None,
        help="Markdown baseline summary output path (default: <baseline_dir>/summary.md)",
    )
    ap.add_argument(
        "--trend",
        type=Path,
        default=None,
        help="Trend JSON output path (default: <baseline_dir>/trend.json)",
    )
    args = ap.parse_args(argv)

    report = _load_json(args.report)

    baseline_path = args.baseline
    baseline_dir = baseline_path.parent if str(baseline_path.parent) else Path(".")
    summary_path = args.summary or (baseline_dir / "summary.md")
    trend_path = args.trend or (baseline_dir / "trend.json")

    baseline = _baseline_from_report(report)
    overall, by_bucket = _compute_report_stats(report)
    trend = _trend_from_stats(overall, by_bucket)
    markdown = _render_baseline_markdown(
        report=report,
        overall=overall,
        by_bucket=by_bucket,
        report_path=baseline_path,
        title="test262 semantic baseline",
    )

    _write_text(baseline_path, _json_pretty(baseline) + "\n")
    _write_text(trend_path, _json_pretty(trend) + "\n")
    _write_text(summary_path, markdown)

    print(f"Updated baseline: {baseline_path}")
    print(f"Baseline summary: {summary_path}")
    print(f"Baseline trend: {trend_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

