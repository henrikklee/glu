use crate::package_list::{self, PackageListItem};
use anyhow::Error;

pub(crate) fn print_runtime_error(error: &Error) {
    if let Some(partial) = error.downcast_ref::<glu_client::install::PartialInstallFailure>() {
        print_partial_install_failure(partial);
        return;
    }
    let detail = format!("{error:?}");
    eprintln!(
        "{}{}",
        glu_client::style::red("Error: "),
        format_diagnostic_message(&detail)
    );
}

fn print_partial_install_failure(error: &glu_client::install::PartialInstallFailure) {
    println!();
    let report = &error.report;
    let failed_at = report
        .failed
        .first()
        .map(|package| package.name.as_str())
        .or(report.failed_node_id.as_deref())
        .unwrap_or("install");
    let phase = report.failed_phase.as_deref().unwrap_or("execution");
    eprintln!(
        "{}{}",
        glu_client::style::red("Error: "),
        glu_client::style::red(&format!("Install failed at {failed_at} during {phase}."))
    );

    if !report.installed.is_empty() {
        eprintln!();
        let items: Vec<_> = report
            .installed
            .iter()
            .map(|package| PackageListItem::package(&package.name, &package.version))
            .collect();
        eprintln!(
            "{}",
            package_list::render_labeled_section("Installed before failure", &items)
        );
    }
    if !report.failed.is_empty() || !report.skipped.is_empty() {
        eprintln!();
        let mut items: Vec<_> = report
            .failed
            .iter()
            .map(|package| {
                PackageListItem::package(&package.name, &package.version).annotated("failed")
            })
            .collect();
        items.extend(report.skipped.iter().map(|package| {
            PackageListItem::package(&package.name, &package.version).annotated("skipped")
        }));
        eprintln!(
            "{}",
            package_list::render_labeled_section("Not installed", &items)
        );
    }
    if !report.partial.is_empty() {
        eprintln!();
        eprintln!("Partial keg left behind:");
        for keg in &report.partial {
            eprintln!("  {}", keg.path.display());
        }
    }
    if let Some(trace_path) = &report.trace_path {
        eprintln!();
        eprintln!("Trace: {}", trace_path.display());
    }
    if !report.suggested_commands.is_empty() {
        eprintln!();
        eprintln!("Next steps:");
        for suggestion in &report.suggested_commands {
            eprintln!("  {suggestion}");
        }
    }
}

pub(crate) fn print_labeled_error(label: &str, message: &str) {
    let mut detail = String::new();
    let mut lines = message.lines();
    if let Some(first) = lines.next() {
        detail.push_str(label);
        detail.push_str(": ");
        detail.push_str(first);
    } else {
        detail.push_str(label);
    }
    for line in lines {
        detail.push('\n');
        detail.push_str(line);
    }

    eprintln!(
        "{}{}",
        glu_client::style::red("Error: "),
        format_diagnostic_message(&detail)
    );
}

fn format_diagnostic_message(detail: &str) -> String {
    let mut message = String::new();
    for (i, line) in detail.lines().enumerate() {
        if i > 0 {
            message.push('\n');
        }
        if line.starts_with("Trace: ") {
            // The trace path is a debug artifact, not the error itself.
            message.push_str(&glu_client::style::dim(line));
        } else if line.contains("Did you mean") {
            message.push_str(&bold_quoted_suggestions(line));
        } else {
            message.push_str(&glu_client::style::red(line));
        }
    }
    message
}

/// Line with every quoted segment bolded (the suggested names); the rest of
/// the line stays default color so the hint reads separately from the error.
fn bold_quoted_suggestions(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find('\'') {
        let (before, after_start) = rest.split_at(start);
        out.push_str(before);
        let after_quote = &after_start[1..];
        if let Some(end) = after_quote.find('\'') {
            let (quoted, after_end) = after_quote.split_at(end);
            out.push('\'');
            out.push_str(&glu_client::style::bold(quoted));
            out.push('\'');
            rest = &after_end[1..];
        } else {
            out.push_str(after_start);
            return out;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labeled_error_preserves_suggestion_line() {
        let formatted = format_diagnostic_message(
            "dog: package 'dog' not found\n       Did you mean any of: 'cog', 'doh', 'dug'?",
        );
        assert!(formatted.contains("dog: package 'dog' not found"));
        assert!(formatted.contains("Did you mean any of:"));
        assert!(formatted.contains("'cog'"));
    }

    #[test]
    fn trace_line_is_not_rendered_as_primary_error_text() {
        let formatted = format_diagnostic_message("failed\nTrace: /tmp/trace.json");
        assert!(formatted.contains("failed"));
        assert!(formatted.contains("Trace: /tmp/trace.json"));
    }
}
