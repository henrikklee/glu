use anyhow::{bail, Result};
use std::collections::HashMap;

/// A registry or state value used as one filesystem path component.
///
/// Homebrew package metadata uses ASCII letters, digits, and `+ - . @ _`.
/// Keeping this grammar deliberately narrow also excludes separators, control
/// characters, colons, and Unicode normalization aliases before a value can
/// reach a pathname or terminal diagnostic.
pub(crate) fn is_safe_path_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.' | b'@' | b'_')
        })
}

/// The pathname-equivalence key for Glu's supported package metadata
/// alphabet on default case-insensitive macOS filesystems. Unicode
/// normalization does not enter into this key because non-ASCII metadata is
/// rejected before insertion.
pub(crate) fn filesystem_component_key(value: &str) -> String {
    debug_assert!(is_safe_path_component(value));
    value.to_ascii_lowercase()
}

#[derive(Debug)]
struct ComponentOwner {
    value: String,
    owner: String,
    label: String,
}

/// Detects two metadata values that would address the same path component on
/// Glu's supported filesystem while claiming different spelling or ownership.
/// Repeated identical facts for the same package are harmless (for example an
/// alias also appearing in `opt_names`) and are accepted.
#[derive(Debug, Default)]
pub(crate) struct PathComponentCollisionTracker {
    components: HashMap<String, ComponentOwner>,
}

impl PathComponentCollisionTracker {
    pub(crate) fn insert(&mut self, value: &str, owner: &str, label: &str) -> Result<()> {
        if !is_safe_path_component(value) {
            bail!("{label} {value:?} is not a supported package path component");
        }

        let key = filesystem_component_key(value);
        if let Some(existing) = self.components.get(&key) {
            if existing.value != value || existing.owner != owner {
                bail!(
                    "filesystem-equivalent package path components collide: {label} {value:?} for {owner:?} conflicts with {} {:?} for {:?}",
                    existing.label,
                    existing.value,
                    existing.owner
                );
            }
            return Ok(());
        }

        self.components.insert(
            key,
            ComponentOwner {
                value: value.to_string(),
                owner: owner.to_string(),
                label: label.to_string(),
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{is_safe_path_component, PathComponentCollisionTracker};

    #[test]
    fn safe_path_component_accepts_homebrew_alphabet() {
        for good in [
            "openssl",
            "openssl@3",
            "aws-sdk-cpp",
            "python@3.13",
            "postgresql@14",
            "1.2.3",
            "2.7.7_1",
            "gcc@13",
            "gtk+3",
            "HEAD",
        ] {
            assert!(is_safe_path_component(good), "expected {good:?} to be safe");
        }
    }

    #[test]
    fn safe_path_component_rejects_non_homebrew_characters() {
        for bad in [
            "",
            ".",
            "..",
            "../..",
            "a/b",
            "a\\b",
            "a\0b",
            "/etc",
            "name:version",
            "line\nbreak",
            "tab\tname",
            "café",
            "cafe\u{301}",
        ] {
            assert!(
                !is_safe_path_component(bad),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn collision_tracker_rejects_case_equivalent_values() {
        let mut tracker = PathComponentCollisionTracker::default();
        tracker.insert("OpenSSL", "package:first", "name").unwrap();

        let error = tracker
            .insert("openssl", "package:second", "opt name")
            .unwrap_err()
            .to_string();

        assert!(error.contains("filesystem-equivalent"));
        assert!(error.contains("OpenSSL"));
        assert!(error.contains("openssl"));
    }

    #[test]
    fn collision_tracker_accepts_repeated_fact_only_for_same_owner() {
        let mut tracker = PathComponentCollisionTracker::default();
        tracker.insert("llvm@22", "package:llvm", "alias").unwrap();
        tracker
            .insert("llvm@22", "package:llvm", "opt name")
            .unwrap();

        assert!(tracker
            .insert("llvm@22", "package:other", "opt name")
            .is_err());
    }
}
