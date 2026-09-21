#!/usr/bin/env bash
# Validate release workflows and the shell scripts on the release path.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

command -v actionlint >/dev/null || { echo 'actionlint is required' >&2; exit 1; }
command -v shellcheck >/dev/null || { echo 'shellcheck is required' >&2; exit 1; }

actionlint .github/workflows/ci.yml .github/workflows/release.yml
shellcheck \
  scripts/check-public-tree.sh \
  scripts/check-release-policy.sh \
  scripts/check-third-party-licenses.sh \
  scripts/generate-third-party-licenses.sh \
  scripts/install.sh \
  scripts/package-release.sh \
  scripts/publish-release.sh \
  scripts/test-dev-build-isolation.sh \
  scripts/test-install.sh \
  scripts/test-publish-release.sh \
  scripts/test-release.sh \
  scripts/test-upgrade.sh \
  scripts/verify-outdated-shape.sh \
  scripts/workspace-version.sh
