#!/usr/bin/env bash
# Print the shared version of all workspace packages.
set -euo pipefail

cd "$(dirname "$0")/.."
cargo metadata --locked --no-deps --format-version 1 |
  python3 -c '
import json
import sys

data = json.load(sys.stdin)
members = set(data["workspace_members"])
versions = {package["version"] for package in data["packages"] if package["id"] in members}
if len(versions) != 1:
    raise SystemExit(f"workspace packages have different versions: {sorted(versions)}")
print(versions.pop())
'
