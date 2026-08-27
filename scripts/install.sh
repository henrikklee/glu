#!/usr/bin/env bash
# glu installer — bootstrap the glu macOS package manager.
#
#   curl -fsSL <distribution-url>/install.sh | bash
#
# Thin by design: it downloads and verifies the glu binary, then hands off to
# the binary (`glu setup`) for shell integration. All package logic lives in
# the binary.
#
# Distribution contract:
#
#   artifact   glu-<target>.tar.gz          tarball with a single `glu` binary
#                                           at its root
#   target     aarch64-apple-darwin         (Apple Silicon only)
#   checksum   <artifact>.sha256            sidecar; first whitespace token is
#                                           the lowercase hex digest
#   version    a semver (e.g. 0.1.0), release tag (e.g. v0.1.0), or `latest`;
#              bare semver is normalized to its v-prefixed Git tag
#   URLs       $GLU_BASE_URL/download/$version/$artifact    (pinned)
#              $GLU_BASE_URL/latest/download/$artifact      (latest)
#
#   GLU_BASE_URL defaults to a GitHub releases root:
#       https://github.com/henrikklee/glu/releases
#
# Environment overrides:
#   GLU_BASE_URL   distribution root (see URL scheme above); file:// mirrors
#                  work, which is how the offline test harness drives this
#   GLU_VERSION    pin a version/tag (equivalent to passing it as $1)
#   GLU_PREFIX     install prefix; default /opt/glustore
#   GLU_ARCH       force a target triple (testing)
#   GLU_BINARY     install this local binary instead of downloading (dev/test)
#   GLU_NO_VERIFY  skip checksum verification (testing only)
#
# Uninstall is out of scope for v0.1.
set -euo pipefail

# --- presentation ----------------------------------------------------------
Color_Off=''; Red=''; Green=''; Dim=''; Bold_White=''
if [[ -t 1 ]]; then
  Color_Off='\033[0m'
  Red='\033[0;31m'
  Green='\033[0;32m'
  Dim='\033[0;2m'
  Bold_White='\033[1m'
fi

error() {
  echo -e "${Red}error${Color_Off}: $*" >&2
  exit 1
}

info() { echo -e "${Dim}$*${Color_Off}"; }
success() { echo -e "${Green}$*${Color_Off}"; }

