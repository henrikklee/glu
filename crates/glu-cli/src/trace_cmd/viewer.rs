use anyhow::{bail, Context, Result};
use std::io::Write;

/// Parse the wall-clock timestamp embedded in a trace filename
/// (`trace-YYYYMMDD-HHMMSS-<plan>-<id>.json`) into a displayable local time
/// string (`YYYY-MM-DD HH:MM:SS`). Symlinks are resolved first so `last.json`
/// reports its target's timestamp. Returns `None` when the name carries no
/// matching timestamp (e.g. an externally named trace file).
fn trace_started_display(source: &std::path::Path) -> Option<String> {
    let resolved = std::fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    let stem = resolved.file_name()?.to_str()?;
    let ts = stem.strip_prefix("trace-")?.get(..15)?;
    // `ts` must be exactly `YYYYMMDD-HHMMSS` (15 bytes) of the right shape.
    if ts.len() != 15
        || !ts[..8].bytes().all(|b| b.is_ascii_digit())
        || ts.as_bytes()[8] != b'-'
        || !ts[9..15].bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(format!(
        "{}-{}-{} {}:{}:{}",
        &ts[..4],
        &ts[4..6],
        &ts[6..8],
        &ts[9..11],
        &ts[11..13],
        &ts[13..15]
    ))
}

/// Write the rendered HTML for a trace to a temp path and return it.
/// Unix-only: the source trace is read, structurally validated, rendered, and
/// atomically moved from an exclusive private temporary file to the viewer path.
pub(super) fn render_trace_viewer(source: &std::path::Path) -> Result<std::path::PathBuf> {
    let text = std::fs::read_to_string(source).context("reading trace")?;
    let mut trace: serde_json::Value =
        serde_json::from_str(&text).context("trace is not valid JSON")?;
    validate_trace_shape(&trace)?;
    // The trace JSON carries only relative times; the run's wall-clock moment
    // lives in the filename (`trace-YYYYMMDD-HHMMSS-...`). Surface it as a
    // display-only `started_at` so the viewer can show it without schema
    // changes. Traces without a parseable timestamp render as before.
    if let Some(started) = trace_started_display(source) {
        trace["started_at"] = serde_json::Value::String(started);
    }
    let name = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "trace".to_string());
    let plan = trace["plan"].as_str().unwrap_or("trace").to_string();
    let html = glu_client::trace::render::render_html(&trace, &format!("glu trace: {plan}"));

    let mut output_path = std::env::temp_dir();
    output_path.push(format!("glu-{name}.html"));

    let mut staging = tempfile::Builder::new()
        .prefix(".glu-trace-viewer-")
        .suffix(".html.tmp")
        .tempfile_in(std::env::temp_dir())
        .context("creating private trace viewer temporary file")?;
    staging
        .write_all(html.as_bytes())
        .context("writing trace HTML")?;
    staging
        .persist(&output_path)
        .with_context(|| format!("moving trace viewer to {}", output_path.display()))?;
    Ok(output_path)
}

fn validate_trace_shape(trace: &serde_json::Value) -> Result<()> {
    let Some(trace) = trace.as_object() else {
        bail!("trace must be a JSON object");
    };
    if !trace.get("plan").is_some_and(serde_json::Value::is_string) {
        bail!("trace field `plan` must be a string");
    }
    for field in ["nodes", "edges", "events"] {
        let Some(entries) = trace.get(field).and_then(serde_json::Value::as_array) else {
            bail!("trace field `{field}` must be an array");
        };
        if entries.iter().any(|entry| !entry.is_object()) {
            bail!("trace field `{field}` must contain only objects");
        }
    }
    Ok(())
}

/// Best-effort `open` on the rendered HTML. Browser-launch failure is part of
/// the typed command result so presentation remains owned by the CLI host.
pub(super) fn open_html(path: &std::path::Path) -> super::TraceOpenResult {
    match std::process::Command::new("open")
        .arg(path.as_os_str())
        .status()
    {
        Ok(status) if status.success() => super::TraceOpenResult::Opened,
        Ok(status) => super::TraceOpenResult::Exited(status.code()),
        Err(error) => super::TraceOpenResult::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_started_display_parses_standard_filename() {
        let p = std::path::Path::new("/tmp/traces/trace-20250817-213059-node-a1b2c3.json");
        assert_eq!(
            trace_started_display(p),
            Some("2025-08-17 21:30:59".to_string())
        );
    }

    #[test]
    fn trace_started_display_rejects_non_matching_names() {
        assert_eq!(
            trace_started_display(std::path::Path::new("last.json")),
            None
        );
        // Too-short / malformed timestamp segments are ignored, not misparsed.
        assert_eq!(
            trace_started_display(std::path::Path::new("trace-12345.json")),
            None
        );
        assert_eq!(
            trace_started_display(std::path::Path::new("trace-20250817-2130xx-node.json")),
            None
        );
    }

    #[test]
    fn render_trace_viewer_injects_filename_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace-20250817-213059-node-a1b2c3.json");
        std::fs::write(
            &path,
            r#"{"plan":"node","nodes":[],"edges":[],"events":[]}"#,
        )
        .unwrap();
        let html_path = render_trace_viewer(&path).unwrap();
        let html = std::fs::read_to_string(&html_path).unwrap();
        // The filename timestamp surfaces as a display-only `started_at` field
        // in the injected trace JSON, and the viewer template is tabbed.
        assert!(html.contains(r#""started_at":"2025-08-17 21:30:59""#));
        assert!(html.contains(r#"id="tabs""#));
        assert!(html.contains("role=\"tablist\""));
        let _ = std::fs::remove_file(&html_path);
    }

    #[test]
    fn trace_shape_requires_the_viewer_collections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid.json");
        std::fs::write(&path, r#"{"plan":"node","nodes":{}}"#).unwrap();
        let error = render_trace_viewer(&path).unwrap_err();
        assert!(format!("{error:#}").contains("trace field `nodes` must be an array"));
    }

    #[cfg(unix)]
    #[test]
    fn viewer_replaces_a_prepositioned_symlink_without_following_it() {
        use std::os::unix::{fs::symlink, fs::PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let stem = format!("symlink-safety-{}", std::process::id());
        let source = dir.path().join(format!("{stem}.json"));
        std::fs::write(
            &source,
            r#"{"plan":"node","nodes":[],"edges":[],"events":[]}"#,
        )
        .unwrap();

        let output = std::env::temp_dir().join(format!("glu-{stem}.html"));
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"do not replace").unwrap();
        let _ = std::fs::remove_file(&output);
        symlink(&victim, &output).unwrap();

        let rendered = render_trace_viewer(&source).unwrap();
        assert_eq!(rendered, output);
        assert_eq!(std::fs::read(&victim).unwrap(), b"do not replace");
        assert!(!std::fs::symlink_metadata(&rendered)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::metadata(&rendered).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_file(rendered);
    }
}
