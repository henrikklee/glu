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
#   GLU_PREFIX     unprivileged development/test prefix; default /opt/glustore
#   GLU_ARCH       force a target triple (testing)
#   GLU_BINARY     install this local binary instead of downloading (dev/test)
#   GLU_NO_VERIFY  skip checksum verification (testing only)
#
# Uninstall is out of scope for v0.1.
set -euo pipefail

# --- presentation ----------------------------------------------------------
Color_Off=''; Red=''; Green=''; Dim=''
if [[ -t 1 ]]; then
  Color_Off='\033[0m'
  Red='\033[0;31m'
  Green='\033[0;32m'
  Dim='\033[0;2m'
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
DEFAULT_BASE_URL='https://github.com/henrikklee/glu/releases'
DEFAULT_PREFIX='/opt/glustore'
base_url_overridden=0
prefix_overridden=0
[[ -n "${GLU_BASE_URL:-}" ]] && base_url_overridden=1
if [[ -n "${GLU_PREFIX:-}" && "${GLU_PREFIX:-}" != "$DEFAULT_PREFIX" ]]; then
  prefix_overridden=1
fi
GLU_BASE_URL="${GLU_BASE_URL:-$DEFAULT_BASE_URL}"
GLU_PREFIX="${GLU_PREFIX:-$DEFAULT_PREFIX}"
BIN_DIR="$GLU_PREFIX/bin"
GLU_BIN="$BIN_DIR/glu"

current_uid="$(id -u)"
if [[ "$current_uid" = '0' ]]; then
  if [[ -z "${SUDO_USER:-}" || "$SUDO_USER" = 'root' ]]; then
    error 'Do not run the installer from a direct root shell. Run it as the account that should manage glu; the installer will request administrator approval only when needed.'
  fi
  install_user="$SUDO_USER"
  id "$install_user" >/dev/null 2>&1 \
    || error "Cannot resolve the invoking account from SUDO_USER=$install_user."
else
  install_user="$(id -un)"
fi
install_uid="$(id -u "$install_user")"
install_gid="$(id -g "$install_user")"

normalize_macos_path() {
  case "$1" in
  /private/tmp) printf '%s\n' '/tmp' ;;
  /private/tmp/*) printf '/tmp/%s\n' "${1#/private/tmp/}" ;;
  /private/var) printf '%s\n' '/var' ;;
  /private/var/*) printf '/var/%s\n' "${1#/private/var/}" ;;
  *) printf '%s\n' "$1" ;;
  esac
}

validate_prefix_syntax() {
  [[ "$GLU_PREFIX" == /* ]] \
    || error "GLU_PREFIX must be an absolute path: $GLU_PREFIX"
  [[ "$GLU_PREFIX" != '/' && "$GLU_PREFIX" != */ ]] \
    || error "Unsafe glu prefix: $GLU_PREFIX"
  case "$GLU_PREFIX/" in
  *'//'*) error "GLU_PREFIX must not contain repeated separators: $GLU_PREFIX" ;;
  *'/./'* | *'/../'*) error "GLU_PREFIX must not contain . or .. components: $GLU_PREFIX" ;;
  esac

  if [[ "$prefix_overridden" = '0' && "$GLU_PREFIX" != "$DEFAULT_PREFIX" ]]; then
    error "The supported production prefix is $DEFAULT_PREFIX."
  fi

  if [[ "$prefix_overridden" = '1' ]]; then
    case "$(normalize_macos_path "$GLU_PREFIX")" in
    / | /opt | /usr | /bin | /sbin | /etc | /var | /tmp | /private | /Library | /System | /Applications | "$HOME")
      error "GLU_PREFIX must name a dedicated development directory, not $GLU_PREFIX."
      ;;
    esac
  fi
}

validate_prefix_syntax

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
'Darwin x86_64')
  # An Intel shell under Rosetta reports x86_64 even on Apple Silicon.
  # Probe hardware support, not the architecture of the calling process.
  if [[ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = '1' ]]; then
    target='aarch64-apple-darwin'
    info 'Detected Apple Silicon through Rosetta; installing the ARM64 build.'
  else
    error 'glu supports Apple Silicon (arm64) Macs only; this Mac is not arm64.'
  fi
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

