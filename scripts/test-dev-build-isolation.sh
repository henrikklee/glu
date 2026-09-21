#!/usr/bin/env bash
# Prove that development capabilities cannot overwrite target/release/glu.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

production_bin='target/release/glu'
[[ -x "$production_bin" ]] || cargo build --release --locked
before="$(shasum -a 256 "$production_bin" | awk '{print $1}')"
log="$(mktemp "${TMPDIR:-/tmp}/glu-dev-isolation.XXXXXX")"
trap 'rm -f "$log"' EXIT

if cargo check --release --locked --features dev-registry >"$log" 2>&1; then
  echo 'FAIL: dev-registry build succeeded in the production target directory' >&2
  exit 1
fi
grep -q 'dev-registry cannot be built into target/release' "$log" \
  || { echo 'FAIL: dev build failed for an unexpected reason' >&2; cat "$log" >&2; exit 1; }

after="$(shasum -a 256 "$production_bin" | awk '{print $1}')"
[[ "$before" == "$after" ]] \
  || { echo 'FAIL: rejected dev build changed the production binary' >&2; exit 1; }

grep -q -- '--target-dir target/dev-registry' .cargo/config.toml \
  || { echo 'FAIL: cargo build-dev does not declare an isolated target directory' >&2; exit 1; }
dev_target_dir="${GLU_DEV_TARGET_DIR:-target/dev-registry}"
CARGO_TARGET_DIR="$dev_target_dir" cargo build --locked --features dev-registry
[[ -x "$dev_target_dir/debug/glu" ]] \
  || { echo 'FAIL: isolated dev binary was not produced' >&2; exit 1; }

echo 'dev build isolation tests passed'
