#!/usr/bin/env bash
# Validate the tracked file set after creating the sanitized public root.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

unexpected=()
while IFS= read -r path; do
  case "$path" in
    .cargo/*|.github/*|crates/*|scripts/*|docs/explanation/*|docs/reference/*) ;;
    .gitattributes|.gitignore|Cargo.lock|Cargo.toml|LICENSE-BSD-2-Clause|LICENSE-MIT|README.md|RELEASING.md|THIRD_PARTY_LICENSES.html|THIRD_PARTY_NOTICES.md|about.hbs|about.toml|docs/README.md|rust-toolchain.toml) ;;
    *) unexpected+=("$path") ;;
  esac
done < <(git ls-files)

if ((${#unexpected[@]})); then
  printf 'Unexpected tracked public file: %s\n' "${unexpected[@]}" >&2
  exit 1
fi

roots="$(git rev-list --max-parents=0 HEAD)"
[[ "$(wc -w <<<"$roots" | tr -d ' ')" == '1' ]] \
  || fail 'public history must contain exactly one parentless root'

echo 'public tree allowlist and history root are valid'
