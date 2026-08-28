use regex::Regex;
use std::{cmp::Ordering, sync::OnceLock};

/// An explicit formula or receipt version ordered like Homebrew `Version`.
///
/// This is a direct port of Homebrew's token scanner and comparison rules at
/// brew commit da12368691a124f3e22a00a02e6587e4f148f8d0. Version detection
/// from URLs is intentionally not part of this type: registry and receipt
/// versions have already been detected by Homebrew.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Version<'a>(&'a str);

impl<'a> Version<'a> {
    pub(crate) fn new(value: &'a str) -> Self {
        Self(value)
    }

    /// Directional comparison matching Homebrew exactly. This deliberately
    /// does not implement Rust `Ord`: Homebrew has asymmetric zero-padding
    /// cases such as `1.0.0 <=> 1.0rc1 == 0` while the reverse comparison is
    /// `-1`, which cannot satisfy the `Ord` contract.
    pub(crate) fn compare(self, other: Self) -> Ordering {
        compare(self.0, other.0)
    }
}

/// A Homebrew `PkgVersion`: upstream version first, formula revision second.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PackageVersion<'a> {
    version: Version<'a>,
    revision: u32,
}

impl<'a> PackageVersion<'a> {
    pub(crate) fn new(version: &'a str, revision: u32) -> Self {
        Self {
            version: Version::new(version),
            revision,
        }
    }

    pub(crate) fn compare(self, other: Self) -> Ordering {
        self.version
            .compare(other.version)
            .then(self.revision.cmp(&other.revision))
    }
}

fn compare(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let a_head = head(a);
    let b_head = head(b);
    if a_head && !b_head {
        return Ordering::Greater;
    }
    if !a_head && b_head {
        return Ordering::Less;
    }
    if a_head && b_head {
        return Ordering::Equal;
    }

    let left = tokens(a);
    let right = tokens(b);
    let max = left.len().max(right.len());
    let (mut l, mut r) = (0, 0);
    while l < max {
        let a = left.get(l).unwrap_or(&Token::Null);
        let b = right.get(r).unwrap_or(&Token::Null);
        if token_cmp(a, b) == Ordering::Equal {
            l += 1;
            r += 1;
        } else if a.numeric() && !b.numeric() {
            if token_cmp(a, &Token::Null) == Ordering::Greater {
                return Ordering::Greater;
            }
            l += 1;
        } else if !a.numeric() && b.numeric() {
            if token_cmp(b, &Token::Null) == Ordering::Greater {
                return Ordering::Less;
            }
            r += 1;
        } else {
            return token_cmp(a, b);
        }
    }
    Ordering::Equal
}

fn head(value: &str) -> bool {
    value == "HEAD" || value.strip_prefix("HEAD-").is_some()
}

#[derive(Debug)]
enum Token {
    Null,
    String(String),
    Numeric(String),
    Alpha { value: String, rev: String },
    Beta { value: String, rev: String },
    Pre { value: String, rev: String },
    Rc { value: String, rev: String },
    Patch { value: String, rev: String },
    Post { value: String, rev: String },
}

impl Token {
    fn numeric(&self) -> bool {
        matches!(self, Self::Numeric(_))
    }

    fn string_value(&self) -> Option<&str> {
        match self {
            Self::String(value)
            | Self::Alpha { value, .. }
            | Self::Beta { value, .. }
            | Self::Pre { value, .. }
            | Self::Rc { value, .. }
            | Self::Patch { value, .. }
            | Self::Post { value, .. } => Some(value),
            Self::Null | Self::Numeric(_) => None,
        }
    }
}

fn tokens(version: &str) -> Vec<Token> {
    // Regexp.union preserves this alternative order. PostToken's leading `.`
    // is intentionally an unescaped wildcard in Homebrew.
    static SCAN: OnceLock<Regex> = OnceLock::new();
    let regex = SCAN.get_or_init(|| {
        Regex::new(
            r"(?i:alpha[0-9]*|a[0-9]+)|(?i:beta[0-9]*|b[0-9]+)|(?i:pre[0-9]*)|(?i:rc[0-9]*)|(?i:p[0-9]*)|(?i:.post[0-9]+)|[0-9]+|(?i:[a-z]+)",
        )
        .expect("Homebrew version token regex must compile")
    });
    regex
        .find_iter(version)
        .map(|m| token(m.as_str()))
        .collect()
}

