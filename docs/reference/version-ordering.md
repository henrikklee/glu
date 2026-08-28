# Version ordering

Glu compares Homebrew package versions with a direct port of `Version#<=>` and `PkgVersion#<=>` from Homebrew revision `da12368691a124f3e22a00a02e6587e4f148f8d0`.

This ordering is used for:

- choosing the newest installed keg for one stable package identity;
- checking an installed dependency against an active minimum version;
- comparing an installed package with the registry's installable update;
- ordering simulated post-install state.

The glu client's own release version remains SemVer and is compared separately.

## Inputs

The comparator accepts explicit upstream version strings that the registry and receipts already identify as versions. It does not detect versions from URLs.

A package version has two parts:

1. the explicit upstream version;
2. the formula revision.

Upstream versions are compared first. Formula revisions are compared only when the upstream comparison returns equal. Revision zero is an ordinary package revision. A missing dependency-floor revision remains a separate requirement-model fact and means that no revision floor is imposed.

Homebrew `version_scheme` is release-selection metadata, not part of `Version#<=>`. The client wire model receives an already-selected release and does not reinterpret formula version schemes.

## Upstream-version tokens

Punctuation separates tokens but does not otherwise determine order. The scanner recognizes alternatives in this order:

1. alpha: `alpha`, `alphaN`, or `aN`;
2. beta: `beta`, `betaN`, or `bN`;
3. pre-release: `pre` or `preN`;
4. release candidate: `rc` or `rcN`;
5. patch: `p` or `pN`;
6. post-release: `.postN`;
7. numeric runs;
8. alphabetic runs.

Numeric tokens compare as arbitrary-size non-negative integers, so comparison does not overflow at `u64`. Leading zeroes do not affect numeric value.

Equivalent composite spellings compare by their numeric suffix. For example, `alpha4`, `a4`, and `A4` compare equally. The prerelease progression is:

```text
alpha < beta < pre < rc < release
```

Patch and post-release tokens sort after a release. Other alphabetic tokens use Homebrew's token comparison, including numeric-versus-string rules.

`HEAD` and `HEAD-*` compare greater than non-HEAD versions. All HEAD versions compare equally regardless of commit suffix.

## Directional compatibility

Homebrew's implementation has directional zero-padding behavior that is not a lawful Rust `Ord`. For example:

```text
1.0.0 <=> 1.0rc1  =  0
1.0rc1 <=> 1.0.0  = -1
```

The port preserves that behavior exactly and therefore exposes an explicit directional comparison method instead of implementing `Ord`. Callers must preserve operand direction when matching a Homebrew decision.

## Requirement checks

The selected concrete dependency release always self-satisfies its own resolution. For another installed release, the planner:

1. compares the installed upstream version with the active installer-root floor;
2. accepts a greater upstream version;
3. rejects a lower upstream version;
4. for an equal upstream version, applies the optional revision floor.

Requirement keys remain exact stable package keys. Version ordering does not introduce alias or old-name guessing.

## Verification

`crates/glu-client/testdata/homebrew_version_ordering.tsv` contains 8,480 directional comparisons generated with the pinned Homebrew implementation. It includes selected formula versions, exact-identity bottle runtime floors, and every captured Glu/Homebrew disagreement class.

Rust tests compare the port against every fixture row. To verify the fixture itself against the pinned local Homebrew checkout, run:

```sh
./scripts/verify-homebrew-version-corpus.sh ../homebrew
```

The script checks the Homebrew Git revision before running the oracle and performs no network access.
