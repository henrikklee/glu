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
VERSION='0.1.3'

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
ORIGINAL_PATH="$PATH"
mkdir -p "$TMPDIR"
SERVER_PID=''
cleanup() {
  if [[ -n "$SERVER_PID" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

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

# Check platform detection in the actual source or packaged installer.
python3 scripts/test-install-platform.py "$INSTALLER"

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
  echo "glu ${GLU_FAKE_VERSION:-0.1.3}"
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
run_installer_from() {
  local base_url="$1" prefix="$2"
  shift 2
  env \
    GLU_BASE_URL="$base_url" \
    GLU_PREFIX="$prefix" \
    GLU_MARKER="$WORK/marker" \
    GLU_FAKE_VERSION="$VERSION" \
    bash "$INSTALLER" "$@" < /dev/null
}

run_installer_at() {
  local prefix="$1"
  shift
  run_installer_from "file://$FIXTURE/releases" "$prefix" "$@"
}

run_installer() {
  run_installer_at "$PREFIX" "$@"
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

# Restore the valid sidecar for the remaining tests.
sha256 "$pinned_dir/$ASSET" > "$pinned_dir/$ASSET.sha256"

# --- test 6: redirects and transient failures are handled quietly ---------
echo "test 6: archive and checksum redirects and transient failures"
cat > "$WORK/redirect-server.py" <<'PY'
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

root, port_file, request_log = sys.argv[1:]

class Handler(BaseHTTPRequestHandler):
    failed_once = set()

    def do_GET(self):
        with open(request_log, "a", encoding="utf-8") as log:
            log.write(self.path + "\n")
        prefix = "/releases/latest/download/"
        if self.path.startswith(prefix):
            name = self.path[len(prefix):]
            self.send_response(302)
            self.send_header("Location", f"/objects/{name}")
            self.end_headers()
            return
        if self.path.startswith("/objects/"):
            name = self.path[len("/objects/"):]
            path = os.path.join(root, name)
            if os.path.isfile(path):
                if name not in Handler.failed_once:
                    Handler.failed_once.add(name)
                    self.send_response(503)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                with open(path, "rb") as source:
                    body = source.read()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *args):
        pass

server = HTTPServer(("127.0.0.1", 0), Handler)
with open(port_file, "w", encoding="ascii") as destination:
    destination.write(str(server.server_port))
server.serve_forever()
PY
redirect_port_file="$WORK/redirect-port"
redirect_log="$WORK/redirect-requests"
python3 "$WORK/redirect-server.py" "$latest_dir" "$redirect_port_file" "$redirect_log" &
SERVER_PID=$!
for _ in {1..50}; do
  [[ -s "$redirect_port_file" ]] && break
  sleep 0.1
done
[[ -s "$redirect_port_file" ]] || fail 'redirect server did not start'
redirect_port="$(cat "$redirect_port_file")"
redirect_prefix="$WORK/redirect-prefix"
run_installer_from "http://127.0.0.1:$redirect_port/releases" "$redirect_prefix" \
  >"$WORK/redirect-out" 2>"$WORK/redirect-err"
kill "$SERVER_PID" 2>/dev/null || true
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=''
[[ -x "$redirect_prefix/bin/glu" ]] || fail 'redirected artifact was not installed'
[[ ! -s "$WORK/redirect-err" ]] \
  || fail "successful retries produced noisy output: $(cat "$WORK/redirect-err")"
grep -q "/releases/latest/download/$ASSET$" "$redirect_log" \
  || fail 'archive redirect endpoint was not requested'
grep -q "/objects/$ASSET$" "$redirect_log" \
  || fail 'archive redirect was not followed'
grep -q "/releases/latest/download/$ASSET.sha256$" "$redirect_log" \
  || fail 'checksum redirect endpoint was not requested'
grep -q "/objects/$ASSET.sha256$" "$redirect_log" \
  || fail 'checksum redirect was not followed'
[[ "$(grep -c "/objects/$ASSET$" "$redirect_log")" == 2 ]] \
  || fail 'archive request was not retried exactly once'
[[ "$(grep -c "/objects/$ASSET.sha256$" "$redirect_log")" == 2 ]] \
  || fail 'checksum request was not retried exactly once'
echo "ok: archive and checksum redirects and silent retries"

# A harmless local binary keeps the prefix-policy tests independent of the
# production binary and shell setup.
SAFE_BIN="$WORK/safe-glu"
cat > "$SAFE_BIN" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\${1:-}" >> "$WORK/safe-glu-invocations"
EOF
chmod +x "$SAFE_BIN"

run_local_installer_at() {
  local prefix="$1"
  env \
    GLU_BASE_URL="file://$FIXTURE/releases" \
    GLU_PREFIX="$prefix" \
    GLU_BINARY="$SAFE_BIN" \
    HOME="$HOME_DIR" \
    TMPDIR="$TMPDIR" \
    bash "${LOCAL_INSTALLER:-$INSTALLER}" < /dev/null
}

# --- test 7: explicit temp prefixes never use sudo -------------------------
echo "test 7: explicit temp prefixes are unprivileged"
fake_path="$WORK/fake-path"
mkdir -p "$fake_path"
cat > "$fake_path/sudo" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${SUDO_LOG:?}"
exit 99
EOF
chmod +x "$fake_path/sudo"
sudo_spy_installer="$WORK/sudo-spy-install.sh"
python3 - "$INSTALLER" "$sudo_spy_installer" "$fake_path/sudo" <<'PY'
from pathlib import Path
import sys
source, destination, sudo = map(Path, sys.argv[1:])
destination.write_text(source.read_text().replace("/usr/bin/sudo", str(sudo)))
PY
chmod +x "$sudo_spy_installer"
unprivileged_prefix="$WORK/unprivileged-prefix"
(
  export LOCAL_INSTALLER="$sudo_spy_installer"
  export SUDO_LOG="$WORK/unexpected-sudo"
  run_local_installer_at "$unprivileged_prefix"
)
[[ -x "$unprivileged_prefix/bin/glu" ]] || fail 'explicit temp-prefix install failed'
[[ ! -e "$WORK/unexpected-sudo" ]] || fail 'explicit temp-prefix install invoked sudo'
echo "ok: explicit temp prefix remained unprivileged"

# --- test 8: unsafe existing prefixes remain untouched --------------------
echo "test 8: unsafe existing prefixes remain untouched"
symlink_target="$WORK/symlink-target"
symlink_prefix="$WORK/symlink-prefix"
mkdir "$symlink_target"
printf '%s\n' untouched > "$symlink_target/sentinel"
ln -s "$symlink_target" "$symlink_prefix"
if run_local_installer_at "$symlink_prefix" >"$WORK/symlink-out" 2>"$WORK/symlink-err"; then
  fail 'installer accepted a symlinked prefix'
fi
grep -q 'prefix must not be a symlink' "$WORK/symlink-err" \
  || fail "unexpected symlink-prefix failure: $(cat "$WORK/symlink-err")"
[[ "$(cat "$symlink_target/sentinel")" == 'untouched' ]] || fail 'symlink target was modified'
[[ ! -e "$symlink_target/bin" ]] || fail 'installer wrote through a symlinked prefix'

nonwritable_prefix="$WORK/nonwritable-prefix"
mkdir -m 0555 "$nonwritable_prefix"
if run_local_installer_at "$nonwritable_prefix" >"$WORK/nonwritable-out" 2>"$WORK/nonwritable-err"; then
  fail 'installer accepted a non-writable existing prefix'
fi
grep -q 'not writable' "$WORK/nonwritable-err" \
  || fail "unexpected non-writable-prefix failure: $(cat "$WORK/nonwritable-err")"
[[ ! -e "$nonwritable_prefix/bin" ]] || fail 'installer modified a non-writable prefix'
chmod 0755 "$nonwritable_prefix"

world_writable_prefix="$WORK/world-writable-prefix"
mkdir -m 0777 "$world_writable_prefix"
if run_local_installer_at "$world_writable_prefix" >"$WORK/mode-out" 2>"$WORK/mode-err"; then
  fail 'installer accepted a world-writable existing prefix'
fi
grep -q 'unsafe permissions 777' "$WORK/mode-err" \
  || fail "unexpected world-writable-prefix failure: $(cat "$WORK/mode-err")"
[[ ! -e "$world_writable_prefix/bin" ]] || fail 'installer modified a world-writable prefix'

outside_bin="$WORK/outside-bin"
symlink_bin_prefix="$WORK/symlink-bin-prefix"
mkdir "$outside_bin" "$symlink_bin_prefix"
printf '%s\n' untouched > "$outside_bin/sentinel"
ln -s "$outside_bin" "$symlink_bin_prefix/bin"
if run_local_installer_at "$symlink_bin_prefix" >"$WORK/bin-out" 2>"$WORK/bin-err"; then
  fail 'installer accepted a symlinked binary directory'
fi
grep -q 'binary directory must not be a symlink' "$WORK/bin-err" \
  || fail "unexpected symlink-bin failure: $(cat "$WORK/bin-err")"
[[ "$(cat "$outside_bin/sentinel")" == 'untouched' ]] || fail 'symlinked binary target was modified'
[[ ! -e "$outside_bin/glu" ]] || fail 'installer wrote through a symlinked binary directory'
echo "ok: unsafe existing prefixes remained untouched"

# --- test 9: broad and remote development inputs fail closed --------------
echo "test 9: broad and remote development inputs fail closed"
if run_local_installer_at /tmp >"$WORK/broad-out" 2>"$WORK/broad-err"; then
  fail 'installer accepted /tmp as a prefix'
fi
grep -q 'dedicated development directory' "$WORK/broad-err" \
  || fail "unexpected broad-prefix failure: $(cat "$WORK/broad-err")"
if env GLU_BASE_URL='http://example.com/releases' GLU_PREFIX="$WORK/remote-prefix" \
  GLU_BINARY="$SAFE_BIN" bash "$INSTALLER" >"$WORK/remote-out" 2>"$WORK/remote-err"; then
  fail 'installer accepted a remote plaintext distribution URL'
fi
grep -q 'must use HTTPS' "$WORK/remote-err" \
  || fail "unexpected plaintext-URL failure: $(cat "$WORK/remote-err")"
echo "ok: broad and remote development inputs rejected"

# --- default-prefix harness ------------------------------------------------
# Replace the one compile-time installer default with an isolated path. This
# exercises the production branch without touching the host's /opt tree.
default_installer="$WORK/default-install.sh"
default_prefix="$WORK/default-prefix"
python3 - "$INSTALLER" "$default_installer" "$default_prefix" <<'PY'
from pathlib import Path
import sys
source, destination, prefix = map(Path, sys.argv[1:])
text = source.read_text()
text = text.replace("/opt/glustore", str(prefix))
text = text.replace("/usr/bin/sudo", str(destination.parent / "sudo-path" / "sudo"))
destination.write_text(text)
PY
chmod +x "$default_installer"

sudo_path="$WORK/sudo-path"
sudo_log="$WORK/sudo.log"
mkdir -p "$sudo_path"
cat > "$sudo_path/sudo" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${SUDO_LOG:?}"
if [[ "${1:-}" == '-n' && "${2:-}" == 'true' ]]; then
  [[ "${SUDO_REQUIRE_PASSWORD:-0}" == '0' ]]
  exit
fi
if [[ "${1:-}" == '-v' ]]; then
  printf 'fake sudo password: ' >/dev/tty
  IFS= read -r password
  [[ "$password" == 'test-password' ]]
  exit
fi
if [[ "${1:-}" == '-n' ]]; then
  shift
fi
exec "$@"
EOF
chmod +x "$sudo_path/sudo"

run_default_installer() {
  env -u GLU_PREFIX \
    PATH="$sudo_path:$ORIGINAL_PATH" \
    SUDO_LOG="$sudo_log" \
    GLU_BASE_URL="file://$FIXTURE/releases" \
    GLU_BINARY="$SAFE_BIN" \
    HOME="$HOME_DIR" \
    TMPDIR="$TMPDIR" \
    bash "$default_installer"
}

# --- test 10: passwordless default creation is exact and quiet ------------
echo "test 10: default creation is exact, non-recursive, and quiet"
: > "$sudo_log"
passwordless_output="$(SUDO_REQUIRE_PASSWORD=0 run_default_installer)"
[[ -d "$default_prefix/bin" && -x "$default_prefix/bin/glu" ]] \
  || fail 'default-prefix harness did not install glu'
[[ "$(stat -f '%u' "$default_prefix")" == "$(id -u)" ]] \
  || fail 'created default prefix has the wrong owner'
[[ "$(stat -f '%OLp' "$default_prefix")" == '755' ]] \
  || fail 'created default prefix has the wrong mode'
grep -q -- "-n /bin/mkdir -m 0755 $default_prefix" "$sudo_log" \
  || fail "default prefix was not created exactly: $(cat "$sudo_log")"
grep -q -- "-n /usr/sbin/chown $(id -u):$(id -g) $default_prefix" "$sudo_log" \
  || fail "default prefix ownership was not assigned exactly: $(cat "$sudo_log")"
if grep -Eq 'chown .*-[A-Za-z]*R|chown -R' "$sudo_log"; then
  fail "recursive ownership change detected: $(cat "$sudo_log")"
fi
[[ "$passwordless_output" != *'Administrator approval is needed'* ]] \
  || fail 'passwordless sudo displayed password-explanation text'
echo "ok: passwordless default creation was exact and quiet"

# --- test 11: piped installer authenticates through its terminal ----------
echo "test 11: piped installer authenticates through /dev/tty"
interactive_installer="$WORK/interactive-install.sh"
interactive_prefix="$WORK/interactive-prefix"
python3 - "$INSTALLER" "$interactive_installer" "$interactive_prefix" <<'PY'
from pathlib import Path
import sys
source, destination, prefix = map(Path, sys.argv[1:])
text = source.read_text().replace("/opt/glustore", str(prefix))
text = text.replace("/usr/bin/sudo", str(destination.parent / "sudo-path" / "sudo"))
destination.write_text(text)
PY
chmod +x "$interactive_installer"
cat > "$WORK/pty-installer.py" <<'PY'
import os
import pty
import select
import signal
import sys
import time

installer = sys.argv[1]
pid, master = pty.fork()
if pid == 0:
    os.execv(
        "/bin/bash",
        ["/bin/bash", "-c", 'cat "$1" | /bin/bash', "installer-pty", installer],
    )

sent = False
seen = b""
deadline = time.monotonic() + 30
status = None
while status is None:
    if time.monotonic() >= deadline:
        os.kill(pid, signal.SIGKILL)
        _, status = os.waitpid(pid, 0)
        break
    if select.select([master], [], [], 0.1)[0]:
        try:
            data = os.read(master, 1024)
        except OSError:
            data = b""
        if data:
            os.write(sys.stdout.fileno(), data)
            seen += data
            if b"fake sudo password:" in seen and not sent:
                os.write(master, b"test-password\n")
                sent = True
    ended, child_status = os.waitpid(pid, os.WNOHANG)
    if ended:
        status = child_status

os.close(master)
sys.exit(os.waitstatus_to_exitcode(status))
PY
: > "$sudo_log"
if ! env -u GLU_PREFIX \
  PATH="$sudo_path:$ORIGINAL_PATH" \
  SUDO_LOG="$sudo_log" \
  SUDO_REQUIRE_PASSWORD=1 \
  GLU_BASE_URL="file://$FIXTURE/releases" \
  GLU_BINARY="$SAFE_BIN" \
  HOME="$HOME_DIR" \
  TMPDIR="$TMPDIR" \
  python3 "$WORK/pty-installer.py" "$interactive_installer" >"$WORK/interactive-out"; then
  fail "piped interactive install failed: $(cat "$WORK/interactive-out")"
fi
[[ -x "$interactive_prefix/bin/glu" ]] || fail 'piped interactive install did not complete'
grep -q 'Administrator approval is needed once' "$WORK/interactive-out" \
  || fail 'interactive install did not explain why administrator approval was needed'
grep -q 'Package installations themselves do not run as root' "$WORK/interactive-out" \
  || fail 'interactive install did not explain the privilege boundary'
grep -q -- '-v -p Administrator password for glu setup' "$sudo_log" \
  || fail "interactive install did not validate sudo through the terminal: $(cat "$sudo_log")"
echo "ok: piped installer authenticated through its terminal"

# --- test 12: an installer already running through sudo does not prompt ----
echo "test 12: root invocation preserves the invoking account"
if /usr/bin/sudo -n true 2>/dev/null; then
  root_installer="$WORK/root-install.sh"
  root_prefix="$WORK/root-prefix"
  python3 - "$INSTALLER" "$root_installer" "$root_prefix" <<'PY'
from pathlib import Path
import sys
source, destination, prefix = map(Path, sys.argv[1:])
destination.write_text(source.read_text().replace("/opt/glustore", str(prefix)))
PY
  chmod +x "$root_installer"
  root_output="$(/usr/bin/sudo -n /usr/bin/env -u GLU_PREFIX \
    GLU_BASE_URL="file://$FIXTURE/releases" \
    GLU_BINARY="$SAFE_BIN" \
    TMPDIR="$TMPDIR" \
    /bin/bash "$root_installer")"
  [[ -x "$root_prefix/bin/glu" ]] || fail 'sudo-invoked installer did not complete'
  [[ "$(stat -f '%u' "$root_prefix")" == "$(id -u)" ]] \
    || fail 'sudo-invoked installer did not preserve the invoking account as owner'
  [[ "$root_output" != *'Administrator approval is needed'* ]] \
    || fail 'sudo-invoked installer displayed password-explanation text'

  # These captures are intentionally opened by the test user, not by sudo.
  # shellcheck disable=SC2024
  if /usr/bin/sudo -n /usr/bin/env -u SUDO_USER -u GLU_PREFIX \
    GLU_BASE_URL="file://$FIXTURE/releases" GLU_BINARY="$SAFE_BIN" \
    /bin/bash "$root_installer" >"$WORK/direct-root-out" 2>"$WORK/direct-root-err"; then
    fail 'direct root invocation without an owning account was accepted'
  fi
  grep -q 'Do not run the installer from a direct root shell' "$WORK/direct-root-err" \
    || fail "unexpected direct-root failure: $(cat "$WORK/direct-root-err")"
  echo "ok: sudo invocation preserved the invoking account without prompting"
else
  echo "ok: skipped root-path execution (passwordless sudo unavailable)"
fi

echo
echo "all installer tests passed"
