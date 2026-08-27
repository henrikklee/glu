use crate::{
    command_model::generated_schema,
    tables::{table_row, table_rule},
};
use anyhow::{Context, Result};
use glu_client::GluClient;
use std::io::IsTerminal;

/// Final `glu trace list` result. The trace directory is presentation-only;
/// JSON retains the existing object-shaped `{ "traces": [...] }` contract.
#[derive(serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "TraceListResult")]
pub(crate) struct TraceListOutput {
    #[serde(skip)]
    #[schemars(skip)]
    directory: std::path::PathBuf,
    traces: Vec<TraceEntry>,
}

pub(super) fn list_output(
    client: &GluClient,
    failures: bool,
    all: bool,
) -> Result<TraceListOutput> {
    Ok(TraceListOutput {
        directory: glu_client::trace::writer::trace_dir(&client.config().prefix),
        traces: list_traces(client, failures, all)?,
    })
}

/// `glu trace list` record. Serialized declaration order matches the piped
/// line order (`id timestamp package status duration`).
#[derive(serde::Serialize, schemars::JsonSchema)]
pub(super) struct TraceEntry {
    id: String,
    timestamp: String,
    package: String,
    status: String,
    duration: f64,
    path: String,
}

/// Read the trace directory into a newest-first list, optionally filtered to
/// failed runs only.
fn list_traces(client: &GluClient, failures: bool, all: bool) -> Result<Vec<TraceEntry>> {
    let dir = glu_client::trace::writer::trace_dir(&client.config().prefix);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<TraceEntry> = std::fs::read_dir(&dir)
        .context("reading traces")?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name()
                .map(|n| {
                    n.to_string_lossy().starts_with("trace-")
                        && n.to_string_lossy().ends_with(".json")
                })
                .unwrap_or(false)
        })
        .filter_map(|p| trace_entry(&p))
        .collect();

    // Newest first: the embedded `YYYYMMDD-HHMMSS` sorts lexically, so
    // reverse. Existing legacy files are already excluded by `trace_entry`.
    entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    let cap = if all {
        entries.len()
    } else {
        entries.len().min(20)
    };
    entries.truncate(cap);

    if failures {
        entries.retain(|e| e.status == "failed");
    }
    Ok(entries)
}

/// Build a `TraceEntry` from a single trace path. Skips entries that do not
/// parse as valid trace JSON (e.g. a partially-written trace from a crash) and
/// skips legacy files that don't match the current
/// `trace-YYYYMMDD-HHMMSS-<pkg>-<id>.json` naming. `timestamp` and `id` are
/// read from the filename.
fn trace_entry(path: &std::path::Path) -> Option<TraceEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned())?;

    // Strict current shape: `trace-<15-digit ts>-...+<6-hex id>.json`. Parse
    // the id as the trailing 6-hex segment; anything else is a legacy file we
    // do not list.
    let stem = name.strip_prefix("trace-")?.strip_suffix(".json")?;
    let mut parts = stem.rsplitn(2, '-');
    let tail = parts.next()?;
    if tail.len() != 6 || !tail.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let id = tail.to_string();
    let head = parts.next()?;
    let ts = head.split(' ').next().unwrap_or(head);
    let mut ts_parts = ts.split('-');
    let date = ts_parts.next()?;
    let clock = ts_parts.next()?;
    if date.len() != 8
        || !date.chars().all(|c| c.is_ascii_digit())
        || clock.len() != 6
        || !clock.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let timestamp = format!("{date} {clock}");

    let package = value["plan"]
        .as_str()
        .or_else(|| value.get("package").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let status = value["status"].as_str().unwrap_or("ok").to_string();
    let duration = match value["events"].as_array() {
        Some(events) if events.is_empty() => 0.0,
        Some(_) => events_duration(&value["events"]),
        None => 0.0,
    };
    Some(TraceEntry {
        id,
        timestamp,
        package,
        status,
        duration,
        path: path.display().to_string(),
    })
}

/// Max-end minus min-start across all events (seconds), or 0 if empty.
fn events_duration(events: &serde_json::Value) -> f64 {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for event in events.as_array().into_iter().flatten() {
        if let (Some(s), Some(e)) = (event["start"].as_f64(), event["end"].as_f64()) {
            if s < min {
                min = s;
            }
            if e > max {
                max = e;
            }
        }
    }
    if max == f64::NEG_INFINITY {
        0.0
    } else {
        (max - min).max(0.0)
    }
}

/// `glu trace list`: newest-first boxed table on a terminal; clean
/// `id timestamp package status duration` lines when piped.
pub(crate) fn render_human(output: &TraceListOutput) {
    let tty = std::io::stdout().is_terminal();
    if tty {
        println!("{}", output.directory.display());
        if !output.traces.is_empty() {
            println!("{}", trace_table(&output.traces));
        }
    } else {
        for entry in &output.traces {
            println!(
                "{} {} {} {} {:.1}s",
                entry.id, entry.timestamp, entry.package, entry.status, entry.duration
            );
        }
    }
}

