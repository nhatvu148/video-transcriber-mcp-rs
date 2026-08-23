#!/usr/bin/env python3
"""Keep the discovery manifests' version fields in step with Cargo.toml.

`Cargo.toml` is the single source of truth. Three other places restate the
version and none of them are read by `cargo release`:

  - server.json                    .version and .packages[].version
  - .claude-plugin/plugin.json     .version

A stale `server.json` version is worse than a missing one: the MCP registry
takes it at face value, so the listing goes on advertising an old release
while crates.io has moved on.

Usage:
    scripts/sync-versions.py            # rewrite the manifests in place
    scripts/sync-versions.py --check    # report drift, exit 1 (for CI)
"""

from __future__ import annotations

import json
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def cargo_version() -> str:
    with (ROOT / "Cargo.toml").open("rb") as fh:
        return tomllib.load(fh)["package"]["version"]


def load(rel: str) -> tuple[Path, dict]:
    path = ROOT / rel
    return path, json.loads(path.read_text())


def apply(version: str) -> tuple[list[tuple[str, str, str]], dict[Path, dict]]:
    """Return (drifts, documents-with-version-applied).

    A drift is (file, json-path, old-value).
    """
    drifts: list[tuple[str, str, str]] = []
    docs: dict[Path, dict] = {}

    server_path, server = load("server.json")
    if server.get("version") != version:
        drifts.append(("server.json", ".version", server.get("version")))
        server["version"] = version
    for i, pkg in enumerate(server.get("packages", [])):
        if pkg.get("version") != version:
            drifts.append(("server.json", f".packages[{i}].version", pkg.get("version")))
            pkg["version"] = version
    docs[server_path] = server

    plugin_path, plugin = load(".claude-plugin/plugin.json")
    if plugin.get("version") != version:
        drifts.append((".claude-plugin/plugin.json", ".version", plugin.get("version")))
        plugin["version"] = version
    docs[plugin_path] = plugin

    return drifts, docs


def main() -> int:
    check = "--check" in sys.argv[1:]
    version = cargo_version()
    drifts, docs = apply(version)

    if not drifts:
        print(f"✅ version manifests match Cargo.toml ({version})")
        return 0

    for file, field, old in drifts:
        print(f"{'❌' if check else '🔄'} {file} {field}: {old} → {version}")

    if check:
        print("\nRun `task version:sync` (or scripts/sync-versions.py) and commit the result.")
        return 1

    for path, doc in docs.items():
        # ensure_ascii=False: the descriptions contain em-dashes, and the
        # default would rewrite each one to —, turning a version bump
        # into a diff that also mangles prose nobody touched.
        path.write_text(json.dumps(doc, indent=2, ensure_ascii=False) + "\n")
    print(f"\n✅ synced to {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
