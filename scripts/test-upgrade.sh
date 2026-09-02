#!/usr/bin/env bash
# Offline e2e for `glu upgrade`: the registry reports x-glu-latest-version,
# the distribution serves the release, and the running binary is swapped
# only after checksum + sanity verification. Uses the real compiled binary.
set -euo pipefail

cd "$(dirname "$0")/.."
DEV_TARGET_DIR="${GLU_DEV_TARGET_DIR:-target/dev-registry}"
CARGO_TARGET_DIR="$DEV_TARGET_DIR" cargo build --release --locked --features dev-registry >/dev/null
DEV_BIN="$DEV_TARGET_DIR/release/glu"

case "$(uname -ms)" in
'Darwin arm64') TARGET='aarch64-apple-darwin' ;;
*) echo "requires Apple Silicon (arm64)"; exit 1 ;;
esac
ASSET="glu-$TARGET.tar.gz"

WORK="$(mktemp -d)"
PREFIX="$WORK/prefix"
DIST="$WORK/dist"
LOG="$WORK/requests.log"
PORT=8901
trap 'pkill -f mock-glu-server.py 2>/dev/null || true; rm -rf "$WORK"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

mkdir -p "$PREFIX/bin"
cp "$DEV_BIN" "$PREFIX/bin/glu"

# --- fake next release: a real ARM64 Mach-O reporting 0.2.0 -------------
mkdir -p "$DIST/dist/download/v0.2.0" "$WORK/release"
cat > "$WORK/fake-glu.rs" <<'EOF'
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("glu 0.2.0");
    }
}
EOF
rustc -O "$WORK/fake-glu.rs" -o "$WORK/release/glu"
for release_file in LICENSE-BSD-2-Clause LICENSE-MIT THIRD_PARTY_LICENSES.html THIRD_PARTY_NOTICES.md; do
  cp "$release_file" "$WORK/release/$release_file"
done
COPYFILE_DISABLE=1 tar -C "$WORK/release" -czf "$DIST/dist/download/v0.2.0/$ASSET" \
  LICENSE-BSD-2-Clause LICENSE-MIT THIRD_PARTY_LICENSES.html THIRD_PARTY_NOTICES.md glu
shasum -a 256 "$DIST/dist/download/v0.2.0/$ASSET" | awk '{print $1}' > "$DIST/dist/download/v0.2.0/$ASSET.sha256"

# --- mock server: /v1/outdated (registry) + /dist/... (distribution) ------
cat > "$WORK/mock-glu-server.py" <<'EOF'
import json, os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
LATEST, DIST, LOG, PORT = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        with open(LOG, "a") as f:
            f.write(self.path + "\n")
        if self.path.startswith("/v1/outdated"):
            body = json.dumps({"schema": "glu.outdated.v1", "packages": []}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("x-glu-latest-version", LATEST)
        else:
            path = os.path.join(DIST, self.path.lstrip("/"))
            if not os.path.isfile(path):
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            with open(path, "rb") as f:
                body = f.read()
            self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", PORT), H).serve_forever()
EOF

start_server() {
  python3 "$WORK/mock-glu-server.py" "$1" "$DIST" "$LOG" "$PORT" 2>/dev/null &
  SERVER_PID=$!
  sleep 0.5
}

run_upgrade() {
  env GLU_BASE_URL="http://127.0.0.1:$PORT/dist" \
    GLU_REGISTRY="http://127.0.0.1:$PORT" \
    GLU_PREFIX="$PREFIX" \
    "$PREFIX/bin/glu" upgrade
}

# --- test 1: already up to date -> no download ----------------------------
echo "test 1: registry reports own version -> already up to date, no download"
: > "$LOG"
start_server "0.1.0"
out="$(run_upgrade 2>&1)"
kill "$SERVER_PID" 2>/dev/null || true
echo "$out" | grep -q "Already up to date (glu 0.1.0)" || fail "unexpected output: $out"
grep -q "download/" "$LOG" && fail "downloaded despite being up to date"
[[ "$("$PREFIX/bin/glu" --version)" == "glu 0.1.0" ]] || fail "binary was modified"
echo "ok: already up to date"

# --- test 2: update -> download, verify, sanity-run, swap -----------------
echo "test 2: registry reports 0.2.0 -> download, verify, swap"
: > "$LOG"
start_server "0.2.0"
GLU_MARKER="$WORK/marker" run_upgrade > "$WORK/out" 2>&1
kill "$SERVER_PID" 2>/dev/null || true
grep -q "glu updated to 0.2.0" "$WORK/out" || fail "unexpected output: $(cat "$WORK/out")"
grep -q "download/v0.2.0/glu-" "$LOG" || fail "v-prefixed release was not downloaded"
[[ "$("$PREFIX/bin/glu" --version)" == "glu 0.2.0" ]] || fail "binary was not swapped"
echo "ok: updated to 0.2.0"

# --- tests 3-4 run against the real binary again --------------------------
cp "$DEV_BIN" "$PREFIX/bin/glu"

# --- test 3: checksum mismatch --------------------------------------------
echo "test 3: checksum mismatch is rejected, binary unchanged"
echo "0000000000000000000000000000000000000000000000000000000000000000" > "$DIST/dist/download/v0.2.0/$ASSET.sha256"
start_server "0.2.0"
if run_upgrade > "$WORK/out" 2>&1; then
  fail 'upgrade succeeded despite checksum mismatch'
fi
kill "$SERVER_PID" 2>/dev/null || true
grep -q "checksum mismatch" "$WORK/out" || fail "unexpected failure output: $(cat "$WORK/out")"
[[ "$("$PREFIX/bin/glu" --version)" == "glu 0.1.0" ]] || fail "binary changed on failed upgrade"
echo "ok: checksum mismatch rejected"

# --- test 4: registry ahead of distribution -------------------------------
echo "test 4: registry says 0.3.0 but distribution lacks it -> clear error"
start_server "0.3.0"
if run_upgrade > "$WORK/out" 2>&1; then
  fail 'upgrade succeeded despite missing release'
fi
kill "$SERVER_PID" 2>/dev/null || true
grep -q "failed to fetch" "$WORK/out" || fail "unexpected failure output: $(cat "$WORK/out")"
[[ "$("$PREFIX/bin/glu" --version)" == "glu 0.1.0" ]] || fail "binary changed on failed upgrade"
echo "ok: missing release rejected"

echo
echo "all upgrade tests passed"
