//! Declarative `link_overwrite` matching and overwrite safety policy.
//!
//! Homebrew source oracle: `Formula#link_overwrite?` in
//! `/opt/homebrew/Library/Homebrew/formula.rb`. glu intentionally keeps this
//! local and metadata-driven: explicit path/glob patterns may allow replacing
//! unowned prefix entries, but never another installed keg's projection.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OverwritePolicy {
    patterns: Vec<OverwritePattern>,
}

impl OverwritePolicy {
    pub(crate) fn new(patterns: &[String]) -> Self {
        Self {
            patterns: patterns
                .iter()
                .map(|pattern| OverwritePattern::new(pattern))
                .collect(),
        }
    }

    pub(crate) fn allows(&self, rel: &Path) -> bool {
        let rel = rel.to_string_lossy().replace('\\', "/");
        self.patterns.iter().any(|pattern| pattern.matches(&rel))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OverwritePattern {
    raw: String,
    directory_prefix: String,
}

impl OverwritePattern {
    fn new(pattern: &str) -> Self {
        Self {
            raw: pattern.to_string(),
            directory_prefix: pattern.trim_end_matches('/').to_string(),
        }
    }

    fn matches(&self, rel: &str) -> bool {
        self.raw == rel
            || (!self.directory_prefix.is_empty()
                && rel.starts_with(&format!("{}/", self.directory_prefix)))
            || wildcard_match(&self.raw, rel)
    }
}

/// Homebrew turns `*` into `.*?` inside an anchored regex. For path-like
/// metadata this is equivalent to ordered substring matching across the whole
/// relative path; `*` may appear anywhere and may cross `/` boundaries.
fn wildcard_match(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return false;
    }
    if pattern == "*" {
        return true;
    }

    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = value;

    if let Some(first) = parts.first().copied() {
        if !first.is_empty() {
            let Some(after_prefix) = rest.strip_prefix(first) else {
                return false;
            };
            rest = after_prefix;
        }
    }

    for part in parts.iter().skip(1).take(parts.len().saturating_sub(2)) {
        if part.is_empty() {
            continue;
        }
        let Some(idx) = rest.find(part) else {
            return false;
        };
        rest = &rest[idx + part.len()..];
    }

    if let Some(last) = parts.last().copied() {
        if !last.is_empty() {
            return rest.ends_with(last);
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(patterns: &[&str]) -> OverwritePolicy {
        OverwritePolicy::new(
            &patterns
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn exact_and_directory_prefix_patterns_match_homebrew_shape() {
        let policy = policy(&["bin/foo", "lib/cmake/ggml/"]);

        assert!(policy.allows(Path::new("bin/foo")));
        assert!(policy.allows(Path::new("lib/cmake/ggml/GGMLConfig.cmake")));
        assert!(!policy.allows(Path::new("bin/foobar")));
        assert!(!policy.allows(Path::new("lib/cmake/ggml2/GGMLConfig.cmake")));
    }

    #[test]
    fn wildcard_patterns_match_anywhere_and_across_directories() {
        let policy = policy(&[
            "bin/gst-*",
            "share/locale/*/LC_MESSAGES/gst-*.mo",
            "share/man/man*/*ssl",
            "include/ggml*",
        ]);

        assert!(policy.allows(Path::new("bin/gst-launch-1.0")));
        assert!(policy.allows(Path::new("share/locale/de/LC_MESSAGES/gst-plugins.mo")));
        assert!(policy.allows(Path::new("share/man/man3/foo_ssl")));
        assert!(policy.allows(Path::new("include/ggml-cpu.h")));
        assert!(!policy.allows(Path::new("bin/xgst-launch-1.0")));
        assert!(!policy.allows(Path::new("share/locale/de/LC_MESSAGES/not-gst.mo")));
    }
}
