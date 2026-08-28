#!/usr/bin/env bash
set -euo pipefail

expected_revision="da12368691a124f3e22a00a02e6587e4f148f8d0"
homebrew_checkout="${1:?usage: $0 PATH_TO_HOMEBREW_CHECKOUT}"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
corpus="$repo_root/crates/glu-client/testdata/homebrew_version_ordering.tsv"

actual_revision="$(git -C "$homebrew_checkout" rev-parse HEAD)"
if [[ "$actual_revision" != "$expected_revision" ]]; then
  printf 'expected Homebrew revision %s, found %s\n' "$expected_revision" "$actual_revision" >&2
  exit 1
fi

"$homebrew_checkout/bin/brew" ruby - "$corpus" <<'RUBY'
require "version"

corpus = ARGV.fetch(0)
compared = 0
File.foreach(corpus).with_index(1) do |line, line_number|
  next if line.start_with?("#") || line.strip.empty?

  identity, left, right, expected_text = line.chomp.split("\t", 4)
  expected = Integer(expected_text)
  actual = Version.new(left) <=> Version.new(right)
  unless actual == expected
    abort "#{identity}: #{left} <=> #{right}: expected #{expected}, got #{actual} (line #{line_number})"
  end
  compared += 1
end

puts "verified #{compared} comparisons against Homebrew Version"
RUBY
