#!/usr/bin/env bash
# Offline end-to-end test for scripts/install.sh.
#
# With --release-dir, consumes the exact package produced by
# scripts/package-release.sh. Without it, builds a local mirror around the
# existing production binary (or a fake glu if no production binary exists).
# HOME, GLU_PREFIX, and TMPDIR are isolated; nothing outside the temp dirs is
# touched. Requires an Apple Silicon Mac.
set -euo pipefail

cd "$(dirname "$0")/.."
INSTALLER="$PWD/scripts/install.sh"
RELEASE_DIR=''
VERSION='0.1.0'

while [[ $# -gt 0 ]]; do
  case "$1" in
  --release-dir)
    [[ $# -ge 2 ]] || { echo 'FAIL: --release-dir requires a path' >&2; exit 1; }
    RELEASE_DIR="$2"
    shift 2
    ;;
  --version)
    [[ $# -ge 2 ]] || { echo 'FAIL: --version requires a value' >&2; exit 1; }
    VERSION="${2#v}"
    shift 2
    ;;
  *)
    echo "FAIL: unknown argument: $1" >&2
    exit 1
    ;;
  esac
done

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z][0-9A-Za-z.-]*)?$ ]] \
  || { echo "FAIL: invalid version: $VERSION" >&2; exit 1; }
TAG="v$VERSION"
if [[ -n "$RELEASE_DIR" ]]; then
  if [[ "$RELEASE_DIR" != /* ]]; then
    RELEASE_DIR="$PWD/$RELEASE_DIR"
  fi
  INSTALLER="$RELEASE_DIR/install.sh"
  [[ -f "$INSTALLER" ]] \
    || { echo "FAIL: packaged installer not found: $INSTALLER" >&2; exit 1; }
fi

WORK="$(mktemp -d)"
FIXTURE="$WORK/mirror"
PREFIX="$WORK/prefix"
HOME_DIR="$WORK/home"
export HOME="$HOME_DIR"
export TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR"
trap 'rm -rf "$WORK"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

case "$(uname -ms)" in
'Darwin arm64') TARGET='aarch64-apple-darwin' ;;
*) fail "this test requires an Apple Silicon Mac (glu is arm64-only); got: $(uname -ms)" ;;
esac
ASSET="glu-$TARGET.tar.gz"

sha256() {
  if command -v sha256sum >/dev/null; then
    sha256sum -b "$1" | awk '{print $1}'
  else
    shasum -a 256 -b "$1" | awk '{print $1}'
  fi
}

# --- fixture mirror (GitHub releases shape) --------------------------------
latest_dir="$FIXTURE/releases/latest/download"
pinned_dir="$FIXTURE/releases/download/$TAG"
mkdir -p "$latest_dir" "$pinned_dir"
REAL=0

if [[ -n "$RELEASE_DIR" ]]; then
  source_asset="$RELEASE_DIR/$ASSET"
  source_checksum="$source_asset.sha256"
  [[ -f "$source_asset" ]] || fail "release artifact not found: $source_asset"
  [[ -f "$source_checksum" ]] || fail "release checksum not found: $source_checksum"
  expected="$(awk '{print $1}' "$source_checksum")"
  [[ "$expected" =~ ^[0-9a-f]{64}$ ]] || fail "malformed release checksum: $source_checksum"
  [[ "$(sha256 "$source_asset")" == "$expected" ]] || fail 'release artifact checksum does not match'
  for dir in "$latest_dir" "$pinned_dir"; do
    cp "$source_asset" "$dir/$ASSET"
    cp "$source_checksum" "$dir/$ASSET.sha256"
  done
  REAL=1
  echo "using exact release artifact: $source_asset"
else
  BIN_SRC="$WORK/glu"
  production_bin="$PWD/${GLU_PRODUCTION_TARGET_DIR:-target}/release/glu"
  if [[ -x "$production_bin" ]]; then
    echo "using real binary: $production_bin"
    cp "$production_bin" "$BIN_SRC"
    REAL=1
  else
    echo "using fake glu (no production release binary)"
    cat > "$BIN_SRC" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "--version" ]]; then
  echo "glu ${GLU_FAKE_VERSION:-0.1.0}"
  exit 0
fi
echo "fake-glu $*" >> "${GLU_MARKER:?}"
EOF
    chmod +x "$BIN_SRC"
  fi
  for dir in "$latest_dir" "$pinned_dir"; do
    tar -C "$WORK" -czf "$dir/$ASSET" glu
    sha256 "$dir/$ASSET" > "$dir/$ASSET.sha256"
  done
fi

# --- installer runner ------------------------------------------------------
run_installer() {
  env \
    GLU_BASE_URL="file://$FIXTURE/releases" \
    GLU_PREFIX="$PREFIX" \
    GLU_MARKER="$WORK/marker" \
    GLU_FAKE_VERSION="$VERSION" \
    bash "$INSTALLER" "$@" < /dev/null
}

assert_installed() {
  [[ -x "$PREFIX/bin/glu" ]] || fail "binary not installed at configured prefix: $PREFIX/bin/glu"
  if [[ $REAL = 0 ]]; then
    [[ -f "$WORK/marker" ]] || fail 'fake glu was never invoked'
    grep -q '^fake-glu setup$' "$WORK/marker" || fail 'setup hand-off missing'
    if grep -q '^fake-glu prefer' "$WORK/marker"; then
      fail 'installer should not call removed prefer command'
    fi
  else
    [[ "$("$PREFIX/bin/glu" --version)" == "glu $VERSION" ]] \
      || fail "installed binary does not report glu $VERSION"
    # With GLU_PREFIX unset, the binary must self-locate its own prefix and
    # emit it on PATH (the shell hook relies on this).
    env -u GLU_PREFIX "$PREFIX/bin/glu" shellenv zsh 2>/dev/null \
      | grep -q "$PREFIX/bin" || fail 'self-location did not resolve the installed prefix'
  fi
}

# --- test 1: latest --------------------------------------------------------
echo "test 1: install from latest"
run_installer
assert_installed
echo "ok: latest install"

# --- test 2: re-run is idempotent -----------------------------------------
echo "test 2: re-run succeeds (idempotent)"
run_installer
assert_installed
echo "ok: idempotent re-run"

# --- test 3: bare semver normalizes to v-prefixed tag ----------------------
echo "test 3: install bare version ($VERSION -> $TAG)"
rm -f "$PREFIX/bin/glu"
run_installer "$VERSION"
assert_installed
echo "ok: bare version normalized"

# --- test 4: explicit v-prefixed tag --------------------------------------
echo "test 4: install explicit tag ($TAG)"
rm -f "$PREFIX/bin/glu"
run_installer "$TAG"
assert_installed
echo "ok: explicit tag install"

# --- test 5: checksum mismatch fails closed -------------------------------
echo "test 5: checksum mismatch is rejected"
echo '0000000000000000000000000000000000000000000000000000000000000000' > "$pinned_dir/$ASSET.sha256"
if run_installer "$VERSION" >/dev/null 2>"$WORK/err"; then
  fail 'installer succeeded despite checksum mismatch'
fi
grep -q 'Checksum mismatch' "$WORK/err" || fail "unexpected failure output: $(cat "$WORK/err")"
echo "ok: checksum mismatch rejected"

echo
echo "all installer tests passed"
