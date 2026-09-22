#!/usr/bin/env bash
# Build, validate, and package a production glu release artifact.
#
# Usage: scripts/package-release.sh <version-or-tag> [output-directory]
# Example: scripts/package-release.sh 0.1.3 dist
#
# Bare versions and v-prefixed tags are both accepted. Cargo package versions
# remain plain semver; the corresponding GitHub release tag is v-prefixed.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

requested_version="${1:-}"
[[ -n "$requested_version" ]] || fail 'usage: scripts/package-release.sh <version-or-tag> [output-directory]'
if [[ "$requested_version" =~ ^v?([0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z][0-9A-Za-z.-]*)?)$ ]]; then
  version="${BASH_REMATCH[1]}"
else
  fail "invalid release version: $requested_version (expected 0.1.3 or v0.1.3)"
fi
tag="v$version"

output_dir="${2:-dist}"
if [[ "$output_dir" != /* ]]; then
  output_dir="$repo_root/$output_dir"
fi

target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
if [[ "$target_dir" != /* ]]; then
  target_dir="$repo_root/$target_dir"
fi
bin="$target_dir/release/glu"
target='aarch64-apple-darwin'
asset="glu-$target.tar.gz"
# These ceilings catch accidental dependency or profile regressions. They leave
# deliberate headroom above the measured 0.1.0 baseline documented in
# RELEASING.md and should only move after reviewing a new baseline.
max_binary_bytes=$((9 * 1024 * 1024))
max_archive_bytes=$((5 * 1024 * 1024))

command -v python3 >/dev/null || fail 'python3 is required to validate Cargo metadata'
command -v file >/dev/null || fail 'file is required to validate the release architecture'
command -v tar >/dev/null || fail 'tar is required to package the release'

license_files=(
  LICENSE-BSD-2-Clause
  LICENSE-MIT
  THIRD_PARTY_LICENSES.html
  THIRD_PARTY_NOTICES.md
)
for license_file in "${license_files[@]}"; do
  [[ -f "$repo_root/$license_file" ]] || fail "required release file is missing: $license_file"
done

python3 - "$repo_root/Cargo.toml" <<'PY'
import pathlib
import sys
import tomllib

manifest = tomllib.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
actual = manifest.get("profile", {}).get("release", {})
expected = {
    "opt-level": 3,
    "lto": "fat",
    "codegen-units": 1,
    "panic": "abort",
    "strip": "symbols",
}
if actual != expected:
    raise SystemExit(
        f"release profile mismatch\nexpected: {expected}\nactual:   {actual}"
    )
PY

workspace_versions="$(
  cargo metadata --locked --no-deps --format-version 1 \
    | python3 -c 'import json,sys; data=json.load(sys.stdin); ids=set(data["workspace_members"]); print("\n".join(sorted({p["version"] for p in data["packages"] if p["id"] in ids})))'
)"
[[ "$workspace_versions" == "$version" ]] \
  || fail "workspace package version(s) do not match $version: ${workspace_versions//$'\n'/, }"

if [[ "${GLU_RELEASE_SKIP_BUILD:-0}" != '1' ]]; then
  echo "Building production glu $version ($tag)..."
  CARGO_TARGET_DIR="$target_dir" cargo build --release --locked
else
  echo "Using existing production glu $version ($tag)..."
fi
[[ -x "$bin" ]] || fail "release binary not found: $bin"

reported_version="$($bin --version)"
[[ "$reported_version" == "glu $version" ]] \
  || fail "release binary reports '$reported_version', expected 'glu $version'"

file_output="$(file "$bin")"
[[ "$file_output" == *'Mach-O 64-bit executable arm64'* ]] \
  || fail "release binary is not an ARM64 Mach-O executable: $file_output"
binary_bytes="$(stat -f '%z' "$bin")"
((binary_bytes <= max_binary_bytes)) \
  || fail "release binary is $binary_bytes bytes; limit is $max_binary_bytes bytes"

CARGO_TARGET_DIR="$target_dir" GLU_RELEASE_SKIP_BUILD=1 scripts/check-release-policy.sh

mkdir -p "$output_dir"
rm -f "$output_dir/$asset" "$output_dir/$asset.sha256" "$output_dir/install.sh"
staging="$(mktemp -d "${TMPDIR:-/tmp}/glu-package.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
install -m 0755 "$bin" "$staging/glu"
for license_file in "${license_files[@]}"; do
  install -m 0644 "$repo_root/$license_file" "$staging/$license_file"
done

# Build a reproducible gzip-compressed tar archive. All metadata that normally
# varies by machine or build time is fixed; only the staged file bytes affect
# output.
python3 - "$staging" "$output_dir/$asset" <<'PY'
import gzip
import hashlib
import pathlib
import sys
import tarfile

staging = pathlib.Path(sys.argv[1])
destination = pathlib.Path(sys.argv[2])
entries = [
    ("LICENSE-BSD-2-Clause", 0o644),
    ("LICENSE-MIT", 0o644),
    ("THIRD_PARTY_LICENSES.html", 0o644),
    ("THIRD_PARTY_NOTICES.md", 0o644),
    ("glu", 0o755),
]

with destination.open("wb") as raw:
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as archive:
            for name, mode in entries:
                source = staging / name
                info = tarfile.TarInfo(name)
                info.size = source.stat().st_size
                info.mode = mode
                info.uid = 0
                info.gid = 0
                info.uname = "root"
                info.gname = "root"
                info.mtime = 0
                with source.open("rb") as contents:
                    archive.addfile(info, contents)

digest = hashlib.sha256(destination.read_bytes()).hexdigest()
destination.with_name(destination.name + ".sha256").write_text(
    f"{digest}  {destination.name}\n",
    encoding="ascii",
)
PY
archive_bytes="$(stat -f '%z' "$output_dir/$asset")"
((archive_bytes <= max_archive_bytes)) \
  || fail "release archive is $archive_bytes bytes; limit is $max_archive_bytes bytes"
install -m 0755 scripts/install.sh "$output_dir/install.sh"

entries="$(tar -tzf "$output_dir/$asset")"
expected_entries=$'LICENSE-BSD-2-Clause\nLICENSE-MIT\nTHIRD_PARTY_LICENSES.html\nTHIRD_PARTY_NOTICES.md\nglu'
[[ "$entries" == "$expected_entries" ]] || fail "release archive has unexpected entries: $entries"

inspect="$staging/inspect"
mkdir -p "$inspect"
tar -xzf "$output_dir/$asset" -C "$inspect"
[[ -x "$inspect/glu" ]] || fail 'release archive does not contain an executable glu at its root'
cmp -s "$bin" "$inspect/glu" || fail 'archived executable differs from the validated production binary'
for license_file in "${license_files[@]}"; do
  cmp -s "$repo_root/$license_file" "$inspect/$license_file" \
    || fail "archived $license_file differs from the reviewed repository file"
done
[[ "$("$inspect/glu" --version)" == "glu $version" ]] \
  || fail 'extracted release binary reports the wrong version'
[[ "$(file "$inspect/glu")" == *'Mach-O 64-bit executable arm64'* ]] \
  || fail 'extracted release binary is not ARM64'

checksum="$(awk '{print $1}' "$output_dir/$asset.sha256")"
[[ "$checksum" =~ ^[0-9a-f]{64}$ ]] || fail 'generated checksum is malformed'

echo
printf 'Release package ready:\n'
printf '  version:  %s\n' "$version"
printf '  tag:      %s\n' "$tag"
printf '  binary:   %s (%s bytes)\n' "$bin" "$binary_bytes"
printf '  artifact: %s (%s bytes)\n' "$output_dir/$asset" "$archive_bytes"
printf '  checksum: %s\n' "$checksum"
printf '  installer:%s\n' " $output_dir/install.sh"
