#!/usr/bin/env bash
# Offline contract tests for retry-safe release publication.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

work="$(mktemp -d "${TMPDIR:-/tmp}/glu-publish-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/bin" "$work/dist"
touch \
  "$work/dist/glu-aarch64-apple-darwin.tar.gz" \
  "$work/dist/glu-aarch64-apple-darwin.tar.gz.sha256" \
  "$work/dist/install.sh"

cat > "$work/bin/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%q ' "$@" >> "${FAKE_GH_LOG:?}"
printf '\n' >> "$FAKE_GH_LOG"

case "${1:-} ${2:-}" in
'release view')
  case "${FAKE_GH_STATE:?}" in
  absent) exit 1 ;;
  draft) printf 'true\tfalse\tv0.1.0\n' ;;
  published) printf 'false\ttrue\tv0.1.0\n' ;;
  esac
  ;;
'release create'|'release upload'|'release edit') ;;
*) exit 2 ;;
esac
EOF
chmod +x "$work/bin/gh"

run_publish() {
  : > "$work/log"
  FAKE_GH_LOG="$work/log" \
    FAKE_GH_STATE="$1" \
    GH_BIN="$work/bin/gh" \
    GITHUB_REPOSITORY='henrikklee/glu' \
    scripts/publish-release.sh v0.1.0 0.1.0 "$work/dist"
}

run_publish absent
grep -q '^release create ' "$work/log"
grep -q '^release edit ' "$work/log"
if grep -q '^release upload ' "$work/log"; then
  echo 'FAIL: new release unexpectedly used the retry upload path' >&2
  exit 1
fi

run_publish draft
grep -q '^release upload ' "$work/log"
grep -q -- '--clobber' "$work/log"
grep -q '^release edit ' "$work/log"
if grep -q '^release create ' "$work/log"; then
  echo 'FAIL: draft retry unexpectedly attempted to create another release' >&2
  exit 1
fi

: > "$work/log"
if FAKE_GH_LOG="$work/log" \
  FAKE_GH_STATE='published' \
  GH_BIN="$work/bin/gh" \
  GITHUB_REPOSITORY='henrikklee/glu' \
  scripts/publish-release.sh v0.1.0 0.1.0 "$work/dist" >/dev/null 2>&1; then
  echo 'FAIL: published release was mutable through the publication script' >&2
  exit 1
fi
if grep -Eq '^release (upload|edit|create) ' "$work/log"; then
  echo 'FAIL: published release was modified' >&2
  exit 1
fi

echo 'publish release tests passed'
