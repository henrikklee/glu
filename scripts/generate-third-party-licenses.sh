#!/usr/bin/env bash
# Regenerate the locked production dependency license report.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

required_version='cargo-about 0.9.2'
actual_version="$(cargo about --version 2>/dev/null || true)"
if [[ "$actual_version" != "$required_version" ]]; then
  echo "cargo-about 0.9.2 is required" >&2
  echo 'install with: cargo install cargo-about --version 0.9.2 --locked --features cli' >&2
  exit 1
fi

output="${1:-THIRD_PARTY_LICENSES.html}"
cargo about generate \
  --locked \
  --workspace \
  --fail \
  --output-file "$output" \
  about.hbs

echo "generated $output"
