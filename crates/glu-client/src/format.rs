//! Human-readable formatting helpers for CLI output.

/// Format a byte count for humans using SI units (1 MB = 1_000_000 bytes),
/// one decimal place with trailing `.0` stripped.
///
/// Examples: `512 B`, `1.5 KB`, `12.4 MB`, `373 MB`, `1.2 GB`.
pub fn human_bytes(bytes: u64) -> String {
    human_bytes_impl(bytes, false)
}

/// Like [`human_bytes`], but always keeps the one decimal place so live
/// counters move smoothly (e.g. `27.0 MB`, not `27 MB`).
pub fn human_bytes_1dp(bytes: u64) -> String {
    human_bytes_impl(bytes, true)
}

/// Whole-unit byte size, always rounded to an integer in its unit (e.g.
/// `358 MB`, never `358.1 MB`). Used for completed counts where a decimal
/// reads as imprecise, matching the whole-number `n/total` progress pair.
pub fn human_bytes_whole(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;

    let b = bytes as f64;
    if b < KB {
        return format!("{bytes} B");
    }
    let (value, unit) = if b < MB {
        (b / KB, "KB")
    } else if b < GB {
        (b / MB, "MB")
    } else {
        (b / GB, "GB")
    };
    let rounded = value.round() as u64;
    format!("{rounded} {unit}")
}

/// Download speed in whole units: `845 KB/s`, `12 MB/s`, `1 GB/s`.
pub fn human_speed(bytes_per_sec: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;

    let b = bytes_per_sec as f64;
    if b < MB {
        format!("{} KB/s", (b / KB) as u64)
    } else if b < GB {
        format!("{} MB/s", (b / MB) as u64)
    } else {
        format!("{} GB/s", (b / GB) as u64)
    }
}

fn human_bytes_impl(bytes: u64, keep_decimal: bool) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;

    let b = bytes as f64;
    let (mut value, unit) = if b < KB {
        return format!("{bytes} B");
    } else if b < MB {
        (b / KB, "KB")
    } else if b < GB {
        (b / MB, "MB")
    } else {
        (b / GB, "GB")
    };

    // Round to one decimal, escalating units on overflow (999.95 KB -> 1 MB).
    value = (value * 10.0).round() / 10.0;
    let (value, unit) = if value >= 1000.0 && unit != "GB" {
        (value / 1000.0, if unit == "KB" { "MB" } else { "GB" })
    } else {
        (value, unit)
    };

    if keep_decimal || value != value.trunc() {
        format!("{value:.1} {unit}")
    } else {
        format!("{} {unit}", value as u64)
    }
}

/// `count` plus a correctly pluralized regular noun: `plural(1, "package")`
/// -> `"1 package"`, `plural(2, "package")` -> `"2 packages"`.
pub fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluralizes_regular_nouns() {
        assert_eq!(plural(1, "package"), "1 package");
        assert_eq!(plural(2, "package"), "2 packages");
        assert_eq!(plural(1, "artifact"), "1 artifact");
        assert_eq!(plural(0, "package"), "0 packages");
    }

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(999), "999 B");
    }

    #[test]
    fn bytes_whole_rounds_to_integer_unit() {
        // Non-whole MB totals round up/down to a whole unit — the completed
        // download line never shows a decimal.
        assert_eq!(human_bytes_whole(0), "0 B");
        assert_eq!(human_bytes_whole(512), "512 B");
        assert_eq!(human_bytes_whole(358_100_000), "358 MB");
        assert_eq!(human_bytes_whole(358_500_000), "359 MB");
        assert_eq!(human_bytes_whole(30_000_000), "30 MB");
        assert_eq!(human_bytes_whole(1_500), "2 KB");
        assert_eq!(human_bytes_whole(2_500_000_000), "3 GB");
    }

    #[test]
    fn kilobytes() {
        assert_eq!(human_bytes(1_500), "1.5 KB");
        assert_eq!(human_bytes(999_999), "1 MB");
    }

    #[test]
    fn megabytes() {
        assert_eq!(human_bytes(1_000_000), "1 MB");
        assert_eq!(human_bytes(12_400_000), "12.4 MB");
        assert_eq!(human_bytes(373_031_061), "373 MB");
    }

    #[test]
    fn one_decimal_keeps_whole_values() {
        assert_eq!(human_bytes_1dp(27_000_000), "27.0 MB");
        assert_eq!(human_bytes_1dp(27_100_000), "27.1 MB");
        assert_eq!(human_bytes_1dp(999_999), "1.0 MB");
    }

    #[test]
    fn gigabytes() {
        assert_eq!(human_bytes(1_165_442_469), "1.2 GB");
        assert_eq!(human_bytes(1_000_000_000), "1 GB");
    }
}
