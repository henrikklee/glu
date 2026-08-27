#!/usr/bin/env bash
# Fail when the checked-in dependency license report is stale.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

tmp="$(mktemp "${TMPDIR:-/tmp}/glu-third-party-licenses.XXXXXX")"
trap 'rm -f "$tmp"' EXIT

scripts/generate-third-party-licenses.sh "$tmp" >/dev/null
if ! cmp -s THIRD_PARTY_LICENSES.html "$tmp"; then
  echo 'THIRD_PARTY_LICENSES.html is stale' >&2
  echo 'run scripts/generate-third-party-licenses.sh and review the changes' >&2
  diff -u THIRD_PARTY_LICENSES.html "$tmp" || true
  exit 1
fi

# The generated report catches every license-graph change. Keep the manually
# reviewed MPL source-availability table synchronized with that graph too.
python3 - <<'PY'
import json
import pathlib
import re
import subprocess

metadata = json.loads(
    subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        text=True,
    )
)
tree = subprocess.check_output(
    [
        "cargo",
        "tree",
        "--locked",
        "--target",
        "aarch64-apple-darwin",
        "--edges",
        "normal",
        "--prefix",
        "none",
        "--format",
        "{p}",
        "-p",
        "glu-cli",
    ],
    text=True,
)
resolved = {
    (match.group(1), match.group(2))
    for line in tree.splitlines()
    if (match := re.match(r"([^ ]+) v([^ ]+)", line))
}
expected = {
    (package["name"], package["version"])
    for package in metadata["packages"]
    if "MPL-2.0" in (package.get("license") or "")
    and (package["name"], package["version"]) in resolved
}

notices = pathlib.Path("THIRD_PARTY_NOTICES.md").read_text(encoding="utf-8")
section = notices.split("## MPL-2.0 runtime components", 1)[1].split("\n## ", 1)[0]
documented = {
    (name.strip(), version.strip())
    for name, version in re.findall(r"^\| ([^|-][^|]*) \| ([^|]+) \|", section, re.MULTILINE)
    if name.strip() != "Component"
}
if documented != expected:
    missing = sorted(expected - documented)
    stale = sorted(documented - expected)
    raise SystemExit(
        "MPL source table is stale"
        f"\nmissing: {missing or 'none'}"
        f"\nstale: {stale or 'none'}"
    )
for name, version in sorted(expected):
    url = f"https://crates.io/api/v1/crates/{name}/{version}/download"
    if url not in section:
        raise SystemExit(f"MPL source table is missing exact source URL: {url}")
PY

echo 'third-party license report and MPL source table are current'
