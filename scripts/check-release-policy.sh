#!/usr/bin/env bash
# CI guard for the build-time registry/prefix policy (see
# docs/reference/cli-behavior.md and
# crates/glu-client/src/config/mod.rs).
#
# The published client is built with default features (`cargo build --release`,
# no `dev-registry`). That build must be structurally incapable of the
# development overrides:
#
#   - registry origin is the fixed HTTPS DEFAULT_REGISTRY_BASE_URL;
#   - GLU_REGISTRY / GLU_PREFIX / GLU_BASE_URL have no effect (compile-time
#     removed from the binary);
#   - dev-only strings (GLU_REGISTRY, the dev default localhost:3000, the
#     installer's GLU_NO_VERIFY) do not appear in the artifact at all.
#
# Failure here means the release pipeline accidentally built with
# `--features dev-registry` (or a dev-only string leaked into shared code).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

target_dir="${CARGO_TARGET_DIR:-target}"
bin="$target_dir/release/glu"
if [[ "${GLU_RELEASE_SKIP_BUILD:-0}" != '1' ]]; then
  cargo build --release --locked
fi
[[ -x "$bin" ]] || { echo "FAIL: production binary not found: $bin" >&2; exit 1; }

# 1) Overrides must be ignored: hostile values, unchanged output.
status="$(
  GLU_REGISTRY=http://localhost:9999 \
  GLU_PREFIX=/tmp/evil \
  GLU_BASE_URL=http://evil.example \
  "$bin" status
)"

grep -q '^Prefix:       /opt/glustore (default)$' <<<"$status" \
  || { echo "FAIL: GLU_PREFIX changed the prefix" >&2; exit 1; }
grep -q '^Registry:     https://registry.glu.run$' <<<"$status" \
  || { echo "FAIL: GLU_REGISTRY changed the registry origin" >&2; exit 1; }
grep -q '^Distribution: https://github.com/henrikklee/glu/releases$' <<<"$status" \
  || { echo "FAIL: GLU_BASE_URL changed the distribution origin" >&2; exit 1; }

# 2) Dev-only capability must be absent from the artifact.
for s in GLU_REGISTRY GLU_NO_VERIFY localhost:3000; do
  if strings "$bin" | grep -q "$s"; then
    echo "FAIL: '$s' found in the published binary — dev-only code leaked in" >&2
    exit 1
  fi
done

echo "check-release-policy: OK (published build is production-only)"
