#!/usr/bin/env python3

"""
FastRender macOS entitlement validation helper.

This is intentionally *not* wired into the build system yet; it's a lightweight
sanity check for future `.app` bundling work.

It verifies:
  - the `*.entitlements` files are valid XML plists
  - the renderer entitlement file keeps a deny-by-default posture (App Sandbox
    enabled, no network/files entitlements)

Run from repo root:

  python tools/macos/entitlements/validate_entitlements.py
"""

from __future__ import annotations

import plistlib
from pathlib import Path
import sys


def load_plist(path: Path) -> dict:
    try:
        data = plistlib.loads(path.read_bytes())
    except Exception as e:
        raise RuntimeError(f"failed to parse plist {path}: {e}") from e
    if not isinstance(data, dict):
        raise RuntimeError(f"expected plist dict in {path}, got {type(data)}")
    return data


def main() -> int:
    ent_dir = Path(__file__).resolve().parent
    ent_files = sorted(ent_dir.glob("*.entitlements"))
    if not ent_files:
        print(f"no *.entitlements files found under {ent_dir}", file=sys.stderr)
        return 2

    plists: dict[str, dict] = {}
    for p in ent_files:
        plists[p.name] = load_plist(p)

    renderer = plists.get("renderer.entitlements")
    if renderer is None:
        print("missing renderer.entitlements", file=sys.stderr)
        return 2

    if renderer.get("com.apple.security.app-sandbox") is not True:
        print(
            "renderer.entitlements must enable com.apple.security.app-sandbox=true",
            file=sys.stderr,
        )
        return 2

    forbidden_prefixes = (
        "com.apple.security.network.",
        "com.apple.security.files.",
    )
    forbidden = [
        k for k in renderer.keys() if any(k.startswith(p) for p in forbidden_prefixes)
    ]
    if forbidden:
        print(
            "renderer.entitlements must not grant network/files entitlements; found: "
            + ", ".join(sorted(forbidden)),
            file=sys.stderr,
        )
        return 2

    print(f"OK: validated {len(ent_files)} entitlement plist(s):")
    for p in ent_files:
        print(f"  - {p.name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