/// Render the terminal table for `glu trace list`. Columns follow
/// `timestamp package status duration id` — the id is last so the 15-char
/// timestamp column is stable and the package column is wide enough for the
/// longest plan (a multi-package closure name like `a+b+c`). Status is
/// coloured (`ok` green, `failed` yellow); duration and id are dimmed. Uses
/// the same `table_rule`/`table_row` helpers as the `outdated` and `list`
/// tables.
fn trace_table(entries: &[TraceEntry]) -> String {
    let status_col = "Status";
    let headers = ["Time", "Package", status_col, "Duration", "Id"];
    let widths = [
        "Time"
            .len()
            .max(entries.iter().map(|e| e.timestamp.len()).max().unwrap_or(0)),
        "Package"
            .len()
            .max(entries.iter().map(|e| e.package.len()).max().unwrap_or(0)),
        status_col.len(),
        "Duration".len().max(
            entries
                .iter()
                .map(|e| format!("{:.1}s", e.duration).len())
                .max()
                .unwrap_or(0),
        ),
        "Id".len()
            .max(entries.iter().map(|e| e.id.len()).max().unwrap_or(0)),
    ];

    let mut lines = Vec::new();
    lines.push(table_rule(&widths, '┌', '┬', '┐'));
    lines.push(table_row(
        &[
            (headers[0], headers[0]),
            (headers[1], headers[1]),
            (headers[2], headers[2]),
            (headers[3], headers[3]),
            (headers[4], headers[4]),
        ],
        &widths,
        true,
    ));
    lines.push(table_rule(&widths, '├', '┼', '┤'));
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            lines.push(table_rule(&widths, '├', '┼', '┤'));
        }
        let status = match entry.status.as_str() {
            "failed" => glu_client::style::yellow("failed"),
            _ => glu_client::style::green("ok"),
        };
        let duration = format!("{:.1}s", entry.duration);
        lines.push(table_row(
            &[
                (&entry.timestamp, &entry.timestamp),
                (&entry.package, &entry.package),
                (&status, &entry.status),
                (&glu_client::style::dim(&duration), &duration),
                (&glu_client::style::dim(&entry.id), &entry.id),
            ],
            &widths,
            false,
        ));
    }
    lines.push(table_rule(&widths, '└', '┴', '┘'));
    lines.join("\n")
}

pub(super) fn result_schema() -> serde_json::Value {
    generated_schema::<TraceListOutput>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TraceEntry` values are only ever built by reading real trace files, so
    /// build them directly here to exercise the table's column alignment.
    fn te(id: &str, ts: &str, pkg: &str, status: &str, dur: f64) -> TraceEntry {
        TraceEntry {
            id: id.to_string(),
            timestamp: ts.to_string(),
            package: pkg.to_string(),
            status: status.to_string(),
            duration: dur,
            path: format!("/traces/trace-{ts}-{pkg}-{id}.json"),
        }
    }

    #[test]
    fn generated_schema_accepts_trace_list_result() {
        let value = serde_json::to_value(TraceListOutput {
            directory: "/tmp/traces".into(),
            traces: Vec::new(),
        })
        .unwrap();
        let schema = result_schema();
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&schema)
            .unwrap();
        assert!(validator.is_valid(&value));
    }

    #[test]
    fn trace_table_aligns_variable_width_columns() {
        let entries = vec![
            te(
                "1b7660",
                "20260817 031738",
                "glib+gdk-pixbuf+pango+vips",
                "ok",
                6.2,
            ),
            te("aaaac9", "20260817 031611", "gnupg", "failed", 0.2),
            te("f51d70", "20260816 225926", "vips", "ok", 22.1),
        ];
        let table = trace_table(&entries);
        let lines: Vec<&str> = table.lines().collect();
        // Top rule + header + rule, then one separator per data row, then
        // bottom rule — the same row-separated style as `glu outdated`.
        assert_eq!(lines.len(), 9);
        assert!(lines[0].starts_with('┌'));
        assert!(lines[1].contains("Time"));
        assert!(lines[1].contains("Package"));
        assert!(lines[1].contains("Duration"));
        assert!(lines.last().unwrap().starts_with('└'));

        // Every data row is the same character length (columns aligned).
        let widths: Vec<usize> = [3usize, 5, 7]
            .iter()
            .map(|i| lines[*i].chars().count())
            .collect();
        assert_eq!(widths[0], widths[1]);
        assert_eq!(widths[1], widths[2]);
    }
}
