/// A manifest value that will be used as one filesystem path component must be
/// a safe single segment. Deliberately permissive of the real Homebrew alphabet
/// (letters, digits, `+ - . @ _`): the goal is to reject traversal and path
/// separators, not to invent a package/version grammar.
pub(crate) fn is_safe_path_component(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\', '\0'])
}

#[cfg(test)]
mod tests {
    use super::is_safe_path_component;

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
            "curl-ca-bundle",
            "libomp",
        ] {
            assert!(is_safe_path_component(good), "expected {good:?} to be safe");
        }
    }

    #[test]
    fn safe_path_component_rejects_traversal() {
        for bad in ["", ".", "..", "../..", "a/b", "a\\b", "a\0b", "/etc"] {
            assert!(
                !is_safe_path_component(bad),
                "expected {bad:?} to be rejected"
            );
        }
    }
}