tildify() {
  local path="$1"
  if [[ "$path" == "$HOME"/* ]]; then
    # The tilde is intentional display text, not shell expansion.
    # shellcheck disable=SC2088
    echo "~/${path#"$HOME"/}"
  else
    echo "$path"
  fi
}

# --- configuration ---------------------------------------------------------
GLU_BASE_URL="${GLU_BASE_URL:-https://github.com/henrikklee/glu/releases}"
GLU_PREFIX="${GLU_PREFIX:-/opt/glustore}"
BIN_DIR="$GLU_PREFIX/bin"
GLU_BIN="$BIN_DIR/glu"

requested_version="${1:-${GLU_VERSION:-latest}}"
if [[ "$requested_version" == 'latest' ]]; then
  version='latest'
elif [[ "$requested_version" =~ ^v?([0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z][0-9A-Za-z.-]*)?)$ ]]; then
  version="v${BASH_REMATCH[1]}"
else
  error "Invalid glu version: $requested_version (expected 0.1.0, v0.1.0, or latest)."
fi

# --- platform --------------------------------------------------------------
# glu supports Apple Silicon (arm64) Macs only: the registry ingests only
# arm64_* bottle tags, and the fixed-cell relocation story is built around the
# byte-length match with /opt/homebrew.
case "$(uname -ms)" in
'Darwin arm64')
  target='aarch64-apple-darwin'
  ;;
'Darwin'*)
  error 'glu supports Apple Silicon (arm64) Macs only; this Mac is not arm64.'
  ;;
*)
  error "glu currently supports macOS only (got: $(uname -ms))."
  ;;
esac
target="${GLU_ARCH:-$target}"

if [[ "$version" == 'latest' ]]; then
  download_base="$GLU_BASE_URL/latest/download"
else
  download_base="$GLU_BASE_URL/download/$version"
fi
asset="glu-$target.tar.gz"
asset_url="$download_base/$asset"
checksum_url="$asset_url.sha256"

# --- download and verify ---------------------------------------------------
command -v curl >/dev/null || error 'curl is required to install glu.'
command -v tar >/dev/null || error 'tar is required to install glu.'

tmp="$(mktemp -d "${TMPDIR:-/tmp}/glu-install.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

if [[ -n "${GLU_BINARY:-}" ]]; then
  [[ -x "$GLU_BINARY" ]] || error "GLU_BINARY is not executable: $GLU_BINARY"
  info "Using local binary $(tildify "$GLU_BINARY") (skipping download and checksum)."
  downloaded_bin="$GLU_BINARY"
else
  info "Downloading glu ($target, $version)..."
  curl --fail --location --progress-bar --output "$tmp/$asset" "$asset_url" \
    || error "Failed to download glu from $asset_url. If no distribution host is live yet, set GLU_BASE_URL to a mirror (e.g. file://...) or GLU_BINARY to a local build."

  if [[ "${GLU_NO_VERIFY:-0}" = '1' ]]; then
    info 'Skipping checksum verification (GLU_NO_VERIFY=1).'
  else
    command -v shasum >/dev/null || command -v sha256sum >/dev/null \
      || error 'No sha256 tool found (need shasum or sha256sum) to verify the download.'
    curl --fail --silent --show-error --output "$tmp/$asset.sha256" "$checksum_url" \
      || error "Failed to fetch checksum from $checksum_url."
    expected="$(awk '{print $1}' "$tmp/$asset.sha256")"
    [[ "$expected" =~ ^[0-9a-f]{64}$ ]] \
      || error "Malformed checksum in $checksum_url."
    if command -v sha256sum >/dev/null; then
      actual="$(sha256sum -b "$tmp/$asset" | awk '{print $1}')"
    else
      actual="$(shasum -a 256 -b "$tmp/$asset" | awk '{print $1}')"
    fi
    [[ "$actual" = "$expected" ]] \
      || error "Checksum mismatch for $asset (expected $expected, got $actual)."
    info "Checksum verified ($actual)."
  fi

  tar -xzf "$tmp/$asset" -C "$tmp" || error "Failed to extract $asset."
  downloaded_bin="$tmp/glu"
  [[ -x "$downloaded_bin" ]] \
    || error "$asset did not contain an executable glu at its root."
fi

# --- prefix (sudo once, then everything runs as the user) ------------------
ensure_prefix() {
  # Make $GLU_PREFIX writable by the current user, asking for sudo once only
  # when needed. Package installs after setup run as the user, never as root.
  if [[ -d "$GLU_PREFIX" ]]; then
    if [[ ! -w "$GLU_PREFIX" ]]; then
      sudo_prefix_create
    fi
  elif ! mkdir -p "$GLU_PREFIX" 2>/dev/null; then
    # Fresh prefix under a non-writable parent (e.g. /opt) — escalate.
    sudo_prefix_create
  fi
  mkdir -p "$BIN_DIR"
}

sudo_prefix_create() {
  if ! command -v sudo >/dev/null; then
    error "Cannot write to $GLU_PREFIX and sudo is not available. Set GLU_PREFIX to a user-writable directory (e.g. ~/.glu)."
  fi
  if [[ -t 0 ]] || sudo -n true 2>/dev/null; then
    sudo mkdir -p "$GLU_PREFIX"
    sudo chown -R "$(id -un)":staff "$GLU_PREFIX" 2>/dev/null || true
  else
    error "Cannot write to $GLU_PREFIX and no passwordless sudo in a non-interactive shell. Run interactively, or set GLU_PREFIX to a user-writable directory (e.g. ~/.glu)."
  fi
}

ensure_prefix

# --- install the binary (atomic replace) -----------------------------------
info "Installing glu to $(tildify "$GLU_BIN")"
tmp_bin="$BIN_DIR/.glu.$$.tmp"
install -m 0755 "$downloaded_bin" "$tmp_bin"
mv -f "$tmp_bin" "$GLU_BIN"

# --- hand off to the binary -------------------------------------------------
"$GLU_BIN" setup

echo
success "glu installed to $(tildify "$GLU_BIN")"
echo
info 'glu is now first on PATH. If one glu package should not shadow another tool:'
info "  ${Bold_White}glu deactivate <package>${Color_Off}"
