use glu_client::outdated::OutdatedPackage;

/// `glu outdated`: terminal table of installed → update → latest versions.
/// `Update` is the newest version installable on this target (what `glu up`
/// would install); `Latest` is the newest visible version overall — it can
/// be ahead of `Update` when the newest release has no compatible bottle
/// yet. Both columns render per the outdated-endpoint spec.
pub(crate) fn print_outdated(outdated: &[OutdatedPackage]) {
    if outdated.is_empty() {
        println!("Nothing outdated.");
        return;
    }
    println!("{}", outdated_table(outdated));
}

/// Renders the boxed `glu outdated` table (empty input → empty string).
fn outdated_table(outdated: &[OutdatedPackage]) -> String {
    if outdated.is_empty() {
        return String::new();
    }
    let widths = [
        "Package"
            .len()
            .max(outdated.iter().map(|p| p.name.0.len()).max().unwrap_or(0)),
        "Current".len().max(
            outdated
                .iter()
                .map(|p| p.installed.len())
                .max()
                .unwrap_or(0),
        ),
        "Update".len().max(
            outdated
                .iter()
                .map(|p| p.update.as_deref().unwrap_or("—").len())
                .max()
                .unwrap_or(0),
        ),
        "Latest"
            .len()
            .max(outdated.iter().map(|p| p.latest.len()).max().unwrap_or(0)),
    ];
    let mut lines = Vec::new();
    lines.push(table_rule(&widths, '┌', '┬', '┐'));
    lines.push(table_row(
        &[
            ("Package", "Package"),
            ("Current", "Current"),
            ("Update", "Update"),
            ("Latest", "Latest"),
        ],
        &widths,
        true,
    ));
    lines.push(table_rule(&widths, '├', '┼', '┤'));
    for (i, package) in outdated.iter().enumerate() {
        if i > 0 {
            lines.push(table_rule(&widths, '├', '┼', '┤'));
        }
        let update_display =
            diff_highlight(&package.installed, package.update.as_deref().unwrap_or("—"));
        let latest_display = diff_highlight(&package.installed, &package.latest);
        lines.push(table_row(
            &[
                (&package.name.0, &package.name.0),
                (&package.installed, &package.installed),
                (&update_display, package.update.as_deref().unwrap_or("—")),
                (&latest_display, &package.latest),
            ],
            &widths,
            false,
        ));
    }
    lines.push(table_rule(&widths, '└', '┴', '┘'));
    lines.join("\n")
}

/// Box-drawing rule row: `┌────┬────┐`, one `─` per column width plus its
/// two padding cells.
pub(crate) fn table_rule(widths: &[usize], left: char, mid: char, right: char) -> String {
    let mut s = String::from(left);
    for (i, width) in widths.iter().enumerate() {
        if i > 0 {
            s.push(mid);
        }
        s.push_str(&"─".repeat(width + 2));
    }
    s.push(right);
    s
}

/// Box-drawing data row: `│ cell │ cell │`, cells left-aligned in their
/// columns; the separators are dimmed on a terminal. The header row
/// (`header = true`) renders its cells bold + blue. Each cell is a
/// `(display, plain)` pair — `display` may carry ANSI styling, `plain` is the
/// unstyled text used for column padding so escapes never widen the column.
pub(crate) fn table_row(cells: &[(&str, &str)], widths: &[usize], header: bool) -> String {
    let mut s = String::new();
    s.push('│');
    for ((display, plain), width) in cells.iter().zip(widths) {
        s.push(' ');
        let styled = if header {
            glu_client::style::bold_blue(display)
        } else {
            (*display).to_string()
        };
        s.push_str(&styled);
        s.push_str(&" ".repeat(width.saturating_sub(plain.len())));
        s.push(' ');
        s.push('│');
    }
    s
}

/// Splits a version on `.` and `_`, returning the segments and the separator
/// after each segment (`""` for the last) so the display can be rebuilt.
/// Revisions use `_` (e.g. `8.18.5_1`), so both are segment boundaries.
fn split_version(version: &str) -> (Vec<&str>, Vec<&str>) {
    let mut segments = Vec::new();
    let mut separators = Vec::new();
    let mut rest = version;
    while let Some(idx) = rest.find(['.', '_']) {
        let (head, tail) = rest.split_at(idx);
        segments.push(head);
        separators.push(&tail[..1]);
        rest = &tail[1..];
    }
    segments.push(rest);
    separators.push("");
    (segments, separators)
}

/// Number of leading version segments (split on `.`/`_`) that `installed` and
/// `latest` share — everything from the first differing segment onward is the
/// part `diff_highlight` marks yellow.
fn common_version_segments(installed: &str, latest: &str) -> usize {
    let (installed, _) = split_version(installed);
    let (latest, _) = split_version(latest);
    let mut common = 0;
    while common < installed.len() && common < latest.len() && installed[common] == latest[common] {
        common += 1;
    }
    common
}