case "$GLU_BASE_URL" in
https://*) curl_transport=(--proto '=https' --proto-redir '=https' --tlsv1.2) ;;
file://*)
  [[ "$base_url_overridden" = '1' ]] \
    || error 'file:// distribution URLs are available only through an explicit GLU_BASE_URL override.'
  curl_transport=(--proto '=file' --proto-redir '=file')
  ;;
http://127.0.0.1:* | http://localhost:* | 'http://[::1]:'*)
  [[ "$base_url_overridden" = '1' ]] \
    || error 'Loopback HTTP distribution URLs require an explicit GLU_BASE_URL override.'
  curl_transport=(--proto '=http' --proto-redir '=http')
  ;;
*) error "GLU_BASE_URL must use HTTPS (or an explicit file:// or loopback HTTP development mirror): $GLU_BASE_URL" ;;
esac

# --- download and verify ---------------------------------------------------
command -v curl >/dev/null || error 'curl is required to install glu.'
command -v tar >/dev/null || error 'tar is required to install glu.'

tmp="$(mktemp -d "${TMPDIR:-/tmp}/glu-install.XXXXXX")"
tmp_bin=''
cleanup() {
  rm -rf "$tmp"
  [[ -z "$tmp_bin" ]] || rm -f "$tmp_bin"
}
trap cleanup EXIT

if [[ -n "${GLU_BINARY:-}" ]]; then
  [[ -x "$GLU_BINARY" ]] || error "GLU_BINARY is not executable: $GLU_BINARY"
  info "Using local binary $(tildify "$GLU_BINARY") (skipping download and checksum)."
  downloaded_bin="$GLU_BINARY"
else
  info "Downloading glu ($target, $version)..."
  curl "${curl_transport[@]}" --fail --location --progress-bar --output "$tmp/$asset" "$asset_url" \
    || error "Failed to download glu from $asset_url. If no distribution host is live yet, set GLU_BASE_URL to a mirror (e.g. file://...) or GLU_BINARY to a local build."

  if [[ "${GLU_NO_VERIFY:-0}" = '1' ]]; then
    info 'Skipping checksum verification (GLU_NO_VERIFY=1).'
  else
    command -v shasum >/dev/null || command -v sha256sum >/dev/null \
      || error 'No sha256 tool found (need shasum or sha256sum) to verify the download.'
    curl "${curl_transport[@]}" --fail --location --silent --show-error --output "$tmp/$asset.sha256" "$checksum_url" \
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
canonical_directory_matches() {
  local path="$1" physical
  physical="$(cd "$path" 2>/dev/null && pwd -P)" || return 1
  [[ "$(normalize_macos_path "$physical")" = "$(normalize_macos_path "$path")" ]]
}

run_as_install_user() {
  if [[ "$current_uid" = '0' ]]; then
    /usr/bin/sudo -n -H -u "$install_user" "$@"
  else
    "$@"
  fi
}

install_user_can_write() {
  if [[ "$current_uid" = '0' ]]; then
    /usr/bin/sudo -n -H -u "$install_user" /bin/test -w "$1"
  else
    [[ -w "$1" ]]
  fi
}

require_owned_directory() {
  local path="$1" label="$2" actual_uid actual_mode
  [[ -d "$path" ]] || error "$label is not a directory: $path"
  [[ ! -L "$path" ]] || error "$label must not be a symlink: $path"
  canonical_directory_matches "$path" \
    || error "$label resolves through an unsupported symlink: $path"
  actual_uid="$(/usr/bin/stat -f '%u' "$path")" \
    || error "Cannot inspect ownership of $path."
  [[ "$actual_uid" = "$install_uid" ]] \
    || error "$label is owned by uid $actual_uid, not $install_user (uid $install_uid); refusing to change ownership of an existing tree."
  install_user_can_write "$path" \
    || error "$label is not writable by $install_user: $path"
  actual_mode="$(/usr/bin/stat -f '%OLp' "$path")" \
    || error "Cannot inspect permissions of $path."
  [[ "$actual_mode" = '755' ]] \
    || error "$label has unsafe permissions $actual_mode; expected 755 and refusing to modify an existing tree: $path"
}