fn token(value: &str) -> Token {
    static ALPHA: OnceLock<Regex> = OnceLock::new();
    static BETA: OnceLock<Regex> = OnceLock::new();
    static RC: OnceLock<Regex> = OnceLock::new();
    static PRE: OnceLock<Regex> = OnceLock::new();
    static PATCH: OnceLock<Regex> = OnceLock::new();
    static POST: OnceLock<Regex> = OnceLock::new();
    static NUMERIC: OnceLock<Regex> = OnceLock::new();
    let matches = |slot: &OnceLock<Regex>, pattern: &str| {
        slot.get_or_init(|| Regex::new(pattern).unwrap())
            .is_match(value)
    };
    let composite = |kind: fn(String, String) -> Token| {
        let rev = value
            .chars()
            .skip_while(|ch| !ch.is_ascii_digit())
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        kind(
            value.to_string(),
            if rev.is_empty() { "0".to_string() } else { rev },
        )
    };

    if matches(&ALPHA, r"(?i)^(?:alpha[0-9]*|a[0-9]+)$") {
        composite(|value, rev| Token::Alpha { value, rev })
    } else if matches(&BETA, r"(?i)^(?:beta[0-9]*|b[0-9]+)$") {
        composite(|value, rev| Token::Beta { value, rev })
    } else if matches(&RC, r"(?i)^rc[0-9]*$") {
        composite(|value, rev| Token::Rc { value, rev })
    } else if matches(&PRE, r"(?i)^pre[0-9]*$") {
        composite(|value, rev| Token::Pre { value, rev })
    } else if matches(&PATCH, r"(?i)^p[0-9]*$") {
        composite(|value, rev| Token::Patch { value, rev })
    } else if matches(&POST, r"(?i)^.post[0-9]+$") {
        composite(|value, rev| Token::Post { value, rev })
    } else if matches(&NUMERIC, r"^[0-9]+$") {
        Token::Numeric(value.to_string())
    } else {
        Token::String(value.to_string())
    }
}

fn token_cmp(a: &Token, b: &Token) -> Ordering {
    use Token::*;
    match (a, b) {
        (Null, Null) => Ordering::Equal,
        (Null, Numeric(value)) => {
            if numeric_zero(value) {
                Ordering::Equal
            } else {
                Ordering::Less
            }
        }
        (Numeric(value), Null) => {
            if numeric_zero(value) {
                Ordering::Equal
            } else {
                Ordering::Greater
            }
        }
        (Null, Alpha { .. } | Beta { .. } | Pre { .. } | Rc { .. }) => Ordering::Greater,
        (Alpha { .. } | Beta { .. } | Pre { .. } | Rc { .. }, Null) => Ordering::Less,
        (Null, _) => Ordering::Less,
        (_, Null) => Ordering::Greater,
        (Numeric(a), Numeric(b)) => numeric_cmp(a, b),
        (Numeric(_), _) => Ordering::Greater,
        (_, Numeric(_)) => Ordering::Less,

        (Alpha { rev: a, .. }, Alpha { rev: b, .. })
        | (Beta { rev: a, .. }, Beta { rev: b, .. })
        | (Pre { rev: a, .. }, Pre { rev: b, .. })
        | (Rc { rev: a, .. }, Rc { rev: b, .. })
        | (Patch { rev: a, .. }, Patch { rev: b, .. })
        | (Post { rev: a, .. }, Post { rev: b, .. }) => numeric_cmp(a, b),

        (Alpha { .. }, Beta { .. } | Rc { .. } | Pre { .. } | Patch { .. } | Post { .. }) => {
            Ordering::Less
        }
        (Beta { .. }, Alpha { .. }) => Ordering::Greater,
        (Beta { .. }, Pre { .. } | Rc { .. } | Patch { .. } | Post { .. }) => Ordering::Less,
        (Pre { .. }, Alpha { .. } | Beta { .. }) => Ordering::Greater,
        (Pre { .. }, Rc { .. } | Patch { .. } | Post { .. }) => Ordering::Less,
        (Rc { .. }, Alpha { .. } | Beta { .. } | Pre { .. }) => Ordering::Greater,
        (Rc { .. }, Patch { .. } | Post { .. }) => Ordering::Less,
        (Patch { .. }, Alpha { .. } | Beta { .. } | Rc { .. } | Pre { .. }) => Ordering::Greater,
        (Post { .. }, Alpha { .. } | Beta { .. } | Rc { .. } | Pre { .. }) => Ordering::Greater,

        _ => a.string_value().unwrap().cmp(b.string_value().unwrap()),
    }
}

