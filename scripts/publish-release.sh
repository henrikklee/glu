#!/usr/bin/env bash
# Create or resume a draft release, upload the exact assets, then publish it.
# Existing published releases are never modified.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

tag="${1:-}"
version="${2:-}"
asset_dir="${3:-dist}"
repo="${GITHUB_REPOSITORY:-}"
gh_bin="${GH_BIN:-gh}"

[[ "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z][0-9A-Za-z.-]*)?)$ ]] \
  || fail 'usage: scripts/publish-release.sh <v-tag> <plain-version> [asset-directory]'
[[ "$version" == "${BASH_REMATCH[1]}" ]] || fail "tag $tag does not match version $version"
[[ -n "$repo" ]] || fail 'GITHUB_REPOSITORY is required'
command -v "$gh_bin" >/dev/null || fail "GitHub CLI not found: $gh_bin"

if [[ "$asset_dir" != /* ]]; then
  asset_dir="$repo_root/$asset_dir"
fi
asset='glu-aarch64-apple-darwin.tar.gz'
files=(
  "$asset_dir/$asset"
  "$asset_dir/$asset.sha256"
  "$asset_dir/install.sh"
)
for file in "${files[@]}"; do
  [[ -f "$file" ]] || fail "release asset not found: $file"
done

release_state=''
if release_state="$($gh_bin release view "$tag" \
  --repo "$repo" \
  --json isDraft,isImmutable,tagName \
  --jq '[.isDraft, .isImmutable, .tagName] | @tsv' 2>/dev/null)"; then
  IFS=$'\t' read -r is_draft is_immutable actual_tag <<<"$release_state"
  [[ "$actual_tag" == "$tag" ]] || fail "existing release has unexpected tag: $actual_tag"
  [[ "$is_draft" == 'true' ]] \
    || fail "release $tag is already published; refusing to modify it"
  [[ "$is_immutable" == 'false' ]] \
    || fail "draft release $tag unexpectedly reports immutable"

  echo "Resuming existing draft release $tag..."
  # --clobber makes retries converge after an interrupted partial upload.
  "$gh_bin" release upload "$tag" "${files[@]}" --repo "$repo" --clobber
else
  echo "Creating draft release $tag..."
  "$gh_bin" release create "$tag" "${files[@]}" \
    --repo "$repo" \
    --verify-tag \
    --draft \
    --generate-notes \
    --title "glu $version"
fi

echo "Publishing release $tag..."
"$gh_bin" release edit "$tag" --repo "$repo" --draft=false
