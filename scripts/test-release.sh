#!/usr/bin/env bash
# Local release smoke test: production build -> package -> checksum -> exact
# artifact installer flow -> installed version and reproducibility verification.
#
# Usage: scripts/test-release.sh [version-or-tag] [output-directory]
# When an output directory is supplied, those exact tested bytes are retained
# for attestation and publication.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

version="${1:-0.1.0}"
work="$(mktemp -d "${TMPDIR:-/tmp}/glu-release-smoke.XXXXXX")"
trap 'rm -rf "$work"' EXIT
release_dir="${2:-$work/dist}"
repro_dir="$work/repro"

echo "Packaging glu $version..."
scripts/package-release.sh "$version" "$release_dir"

echo
echo "Testing exact packaged artifact..."
scripts/test-install.sh --release-dir "$release_dir" --version "$version"

echo
echo "Verifying reproducible archive bytes..."
GLU_RELEASE_SKIP_BUILD=1 scripts/package-release.sh "$version" "$repro_dir"
asset='glu-aarch64-apple-darwin.tar.gz'
cmp -s "$release_dir/$asset" "$repro_dir/$asset" \
  || { echo 'FAIL: repeated packaging produced different archive bytes' >&2; exit 1; }
cmp -s "$release_dir/$asset.sha256" "$repro_dir/$asset.sha256" \
  || { echo 'FAIL: repeated packaging produced different checksum metadata' >&2; exit 1; }

echo
echo "release smoke test passed for ${version#v}"