fn numeric_zero(value: &str) -> bool {
    value.bytes().all(|byte| byte == b'0')
}

fn numeric_cmp(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    let a = if a.is_empty() { "0" } else { a };
    let b = if b.is_empty() { "0" } else { b };
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ord(value: i8) -> Ordering {
        value.cmp(&0)
    }

    #[test]
    fn matches_homebrew_oracle_cases() {
        // Generated with pinned `brew ruby` and Version.new(a) <=> Version.new(b).
        let cases = [
            ("1.0", "1.0.0", 0),
            ("1.0", "1.0rc1", 1),
            ("1.0", "1.0beta1", 1),
            ("1.0", "1.0p1", -1),
            ("1.0", "1.0.post1", -1),
            ("1.0.0", "1.0rc1", 0),
            ("1.0.0", "1.0p1", 0),
            ("1.0", "1.0-1", -1),
            ("1.0", "1.0foo", -1),
            ("1.01", "1.1", 0),
            ("2.0alpha", "2.0beta", -1),
            ("2.0pre", "2.0rc", -1),
            ("2.0rc1", "2.0", -1),
            ("2.0", "2.0p1", -1),
            ("HEAD-a", "HEAD-b", 0),
            ("HEAD", "999999999999999999999999", 1),
        ];
        for (a, b, expected) in cases {
            assert_eq!(
                Version::new(a).compare(Version::new(b)),
                ord(expected),
                "{a} <=> {b}"
            );
        }
    }

    #[test]
    fn preserves_homebrew_directional_zero_padding_behavior() {
        assert_eq!(
            Version::new("1.0.0").compare(Version::new("1.0rc1")),
            Ordering::Equal
        );
        assert_eq!(
            Version::new("1.0rc1").compare(Version::new("1.0.0")),
            Ordering::Less
        );
    }

    #[test]
    fn matches_captured_homebrew_disagreements() {
        let cases = [
            ("0.99.beta20", "0.99b19", Ordering::Greater),
            ("0.99.beta20", "0.99b20", Ordering::Equal),
            ("10", "9d", Ordering::Greater),
            ("103", "r104", Ordering::Greater),
            ("104", "r104", Ordering::Greater),
            ("r104", "103", Ordering::Less),
            ("r104", "104", Ordering::Less),
            ("3.3.16", "p17", Ordering::Greater),
            ("6.2.1", "p6.2.20260125.0", Ordering::Greater),
        ];
        for (left, right, expected) in cases {
            assert_eq!(Version::new(left).compare(Version::new(right)), expected);
        }
    }

    #[test]
    fn compares_formula_revision_after_upstream_version() {
        assert_eq!(
            PackageVersion::new("1.0", 1).compare(PackageVersion::new("1.0.0", 0)),
            Ordering::Greater
        );
        assert_eq!(
            PackageVersion::new("1.1", 0).compare(PackageVersion::new("1.0", 99)),
            Ordering::Greater
        );
    }

    #[test]
    fn matches_pinned_homebrew_registry_corpus() {
        let corpus = include_str!("../testdata/homebrew_version_ordering.tsv");
        let mut compared = 0;
        for (line_number, line) in corpus.lines().enumerate() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let mut fields = line.split('\t');
            let identity = fields.next().unwrap();
            let left = fields.next().unwrap();
            let right = fields.next().unwrap();
            let expected = match fields.next().unwrap() {
                "-1" => Ordering::Less,
                "0" => Ordering::Equal,
                "1" => Ordering::Greater,
                value => panic!(
                    "invalid oracle ordering {value} on line {}",
                    line_number + 1
                ),
            };
            assert_eq!(
                Version::new(left).compare(Version::new(right)),
                expected,
                "{identity}: {left} <=> {right} (corpus line {})",
                line_number + 1
            );
            compared += 1;
        }
        assert_eq!(compared, 8_480);
    }
}
