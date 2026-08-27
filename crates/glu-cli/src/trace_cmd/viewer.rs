use anyhow::{Context, Result};

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
/// Unix-only: the source trace is read, `render_html` is applied, and the
/// result is written to `$TMPDIR/glu-trace-<name>-<id>.html`.
pub(super) fn render_trace_viewer(source: &std::path::Path) -> Result<std::path::PathBuf> {
    let text = std::fs::read_to_string(source).context("reading trace")?;
    let mut trace: serde_json::Value =
        serde_json::from_str(&text).context("trace is not valid JSON")?;
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

    let mut out_dir = std::env::temp_dir();
    let file_name = format!("glu-{name}.html");
    out_dir.push(file_name);
    std::fs::write(&out_dir, html).context("writing trace HTML")?;
    Ok(out_dir)
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
        let dir = std::env::temp_dir().join(format!("glu-render-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace-20250817-213059-node-a1b2c3.json");
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