authorize_sudo_if_needed() {
  [[ "$current_uid" != '0' ]] || return 0
  [[ -x /usr/bin/sudo ]] \
    || error "Administrator approval is required to create $DEFAULT_PREFIX, but sudo is not available."
  if /usr/bin/sudo -n true 2>/dev/null; then
    return 0
  fi

  info "glu installs shared tools in $DEFAULT_PREFIX."
  info "Administrator approval is needed once to create that directory and assign package administration to $install_user."
  info 'Package installations themselves do not run as root.'
  [[ -c /dev/tty ]] \
    || error 'Administrator approval is required, but this process has no controlling terminal.'
  # The installer may itself arrive on stdin; this redirect deliberately gives
  # sudo the process's controlling terminal instead of that script pipe.
  # shellcheck disable=SC2024
  /usr/bin/sudo -v -p "Administrator password for glu setup ($install_user): " </dev/tty \
    || error 'Administrator approval was not granted.'
}

run_privileged() {
  if [[ "$current_uid" = '0' ]]; then
    "$@"
  else
    /usr/bin/sudo -n "$@"
  fi
}

create_default_prefix() {
  authorize_sudo_if_needed
  run_privileged /bin/mkdir -m 0755 "$DEFAULT_PREFIX" \
    || error "Failed to create $DEFAULT_PREFIX."
  if ! run_privileged /usr/sbin/chown "$install_uid:$install_gid" "$DEFAULT_PREFIX"; then
    run_privileged /bin/rmdir "$DEFAULT_PREFIX" 2>/dev/null || true
    error "Failed to assign $DEFAULT_PREFIX to $install_user."
  fi
}

create_custom_prefix() {
  local parent="${GLU_PREFIX%/*}"
  [[ -n "$parent" ]] || parent='/'
  [[ -d "$parent" ]] \
    || error "The parent of GLU_PREFIX must already exist: $parent"
  canonical_directory_matches "$parent" \
    || error "The parent of GLU_PREFIX resolves through an unsupported symlink: $parent"
  install_user_can_write "$parent" \
    || error "The parent of GLU_PREFIX is not writable by $install_user: $parent"
  run_as_install_user /bin/mkdir -m 0755 "$GLU_PREFIX" \
    || error "Failed to create development prefix $GLU_PREFIX."
}

ensure_prefix() {
  if [[ -e "$GLU_PREFIX" || -L "$GLU_PREFIX" ]]; then
    require_owned_directory "$GLU_PREFIX" 'The glu prefix'
  elif [[ "$prefix_overridden" = '1' ]]; then
    create_custom_prefix
    require_owned_directory "$GLU_PREFIX" 'The glu prefix'
  else
    create_default_prefix
    require_owned_directory "$GLU_PREFIX" 'The glu prefix'
  fi

  if [[ -e "$BIN_DIR" || -L "$BIN_DIR" ]]; then
    require_owned_directory "$BIN_DIR" 'The glu binary directory'
  else
    run_as_install_user /bin/mkdir -m 0755 "$BIN_DIR" \
      || error "Failed to create $BIN_DIR."
    require_owned_directory "$BIN_DIR" 'The glu binary directory'
  fi
}

ensure_prefix

# --- install the binary (atomic replace) -----------------------------------
info "Installing glu to $(tildify "$GLU_BIN")"
tmp_bin="$(mktemp "$BIN_DIR/.glu.XXXXXX")"
install -m 0755 "$downloaded_bin" "$tmp_bin"
if [[ "$current_uid" = '0' ]]; then
  /usr/sbin/chown "$install_uid:$install_gid" "$tmp_bin" \
    || error "Failed to assign the glu binary to $install_user."
fi
mv -f "$tmp_bin" "$GLU_BIN"
tmp_bin=''

# --- hand off to the binary -------------------------------------------------
if [[ "$current_uid" = '0' ]]; then
  install_home="$(/usr/bin/dscl . -read "/Users/$install_user" NFSHomeDirectory 2>/dev/null | cut -d' ' -f2-)"
  install_shell="$(/usr/bin/dscl . -read "/Users/$install_user" UserShell 2>/dev/null | cut -d' ' -f2-)"
  [[ -n "$install_home" && -n "$install_shell" ]] \
    || error "Cannot resolve the home directory and shell for $install_user."
  /usr/bin/sudo -n -H -u "$install_user" /usr/bin/env \
    HOME="$install_home" SHELL="$install_shell" GLU_SETUP_SHELL="$install_shell" \
    "$GLU_BIN" setup
else
  GLU_SETUP_SHELL="${SHELL:-}" "$GLU_BIN" setup
fi

echo
success "glu installed to $(tildify "$GLU_BIN")"
