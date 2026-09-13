#!/usr/bin/env python3
"""Exercise the shipped installer's platform block without downloading/installing.

Only uname and sysctl are stubbed; no hardware probe or prefix is modified.
An optional installer path also covers the exact packaged release script.
"""
import os
from pathlib import Path
import subprocess
import sys

installer = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).with_name("install.sh")
source = installer.read_text()
platform = source.split("# --- platform ", 1)[1].split('target="${GLU_ARCH:-$target}"', 1)[0]
# Drop the remainder of the section-heading comment.
platform = platform.split("\n", 1)[1]
shell = r'''
set -euo pipefail
uname() { printf '%s\n' "$TEST_PLATFORM"; }
sysctl() {
  [[ "$*" = '-n hw.optional.arm64' ]] || exit 99
  printf '%s\n' "$TEST_ARM64"
  return "$TEST_SYSCTL_STATUS"
}
error() { printf '%s\n' "$*" >&2; exit 1; }
info() { printf '%s\n' "$*" >&2; }
''' + platform + '\nprintf "%s\\n" "$target"\n'

cases = [
    ("native Apple Silicon", "Darwin arm64", "", "99", True),
    ("Rosetta on Apple Silicon", "Darwin x86_64", "1", "0", True),
    ("Intel Mac", "Darwin x86_64", "0", "0", False),
    ("missing hardware key", "Darwin x86_64", "", "1", False),
    ("unexpected hardware value", "Darwin x86_64", "unknown", "0", False),
    ("Linux ARM64", "Linux aarch64", "1", "0", False),
    ("other Darwin architecture", "Darwin i386", "1", "0", False),
]
for name, reported, arm64, status, supported in cases:
    result = subprocess.run(
        ["/bin/bash", "-c", shell], text=True, capture_output=True,
        env={**os.environ, "TEST_PLATFORM": reported, "TEST_ARM64": arm64,
             "TEST_SYSCTL_STATUS": status},
        check=False,
    )
    if supported:
        assert result.returncode == 0, (name, result.stderr)
        assert result.stdout.strip() == "aarch64-apple-darwin", (name, result.stdout)
    else:
        assert result.returncode == 1, (name, result.returncode, result.stderr)
        assert "supports Apple Silicon" in result.stderr or "currently supports macOS only" in result.stderr
    print(f"ok: installer platform — {name}")