/// Renders `latest` with the changed version part highlighted: the shared
/// segment prefix (and its separators) stays plain, everything from the first
/// differing segment onward is yellow. `1.2.3_0 → 1.2.3_1` yellow `1`;
/// `1.2.3 → 1.3.4` yellow `3.4`.
/// Splits `latest` into the unchanged prefix and the changed remainder,
/// semver + revision aware (segments on `.` and `_`): everything from the
/// first segment that differs from `installed` onward, separators included,
/// is the changed part. `1.2.3 → 1.2.3_1` → (`1.2.3_`, `1`);
/// `1.2.3 → 1.3.4` → (`1.`, `3.4`).
fn diff_parts(installed: &str, latest: &str) -> (String, String) {
    let common = common_version_segments(installed, latest);
    let (segments, separators) = split_version(latest);
    let mut prefix = String::new();
    let mut changed = String::new();
    for (i, segment) in segments.iter().enumerate() {
        let piece = format!("{segment}{}", separators[i]);
        if i < common {
            prefix.push_str(&piece);
        } else {
            changed.push_str(&piece);
        }
    }
    (prefix, changed)
}

/// Renders `latest` with the changed version part highlighted yellow; the
/// shared segment prefix stays plain.
fn diff_highlight(installed: &str, latest: &str) -> String {
    let (prefix, changed) = diff_parts(installed, latest);
    format!("{prefix}{}", glu_client::style::yellow(&changed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glu_client::outdated::OutdatedPackage;
    use glu_core::{PackageKey, PackageName};

    #[test]
    fn outdated_table_renders_boxed_terminal_style() {
        let outdated = vec![
            OutdatedPackage {
                package_key: PackageKey("package:node".to_string()),
                name: PackageName("node".to_string()),
                installed: "22.1.0".to_string(),
                update: Some("22.2.0".to_string()),
                latest: "22.2.0".to_string(),
            },
            OutdatedPackage {
                package_key: PackageKey("package:confuse".to_string()),
                name: PackageName("confuse".to_string()),
                installed: "3.2.0".to_string(),
                update: Some("3.3.0".to_string()),
                latest: "3.4.0".to_string(),
            },
        ];
        let expected = concat!(
            "┌─────────┬─────────┬────────┬────────┐\n",
            "│ Package │ Current │ Update │ Latest │\n",
            "├─────────┼─────────┼────────┼────────┤\n",
            "│ node    │ 22.1.0  │ 22.2.0 │ 22.2.0 │\n",
            "├─────────┼─────────┼────────┼────────┤\n",
            "│ confuse │ 3.2.0   │ 3.3.0  │ 3.4.0  │\n",
            "└─────────┴─────────┴────────┴────────┘",
        );
        assert_eq!(outdated_table(&outdated), expected);
        // Empty input renders nothing.
        assert_eq!(outdated_table(&[]), "");
    }

    #[test]
    fn diff_highlight_marks_only_changed_segments() {
        assert_eq!(
            split_version("1.2.3_1"),
            (vec!["1", "2", "3", "1"], vec![".", ".", "_", ""])
        );
        assert_eq!(common_version_segments("1.2.3_0", "1.2.3_1"), 3);
        assert_eq!(common_version_segments("1.2.3", "1.3.4"), 1);
        assert_eq!(common_version_segments("8.18.5_1", "8.19.0"), 1);
        assert_eq!(common_version_segments("1.2", "1.2.3"), 2);
        assert_eq!(common_version_segments("1.2.3", "2.0"), 0);

        // Revision-aware: a revision bump changes only the trailing digit;
        // adding a revision changes only the new segment. The changed part
        // includes its separators, so `3.4` / `5_1` color as a unit.
        assert_eq!(
            diff_parts("1.2.3", "1.2.3_1"),
            ("1.2.3_".into(), "1".into())
        );
        assert_eq!(
            diff_parts("1.2.3_0", "1.2.3_1"),
            ("1.2.3_".into(), "1".into())
        );
        assert_eq!(diff_parts("1.2.3", "1.3.4"), ("1.".into(), "3.4".into()));
        assert_eq!(
            diff_parts("8.18.0", "8.18.5_1"),
            ("8.18.".into(), "5_1".into())
        );
        assert_eq!(diff_parts("1.2", "1.2.3"), ("1.2.".into(), "3".into()));
        assert_eq!(diff_parts("1.2.3", "2.0"), ("".into(), "2.0".into()));
        assert_eq!(diff_parts("1.2.3_1", "1.2.4"), ("1.2.".into(), "4".into()));

        // Styling is disabled in tests, so diff_highlight returns the plain
        // latest — the split/common logic above carries the highlighting.
        assert_eq!(diff_highlight("1.2.3_0", "1.2.3_1"), "1.2.3_1");
        assert_eq!(diff_highlight("1.2.3", "1.3.4"), "1.3.4");
    }
}
