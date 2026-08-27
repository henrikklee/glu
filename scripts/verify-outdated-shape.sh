#!/usr/bin/env bash
# Offline e2e for the new /v1/outdated response shape: each package carries
# an `update` (newest installable on this target) / `latest` (newest visible
# overall) envelope, and `glu outdated` renders both columns. Uses the real
# compiled binary against a mock registry.
set -euo pipefail

cd "$(dirname "$0")/.."
DEV_TARGET_DIR="${GLU_DEV_TARGET_DIR:-target/dev-registry}"
CARGO_TARGET_DIR="$DEV_TARGET_DIR" cargo build --locked --features dev-registry >/dev/null
BIN="$DEV_TARGET_DIR/debug/glu"
[[ -x "$BIN" ]] || { echo "dev build missing: $BIN"; exit 1; }

WORK="$(mktemp -d)"
PREFIX="$WORK/prefix"
PORT=8907
trap 'pkill -f mock-outdated.py 2>/dev/null || true; rm -rf "$WORK"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# Fake installed receipts: confuse 3.2, node 22.1.0
mkdir -p "$PREFIX/Cellar/confuse/3.2_0/.glu" "$PREFIX/Cellar/node/22.1.0_0/.glu" "$PREFIX/opt"
for pkg in confuse:3.2:0 node:22.1.0:0; do
  name="${pkg%%:*}"; rest="${pkg#*:}"; ver="${rest%%:*}"; rev="${rest##*:}"
  cat > "$PREFIX/Cellar/$name/${ver}_${rev}/.glu/receipt.json" <<EOF
{"schema":"glu.receipt.v1","status":"complete",
 "package":{"id":"pkg:test/$name@${ver}_${rev}","name":"$name","version":"$ver","revision":$rev,"keg_version":"${ver}_${rev}"},
 "artifact":{"id":"art:test","sha256":"0","bottle_tag":"arm64_sequoia","cellar":":any"},
 "paths":{"keg":"$PREFIX/Cellar/$name/${ver}_${rev}","opt":"$PREFIX/opt/$name"},
 "install":{"keg_only":false,"linked":true,"deps":[]}}
EOF
  ln -sf "$PREFIX/Cellar/$name/${ver}_${rev}" "$PREFIX/opt/$name"
done

cat > "$WORK/mock-outdated.py" <<'EOF'
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/v1/outdated"):
            # NEW shape: update (installable) vs latest (visible overall)
            body = json.dumps({"schema":"glu.outdated.v1","packages":{
                "confuse":{"update":{"version":"3.3","revision":0},"latest":{"version":"3.4","revision":0}},
                "node":{"update":{"version":"22.2.0","revision":0},"latest":{"version":"22.2.0","revision":0}},
                "update-null":{"update":None,"latest":{"version":"9.9","revision":0}},
            }}).encode()
            self.send_response(200)
            self.send_header("Content-Type","application/json")
            self.send_header("Content-Length",str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404); self.end_headers()
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", 8907), H).serve_forever()
EOF
python3 "$WORK/mock-outdated.py" & SERVER_PID=$!
sleep 0.5

OUT="$(GLU_REGISTRY="http://127.0.0.1:$PORT" GLU_PREFIX="$PREFIX" "$BIN" outdated)"
echo "$OUT"

# Both columns render, and update-null (nothing installable) is omitted.
echo "$OUT" | grep -q "│ confuse │ 3.2_0    │ 3.3    │ 3.4    │" || fail "confuse row wrong:\n$OUT"
echo "$OUT" | grep -q "│ node    │ 22.1.0_0 │ 22.2.0 │ 22.2.0 │" || fail "node row wrong:\n$OUT"
echo "$OUT" | grep -q "Update" || fail "missing Update column:\n$OUT"
echo "$OUT" | grep -q "Latest" || fail "missing Latest column:\n$OUT"
echo "$OUT" | grep -q "update-null" && fail "update:null package listed:\n$OUT" || true

echo "ok: outdated shape renders both columns"
kill "$SERVER_PID" 2>/dev/null || true
